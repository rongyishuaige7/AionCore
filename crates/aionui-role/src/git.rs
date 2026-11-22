//! Git version history (`git2`) for resident roles and the shared boss profile.
//!
//! Each role directory `<data_dir>/roles/<role_dir>/` owns an isolated git
//! repository tracking durable identity and learned knowledge (`role.json`,
//! `memory/MEMORY.md`, `memory/PLAYBOOK.md`, `memory/PLAYBOOK-archived.md`).
//! High-frequency runtime state (`thread.json`, `thread-leaves/`, `progress/`,
//! `proposals.json`, `dreamer.json`) stays out of version control.

use std::path::{Path, PathBuf};

use aionui_api_types::RoleCommit;
use git2::build::CheckoutBuilder;
use git2::{DiffFormat, DiffOptions, ErrorCode, Oid, Repository, Signature, Sort, Time};

use crate::error::RoleError;

const GITIGNORE_CONTENT: &str = "\
thread.json
thread-leaves/
proposals.json
dreamer.json
.playbook_meta.json
memory/archive/
memory/progress/
memory/reported.jsonl
projects/
debriefs.jsonl
*.tmp
";

/// Tracked paths inside a role repository.
pub(crate) const ROLE_TRACKED_PATHS: &[&str] = &[
    ".gitignore",
    "role.json",
    "memory/MEMORY.md",
    "memory/PLAYBOOK.md",
    "memory/PLAYBOOK-archived.md",
];

/// Tracked paths inside `<data_dir>/roles/_boss/`.
pub(crate) const BOSS_TRACKED_PATHS: &[&str] = &[".gitignore", "USER.md"];

const MAX_DIFF_CHARS: usize = 4_000;
const AUTHOR_EMAIL: &str = "role@aionui.local";

/// Snapshot an existing unversioned directory BEFORE mutating it so the very
/// first change has a parent commit to diff against and revert to.
pub(crate) async fn ensure_baseline(
    repo_dir: PathBuf,
    tracked_paths: &'static [&'static str],
    at_ms: i64,
) -> Result<(), RoleError> {
    tokio::task::spawn_blocking(move || ensure_baseline_sync(&repo_dir, tracked_paths, at_ms))
        .await
        .map_err(|err| RoleError::Io(std::io::Error::other(err)))?
}

/// Record one commit over `tracked_paths` if any tracked file changed relative
/// to `HEAD`. Returns the new commit SHA, or `None` when nothing changed.
pub(crate) async fn commit(
    repo_dir: PathBuf,
    tracked_paths: &'static [&'static str],
    actor: String,
    message: String,
    at_ms: i64,
) -> Result<Option<String>, RoleError> {
    tokio::task::spawn_blocking(move || commit_sync(&repo_dir, tracked_paths, &actor, &message, at_ms))
        .await
        .map_err(|err| RoleError::Io(std::io::Error::other(err)))?
}

/// Read the newest `limit` commits in `repo_dir`, newest first.
pub(crate) async fn timeline(repo_dir: PathBuf, limit: usize) -> Result<Vec<RoleCommit>, RoleError> {
    tokio::task::spawn_blocking(move || timeline_sync(&repo_dir, limit))
        .await
        .map_err(|err| RoleError::Io(std::io::Error::other(err)))?
}

/// Revert `sha` by creating a new forward commit on `HEAD` and checking out the
/// tracked files. Fails closed on merge conflicts or root commits, and never
/// touches untracked/ignored runtime files.
pub(crate) async fn revert(
    repo_dir: PathBuf,
    tracked_paths: &'static [&'static str],
    sha: String,
    actor: String,
    at_ms: i64,
) -> Result<RoleCommit, RoleError> {
    tokio::task::spawn_blocking(move || revert_sync(&repo_dir, tracked_paths, &sha, &actor, at_ms))
        .await
        .map_err(|err| RoleError::Io(std::io::Error::other(err)))?
}

fn git_err(err: git2::Error) -> RoleError {
    RoleError::Io(std::io::Error::other(err))
}

fn ensure_gitignore(repo_dir: &Path) -> Result<(), RoleError> {
    std::fs::create_dir_all(repo_dir)?;
    let ignore_path = repo_dir.join(".gitignore");
    let current = match std::fs::read_to_string(&ignore_path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&ignore_path, GITIGNORE_CONTENT)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(updated) = with_ignored_runtime_files(&current) {
        std::fs::write(&ignore_path, updated)?;
    }
    Ok(())
}

