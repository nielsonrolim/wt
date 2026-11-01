use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{Config, Mode};
use crate::database::{self, MarkerState};
use crate::docker::{Container, Volume};
use crate::project::{self, Env};
use crate::services::{self, ContainerSource, RestoreOnDrop, Source};
use crate::state::{self, DatabaseCopy, State};
use crate::{docker, envfile, files, git, herdr, hooks, ports};

/// The resolved plan for one service, before anything is created.
enum ServiceUp {
    Container {
        source: Source,
    },
    Database {
        src: ContainerSource,
        /// The source's published host port; the only port the worktree gets.
        port: u16,
        copies: Vec<(DatabaseCopy, MarkerState)>,
    },
}

pub fn run(branch: &str, base: Option<&str>) -> Result<()> {
    let main = project::main_root()?;
    project::assert_main_checkout(&main)?;
    let cfg = Config::load(&main)?;
    if cfg.path.is_none() {
        println!("No .wt.toml found; creating a worktree without services (wt init writes one)");
    }
    if !cfg.services.is_empty() {
        docker::ensure_running()?;
    }

    let slug = project::slugify(branch);
    if slug.is_empty() {
        bail!("{branch} does not reduce to a usable name");
    }
    fs::create_dir_all(&cfg.root)?;
    let env = Env::new(&cfg, &slug);
    let previous = State::load(&env.project, &env.slug);

    // Every source is resolved before the checkout exists: a refused run (a
    // missing volume, an unpublished port, a foreign database, a mode change)
    // must leave no worktree, no state and no database behind.
    let mut resolved = Vec::new();
    for svc in &cfg.services {
        if services::copies_volume(svc) {
            resolved.push(ServiceUp::Container {
                source: services::resolve_source(svc)?,
            });
        } else {
            let src = services::resolve_container(svc)?;
            // Checked before the port: a stopped container may have lost its
            // ephemeral binding, and "run docker start" is the useful error.
            if !src.container.state.running {
                bail!(
                    "source container {} is not running; run: docker start {}",
                    src.container.name(),
                    src.container.name()
                );
            }
            let port = services::host_port_for(&src)?;
            let copies = database::plan(svc, &src, &env)?;
            resolved.push(ServiceUp::Database { src, port, copies });
        }
    }
    refuse_mode_changes(&cfg, &env, &previous, &resolved)?;

    let mut next = resolve_ports(&cfg, &env, &previous, &resolved)?;
    let (databases, kept) = planned_databases(&previous, &resolved);
    next.databases = databases;

    git::prune(&main)?;
    create_worktree(&main, &env, branch, base)?;
    next.save(&env.project, &env.slug)?;

    write_env(&main, &cfg, &env, &next, &resolved)?;
    warn_kept(&cfg, &env, &kept);
    if let Some(sqlite) = &cfg.sqlite {
        files::copy_sqlite(&main, &env.path, &sqlite.files)?;
    }
    files::copy_paths(&main, &env.path, &cfg.app.copy)?;
    files::refresh_paths(&main, &env.path, &cfg.app.refresh)?;

    let hook_env = hook_env(&main, &cfg, &env, branch, &next, &resolved);
    hooks::run("setup", &cfg.hooks.setup, &env.path, &hook_env)?;

    {
        // Restarts any source still stopped if a step below fails.
        let _restore = RestoreOnDrop;
        for (svc, plan) in cfg.services.iter().zip(&resolved) {
            match plan {
                ServiceUp::Container { source } => {
                    services::prepare_volume(svc, source, &env)?;
                    services::start(svc, source, &env, next.ports[svc.kind.name()])?;
                    services::wait_ready(svc, source, &env)?;
                }
                ServiceUp::Database { src, copies, .. } => {
                    database::prepare(svc, src, &env, copies)?;
                    let owned: Vec<DatabaseCopy> =
                        copies.iter().map(|(copy, _)| copy.clone()).collect();
                    let primary = database::primary(&owned).ok_or_else(|| {
                        anyhow::anyhow!("no database planned for {}", svc.kind.name())
                    })?;
                    database::verify(svc, src, &primary.target)?;
                }
            }
        }
    }

    hooks::run(
        "after_services",
        &cfg.hooks.after_services,
        &env.path,
        &hook_env,
    )?;
    report(&cfg, &env, branch, &next, &resolved);
    Ok(())
}

