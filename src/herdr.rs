//! Herdr, when it is installed and its server answers. Every call has a plain
//! git fallback in the caller.

use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::sh;

/// HERDR_ENV is only set inside a Herdr pane, but the CLI reaches the server
/// from any terminal, so probe instead of trusting the variable.
pub fn available(main: &Path) -> bool {
    env::var("WT_NO_HERDR").as_deref() != Ok("1")
        && sh::which("herdr")
        && sh::succeeds(&mut list_cmd(main))
}

fn list_cmd(main: &Path) -> Command {
    let mut cmd = Command::new("herdr");
    cmd.args(["worktree", "list", "--cwd"]).arg(main);
    cmd
}

pub fn create(main: &Path, branch: &str, path: &Path, base: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("herdr");
    cmd.args(["worktree", "create", "--cwd"])
        .arg(main)
        .args(["--branch", branch, "--path"])
        .arg(path)
        .arg("--no-focus");
    if let Some(base) = base {
        cmd.args(["--base", base]);
    }
    if env::var("WT_TRUST_REPOSITORY").as_deref() == Ok("1") {
        cmd.arg("--trust-repository");
    }
    if sh::output(&mut cmd).is_err() {
        bail!("herdr worktree create failed; retry with WT_NO_HERDR=1 to use plain git");
    }
    Ok(())
}

/// The workspace the worktree is open in, if any; closing it removes it too.
pub fn workspace_id(main: &Path, path: &Path) -> Option<String> {
    let out = sh::output(&mut list_cmd(main)).ok()?;
    parse_workspace_id(&out, path)
}

fn parse_workspace_id(json: &str, path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Doc {
        result: Option<Res>,
    }
    #[derive(Deserialize)]
    struct Res {
        #[serde(default)]
        worktrees: Vec<Tree>,
    }
    #[derive(Deserialize)]
    struct Tree {
        path: String,
        open_workspace_id: Option<serde_json::Value>,
    }

    let doc: Doc = serde_json::from_str(json).ok()?;
    let tree = doc
        .result?
        .worktrees
        .into_iter()
        .find(|w| Path::new(&w.path) == path)?;
    match tree.open_workspace_id? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn remove(workspace: &str) -> Result<()> {
    sh::output(Command::new("herdr").args([
        "worktree",
        "remove",
        "--workspace",
        workspace,
        "--force",
    ]))
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_open_workspace() {
        let json = r#"{"result": {"worktrees": [
            {"path": "/w/a", "open_workspace_id": null},
            {"path": "/w/b", "open_workspace_id": "ws-7"}
        ]}}"#;
        assert_eq!(parse_workspace_id(json, Path::new("/w/a")), None);
        assert_eq!(
            parse_workspace_id(json, Path::new("/w/b")).as_deref(),
            Some("ws-7")
        );
        assert_eq!(parse_workspace_id(json, Path::new("/w/c")), None);
        assert_eq!(parse_workspace_id("not json", Path::new("/w/b")), None);
    }
}
