use anyhow::Result;

use crate::cmd::known_slugs;
use crate::config::Config;
use crate::database;
use crate::docker::{self, Container, Volume};
use crate::project::{self, Env, LABEL_PROJECT};
use crate::state::State;
use crate::{git, services::Kind};

pub fn run() -> Result<()> {
    let main = project::main_root()?;
    let cfg = Config::load(&main)?;
    let slugs = known_slugs(&main, &cfg)?;
    if slugs.is_empty() {
        println!("No worktree environments (wt up <branch>)");
        return Ok(());
    }

    let docker_ok = docker::ensure_running().is_ok();
    let sizes = if docker_ok {
        docker::volume_sizes()
    } else {
        Default::default()
    };
    // A column for each configured service, and for any a worktree still has
    // from before the config dropped it.
    let mut kinds: Vec<Kind> = cfg.services.iter().map(|s| s.kind).collect();

    // One query for the whole project, split by slug below.
    let (all_containers, all_volumes) = if docker_ok {
        project_resources(&cfg)?
    } else {
        (Vec::new(), Vec::new())
    };

    let mut rows = Vec::new();
    for slug in &slugs {
        let env = Env::new(&cfg, slug);
        let containers: Vec<Container> = all_containers
            .iter()
            .filter(|c| env.owns(|k| c.label(k)))
            .cloned()
            .collect();
        let volumes: Vec<_> = all_volumes
            .iter()
            .filter(|v| env.owns(|k| v.label(k)))
            .cloned()
            .collect();
        for c in &containers {
            if let Some(kind) = env.service_of(c.name(), |k| c.label(k))
                && !kinds.contains(&kind)
            {
                kinds.push(kind);
            }
        }
        let state = State::load(&env.project, &env.slug);
        // Copies whose service left the config still need their column.
        for copy in &state.databases {
            if let Some(kind) = Kind::from_name(&copy.service)
                && !kinds.contains(&kind)
            {
                kinds.push(kind);
            }
        }
        rows.push((env, containers, volumes, state));
    }

    let mut header = vec!["SLUG", "BRANCH", "WEB"];
    header.extend(kinds.iter().map(|k| k.name()));
    header.extend(["STATUS", "DISK"]);
    let mut table = vec![header.iter().map(|s| s.to_uppercase()).collect::<Vec<_>>()];

    for (env, containers, volumes, state) in &rows {
        let mut row = vec![
            env.slug.clone(),
            git::current_branch(&env.path).unwrap_or_else(|| "-".into()),
            port_cell(state.app_port),
        ];
        for kind in &kinds {
            let published = containers
                .iter()
                .find(|c| env.service_of(c.name(), |k| c.label(k)) == Some(*kind))
                .and_then(|c| c.host_ports().first().copied());
            row.push(port_cell(
                published.or(state.ports.get(kind.name()).copied()),
            ));
        }
        row.push(status(&cfg, env, containers, !volumes.is_empty(), state, docker_ok).into());
        let disk: Vec<&str> = volumes
            .iter()
            .filter_map(|v| sizes.get(&v.name).map(String::as_str))
            .collect();
        row.push(if disk.is_empty() {
            "-".into()
        } else {
            disk.join("+")
        });
        table.push(row);
    }

    print_table(&table);
    Ok(())
}

/// Everything of this project, plus what another tool made under the
/// configured slug label. A resource with both label sets is listed once.
fn project_resources(cfg: &Config) -> Result<(Vec<Container>, Vec<Volume>)> {
    let label = [(LABEL_PROJECT, cfg.project.clone())];
    let mut containers = docker::containers_labelled(&label)?;
    let mut volumes = docker::volumes_labelled(&label)?;
    if let Some(key) = &cfg.docker.slug_label {
        let prefix = format!("{}-wt-", cfg.project);
        for c in docker::containers_with_label(key)? {
            if c.name().starts_with(&prefix)
                && !containers.iter().any(|known| known.name() == c.name())
            {
                containers.push(c);
            }
        }
        for v in docker::volumes_with_label(key)? {
            if v.name.starts_with(&prefix) && !volumes.iter().any(|known| known.name == v.name) {
                volumes.push(v);
            }
        }
    }
    Ok((containers, volumes))
}

fn port_cell(port: Option<u16>) -> String {
    port.map_or_else(|| "-".into(), |p| p.to_string())
}

fn status(
    cfg: &Config,
    env: &Env,
    containers: &[Container],
    has_volumes: bool,
    state: &State,
    docker_ok: bool,
) -> &'static str {
    // A missing checkout is a filesystem fact and needs no daemon.
    if !env.path.is_dir() {
        return "orphan";
    }
    if !docker_ok && !state.databases.is_empty() {
        return "unknown";
    }
    if !containers.is_empty() {
        let running = containers.iter().filter(|c| c.state.running).count();
        return match (containers.len(), running) {
            (n, r) if n == r => "running",
            (_, 0) => "stopped",
            _ => "partial",
        };
    }
    if docker_ok
        && !state.databases.is_empty()
        && let Some(verdict) = database_status(state)
    {
        return verdict;
    }
    if cfg.services.is_empty() && !has_volumes {
        "ready"
    } else {
        "no-db"
    }
}

/// Whether the registered copies still exist. One query per copy, and only
/// when the worktree has no container of its own. A stopped source (or a
/// probe that fails) means "cannot tell", never "the copies are gone".
fn database_status(state: &State) -> Option<&'static str> {
    if state.databases.is_empty() {
        return None;
    }
    let mut existing = 0;
    for copy in &state.databases {
        // The volume may outlive the container, so a missing source is
        // "cannot tell", not "the copies are gone".
        let Some(container) = docker::container(&copy.source) else {
            return Some("unknown");
        };
        if !container.state.running {
            return Some("unknown");
        }
        match database::exists(copy, &container) {
            Ok(true) => existing += 1,
            Ok(false) => {}
            Err(_) => return Some("unknown"),
        }
    }
    Some(match existing {
        n if n == state.databases.len() => "shared",
        0 => "no-db",
        _ => "partial",
    })
}

fn print_table(rows: &[Vec<String>]) {
    // Widths in characters, not bytes, so a non-ASCII branch name still lines up.
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                format!(
                    "{cell}{}",
                    " ".repeat(w.saturating_sub(cell.chars().count()))
                )
            })
            .collect();
        println!("{}", cells.join("  ").trim_end());
    }
}