/// A database-mode service must not run next to a container or volume of the
/// same slug, and a container-mode service must not silently abandon copies a
/// previous run registered. Both refuse before `resolve_ports`, `state.save`
/// and `create_worktree`.
fn refuse_mode_changes(
    cfg: &Config,
    env: &Env,
    previous: &State,
    resolved: &[ServiceUp],
) -> Result<()> {
    for (svc, plan) in cfg.services.iter().zip(resolved) {
        let container = docker::container(&env.container(svc.kind));
        let volume = docker::volume(&env.volume(svc.kind));
        match plan {
            ServiceUp::Database { .. } => {
                refuse_leftover(svc.kind, svc.mode, container.as_ref(), volume.as_ref(), env)?
            }
            ServiceUp::Container { .. } => refuse_copies(
                &previous.databases,
                svc.kind,
                svc.mode,
                container.as_ref(),
                env,
            )?,
        }
    }
    Ok(())
}

/// Refuses to mix a leftover container with `mode = "database"`.
pub fn refuse_leftover(
    kind: services::Kind,
    mode: Mode,
    container: Option<&Container>,
    volume: Option<&Volume>,
    env: &Env,
) -> Result<()> {
    if mode != Mode::Database {
        return Ok(());
    }
    let ours_container = container.is_some_and(|c| env.owns(|k| c.label(k)));
    let ours_volume = volume.is_some_and(|v| env.owns(|k| v.label(k)));
    if ours_container || ours_volume {
        bail!(
            "{} has a container or volume from container mode, but services.{} is now mode = \"database\"; run: wt down {}",
            env.slug,
            kind.name(),
            env.slug
        );
    }
    Ok(())
}

/// Refuses to leave registered copies behind when the service goes back to
/// `mode = "container"` and no container of ours exists yet.
pub fn refuse_copies(
    copies: &[DatabaseCopy],
    kind: services::Kind,
    mode: Mode,
    container: Option<&Container>,
    env: &Env,
) -> Result<()> {
    if mode != Mode::Container {
        return Ok(());
    }
    let mine: Vec<&DatabaseCopy> = copies
        .iter()
        .filter(|copy| copy.service == kind.name())
        .collect();
    if mine.is_empty() {
        return Ok(());
    }
    if container.is_some_and(|c| env.owns(|k| c.label(k))) {
        return Ok(());
    }
    let what = if mine.len() == 1 {
        "1 database copy is".to_string()
    } else {
        format!("{} database copies are", mine.len())
    };
    let targets: Vec<&str> = mine.iter().map(|copy| copy.target.as_str()).collect();
    bail!(
        "{what} registered for {} ({}), but services.{} is now mode = \"container\"; run: wt down {}",
        env.slug,
        targets.join(", "),
        kind.name(),
        env.slug
    )
}

fn create_worktree(main: &Path, env: &Env, branch: &str, base: Option<&str>) -> Result<()> {
    let path = &env.path;
    if let Some(base) = base
        && git::branch_exists(main, branch)
    {
        eprintln!("warning: {branch} already exists, so --base {base} is ignored");
    }
    if let Some(existing) = git::list(main)?.into_iter().find(|e| &e.path == path) {
        // Another branch can share the slug (feat/x and feat-x); reusing its
        // checkout would hand this branch the other one's databases.
        if let Some(other) = existing.branch.filter(|b| b != branch) {
            bail!(
                "{} already holds branch {other}, which has the same slug as {branch}",
                path.display()
            );
        }
        println!("Reusing worktree at {}", path.display());
        return Ok(());
    }
    if path.exists() {
        bail!("{} exists but is not a registered worktree", path.display());
    }
    if let Some(holder) = git::holder_of(main, branch)? {
        bail!("{branch} is already checked out at {}", holder.display());
    }

    if herdr::available(main) {
        let base = if git::branch_exists(main, branch) {
            None
        } else {
            base
        };
        herdr::create(main, branch, path, base)?;
    } else {
        git::add(main, path, branch, base)?;
    }

    // Herdr picks its own directory when --path is not honoured, and every
    // name downstream comes from the slug, so confirm the checkout landed here.
    if !git::registered(main, path)? {
        bail!("no worktree registered at {}", path.display());
    }
    println!("Created worktree at {}", path.display());
    Ok(())
}

