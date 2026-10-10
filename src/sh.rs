//! Thin helpers over `std::process::Command`, so every caller reports a failed
//! child the same way: the command line and its stderr.

use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

fn describe(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Captures stdout; a non-zero exit is an error carrying stderr.
pub fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {}", describe(cmd)))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            describe(cmd),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Captures stdout whatever the exit status. For tolerant lookups such as
/// `docker inspect` over ids that may vanish between listing and inspecting.
pub fn stdout_lossy(cmd: &mut Command) -> String {
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// True when the command exits zero; all output discarded.
pub fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs with the terminal attached, for output the user should watch.
pub fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("could not run {}", describe(cmd)))?;
    if !status.success() {
        bail!("{} failed ({status})", describe(cmd));
    }
    Ok(())
}

pub fn which(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| is_executable(&dir.join(program))))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
