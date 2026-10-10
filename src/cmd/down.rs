use std::collections::BTreeSet;
use std::env;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Result, bail};

use crate::cmd::known_slugs;
use crate::config::Config;
use crate::docker::{self, Volume};
use crate::project::{self, Env, LABEL_PROJECT, LABEL_SLUG};
use crate::services::Kind;
use crate::state::State;
use crate::{git, herdr};

pub struct Options {
    pub keep_checkout: bool,
    pub yes: bool,
    pub force: bool,
}

/// What would be removed: resources labelled for this slug, plus any that
/// merely carry the expected name (refused without --force).
struct Target {
    containers: BTreeSet<String>,
    volumes: BTreeSet<String>,
}

pub fn run(slug_arg: Option<&str>, opts: Options) -> Result<()> {
    let main = project::main_root()?;
    let cfg = Config::load(&main)?;
    let docker_ok = docker::ensure_running().is_ok();
    let slug = resolve_slug(&main, &cfg, slug_arg)?;
    let env = Env::new(&cfg, &slug);

    assert_safe_name(&env, &main)?;
    let target = if docker_ok {
        find_target(&cfg, &env, opts.force)?
    } else {
        println!("Docker is not reachable; only the checkout will be removed");
        Target {
            containers: BTreeSet::new(),
            volumes: BTreeSet::new(),
        }
    };
    assert_not_inside(&env, &main)?;

    let has_checkout = env.path.is_dir();
    if target.containers.is_empty() && target.volumes.is_empty() && !has_checkout {
        State::remove(&env.project, &env.slug);
        bail!("nothing to remove for {slug}");
    }

    describe(&env, &target);
    if has_checkout && !opts.keep_checkout {
        check_dirty(&env, opts.force)?;
    }
    confirm(&slug, &opts)?;

    // Read before the checkout goes away, so the closing line can name it.
    let branch = git::current_branch(&env.path);

    for name in &target.containers {
        docker::remove_container(name)?;
        println!("Removed {name}");
    }
    for name in &target.volumes {
        docker::remove_volume(name)?;
        println!("Removed {name}");
    }

    if opts.keep_checkout {
        let mut state = State::load(&env.project, &env.slug);
        state.ports.clear();
        state.save(&env.project, &env.slug)?;
    } else {
        remove_checkout(&main, &env, opts.force)?;
        State::remove(&env.project, &env.slug);
    }

    if let Some(branch) = branch {
        println!("Branch {branch} was kept.");
    }
    Ok(())
}

/// A slug or branch name, the worktree you are standing in, or a picker.
fn resolve_slug(main: &Path, cfg: &Config, arg: Option<&str>) -> Result<String> {
    if let Some(arg) = arg {
        return Ok(project::slugify(arg));
    }

    let root = project::canon(&cfg.root);
    if let Some(top) = git::toplevel().map(|t| project::canon(&t))
        && top != main
        && top.parent() == Some(root.as_path())
        && let Some(name) = top.file_name()
    {
        return Ok(name.to_string_lossy().into_owned());
    }

    let slugs = known_slugs(main, cfg)?;
    if slugs.is_empty() {
        bail!("no worktree environments found (wt up <branch>)");
    }
    if !std::io::stdin().is_terminal() {
        bail!("pass a slug; one of: {}", slugs.join(", "));
    }
    let choice = dialoguer::Select::new()
        .with_prompt("Which worktree?")
        .items(&slugs)
        .default(0)
        .interact_opt()?;
    match choice {
        Some(i) => Ok(slugs[i].clone()),
        None => bail!("cancelled"),
    }
}

fn assert_safe_name(env: &Env, main: &Path) -> Result<()> {
    let slug = &env.slug;
    if slug.is_empty() || slug.contains(['/', '.']) {
        bail!("refusing to act on the name '{slug}'");
    }
    if env.path == main {
        bail!("refusing to remove the main checkout");
    }
    Ok(())
}

