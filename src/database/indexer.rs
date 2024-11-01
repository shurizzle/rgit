use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::OsStr,
    fmt::Debug,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use git2::{ErrorCode, Reference, Sort};
use ini::Ini;
use itertools::Itertools;
use rocksdb::WriteBatch;
use time::OffsetDateTime;
use tracing::{error, info, info_span, instrument, warn};

use crate::database::schema::{
    commit::Commit,
    repository::{Repository, RepositoryId},
    tag::{Tag, TagTree},
};

pub fn run(scan_path: &Path, projects_list: Option<&Path>, db: &Arc<rocksdb::DB>) {
    let span = info_span!("index_update");
    let _entered = span.enter();

    info!("Starting index update");

    let projects = update_repository_metadata(scan_path, projects_list, db);
    update_repository_reflog(scan_path, projects.as_ref(), db.clone());
    update_repository_tags(scan_path, projects.as_ref(), db.clone());

    info!("Flushing to disk");

    if let Err(error) = db.flush() {
        error!(%error, "Failed to flush database to disk");
    }

    info!("Finished index update");
}

#[instrument(skip(db))]
fn update_repository_metadata(
    scan_path: &Path,
    projects_list: Option<&Path>,
    db: &rocksdb::DB,
) -> Option<HashSet<PathBuf>> {
    let mut discovered = Vec::new();
    discover_repositories(scan_path, projects_list, &mut discovered);
    let mut projects = if projects_list.is_some() {
        Some(HashSet::new())
    } else {
        None
    };

    for repository in discovered {
        let Some(relative) = get_relative_path(scan_path, &repository) else {
            continue;
        };

        let id = match Repository::open(db, relative) {
            Ok(v) => v.map_or_else(RepositoryId::new, |v| v.get().id),
            Err(error) => {
                // maybe we could nuke it ourselves, but we need to instantly trigger
                // a reindex and we could enter into an infinite loop if there's a bug
                // or something
                error!(%error, "Failed to open repository index {}, please consider nuking database", relative.display());
                continue;
            }
        };

        let Some(name) = relative.file_name().map(OsStr::to_string_lossy) else {
            continue;
        };
        let description = std::fs::read(repository.join("description")).unwrap_or_default();
        let description = Some(String::from_utf8_lossy(&description)).filter(|v| !v.is_empty());

        let repository_path = scan_path.join(relative);

        let git_repository = match git2::Repository::open(repository_path.clone()) {
            Ok(v) => v,
            Err(error) => {
                warn!(%error, "Failed to open repository {} to update metadata, skipping", relative.display());
                continue;
            }
        };
        projects.as_mut().map(|p| p.insert(relative.to_path_buf()));

        let res = Repository {
            id,
            name,
            description,
            owner: find_gitweb_owner(repository_path.as_path()),
            last_modified: find_last_committed_time(&git_repository)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            default_branch: find_default_branch(&git_repository)
                .ok()
                .flatten()
                .map(Cow::Owned),
        }
        .insert(db, relative);

        if let Err(error) = res {
            warn!(%error, "Failed to insert repository");
        }
    }

    projects
}

fn find_default_branch(repo: &git2::Repository) -> Result<Option<String>, git2::Error> {
    Ok(repo.head()?.name().map(ToString::to_string))
}

fn find_last_committed_time(repo: &git2::Repository) -> Result<OffsetDateTime, git2::Error> {
    let mut timestamp = OffsetDateTime::UNIX_EPOCH;

    for reference in repo.references()? {
        let Ok(commit) = reference?.peel_to_commit() else {
            continue;
        };

        let committed_time = commit.committer().when().seconds();
        let committed_time = OffsetDateTime::from_unix_timestamp(committed_time)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);

        if committed_time > timestamp {
            timestamp = committed_time;
        }
    }

    Ok(timestamp)
}

#[instrument(skip(projects, db))]
fn update_repository_reflog(
    scan_path: &Path,
    projects: Option<&HashSet<PathBuf>>,
    db: Arc<rocksdb::DB>,
) {
    let repos = match Repository::fetch_all(&db) {
        Ok(v) => v,
        Err(error) => {
            error!(%error, "Failed to read repository index to update reflog, consider deleting database directory");
            return;
        }
    };

    for (relative_path, db_repository) in repos {
        let Some(git_repository) = open_repo(
            scan_path,
            &relative_path,
            projects,
            db_repository.get(),
            &db,
        ) else {
            continue;
        };

        let references = match git_repository.references() {
            Ok(v) => v,
            Err(error) => {
                error!(%error, "Failed to read references for {relative_path}");
                continue;
            }
        };

        let mut valid_references = Vec::new();

        for reference in references.filter_map(Result::ok) {
            let reference_name = String::from_utf8_lossy(reference.name_bytes());
            if !reference_name.starts_with("refs/heads/")
                && !reference_name.starts_with("refs/tags/")
            {
                continue;
            }

            valid_references.push(reference_name.to_string());

            if let Err(error) = branch_index_update(
                &reference,
                &reference_name,
                &relative_path,
                db_repository.get(),
                db.clone(),
                &git_repository,
                false,
            ) {
                error!(%error, "Failed to update reflog for {relative_path}@{reference_name}");
            }
        }

        if let Err(error) = db_repository.get().replace_heads(&db, &valid_references) {
            error!(%error, "Failed to update heads");
        }
    }
}