/// Re-runs must land on the same ports, or the .env and the containers
/// disagree: the published port wins, then the state file, then a free one.
/// A database-mode service has no port of its own: it uses the source's.
fn resolve_ports(
    cfg: &Config,
    env: &Env,
    previous: &State,
    resolved: &[ServiceUp],
) -> Result<State> {
    let mut claimed: HashSet<u16> = state::claimed_ports();
    // Every container's published ports, stopped ones included, so a re-run
    // never hands out a port a stopped clone still owns. Without services
    // Docker may not be installed at all, and a failure here is not fatal.
    match docker::claimed_host_ports() {
        Ok(ports) => claimed.extend(ports),
        Err(e) => {
            if !cfg.services.is_empty() {
                return Err(e);
            }
        }
    }

    let mut next = State::default();
    if !cfg.app.port_env.is_empty() {
        let port = match previous
            .app_port
            .filter(|p| in_range(*p, cfg.app.port_range))
        {
            Some(p) => p,
            None => ports::find_free(cfg.app.port_range, &claimed)?,
        };
        claimed.insert(port);
        next.app_port = Some(port);
    }

    for (svc, plan) in cfg.services.iter().zip(resolved) {
        let name = svc.kind.name();
        let port = match plan {
            ServiceUp::Database { port, .. } => *port,
            ServiceUp::Container { .. } => {
                // The published port wins even if it is now outside the
                // configured range: the container already bound it. The state
                // file only counts while it still sits in the range.
                let published = docker::container(&env.container(svc.kind))
                    .and_then(|c| c.host_ports().first().copied());
                let recorded = previous
                    .ports
                    .get(name)
                    .copied()
                    .filter(|p| in_range(*p, svc.port_range));
                match published.or(recorded) {
                    Some(p) => p,
                    None => ports::find_free(svc.port_range, &claimed)?,
                }
            }
        };
        claimed.insert(port);
        next.ports.insert(name.to_string(), port);
    }
    Ok(next)
}

/// The state's copy list: what the config asks for, then the copies a previous
/// run left behind that the config no longer asks for. Those are never dropped
/// by `up`; `down` removes them.
fn planned_databases(
    previous: &State,
    resolved: &[ServiceUp],
) -> (Vec<DatabaseCopy>, Vec<DatabaseCopy>) {
    let desired: Vec<(DatabaseCopy, MarkerState)> = resolved
        .iter()
        .filter_map(|plan| match plan {
            ServiceUp::Database { copies, .. } => Some(copies.iter().cloned()),
            ServiceUp::Container { .. } => None,
        })
        .flatten()
        .collect();
    let reconciled = database::reconcile(&previous.databases, &desired);
    let mut all: Vec<DatabaseCopy> = desired.iter().map(|(copy, _)| copy.clone()).collect();
    all.extend(reconciled.keep.iter().cloned());
    (all, reconciled.keep)
}

/// The `app_env` key whose value points at a target, for the "kept" warning.
fn key_pointing_at(path: &Path, target: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let key = key.strip_prefix("export ").unwrap_or(key).trim();
        (!key.is_empty() && value.contains(target)).then(|| key.to_string())
    })
}

fn warn_kept(cfg: &Config, env: &Env, kept: &[DatabaseCopy]) {
    if kept.is_empty() {
        return;
    }
    let file = env.path.join(&cfg.app.env_file);
    for copy in kept {
        let key = key_pointing_at(&file, &copy.target)
            .map(|key| format!("; {key} in .env still points at it"))
            .unwrap_or_default();
        eprintln!(
            "warning: keeping {} (no longer in services.{}.databases){} (wt down {} removes it)",
            copy.target, copy.service, key, env.slug
        );
    }
}

fn in_range(port: u16, (first, last): (u16, u16)) -> bool {
    (first..=last).contains(&port)
}

fn env_values(
    cfg: &Config,
    state: &State,
    resolved: &[ServiceUp],
) -> Result<Vec<(String, String)>> {
    let mut values = Vec::new();
    for (svc, plan) in cfg.services.iter().zip(resolved) {
        let port = state.ports[svc.kind.name()];
        let targets: Vec<(String, String)> = match plan {
            ServiceUp::Database { copies, .. } => copies
                .iter()
                .map(|(copy, _)| (copy.source_db.clone(), copy.target.clone()))
                .collect(),
            ServiceUp::Container { .. } => Vec::new(),
        };
        let pairs: Vec<(String, String)> = svc
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in database::render_env(&pairs, &targets, port)? {
            set_value(&mut values, k, v);
        }
    }
    if let Some(port) = state.app_port {
        set_value(&mut values, cfg.app.port_env.clone(), port.to_string());
    }
    Ok(values)
}

