use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sh;

/// Runs each argv in order, without a shell; the first failure aborts.
pub fn run(phase: &str, hooks: &[Vec<String>], cwd: &Path, env: &[(String, String)]) -> Result<()> {
    for argv in hooks {
        let line = argv.join(" ");
        println!("→ {line}");
        sh::run(
            Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(cwd)
                .envs(env.iter().map(|(k, v)| (k, v))),
        )
        .with_context(|| format!("hooks.{phase} failed: {line}"))?;
    }
    Ok(())
}