#[instrument(skip(reference, db_repository, db, git_repository))]
fn branch_index_update(
    reference: &Reference<'_>,
    reference_name: &str,
    relative_path: &str,
    db_repository: &Repository<'_>,
    db: Arc<rocksdb::DB>,
    git_repository: &git2::Repository,
    force_reindex: bool,
) -> Result<(), anyhow::Error> {
    info!("Refreshing indexes");

    let commit_tree = db_repository.commit_tree(db.clone(), reference_name);

    if force_reindex {
        commit_tree.drop_commits()?;
    }

    let commit = reference.peel_to_commit()?;

    let latest_indexed = if let Some(latest_indexed) = commit_tree.fetch_latest_one()? {
        if commit.id().as_bytes() == &*latest_indexed.get().hash {
            info!("No commits since last index");
            return Ok(());
        }

        Some(latest_indexed)
    } else {
        None
    };

    let mut revwalk = git_repository.revwalk()?;
    revwalk.set_sorting(Sort::REVERSE)?;
    revwalk.push_ref(reference_name)?;

    let tree_len = commit_tree.len()?;
    let mut seen = false;
    let mut i = 0;
    for revs in &revwalk.chunks(250) {
        let mut batch = WriteBatch::default();

        for rev in revs {
            let rev = rev?;

            if let (false, Some(latest_indexed)) = (seen, &latest_indexed) {
                if rev.as_bytes() == &*latest_indexed.get().hash {
                    seen = true;
                }

                continue;
            }

            seen = true;

            if ((i + 1) % 25_000) == 0 {
                info!("{} commits ingested", i + 1);
            }

            let commit = git_repository.find_commit(rev)?;
            let author = commit.author();
            let committer = commit.committer();

            Commit::new(&commit, &author, &committer).insert(
                &commit_tree,
                tree_len + i,
                &mut batch,
            )?;
            i += 1;
        }

        commit_tree.update_counter(tree_len + i, &mut batch)?;
        db.write_without_wal(batch)?;
    }

    if !seen && !force_reindex {
        warn!("Detected converged history, forcing reindex");

        return branch_index_update(
            reference,
            reference_name,
            relative_path,
            db_repository,
            db,
            git_repository,
            true,
        );
    }

    Ok(())
}

#[instrument(skip(projects, db))]
fn update_repository_tags(
    scan_path: &Path,
    projects: Option<&HashSet<PathBuf>>,
    db: Arc<rocksdb::DB>,
) {
    let repos = match Repository::fetch_all(&db) {
        Ok(v) => v,
        Err(error) => {
            error!(%error, "Failed to read repository index to update tags, consider deleting database directory");
            return;
        }
    };

    for (relative_path, db_repository) in repos {
        let Some(git_repository) = open_repo(
            scan_path,
            &relative_path,
            projects,
            db_repository.get(),
            &db,
        ) else {
            continue;
        };

        if let Err(error) = tag_index_scan(
            &relative_path,
            db_repository.get(),
            db.clone(),
            &git_repository,
        ) {
            error!(%error, "Failed to update tags for {relative_path}");
        }
    }
}

#[instrument(skip(db_repository, db, git_repository))]
fn tag_index_scan(
    relative_path: &str,
    db_repository: &Repository<'_>,
    db: Arc<rocksdb::DB>,
    git_repository: &git2::Repository,
) -> Result<(), anyhow::Error> {
    let tag_tree = db_repository.tag_tree(db);

    let git_tags: HashSet<_> = git_repository
        .references()
        .context("Failed to scan indexes on git repository")?
        .filter_map(Result::ok)
        .filter(|v| v.name_bytes().starts_with(b"refs/tags/"))
        .map(|v| String::from_utf8_lossy(v.name_bytes()).into_owned())
        .collect();
    let indexed_tags: HashSet<String> = tag_tree.list()?.into_iter().collect();

    // insert any git tags that are missing from the index
    for tag_name in git_tags.difference(&indexed_tags) {
        tag_index_update(tag_name, git_repository, &tag_tree)?;
    }

    // remove any extra tags that the index has
    // TODO: this also needs to check peel_to_tag
    for tag_name in indexed_tags.difference(&git_tags) {
        tag_index_delete(tag_name, &tag_tree)?;
    }

    Ok(())
}