/// One entry per key. A key a later service — or the web port — defines again
/// replaces the earlier value instead of appending a duplicate line to `.env`.
fn set_value(values: &mut Vec<(String, String)>, key: String, value: String) {
    match values.iter_mut().find(|(k, _)| k == &key) {
        Some((_, v)) => *v = value,
        None => values.push((key, value)),
    }
}

/// Rewrites the worktree's own file on re-runs, so hand edits survive.
fn write_env(
    main: &Path,
    cfg: &Config,
    env: &Env,
    state: &State,
    resolved: &[ServiceUp],
) -> Result<()> {
    let values = env_values(cfg, state, resolved)?;
    let target = env.path.join(&cfg.app.env_file);
    let source = [target.clone(), main.join(&cfg.app.env_file)]
        .into_iter()
        .find(|p| p.is_file());
    let text = match source {
        Some(p) => fs::read_to_string(p)?,
        None => String::new(),
    };
    if !values.is_empty() || !text.is_empty() {
        envfile::write_private(&target, &envfile::rewrite(&text, &values))?;
        let summary: Vec<String> = values.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("Wrote {} ({})", target.display(), summary.join(", "));
    }

    if let Some(port) = state.app_port
        && cfg.app.envrc
    {
        if git::is_tracked(&env.path, ".envrc") {
            println!("Skipping .envrc: the repo tracks its own");
        } else {
            envfile::write_envrc(&env.path, &cfg.app.port_env, port)?;
            println!(
                "Wrote {}/.envrc ({}={port})",
                env.path.display(),
                cfg.app.port_env
            );
        }
    }
    Ok(())
}

fn hook_env(
    main: &Path,
    cfg: &Config,
    env: &Env,
    branch: &str,
    state: &State,
    resolved: &[ServiceUp],
) -> Vec<(String, String)> {
    let mut vars = vec![
        ("WT_SLUG".to_string(), env.slug.clone()),
        ("WT_BRANCH".into(), branch.to_string()),
        ("WT_PATH".into(), env.path.display().to_string()),
        ("WT_MAIN_ROOT".into(), main.display().to_string()),
    ];
    if let Some(port) = state.app_port {
        vars.push(("WT_APP_PORT".into(), port.to_string()));
    }
    for (svc, plan) in cfg.services.iter().zip(resolved) {
        vars.push((
            svc.kind.port_env_var(),
            state.ports[svc.kind.name()].to_string(),
        ));
        if let ServiceUp::Database { copies, .. } = plan {
            let upper = svc.kind.name().to_uppercase();
            if let Some((primary, _)) = copies.first() {
                vars.push((format!("WT_{upper}_DATABASE"), primary.target.clone()));
            }
            let pairs: Vec<String> = copies
                .iter()
                .map(|(copy, _)| format!("{}={}", copy.source_db, copy.target))
                .collect();
            vars.push((format!("WT_{upper}_DATABASES"), pairs.join(",")));
        }
    }
    vars
}

