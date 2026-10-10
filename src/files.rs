//! Untracked files the worktree needs from the main checkout: `app.copy`
//! paths and SQLite databases.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sh;

/// Copies whatever `src` holds that `dst` lacks; never overwrites, so files
/// created or changed inside the worktree survive a re-run. `fs::copy` is a
/// clonefile on APFS, so a large tree costs nothing until something writes.
pub fn copy_missing(src: &Path, dst: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(src)?;
    if fs::symlink_metadata(dst).is_ok() && !meta.is_dir() {
        return Ok(0);
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
        copied += copy_missing(&entry.path(), &dst.join(entry.file_name()))?;
    }
    Ok(copied)
}

pub fn copy_paths(main: &Path, worktree: &Path, paths: &[String]) -> Result<()> {
    for rel in paths {
        let src = main.join(rel);
        if !src.exists() {
            println!("Skipping {rel}: not in the main checkout");
            continue;
        }
        let n = copy_missing(&src, &worktree.join(rel))
            .with_context(|| format!("could not copy {rel}"))?;
        if n > 0 {
            println!("Copied {rel} ({n} files)");
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
}