#[instrument(skip(git_repository, tag_tree))]
fn tag_index_update(
    tag_name: &str,
    git_repository: &git2::Repository,
    tag_tree: &TagTree,
) -> Result<(), anyhow::Error> {
    let reference = git_repository
        .find_reference(tag_name)
        .context("Failed to read newly discovered tag")?;

    if let Ok(tag) = reference.peel_to_tag() {
        info!("Inserting newly discovered tag to index");

        Tag::new(tag.tagger().as_ref()).insert(tag_tree, tag_name)?;
    }

    Ok(())
}

#[instrument(skip(tag_tree))]
fn tag_index_delete(tag_name: &str, tag_tree: &TagTree) -> Result<(), anyhow::Error> {
    info!("Removing stale tag from index");
    tag_tree.remove(tag_name)?;

    Ok(())
}

#[instrument(skip(scan_path, projects, db_repository, db))]
fn open_repo<P: AsRef<Path> + Debug>(
    scan_path: &Path,
    relative_path: P,
    projects: Option<&HashSet<PathBuf>>,
    db_repository: &Repository<'_>,
    db: &rocksdb::DB,
) -> Option<git2::Repository> {
    if let Some(projects) = projects {
        if !projects.contains(relative_path.as_ref()) {
            warn!("Repository gone from projects file, removing from db");

            if let Err(error) = db_repository.delete(db, relative_path) {
                warn!(%error, "Failed to delete dangling index");
            }

            return None;
        }
    }

    match git2::Repository::open(scan_path.join(relative_path.as_ref())) {
        Ok(v) => Some(v),
        Err(e) if e.code() == ErrorCode::NotFound => {
            warn!("Repository gone from disk, removing from db");

            if let Err(error) = db_repository.delete(db, relative_path) {
                warn!(%error, "Failed to delete dangling index");
            }

            None
        }
        Err(error) => {
            warn!(%error, "Failed to reindex, skipping");
            None
        }
    }
}

fn get_relative_path<'a>(relative_to: &Path, full_path: &'a Path) -> Option<&'a Path> {
    full_path.strip_prefix(relative_to).ok()
}

fn discover_repositories(
    current: &Path,
    projects_list: Option<&Path>,
    discovered_repos: &mut Vec<PathBuf>,
) {
    if let Some(projects_list) = projects_list {
        let mut f = match File::open(projects_list) {
            Ok(f) => BufReader::new(f),
            Err(error) => {
                error!(%error, "Failed to open projects.list {}", projects_list.display());
                return;
            }
        };
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match f.read_line(&mut buffer) {
                Ok(0) => return,
                Ok(_) => (),
                Err(error) => {
                    error!(%error, "Failed to read projects.list {}", projects_list.display());
                    return;
                }
            };
            let line = buffer.strip_suffix('\n').map_or(buffer.as_str(), |line| {
                line.strip_suffix('r').unwrap_or(line)
            });
            discovered_repos.push(current.join(line));
        }
    } else {
        discover_repositories_recursive(current, discovered_repos);
    }
}

fn discover_repositories_recursive(current: &Path, discovered_repos: &mut Vec<PathBuf>) {
    let current = match std::fs::read_dir(current) {
        Ok(v) => v,
        Err(error) => {
            error!(%error, "Failed to enter repository directory {}", current.display());
            return;
        }
    };

    let dirs = current
        .filter_map(Result::ok)
        .map(|v| v.path())
        .filter(|path| path.is_dir());

    for dir in dirs {
        if dir.join("packed-refs").is_file() {
            // we've hit what looks like a bare git repo, lets take it
            discovered_repos.push(dir);
        } else {
            // probably not a bare git repo, lets recurse deeper
            discover_repositories_recursive(&dir, discovered_repos);
        }
    }
}

fn find_gitweb_owner(repository_path: &Path) -> Option<Cow<'_, str>> {
    // Load the Git config file and attempt to extract the owner from the "gitweb" section.
    // If the owner is not found, an empty string is returned.
    Ini::load_from_file(repository_path.join("config"))
        .ok()?
        .section(Some("gitweb"))
        .and_then(|section| section.get("owner"))
        .map(String::from)
        .map(Cow::Owned)
}
