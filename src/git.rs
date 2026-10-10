use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::sh;

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

pub fn common_dir() -> Result<PathBuf> {
    let out = sh::output(Command::new("git").args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))?;
    Ok(PathBuf::from(out.trim()))
}

pub fn toplevel() -> Option<PathBuf> {
    sh::output(Command::new("git").args(["rev-parse", "--show-toplevel"]))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
}

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub fn list(main: &Path) -> Result<Vec<Entry>> {
    Ok(parse_porcelain(&sh::output(git(main).args([
        "worktree",
        "list",
        "--porcelain",
    ]))?))
}

fn parse_porcelain(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.push(Entry {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(last) = entries.last_mut()
        {
            last.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
        }
    }
    entries
}

pub fn registered(main: &Path, path: &Path) -> Result<bool> {
    Ok(list(main)?.iter().any(|e| e.path == path))
}

pub fn holder_of(main: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(list(main)?
        .into_iter()
        .find(|e| e.branch.as_deref() == Some(branch))
        .map(|e| e.path))
}

pub fn branch_exists(main: &Path, branch: &str) -> bool {
    sh::succeeds(git(main).args([
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ]))
}

/// Checks out `branch` at `path`, creating it from `base` when it is new.
pub fn add(main: &Path, path: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    let mut cmd = git(main);
    cmd.args(["worktree", "add", "--quiet"]);
    if branch_exists(main, branch) {
        cmd.arg(path).arg(branch);
    } else {
        cmd.args(["-b", branch])
            .arg(path)
            .arg(base.unwrap_or("HEAD"));
    }
    sh::output(&mut cmd).map(drop)
}

pub fn remove(main: &Path, path: &Path, force: bool) -> Result<()> {
    let mut cmd = git(main);
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    sh::output(cmd.arg(path)).map(drop)
}

/// Clears registrations whose directory was deleted by hand.
pub fn prune(main: &Path) -> Result<()> {
    sh::output(git(main).args(["worktree", "prune"])).map(drop)
}

pub fn current_branch(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    sh::output(git(path).args(["rev-parse", "--abbrev-ref", "HEAD"]))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ignored files stay out of it, so node_modules, .env and copies stay quiet.
pub fn dirty(path: &Path) -> String {
    sh::output(git(path).args(["status", "--porcelain"])).unwrap_or_default()
}

pub fn is_tracked(repo: &Path, rel: &str) -> bool {
    sh::succeeds(git(repo).args(["ls-files", "--error-unmatch", rel]))
}

pub fn is_ignored(repo: &Path, rel: &str) -> bool {
    sh::succeeds(git(repo).args(["check-ignore", "-q", rel]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let text = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /w/feat-x\nHEAD def\nbranch refs/heads/feat/x\n\n\
                    worktree /w/detached\nHEAD 123\ndetached\n";
        let entries = parse_porcelain(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].path, PathBuf::from("/w/feat-x"));
        assert_eq!(entries[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[2].branch, None);
    }
}
