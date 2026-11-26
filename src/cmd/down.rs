use std::collections::BTreeSet;
use std::env;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Result, bail};

use crate::cmd::known_slugs;
use crate::config::Config;
use crate::database;
use crate::docker;
use crate::project::{self, Env, LABEL_PROJECT, LABEL_SLUG};
use crate::services::Kind;
use crate::state::{DatabaseCopy, State};
use crate::{git, herdr};

pub struct Options {
    pub keep_checkout: bool,
    pub delete_branch: bool,
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
    let state = State::load_strict(&env.project, &env.slug)?;

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
    // Every app's copies, gathered up front; a state key that is not an app
    // name stops here rather than leave its copies unlisted.
    let copies = copies_by_app(&env, &state)?;
    if nothing_to_remove(&target, &copies, has_checkout) {
        State::remove(&env.project, &env.slug);
        bail!("nothing to remove for {slug}");
    }

    describe(&env, &target, &copies);
    if has_checkout && !opts.keep_checkout {
        check_dirty(&env, opts.force)?;
    }
    if opts.delete_branch && opts.keep_checkout {
        bail!("--delete-branch and --keep-checkout contradict each other");
    }
    confirm(&slug, &opts)?;

    // Read before the checkout goes away, so the closing line can name it, and
    // decide while it is still there whether the branch is ours to delete.
    let branch = git::current_branch(&env.path);
    let deletable = branch
        .as_deref()
        .filter(|b| !opts.keep_checkout && is_deletable(&main, &env, b));
    if opts.delete_branch && deletable.is_none() {
        eprintln!("No branch to delete for {slug}.");
    }
    let delete_branch = match deletable {
        Some(branch) => confirm_branch_delete(branch, &opts)?,
        None => false,
    };

    // Drop the copies before the containers, so a refusal leaves everything
    // intact instead of removing the clones first.
    let protected = protected_databases(&cfg);
    // `dropped` is false when the copies could not be dropped (Docker down, or
    // the source stopped or gone): the state file stays behind so a later run
    // can finish the job.
    let dropped = if copies.is_empty() || !docker_ok {
        copies.is_empty()
    } else {
        database::drop_all(&copies, opts.force, &protected)?
    };

    for name in &target.containers {
        docker::remove_container(name)?;
        println!("Removed {name}");
    }
    for name in &target.volumes {
        docker::remove_volume(name)?;
        println!("Removed {name}");
    }

    if !opts.keep_checkout {
        remove_checkout(&main, &env, opts.force)?;
    }
    match state_after(opts.keep_checkout, dropped) {
        StateAfter::SaveKept => kept_state(&state, docker_ok).save(&env.project, &env.slug)?,
        StateAfter::Remove => State::remove(&env.project, &env.slug),
        StateAfter::Leave => {}
    }