/// `current` with every runtime path a newer release keeps out of history
/// appended, keeping whatever the file already holds; `None` when nothing is
/// missing. Repositories made before a path was added learn it this way.
fn with_ignored_runtime_files(current: &str) -> Option<String> {
    let present: std::collections::HashSet<&str> = current.lines().map(str::trim).collect();
    let missing: Vec<&str> = GITIGNORE_CONTENT
        .lines()
        .filter(|line| !line.is_empty() && !present.contains(line))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut updated = current.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    for line in missing {
        updated.push_str(line);
        updated.push('\n');
    }
    Some(updated)
}

fn open_or_init(repo_dir: &Path) -> Result<(Repository, bool), RoleError> {
    if repo_dir.join(".git").exists() {
        let repo = Repository::open(repo_dir).map_err(git_err)?;
        Ok((repo, false))
    } else {
        ensure_gitignore(repo_dir)?;
        let repo = Repository::init(repo_dir).map_err(git_err)?;
        Ok((repo, true))
    }
}

fn ensure_baseline_sync(repo_dir: &Path, tracked_paths: &[&str], at_ms: i64) -> Result<(), RoleError> {
    if !repo_dir.exists() || repo_dir.join(".git").exists() {
        return Ok(());
    }
    let has_any_tracked = tracked_paths
        .iter()
        .filter(|p| **p != ".gitignore")
        .any(|rel| repo_dir.join(rel).exists());
    if !has_any_tracked {
        return Ok(());
    }
    let _ = commit_sync(
        repo_dir,
        tracked_paths,
        "system",
        "init: snapshot existing role files",
        at_ms,
    )?;
    Ok(())
}

fn commit_sync(
    repo_dir: &Path,
    tracked_paths: &[&str],
    actor: &str,
    message: &str,
    at_ms: i64,
) -> Result<Option<String>, RoleError> {
    ensure_gitignore(repo_dir)?;
    let (repo, _) = open_or_init(repo_dir)?;
    let mut index = repo.index().map_err(git_err)?;

    for rel in tracked_paths {
        let full = repo_dir.join(rel);
        if full.is_file() {
            index.add_path(Path::new(rel)).map_err(git_err)?;
        } else {
            let _ = index.remove_path(Path::new(rel));
        }
    }
    index.write().map_err(git_err)?;
    let tree_oid = index.write_tree().map_err(git_err)?;
    let tree = repo.find_tree(tree_oid).map_err(git_err)?;

    let parent = match repo.head() {
        Ok(head) => Some(head.peel_to_commit().map_err(git_err)?),
        Err(err) if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound => None,
        Err(err) => return Err(git_err(err)),
    };
    if let Some(ref parent_commit) = parent
        && parent_commit.tree_id() == tree_oid
    {
        return Ok(None);
    }

    let secs = (at_ms / 1_000).max(0);
    let sig = Signature::new(actor, AUTHOR_EMAIL, &Time::new(secs, 0)).map_err(git_err)?;
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(git_err)?;
    Ok(Some(oid.to_string()))
}

fn timeline_sync(repo_dir: &Path, limit: usize) -> Result<Vec<RoleCommit>, RoleError> {
    if !repo_dir.join(".git").exists() {
        return Ok(Vec::new());
    }
    let repo = Repository::open(repo_dir).map_err(git_err)?;
    let mut revwalk = repo.revwalk().map_err(git_err)?;
    match revwalk.push_head() {
        Ok(()) => {}
        Err(err) if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(git_err(err)),
    }
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME).map_err(git_err)?;

    let mut out = Vec::new();
    for oid_res in revwalk.take(limit) {
        let oid = oid_res.map_err(git_err)?;
        let commit = repo.find_commit(oid).map_err(git_err)?;
        out.push(describe_commit(&repo, &commit)?);
    }
    Ok(out)
}