fn report(cfg: &Config, env: &Env, branch: &str, state: &State, resolved: &[ServiceUp]) {
    println!();
    println!(
        "Worktree {} ready at {} (branch {branch})",
        env.slug,
        env.path.display()
    );
    for (svc, plan) in cfg.services.iter().zip(resolved) {
        let port = state.ports[svc.kind.name()];
        match plan {
            ServiceUp::Container { .. } => {
                println!("  {:<9} 127.0.0.1:{port}", svc.kind.name());
            }
            ServiceUp::Database { copies, .. } => {
                let targets: Vec<&str> = copies
                    .iter()
                    .map(|(copy, _)| copy.target.as_str())
                    .collect();
                println!(
                    "  {:<9} 127.0.0.1:{port} (databases {})",
                    svc.kind.name(),
                    targets.join(", ")
                );
            }
        }
    }
    if let Some(port) = state.app_port {
        println!(
            "  cd {} && source .envrc   # exports {}={port}",
            env.path.display(),
            cfg.app.port_env
        );
        // Cookies are scoped by host and ignore the port, so worktrees served
        // on localhost share one session jar and sign each other out.
        println!(
            "  open http://{}.localhost:{port}   # a subdomain per worktree keeps sessions apart",
            env.slug
        );
    }
    println!(
        "  wt down {}   # removes the services and the checkout; keeps the branch",
        env.slug
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{Container, Volume};

    #[test]
    fn in_range_is_inclusive() {
        assert!(in_range(5433, (5433, 5499)));
        assert!(in_range(5499, (5433, 5499)));
        assert!(!in_range(5432, (5433, 5499)));
        assert!(!in_range(5500, (5433, 5499)));
    }

    #[test]
    fn set_value_keeps_one_entry_per_key() {
        let mut values = Vec::new();
        set_value(&mut values, "DB_PORT".into(), "5433".into());
        set_value(&mut values, "PORT".into(), "3001".into());
        // A later service redefining the key replaces the value, not appends.
        set_value(&mut values, "DB_PORT".into(), "5434".into());
        assert_eq!(
            values,
            vec![
                ("DB_PORT".to_string(), "5434".to_string()),
                ("PORT".to_string(), "3001".to_string()),
            ]
        );
    }

    fn env() -> Env {
        Env {
            project: "wte2e".into(),
            slug: "db-legacy".into(),
            path: "/tmp/wt-test-db-legacy".into(),
            legacy_names: false,
            slug_label: None,
        }
    }

    fn container_with(labels: &str) -> Container {
        let json = format!(
            r#"[{{"Name": "/x", "Config": {{"Image": "postgres", "Labels": {labels}}},
            "State": {{"Running": true}}, "Mounts": [], "HostConfig": {{}}}}]"#
        );
        serde_json::from_str::<Vec<Container>>(&json)
            .unwrap()
            .remove(0)
    }

    fn our_container(slug: &str) -> Container {
        container_with(&format!(
            r#"{{"wt.project": "wte2e", "wt.slug": "{slug}", "wt.service": "postgres"}}"#
        ))
    }

    #[test]
    fn refuses_a_leftover_container_when_mode_changes() {
        let env = env();
        let err = refuse_leftover(
            services::Kind::Postgres,
            Mode::Database,
            Some(&our_container("db-legacy")),
            None,
            &env,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("is now mode = \"database\""),
            "{err}"
        );
        assert!(err.to_string().contains("wt down db-legacy"), "{err}");

        let volume = r#"[{"Name": "wte2e-wt-db-legacy-postgres-data",
            "Labels": {"wt.project": "wte2e", "wt.slug": "db-legacy"}}]"#;
        let volume: Volume = serde_json::from_str::<Vec<Volume>>(volume)
            .unwrap()
            .remove(0);
        assert!(
            refuse_leftover(
                services::Kind::Postgres,
                Mode::Database,
                None,
                Some(&volume),
                &env
            )
            .is_err()
        );

        // The normal container-mode path, and another project's resources.
        assert!(
            refuse_leftover(
                services::Kind::Postgres,
                Mode::Container,
                Some(&our_container("db-legacy")),
                None,
                &env
            )
            .is_ok()
        );
        let other = container_with(r#"{"wt.project": "other", "wt.slug": "db-legacy"}"#);
        assert!(
            refuse_leftover(
                services::Kind::Postgres,
                Mode::Database,
                Some(&other),
                None,
                &env
            )
            .is_ok()
        );
    }

    fn copies() -> Vec<DatabaseCopy> {
        vec![
            DatabaseCopy {
                service: "postgres".into(),
                source: "wte2e-postgres-1".into(),
                source_db: "app".into(),
                target: "wt_wte2e_db_legacy2__app".into(),
            },
            DatabaseCopy {
                service: "postgres".into(),
                source: "wte2e-postgres-1".into(),
                source_db: "app_cache".into(),
                target: "wt_wte2e_db_legacy2__app_cache".into(),
            },
        ]
    }

    #[test]
    fn refuses_to_migrate_back_to_container_silently() {
        let env = Env {
            slug: "db-legacy2".into(),
            ..env()
        };
        let err = refuse_copies(
            &copies(),
            services::Kind::Postgres,
            Mode::Container,
            None,
            &env,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("is now mode = \"container\""), "{text}");
        assert!(text.contains("2 database copies"), "{text}");
        assert!(text.contains("wt_wte2e_db_legacy2__app"), "{text}");
        assert!(text.contains("wt_wte2e_db_legacy2__app_cache"), "{text}");
        assert!(text.contains("wt down db-legacy2"), "{text}");

        // A re-run of database mode, and a container that already exists.
        assert!(
            refuse_copies(
                &copies(),
                services::Kind::Postgres,
                Mode::Database,
                None,
                &env
            )
            .is_ok()
        );
        assert!(
            refuse_copies(
                &copies(),
                services::Kind::Postgres,
                Mode::Container,
                Some(&our_container("db-legacy2")),
                &env
            )
            .is_ok()
        );
        assert!(refuse_copies(&[], services::Kind::Postgres, Mode::Container, None, &env).is_ok());
    }
}
