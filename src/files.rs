//! Untracked files the worktree needs from the main checkout: `app.copy`
//! paths and SQLite databases.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sh;

/// How a copy treats a file the worktree already has.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Keep it, so files created or changed inside the worktree survive.
    Missing,
    /// Replace it when the content differs; the main checkout wins.
    Refresh,
}

/// Copies whatever `src` holds that `dst` lacks; never overwrites, so files
/// created or changed inside the worktree survive a re-run. `fs::copy` is a
/// clonefile on APFS, so a large tree costs nothing until something writes.
pub fn copy_missing(src: &Path, dst: &Path) -> io::Result<u64> {
    copy_tree(src, dst, Mode::Missing)
}

/// Like `copy_missing`, but a file whose content differs is replaced, so an
/// edit in the main checkout reaches worktrees that already exist. Files only
/// the worktree has are left alone.
pub fn copy_refresh(src: &Path, dst: &Path) -> io::Result<u64> {
    copy_tree(src, dst, Mode::Refresh)
}

fn copy_tree(src: &Path, dst: &Path, mode: Mode) -> io::Result<u64> {
    let meta = fs::symlink_metadata(src)?;
    let existing = fs::symlink_metadata(dst).ok();
    if existing.is_some() && !meta.is_dir() {
        if mode == Mode::Missing {
            return Ok(0);
        }
        if meta.is_file() && existing.as_ref().is_some_and(|e| e.is_file()) {
            if same_content(src, dst)? {
                return Ok(0);
            }
        } else if meta.file_type().is_symlink()
            && fs::read_link(dst).ok() == fs::read_link(src).ok()
        {
            return Ok(0);
        }
        fs::remove_file(dst)?;
    }
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(src)?, dst)?;
        return Ok(1);
    }
    if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        return Ok(1);
    }
    fs::create_dir_all(dst)?;
    let mut copied = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        copied += copy_tree(&entry.path(), &dst.join(entry.file_name()), mode)?;
    }
    Ok(copied)
}

fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

pub fn copy_paths(main: &Path, worktree: &Path, paths: &[String]) -> Result<()> {
    copy_listed(main, worktree, paths, "Copied", copy_missing)
}

/// `app.refresh`: paths kept in step with the main checkout on every run.
pub fn refresh_paths(main: &Path, worktree: &Path, paths: &[String]) -> Result<()> {
    copy_listed(main, worktree, paths, "Refreshed", copy_refresh)
}

fn copy_listed(
    main: &Path,
    worktree: &Path,
    paths: &[String],
    verb: &str,
    copy: fn(&Path, &Path) -> io::Result<u64>,
) -> Result<()> {
    for rel in paths {
        let src = main.join(rel);
        if !src.exists() {
            println!("Skipping {rel}: not in the main checkout");
            continue;
        }
        let n = copy(&src, &worktree.join(rel)).with_context(|| format!("could not copy {rel}"))?;
        if n > 0 {
            println!("{verb} {rel} ({n} files)");
        }
    }
    Ok(())
}

/// Seeds each database once. `.backup` takes a consistent snapshot even while
/// the main app holds the file open; a plain copy of the file and its WAL is
/// the fallback without the sqlite3 CLI.
pub fn copy_sqlite(main: &Path, worktree: &Path, files: &[String]) -> Result<()> {
    let has_cli = sh::which("sqlite3");
    for rel in files {
        let (src, dst) = (main.join(rel), worktree.join(rel));
        if dst.exists() {
            println!("Reusing {rel}");
            continue;
        }
        if !src.exists() {
            println!("Skipping {rel}: not in the main checkout");
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if has_cli {
            let target = dst.to_string_lossy().replace('\'', "''");
            sh::output(
                Command::new("sqlite3")
                    .arg(&src)
                    .arg(format!(".backup '{target}'")),
            )?;
        } else {
            for suffix in ["", "-wal", "-shm"] {
                let from = main.join(format!("{rel}{suffix}"));
                if from.exists() {
                    fs::copy(&from, worktree.join(format!("{rel}{suffix}")))?;
                }
            }
        }
        println!("Copied {rel}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wt-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_only_what_is_missing() {
        let root = scratch("copy");
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("ab/cd")).unwrap();
        fs::write(src.join("ab/cd/blob"), "main").unwrap();
        fs::write(src.join(".keep"), "").unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join(".keep"), "").unwrap();
        fs::create_dir_all(dst.join("ab/cd")).unwrap();

        assert_eq!(copy_missing(&src, &dst).unwrap(), 1);
        assert_eq!(fs::read_to_string(dst.join("ab/cd/blob")).unwrap(), "main");

        fs::write(dst.join("ab/cd/blob"), "changed in worktree").unwrap();
        assert_eq!(copy_missing(&src, &dst).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(dst.join("ab/cd/blob")).unwrap(),
            "changed in worktree"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_replaces_changed_files_and_keeps_extras() {
        let root = scratch("refresh");
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("agents")).unwrap();
        fs::write(src.join("agents/a.md"), "v2").unwrap();
        fs::write(src.join("agents/same.md"), "same").unwrap();
        fs::create_dir_all(dst.join("agents")).unwrap();
        fs::write(dst.join("agents/a.md"), "v1").unwrap();
        fs::write(dst.join("agents/same.md"), "same").unwrap();
        fs::write(dst.join("agents/local.md"), "mine").unwrap();

        assert_eq!(copy_refresh(&src, &dst).unwrap(), 1);
        assert_eq!(fs::read_to_string(dst.join("agents/a.md")).unwrap(), "v2");
        assert_eq!(
            fs::read_to_string(dst.join("agents/local.md")).unwrap(),
            "mine"
        );
        assert_eq!(copy_refresh(&src, &dst).unwrap(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