    if let Some(branch) = &branch {
        if delete_branch {
            git::delete_branch(&main, branch)?;
            println!("Deleted branch {branch}.");
        } else {
            println!("Branch {branch} was kept.");
        }
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
    let mut labelled = docker::containers_labelled(&labels)?;
    let mut volumes: BTreeSet<String> = docker::volumes_labelled(&labels)?
        .into_iter()
        .map(|v| v.name)
        .collect();
    if let Some(filter) = env.slug_filter() {
        labelled.extend(docker::containers_labelled(&filter)?);
        volumes.extend(
            docker::volumes_labelled(&filter)?
                .into_iter()
                .map(|v| v.name),
        );
    }
    let mut containers: BTreeSet<String> = labelled.iter().map(|c| c.name().to_string()).collect();
    // A short-lived probe gets a random Docker name; its labels prove it is ours.
    let helpers: BTreeSet<String> = labelled
        .iter()
        .filter(|c| env.owns_helper(|k| c.label(k)))
        .map(|c| c.name().to_string())
        .collect();

    for kind in Kind::ALL {
        let name = env.container(kind);
        if !containers.contains(&name)
            && let Some(c) = docker::container(&name)
        {
            if !force && !env.owns(|k| c.label(k)) {
                bail!(
                    "{name} {}; pass --force if it is really yours",
                    not_labelled(env)
                );
            }
            containers.insert(name);
        }
        let name = env.volume(kind);
        if !volumes.contains(&name)
            && let Some(v) = docker::volume(&name)
        {
            if !force && !env.owns(|k| v.label(k)) {
                bail!(
                    "{name} {}; pass --force if it is really yours",
                    not_labelled(env)
                );
            }
            volumes.insert(name);
        }
    }

    let prefix = env.prefix();
    let (sources, source_volumes) = sources(cfg);
    for name in containers.iter().chain(&volumes) {
        if !name.starts_with(&prefix) && !helpers.contains(name) {
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

fn not_labelled(env: &Env) -> String {
    let mut text = format!(
        "is not labelled {LABEL_PROJECT}={} and {LABEL_SLUG}={}",
        env.project, env.slug
    );
    if let Some(key) = &env.slug_label {
        text.push_str(&format!(" (or {key}={})", env.slug));
    }
    text
}

/// Every app's copies with the environment that names them: the default app's
/// with `env`, each extra's with `env` seen as that app. Run before anything is
/// dropped: a table whose key is not an app name and that lists copies refuses
/// the whole `down`, since nothing else knows whose copies they are.
fn copies_by_app<'a>(env: &Env, state: &'a State) -> Result<Vec<(Env, &'a DatabaseCopy)>> {
    let mut out: Vec<(Env, &DatabaseCopy)> = state
        .default
        .databases
        .iter()
        .map(|c| (env.clone(), c))
        .collect();
    for (key, id, app) in state.extras() {
        let id = match id {
            Ok(id) => id,
            Err(_) if app.databases.is_empty() => continue,
            Err(e) => {
                let file = crate::state::path(&env.project, &env.slug);
                return Err(e.context(format!(
                    "apps.{key:?} in {} lists database copies but is not an app name; rename or remove that table",
                    file.display()
                )));
            }
        };
        out.extend(app.databases.iter().map(|c| (env.clone().with_app(&id), c)));
    }
    // A copy listed twice (by hand, or under two apps) is dropped once: the
    // second DROP would fail after the first ran and leave the state stale.
    let mut seen = BTreeSet::new();
    out.retain(|(_, c)| seen.insert((c.source.clone(), c.target.clone())));
    Ok(out)
}

/// What `down` does with the state file once it has removed what it could.
#[derive(Debug, PartialEq, Eq)]
enum StateAfter {
    SaveKept,
    Remove,
    Leave,
}

/// Only once every copy is gone may the file change: it is the only record of
/// a copy `drop_all` had to keep (its source gone or stopped, Docker down).
fn state_after(keep_checkout: bool, dropped: bool) -> StateAfter {
    match (dropped, keep_checkout) {
        (false, _) => StateAfter::Leave,
        (true, true) => StateAfter::SaveKept,
        (true, false) => StateAfter::Remove,
    }
}

/// No container, volume, copy or checkout left: `down` only clears the state.
fn nothing_to_remove(target: &Target, copies: &[(Env, &DatabaseCopy)], has_checkout: bool) -> bool {
    target.containers.is_empty() && target.volumes.is_empty() && copies.is_empty() && !has_checkout
}

/// The state `down --keep-checkout` leaves once every copy is gone: each app
/// keeps its web port, since the checkout stays, and loses its copies. Service
/// ports go too, but only when Docker was reachable and their containers were
/// removed; an app left with nothing disappears.
fn kept_state(state: &State, docker_ok: bool) -> State {
    let mut kept = state.clone();
    for app in std::iter::once(&mut kept.default).chain(kept.apps.values_mut()) {
        app.databases.clear();
        if docker_ok {
            app.ports.clear();
        }
    }
    kept.apps.retain(|_, app| {
        app.app_port.is_some() || !app.ports.is_empty() || !app.databases.is_empty()
    });
    kept
}

/// Every app's source databases: `down --force` never drops one, whichever
/// app names it.
fn protected_databases(cfg: &Config) -> BTreeSet<String> {
    let mut protected = BTreeSet::new();
    for svc in cfg.services() {
        protected.extend(svc.databases.iter().cloned());
        protected.extend(svc.database.clone());
    }
    protected
}

/// Every app's source container names.
fn source_names(cfg: &Config) -> BTreeSet<String> {
    cfg.services().map(|svc| svc.source.clone()).collect()
}

/// Every app's source containers and every volume they mount.
fn sources(cfg: &Config) -> (BTreeSet<String>, BTreeSet<String>) {
    let containers = source_names(cfg);
    let mut volumes = BTreeSet::new();
    for svc in cfg.services() {
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

fn describe(env: &Env, target: &Target, copies: &[(Env, &DatabaseCopy)]) {
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
    for (copy_env, copy) in copies {
        let app = copy_env
            .app
            .label()
            .map(|a| format!(", app {a}"))
            .unwrap_or_default();
        println!(
            "  database {} (source {}, from {}{app})",
            copy.target, copy.source, copy.source_db
        );
    }
    if env.path.is_dir() {
        let branch = git::current_branch(&env.path).unwrap_or_else(|| "?".into());
        println!("  checkout  {} (branch {branch})", env.path.display());
    }
}

/// Whether `git branch -D` may take this branch once its checkout is gone: it
/// exists, HEAD is not detached, and no other worktree — the main checkout
/// included — has it checked out.
fn is_deletable(main: &Path, env: &Env, branch: &str) -> bool {
    branch != "HEAD"
        && git::branch_exists(main, branch)
        && matches!(git::holder_of(main, branch), Ok(Some(holder)) if project::canon(&holder) == env.path)
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

/// `--yes` and `WT_YES=1` answer every prompt with its default.
fn assumed_yes(opts: &Options) -> bool {
    opts.yes || env::var("WT_YES").as_deref() == Ok("1")
}

fn confirm(slug: &str, opts: &Options) -> Result<()> {
    if assumed_yes(opts) {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("refusing to destroy {slug} without a terminal; pass --yes");
    }
    let ok = dialoguer::Confirm::new()
        .with_prompt(format!("Destroy the {slug} environment?"))
        .default(false)
        .interact_opt()?;
    if ok != Some(true) {
        bail!("cancelled");
    }
    Ok(())
}

/// Asked only once the checkout is on its way out. The default is to keep the
/// branch, and only `--delete-branch` asks for it without a prompt.
fn confirm_branch_delete(branch: &str, opts: &Options) -> Result<bool> {
    if opts.delete_branch {
        return Ok(true);
    }
    if assumed_yes(opts) {
        return Ok(false);
    }
    let ok = dialoguer::Confirm::new()
        .with_prompt(format!(
            "Also delete branch {branch}? Commits it has not merged anywhere are lost."
        ))
        .default(false)
        .interact_opt()?;
    Ok(ok == Some(true))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn two_apps() -> Config {
        Config::from_toml(
            r#"
            [services.postgres]
            database = "main_db"
            [apps.b]
            dir = "b"
            [apps.b.services.postgres]
            source = "b-pg-1"
            mode = "database"
            databases = ["b_one", "b_two"]
            env = { U = "x/{database}" }
            "#,
        )
        .unwrap()
    }

    #[test]
    fn every_apps_source_databases_are_protected() {
        let protected = protected_databases(&two_apps());
        assert_eq!(
            protected.into_iter().collect::<Vec<_>>(),
            ["b_one", "b_two", "main_db"]
        );
    }

    #[test]
    fn every_apps_source_container_is_a_source() {
        let names = source_names(&two_apps());
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["b-pg-1", "myapp-postgres-1"]
        );
    }

    #[test]
    fn every_apps_copies_are_dropped_with_its_own_env() {
        use crate::config::{AppId, AppName};
        use crate::state::{AppState, DatabaseCopy};
        let cfg = two_apps();
        let env = Env::new(&cfg, "feat-x");
        let copy = |target: &str| DatabaseCopy {
            service: "postgres".into(),
            source: "pg".into(),
            source_db: "app".into(),
            target: target.into(),
        };
        let mut state = State::default();
        state.default.databases.push(copy("wt_myapp_feat_x__app"));
        state.apps.insert(
            "b".into(),
            AppState {
                databases: vec![copy("wt_myapp_feat_x__b__app")],
                ..Default::default()
            },
        );
        let pairs = copies_by_app(&env, &state).unwrap();
        let seen: Vec<(Option<&str>, &str)> = pairs
            .iter()
            .map(|(e, c)| (e.app.label(), c.target.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (None, "wt_myapp_feat_x__app"),
                (Some("b"), "wt_myapp_feat_x__b__app")
            ]
        );
        assert_eq!(pairs[1].0.app, AppId::Extra(AppName::parse("b").unwrap()));
        // A key no app name could be: never guess whose copies those are.
        state.apps.insert(
            "Bad Key".into(),
            AppState {
                databases: vec![copy("x")],
                ..Default::default()
            },
        );
        assert!(copies_by_app(&env, &state).is_err());
    }

    fn copy(target: &str) -> crate::state::DatabaseCopy {
        crate::state::DatabaseCopy {
            service: "postgres".into(),
            source: "pg".into(),
            source_db: "app".into(),
            target: target.into(),
        }
    }

    fn three_apps_state() -> State {
        use crate::state::AppState;
        let mut state = State::default();
        state.default.app_port = Some(3001);
        state.default.ports.insert("postgres".into(), 5433);
        state.default.databases.push(copy("a"));
        state.apps.insert(
            "web".into(),
            AppState {
                app_port: Some(8001),
                ports: [("redis".to_string(), 6380)].into(),
                databases: vec![copy("b")],
            },
        );
        state.apps.insert(
            "worker".into(),
            AppState {
                ports: [("postgres".to_string(), 5434)].into(),
                databases: vec![copy("c")],
                ..Default::default()
            },
        );
        state
    }

    #[test]
    fn keeping_the_checkout_keeps_only_web_ports() {
        let kept = kept_state(&three_apps_state(), true);
        assert_eq!(kept.default.app_port, Some(3001));
        assert!(kept.default.ports.is_empty() && kept.default.databases.is_empty());
        assert_eq!(kept.apps.len(), 1, "{kept:?}");
        let web = &kept.apps["web"];
        assert_eq!(web.app_port, Some(8001));
        assert!(web.ports.is_empty() && web.databases.is_empty());
        assert_eq!(
            toml::to_string(&kept).unwrap(),
            "app_port = 3001\n\n[apps.web]\napp_port = 8001\n"
        );
    }

    #[test]
    fn without_docker_the_service_ports_stay() {
        // No container was touched, so their ports still belong to them.
        let kept = kept_state(&three_apps_state(), false);
        assert_eq!(kept.default.ports["postgres"], 5433);
        assert_eq!(kept.apps["worker"].ports["postgres"], 5434);
        assert!(kept.apps.values().all(|a| a.databases.is_empty()));
    }

    #[test]
    fn copies_of_an_extra_alone_are_something_to_remove() {
        let cfg = two_apps();
        let env = Env::new(&cfg, "feat-x");
        let mut state = State::default();
        state.apps.insert(
            "b".into(),
            crate::state::AppState {
                databases: vec![copy("wt_myapp_feat_x__b__app")],
                ..Default::default()
            },
        );
        let copies = copies_by_app(&env, &state).unwrap();
        let none = Target {
            containers: BTreeSet::new(),
            volumes: BTreeSet::new(),
        };
        assert!(!nothing_to_remove(&none, &copies, false));
        assert!(nothing_to_remove(&none, &[], false));
        assert!(!nothing_to_remove(&none, &[], true));
    }

    #[test]
    fn a_bad_state_key_refuses_only_when_it_holds_copies() {
        let cfg = two_apps();
        let env = Env::new(&cfg, "feat-x");
        let mut state = State::default();
        state.apps.insert(
            "Bad Key".into(),
            crate::state::AppState {
                app_port: Some(8001),
                ..Default::default()
            },
        );
        assert!(copies_by_app(&env, &state).unwrap().is_empty());
        state
            .apps
            .get_mut("Bad Key")
            .unwrap()
            .databases
            .push(copy("x"));
        let e = format!("{:#}", copies_by_app(&env, &state).unwrap_err());
        assert!(
            e.starts_with("apps.\"Bad Key\" in ")
                && e.contains("feat-x.toml lists database copies"),
            "{e}"
        );
        assert_eq!(e.matches("state file").count(), 0, "{e}");
    }

    #[test]
    fn the_state_is_only_rewritten_once_every_copy_is_gone() {
        use StateAfter::*;
        // keep_checkout, dropped
        assert_eq!(state_after(true, true), SaveKept);
        assert_eq!(state_after(false, true), Remove);
        // Copies left behind: their only record must stay as it is.
        assert_eq!(state_after(true, false), Leave);
        assert_eq!(state_after(false, false), Leave);
    }

    #[test]
    fn a_container_or_a_volume_is_something_to_remove() {
        let one = |c: &[&str], v: &[&str]| Target {
            containers: c.iter().map(|s| s.to_string()).collect(),
            volumes: v.iter().map(|s| s.to_string()).collect(),
        };
        assert!(!nothing_to_remove(&one(&["c"], &[]), &[], false));
        assert!(!nothing_to_remove(&one(&[], &["v"]), &[], false));
    }

    #[test]
    fn a_copy_listed_twice_is_dropped_once() {
        let cfg = two_apps();
        let env = Env::new(&cfg, "feat-x");
        let mut state = State::default();
        state.default.databases.push(copy("wt_myapp_feat_x__app"));
        state.apps.insert(
            "b".into(),
            crate::state::AppState {
                databases: vec![
                    copy("wt_myapp_feat_x__app"),
                    copy("wt_myapp_feat_x__b__app"),
                ],
                ..Default::default()
            },
        );
        let targets: Vec<&str> = copies_by_app(&env, &state)
            .unwrap()
            .iter()
            .map(|(_, c)| c.target.as_str())
            .collect();
        assert_eq!(targets, ["wt_myapp_feat_x__app", "wt_myapp_feat_x__b__app"]);
    }
}