fn revert_sync(
    repo_dir: &Path,
    tracked_paths: &[&str],
    sha: &str,
    actor: &str,
    at_ms: i64,
) -> Result<RoleCommit, RoleError> {
    if !repo_dir.join(".git").exists() {
        return Err(RoleError::InvalidRequest("role has no commit history yet".into()));
    }
    let repo = Repository::open(repo_dir).map_err(git_err)?;
    let target_oid = Oid::from_str(sha.trim())
        .or_else(|_| repo.revparse_single(sha.trim()).map(|obj| obj.id()))
        .map_err(|_| RoleError::InvalidRequest(format!("unknown commit: {sha}")))?;
    let target = repo
        .find_commit(target_oid)
        .map_err(|_| RoleError::InvalidRequest(format!("unknown commit: {sha}")))?;

    if target.parent_count() != 1 {
        return Err(RoleError::InvalidRequest(
            "the initial snapshot commit cannot be reverted".into(),
        ));
    }

    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|_| RoleError::InvalidRequest("role repository has no HEAD commit".into()))?;

    let mut revert_index = repo.revert_commit(&target, &head, 0, None).map_err(git_err)?;
    if revert_index.has_conflicts() {
        return Err(RoleError::InvalidRequest(
            "cannot cleanly revert this commit because newer edits touch the same lines".into(),
        ));
    }

    let tree_oid = revert_index.write_tree_to(&repo).map_err(git_err)?;
    if tree_oid == head.tree_id() {
        return Err(RoleError::InvalidRequest(
            "this commit's changes are already reverted".into(),
        ));
    }
    let tree = repo.find_tree(tree_oid).map_err(git_err)?;

    let short_sha: String = target.id().to_string().chars().take(7).collect();
    let subject = target.summary().ok().flatten().unwrap_or("change");
    let message = format!("revert: {short_sha} ({subject})");
    let secs = (at_ms / 1_000).max(0);
    let sig = Signature::new(actor, AUTHOR_EMAIL, &Time::new(secs, 0)).map_err(git_err)?;
    let new_oid = repo
        .commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&head])
        .map_err(git_err)?;

    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    for path in tracked_paths {
        checkout.path(path);
    }
    repo.checkout_head(Some(&mut checkout)).map_err(git_err)?;

    let created = repo.find_commit(new_oid).map_err(git_err)?;
    describe_commit(&repo, &created)
}

fn describe_commit(repo: &Repository, commit: &git2::Commit<'_>) -> Result<RoleCommit, RoleError> {
    let sha = commit.id().to_string();
    let short_sha: String = sha.chars().take(7).collect();
    let at = commit.time().seconds().saturating_mul(1_000);
    let actor = commit.author().name().unwrap_or("system").to_owned();
    let message = commit.summary().ok().flatten().unwrap_or("").to_owned();
    let can_revert = commit.parent_count() == 1;

    let commit_tree = commit.tree().map_err(git_err)?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0).and_then(|p| p.tree()).map_err(git_err)?)
    } else {
        None
    };

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(2);
    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(&mut diff_opts))
        .map_err(git_err)?;

    let stats = diff.stats().map_err(git_err)?;
    let additions = stats.insertions();
    let deletions = stats.deletions();

    let mut files = Vec::new();
    for delta in diff.deltas() {
        if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
            let rel = path.to_string_lossy().into_owned();
            if rel != ".gitignore" && !files.contains(&rel) {
                files.push(rel);
            }
        }
    }

    let mut rendered = String::new();
    let _ = diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let is_gitignore = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .is_some_and(|p| p == Path::new(".gitignore"));
        if is_gitignore {
            return true;
        }
        if rendered.len() > MAX_DIFF_CHARS * 4 {
            return false;
        }
        let origin = line.origin();
        if matches!(origin, '+' | '-' | ' ') {
            rendered.push(origin);
        }
        if let Ok(text) = std::str::from_utf8(line.content()) {
            rendered.push_str(text);
        }
        true
    });

    if rendered.chars().count() > MAX_DIFF_CHARS {
        rendered = format!(
            "{}\n…",
            rendered.chars().take(MAX_DIFF_CHARS).collect::<String>().trim_end()
        );
    }

    Ok(RoleCommit {
        sha,
        short_sha,
        at,
        actor,
        message,
        files,
        additions,
        deletions,
        diff: rendered.trim_end().to_owned(),
        can_revert,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_ignore_file_learns_the_runtime_paths_it_misses_and_keeps_its_own() {
        let old = "thread.json\nmy-notes/\n";
        let updated = with_ignored_runtime_files(old).expect("paths missing");
        assert!(updated.starts_with(old));
        for line in ["projects/", "debriefs.jsonl", "memory/progress/"] {
            assert!(updated.lines().any(|l| l == line), "{line} missing");
        }
        assert_eq!(updated.matches("thread.json").count(), 1);
        assert_eq!(with_ignored_runtime_files(&updated), None);
        assert_eq!(with_ignored_runtime_files(GITIGNORE_CONTENT), None);
        assert!(with_ignored_runtime_files("thread.json").is_some_and(|u| u.starts_with("thread.json\n")));
    }
}
