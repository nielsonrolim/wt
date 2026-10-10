//! `wt init`: writes a starting `.wt.toml` from the repo's compose containers
//! and the files that reveal its toolchain.

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{self, FILE_NAME};
use crate::docker;
use crate::git;
use crate::project;
use crate::services::Kind;

#[derive(Debug, Default)]
struct Detected {
    /// Each adapter found, with its container name when it differs from the
    /// `{project}-{kind}-1` default.
    services: Vec<(Kind, Option<String>)>,
    sqlite: Vec<String>,
    copy: Vec<String>,
    setup: Vec<Vec<String>>,
    after_services: Vec<Vec<String>>,
}

pub fn run(force: bool) -> Result<()> {
    let main = project::main_root()?;
    project::assert_main_checkout(&main)?;
    let target = main.join(FILE_NAME);
    if target.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            target.display()
        );
    }

    let project = config::default_project(&main);
    let mut detected = detect_files(&main);
    if docker::ensure_running().is_ok() {
        detected.services = detect_services(&main, &project)?;
    } else {
        println!("Docker is not reachable; no services detected");
    }

    fs::write(&target, render(&detected))?;
    println!("Wrote {}", target.display());
    for (kind, source) in &detected.services {
        let source = source
            .clone()
            .unwrap_or_else(|| format!("{project}-{}-1", kind.name()));
        println!("  {:<9} from {source}", kind.name());
    }
    if !detected.sqlite.is_empty() {
        println!("  sqlite    {}", detected.sqlite.join(", "));
    }
    println!("Review it, then: wt up <branch>");
    Ok(())
}

fn detect_services(main: &Path, project: &str) -> Result<Vec<(Kind, Option<String>)>> {
    // Compose records the directory as typed, symlinks unresolved.
    let key = "com.docker.compose.project.working_dir";
    let mut found: Vec<(Kind, Option<String>)> = Vec::new();
    let mut containers: Vec<_> = docker::containers_with_label(key)?
        .into_iter()
        .filter(|c| {
            c.label(key)
                .is_some_and(|dir| project::canon(Path::new(dir)) == main)
        })
        .collect();
    containers.sort_by(|a, b| a.name().cmp(b.name()));
    for c in containers {
        let Some(kind) = Kind::from_image(&c.config.image) else {
            continue;
        };
        if found.iter().any(|(k, _)| *k == kind) {
            println!(
                "Ignoring {}: only one {} per project",
                c.name(),
                kind.name()
            );
            continue;
        }
        let default = format!("{project}-{}-1", kind.name());
        let source = (c.name() != default).then(|| c.name().to_string());
        found.push((kind, source));
    }
    found.sort_by_key(|(k, _)| Kind::ALL.iter().position(|x| x == k));
    Ok(found)
}

fn detect_files(main: &Path) -> Detected {
    let mut d = Detected::default();

    for dir in ["storage", "db"] {
        let Ok(entries) = fs::read_dir(main.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".sqlite3") && !n.contains("test"))
            .map(|n| format!("{dir}/{n}"))
            .filter(|rel| git::is_ignored(main, rel))
            .collect();
        names.sort();
        d.sqlite.extend(names);
    }

    // Active Storage blobs: storage/ holds more than its placeholder and the
    // databases already copied on their own.
    if let Ok(entries) = fs::read_dir(main.join("storage")) {
        let has_blobs = entries.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !matches!(name.as_str(), ".keep" | ".gitkeep") && !name.contains(".sqlite3")
        });
        if has_blobs {
            d.copy.push("storage".into());
        }
    }

    let has = |f: &str| main.join(f).exists();
    if has("Gemfile") {
        d.setup.push(argv(&["bundle", "install"]));
    }
    if has("package.json") {
        let install = if has("bun.lock") || has("bun.lockb") {
            &["bun", "install"]
        } else if has("pnpm-lock.yaml") {
            &["pnpm", "install"]
        } else if has("yarn.lock") {
            &["yarn", "install"]
        } else {
            &["npm", "install"]
        };
        d.setup.push(argv(install));
    }
    if has("bin/rails") {
        d.after_services.push(argv(&["bin/rails", "db:prepare"]));
    }
    d
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn quoted(list: &[String]) -> String {
    let items: Vec<String> = list.iter().map(|s| format!("{s:?}")).collect();
    format!("[{}]", items.join(", "))
}

fn render(d: &Detected) -> String {
    let mut out = String::from(
        "# wt: a git worktree with its own services and web port.\n\
         # Every option and its default: see the wt README.\n\n",
    );

    out.push_str("[app]\n");
    if d.copy.is_empty() {
        out.push_str("# copy = [\"storage\"]   # untracked paths copied from the main checkout\n");
    } else {
        out.push_str(&format!("copy = {}\n", quoted(&d.copy)));
    }

    for (kind, source) in &d.services {
        out.push_str(&format!("\n[services.{}]\n", kind.name()));
        if let Some(source) = source {
            out.push_str(&format!("source = {source:?}\n"));
        }
        let env: Vec<String> = kind
            .default_env()
            .iter()
            .map(|(k, v)| format!("{k} = {v:?}"))
            .collect();
        out.push_str(&format!("# env = {{ {} }}\n", env.join(", ")));
    }
    if !d.sqlite.is_empty() {
        out.push_str(&format!(
            "\n[services.sqlite]\nfiles = {}\n",
            quoted(&d.sqlite)
        ));
    }

    out.push_str("\n[hooks]\n");
    for (phase, hooks) in [("setup", &d.setup), ("after_services", &d.after_services)] {
        if hooks.is_empty() {
            out.push_str(&format!("{phase} = []\n"));
            continue;
        }
        out.push_str(&format!("{phase} = [\n"));
        for argv in hooks {
            out.push_str(&format!("  {},\n", quoted(argv)));
        }
        out.push_str("]\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_file_loads_back() {
        let d = Detected {
            services: vec![
                (Kind::Postgres, None),
                (Kind::Redis, Some("cache-1".into())),
            ],
            sqlite: vec!["storage/development.sqlite3".into()],
            copy: vec!["storage".into()],
            setup: vec![argv(&["bundle", "install"]), argv(&["bun", "install"])],
            after_services: vec![argv(&["bin/rails", "db:prepare"])],
        };
        let text = render(&d);
        let dir = std::env::temp_dir().join(format!("wt-init-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), &text).unwrap();
        let cfg = config::Config::load(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(cfg.services.len(), 2);
        assert_eq!(cfg.service(Kind::Redis).unwrap().source, "cache-1");
        assert_eq!(cfg.app.copy, vec!["storage"]);
        assert_eq!(cfg.hooks.setup.len(), 2);
        assert_eq!(cfg.sqlite.unwrap().files.len(), 1);
    }

    #[test]
    fn empty_detection_still_loads() {
        let text = render(&Detected::default());
        assert!(toml::from_str::<toml::Value>(&text).is_ok());
    }
}
