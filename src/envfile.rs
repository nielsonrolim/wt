//! The worktree's `.env` and `.envrc`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sh;

const BANNER: &str = "# Written by wt up";

/// Drops every definition of the keys we own (commented and `export`ed ones
/// too, so a copy further down cannot shadow ours) and our banner, then appends
/// fresh values. Running it on its own output changes nothing.
pub fn rewrite(source: &str, values: &[(String, String)]) -> String {
    let owned: Vec<&str> = values.iter().map(|(k, _)| k.as_str()).collect();
    let mut kept: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with(BANNER))
        .filter(|line| !defined_key(line).is_some_and(|k| owned.contains(&k)))
        .collect();
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }

    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(BANNER);
    out.push_str(": this worktree's own services and web port.\n");
    for (k, v) in values {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// The key a line assigns, whether live, commented out or exported.
fn defined_key(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    if let Some(r) = rest.strip_prefix('#') {
        rest = r.trim_start();
    }
    if let Some(r) = rest.strip_prefix("export")
        && r.starts_with(char::is_whitespace)
    {
        rest = r.trim_start();
    }
    let (key, _) = rest.split_once('=')?;
    let key = key.trim_end();
    let valid = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(key)
}

/// 0600: the copy carries the same secrets as the main checkout's file.
/// Created 0600 and narrowed before any byte lands, so the secrets are never
/// readable by others, not even for a moment or on a file that existed.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(content.as_bytes())
    };
    write().with_context(|| format!("could not write {}", path.display()))
}

/// A child process cannot export into the shell that called it, so leave a
/// file for direnv. Only the web port, deliberately: direnv unloads on leaving
/// the directory, but a shell that sourced this by hand does not, and a
/// database port following you back to the main checkout would point it at
/// this worktree's database.
pub fn write_envrc(dir: &Path, key: &str, port: u16) -> Result<()> {
    let path = dir.join(".envrc");
    fs::write(
        &path,
        format!(
            "{BANNER}. direnv loads this on cd; without it: source .envrc\nexport {key}={port}\n"
        ),
    )?;
    // direnv trusts a content hash, so every rewrite needs a fresh grant.
    if sh::which("direnv") {
        sh::succeeds(Command::new("direnv").arg("allow").arg(dir));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn strips_owned_keys_in_every_form() {
        let src = "SECRET=1\nDB_PORT=5432\n# DB_HOST=localhost\nexport PORT=3000\n#export DB_PORT=1\nDB_PORTS=keep\n";
        let out = rewrite(
            src,
            &vals(&[
                ("DB_HOST", "127.0.0.1"),
                ("DB_PORT", "5433"),
                ("PORT", "3001"),
            ]),
        );
        assert_eq!(
            out,
            "SECRET=1\nDB_PORTS=keep\n\n# Written by wt up: this worktree's own services and web port.\nDB_HOST=127.0.0.1\nDB_PORT=5433\nPORT=3001\n"
        );
    }

    #[test]
    fn is_idempotent() {
        let v = vals(&[("PORT", "3001"), ("REDIS_URL", "redis://127.0.0.1:6380/0")]);
        let once = rewrite("A=1\n\n\n", &v);
        assert_eq!(rewrite(&once, &v), once);
    }

    #[test]
    fn handles_empty_source() {
        let out = rewrite("", &vals(&[("PORT", "3001")]));
        assert!(out.starts_with(BANNER));
        assert!(out.ends_with("PORT=3001\n"));
    }

    #[test]
    fn reads_keys() {
        assert_eq!(defined_key("  export  FOO = bar"), Some("FOO"));
        assert_eq!(defined_key("# FOO=bar"), Some("FOO"));
        assert_eq!(defined_key("# a comment = with equals"), None);
        assert_eq!(defined_key("exporter=1"), Some("exporter"));
    }

    #[test]
    fn write_private_narrows_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("wt-envfile-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        fs::write(&path, "OLD=1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "NEW=1\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "NEW=1\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
