use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::project::{self, Env};
use crate::services::{self, RestoreOnDrop};
use crate::state::{self, State};
use crate::{docker, envfile, files, git, herdr, hooks, ports};

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

    git::prune(&main)?;
    create_worktree(&main, &env, branch, base)?;

    let state = resolve_ports(&cfg, &env)?;
    state.save(&env.project, &env.slug)?;

    write_env(&main, &cfg, &env, &state)?;
    if let Some(sqlite) = &cfg.sqlite {
        files::copy_sqlite(&main, &env.path, &sqlite.files)?;
    }
    files::copy_paths(&main, &env.path, &cfg.app.copy)?;

    let hook_env = hook_env(&main, &cfg, &env, branch, &state);
    hooks::run("setup", &cfg.hooks.setup, &env.path, &hook_env)?;

    {
        // Restarts any source still stopped if a step below fails.
        let _restore = RestoreOnDrop;
        for svc in &cfg.services {
            let src = services::resolve_source(svc)?;
            services::prepare_volume(svc, &src, &env)?;
            services::start(svc, &src, &env, state.ports[svc.kind.name()])?;
            services::wait_ready(svc, &src, &env)?;
        }
    }

    hooks::run(
        "after_services",
        &cfg.hooks.after_services,
        &env.path,
        &hook_env,
    )?;
    report(&cfg, &env, branch, &state);
    Ok(())
}

fn create_worktree(main: &Path, env: &Env, branch: &str, base: Option<&str>) -> Result<()> {
    let path = &env.path;
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
fn resolve_ports(cfg: &Config, env: &Env) -> Result<State> {
    let previous = State::load(&env.project, &env.slug);
    let mut claimed: HashSet<u16> = state::claimed_ports();
    if !cfg.services.is_empty() {
        claimed.extend(docker::claimed_host_ports()?);
    }

    let mut next = State::default();
    if !cfg.app.port_env.is_empty() {
        let port = match previous.app_port {
            Some(p) => p,
            None => ports::find_free(cfg.app.port_range, &claimed)?,
        };
        claimed.insert(port);
        next.app_port = Some(port);
    }

    for svc in &cfg.services {
        let name = svc.kind.name();
        let published = docker::container(&env.container(svc.kind))
            .and_then(|c| c.host_ports().first().copied());
        let port = match published.or(previous.ports.get(name).copied()) {
            Some(p) => p,
            None => ports::find_free(svc.port_range, &claimed)?,
        };
        claimed.insert(port);
        next.ports.insert(name.to_string(), port);
    }
    Ok(next)
}

fn env_values(cfg: &Config, state: &State) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for svc in &cfg.services {
        let port = state.ports[svc.kind.name()].to_string();
        for (k, v) in &svc.env {
            values.push((k.clone(), v.replace("{port}", &port)));
        }
    }
    if let Some(port) = state.app_port {
        values.push((cfg.app.port_env.clone(), port.to_string()));
    }
    values
}

/// Rewrites the worktree's own file on re-runs, so hand edits survive.
fn write_env(main: &Path, cfg: &Config, env: &Env, state: &State) -> Result<()> {
    let values = env_values(cfg, state);
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
    for svc in &cfg.services {
        vars.push((
            svc.kind.port_env_var(),
            state.ports[svc.kind.name()].to_string(),
        ));
    }
    vars
}

fn report(cfg: &Config, env: &Env, branch: &str, state: &State) {
    println!();
    println!(
        "Worktree {} ready at {} (branch {branch})",
        env.slug,
        env.path.display()
    );
    for svc in &cfg.services {
        println!(
            "  {:<9} 127.0.0.1:{}",
            svc.kind.name(),
            state.ports[svc.kind.name()]
        );
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