/// The layers that keep `down` off anything it did not create. The labels are
/// the only proof `wt up` made a resource; a matching name proves nothing.
fn find_target(cfg: &Config, env: &Env, force: bool) -> Result<Target> {
    let labels = env.labels();
    let mut containers: BTreeSet<String> = docker::containers_labelled(&labels)?
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let mut volumes: BTreeSet<String> = docker::volumes_labelled(&labels)?
        .into_iter()
        .map(|v| v.name)
        .collect();

    for kind in Kind::ALL {
        let name = env.container(kind);
        if !containers.contains(&name)
            && let Some(c) = docker::container(&name)
        {
            if !force && !labelled_for(c.label(LABEL_PROJECT), c.label(LABEL_SLUG), env) {
                bail!(
                    "{name} is not labelled {LABEL_SLUG}={}; pass --force if it is really yours",
                    env.slug
                );
            }
            containers.insert(name);
        }
        let name = env.volume(kind);
        if !volumes.contains(&name)
            && let Some(v) = docker::volume(&name)
        {
            if !force && !volume_labelled(&v, env) {
                bail!(
                    "{name} is not labelled {LABEL_SLUG}={}; pass --force if it is really yours",
                    env.slug
                );
            }
            volumes.insert(name);
        }
    }

    let prefix = env.prefix();
    let (sources, source_volumes) = sources(cfg);
    for name in containers.iter().chain(&volumes) {
        if !name.starts_with(&prefix) {
            bail!("{name} is outside the {prefix} namespace");
        }
        if sources.contains(name) || source_volumes.contains(name) {
            bail!("refusing to remove {name}: it belongs to the main stack");
        }
    }
    Ok(Target {
        containers,
        volumes,
    })
}

fn labelled_for(project: Option<&str>, slug: Option<&str>, env: &Env) -> bool {
    project == Some(env.project.as_str()) && slug == Some(env.slug.as_str())
}

fn volume_labelled(v: &Volume, env: &Env) -> bool {
    labelled_for(v.label(LABEL_PROJECT), v.label(LABEL_SLUG), env)
}

/// The main stack's containers and every volume they mount.
fn sources(cfg: &Config) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut containers = BTreeSet::new();
    let mut volumes = BTreeSet::new();
    for svc in &cfg.services {
        containers.insert(svc.source.clone());
        volumes.extend(svc.volume.clone());
        if let Some(c) = docker::container(&svc.source) {
            volumes.extend(c.volume_mounts().filter_map(|m| m.name.clone()));
        }
    }
    (containers, volumes)
}

/// `git worktree remove` and closing your own Herdr workspace both fail from
/// inside the checkout.
fn assert_not_inside(env: &Env, main: &Path) -> Result<()> {
    let cwd = project::canon(&env::current_dir()?);
    if cwd.starts_with(&env.path) {
        bail!(
            "you are inside {}; run: cd {} && wt down {}",
            env.path.display(),
            main.display(),
            env.slug
        );
    }
    Ok(())
}

fn describe(env: &Env, target: &Target) {
    let sizes = if target.volumes.is_empty() {
        Default::default()
    } else {
        docker::volume_sizes()
    };
    println!("Worktree {}", env.slug);
    for name in &target.containers {
        println!("  container {name}");
    }
    for name in &target.volumes {
        let size = sizes.get(name).map(String::as_str).unwrap_or("?");
        println!("  volume    {name} ({size})");
    }
    if env.path.is_dir() {
        let branch = git::current_branch(&env.path).unwrap_or_else(|| "?".into());
        println!("  checkout  {} (branch {branch})", env.path.display());
    }
}

fn check_dirty(env: &Env, force: bool) -> Result<()> {
    let dirty = git::dirty(&env.path);
    if dirty.trim().is_empty() {
        return Ok(());
    }
    eprintln!("  uncommitted changes:");
    for line in dirty.lines() {
        eprintln!("    {line}");
    }
    if !force {
        bail!(
            "{} has uncommitted work; commit it or pass --force",
            env.path.display()
        );
    }
    Ok(())
}

fn confirm(slug: &str, opts: &Options) -> Result<()> {
    if opts.yes || opts.force || env::var("WT_YES").as_deref() == Ok("1") {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("refusing to destroy {slug} without a terminal; pass --yes");
    }
    let ok = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Destroy the {slug} environment? The branch is kept."
        ))
        .default(false)
        .interact_opt()?;
    if ok != Some(true) {
        bail!("cancelled");
    }
    Ok(())
}

fn remove_checkout(main: &Path, env: &Env, force: bool) -> Result<()> {
    if !env.path.is_dir() {
        git::prune(main)?;
        return Ok(());
    }

    let workspace = if herdr::available(main) {
        herdr::workspace_id(main, &env.path)
    } else {
        None
    };
    match workspace {
        Some(ws) => herdr::remove(&ws)?,
        None => {
            if git::remove(main, &env.path, force).is_err() {
                bail!(
                    "git refused to remove {}; re-run with --force",
                    env.path.display()
                );
            }
        }
    }
    git::prune(main)?;
    println!("Removed {}", env.path.display());
    Ok(())
}
