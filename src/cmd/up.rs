use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, AppConfig, AppId, Config, Mode, Service};
use crate::database::{self, MarkerState};
use crate::docker::{Container, Volume};
use crate::project::{self, Env};
use crate::services::{self, ContainerSource, Kind, RestoreOnDrop, Source};
use crate::state::{self, AppState, DatabaseCopy, State};
use crate::{docker, envfile, files, git, herdr, hooks, ports};

/// How one service will come up, resolved before anything is created.
enum Resolved {
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

/// One service of one app, with its config, so nothing lines services up by
/// position.
struct ServiceUp<'a> {
    svc: &'a Service,
    resolved: Resolved,
}

impl ServiceUp<'_> {
    /// The container the source resolved to, whatever `source` said.
    fn container(&self) -> &str {
        match &self.resolved {
            Resolved::Container { source } => source.container.name(),
            Resolved::Database { src, .. } => src.container.name(),
        }
    }

    fn copies(&self) -> &[(DatabaseCopy, MarkerState)] {
        match &self.resolved {
            Resolved::Database { copies, .. } => copies,
            Resolved::Container { .. } => &[],
        }
    }
}

/// One app's part of a run. `new` plans it before the checkout exists;
/// `resolve_ports` fills its ports, `resolve_app_dirs` re-derives its
/// tracked-`.envrc` path from the folder `check_app_dir` resolves, and
/// `write_env` records whether its `.envrc` was written.
struct AppUp<'a> {
    app: &'a AppConfig,
    /// The worktree seen as this app: its names, labels and targets.
    env: Env,
    services: Vec<ServiceUp<'a>>,
    /// What the previous run left for this app.
    prior: AppState,
    /// What this run records for it.
    state: AppState,
    /// Copies a previous run made that the config no longer asks for.
    kept: Vec<DatabaseCopy>,
    /// The app's folder as the worktree names it (`WT_APP_DIR`, `.envrc`,
    /// the report): `WT_PATH` joined with its `dir`.
    dir: PathBuf,
    /// Its `.envrc` relative to the worktree, for the tracked-file check;
    /// symlinks resolved once `resolve_app_dirs` has run, since git does not
    /// follow a symlinked folder.
    envrc_rel: String,
    envrc_written: bool,
}

impl<'a> AppUp<'a> {
    /// Resolves the app's sources and refuses a mode change, all before the
    /// checkout exists.
    fn new(app: &'a AppConfig, worktree: &Env, previous: &State) -> Result<AppUp<'a>> {
        let env = worktree.clone().with_app(&app.id);
        let prior = previous.app(&app.id).cloned().unwrap_or_default();
        let services = resolve_services(app, &env, &prior)?;
        refuse_mode_changes(app, &env, &prior, &services)?;
        let (databases, kept) = planned_databases(&prior, &services);
        Ok(AppUp {
            dir: app_root(app, &env),
            envrc_rel: envrc_rel(app),
            app,
            env,
            services,
            prior,
            state: AppState {
                databases,
                ..AppState::default()
            },
            kept,
            envrc_written: false,
        })
    }
}

pub fn run(branch: &str, base: Option<&str>, names: &[String], all: bool) -> Result<()> {
    let main = project::main_root()?;
    project::assert_main_checkout(&main)?;
    let cfg = Config::load(&main)?;
    if cfg.path.is_none() {
        println!("No .wt.toml found; creating a worktree without services (wt init writes one)");
    }
    let selected = cfg.select(names, all)?;
    note_other_apps(&cfg, &selected);
    let any_services = needs_docker(&selected);
    if any_services {
        docker::ensure_running()?;
    }
    let config_file = cfg
        .path
        .clone()
        .unwrap_or_else(|| main.join(config::FILE_NAME));

    let slug = project::slugify(branch);
    if slug.is_empty() {
        bail!("{branch} does not reduce to a usable name");
    }
    fs::create_dir_all(&cfg.root)?;
    let worktree = Env::new(&cfg, &slug);
    let previous = State::load_strict(&worktree.project, &worktree.slug)?;

    // Every app's sources are resolved before the checkout exists: a refused
    // run (a missing volume, an unpublished port, a foreign database, a mode
    // change, an app dir the branch lacks) must leave no worktree, no state
    // and no database behind.
    let mut plans = Vec::new();
    for app in &selected {
        plans.push(AppUp::new(app, &worktree, &previous)?);
    }
    // Without a selected service there is nothing to compare, and Docker may
    // not even run.
    if any_services {
        let mut sources = selected_sources(&plans);
        sources.extend(other_sources(&cfg, &selected, &previous, |source| {
            Ok(docker::find_container(source)?.map(|c| c.name().to_string()))
        })?);
        check_sources(&sources)?;
    }

    let mut claimed = taken_ports(any_services, cfg.services().next().is_some())?;
    for plan in &mut plans {
        resolve_ports(plan, &mut claimed)?;
        // Rendered once here so a value that cannot be rendered is refused
        // before the checkout and the state exist.
        env_values(plan)?;
    }
    // A worktree this run reuses is checked as it is, after the checkout; a
    // new one must not be created for a branch that lacks an app's folder.
    // Pruned first, so a worktree deleted by hand does not count as reused.
    git::prune(&main)?;
    let reused = match git::list(&main)?.iter().find(|e| e.path == worktree.path) {
        Some(entry) => reuses(entry, &worktree.path, branch)?,
        None => false,
    };
    if !reused {
        let rev = app_dir_rev(&main, branch, base);
        check_app_dirs_on(&main, &rev, &selected, &config_file)?;
    }
    create_worktree(&main, &worktree, branch, base)?;
    // The branch had each dir; the checkout may still differ (a local edit,
    // a symlink), so check what is really there before writing into it.
    resolve_app_dirs(&mut plans, &config_file)?;
    check_shared_outputs(&cfg, &plans, &worktree.path)?;
    // Read again just before writing, and only this run's apps replaced: a
    // `wt up --app` for another app of this worktree may have saved since.
    // Two runs saving at the very same moment can still lose one; do not
    // run two `wt up` for one worktree at once.
    let next = next_state(
        &State::load_strict(&worktree.project, &worktree.slug)?,
        plans
            .iter()
            .map(|p| (p.app.id.clone(), p.state.clone()))
            .collect(),
    );
    next.save(&worktree.project, &worktree.slug)?;

    for plan in &mut plans {
        plan.envrc_written = write_env(&main, plan)?;
        warn_kept(plan);
        let (app, path) = (plan.app, &plan.env.path);
        if let Some(sqlite) = &app.sqlite {
            files::copy_sqlite(&main, path, &sqlite.files)?;
        }
        files::copy_paths(&main, path, &app.app.copy)?;
        files::refresh_paths(&main, path, &app.app.refresh)?;
    }

    let ports = web_ports(&cfg, &next);
    let hook_envs: Vec<Vec<(String, String)>> = plans
        .iter()
        .map(|p| hook_env(&main, p, branch, &ports))
        .collect();
    for plan in &plans {
        let hooks = &plan.app.hooks;
        for note in hooks::early_notes(
            &plan.app.id.prefix(),
            &hooks.setup,
            &hooks.after_services,
            &plan.env.path,
        ) {
            eprintln!("warning: {note}");
        }
    }
    for (plan, vars) in plans.iter().zip(&hook_envs) {
        let phase = plan.app.id.key("hooks.setup");
        hooks::run(
            &phase,
            &plan.app.hooks.setup,
            &plan.env.path,
            vars,
            &config_file,
        )?;
    }
    check_first_after_services(&plans, &config_file)?;

    {
        // Restarts any source still stopped if a step below fails.
        let _restore = RestoreOnDrop;
        for plan in &plans {
            start_services(plan)?;
        }
    }

    for (plan, vars) in plans.iter().zip(&hook_envs) {
        let phase = plan.app.id.key("hooks.after_services");
        hooks::run(
            &phase,
            &plan.app.hooks.after_services,
            &plan.env.path,
            vars,
            &config_file,
        )?;
    }
    report(&worktree, branch, &plans);
    Ok(())
}

/// Once setup is done nothing can create an after_services[0] dir, so a
/// missing one fails here rather than after the sources are copied. Hook dirs
/// are relative to the worktree root, like every hook's.
fn check_first_after_services(plans: &[AppUp], config: &Path) -> Result<()> {
    for plan in plans {
        if let Some(first) = plan.app.hooks.after_services.first() {
            let phase = plan.app.id.key("hooks.after_services");
            hooks::dir(&phase, 0, first, &plan.env.path, config)?;
        }
    }
    Ok(())
}

/// Checks every app's folder in the checkout and, from where it resolves,
/// the path git knows its `.envrc` by.
fn resolve_app_dirs(plans: &mut [AppUp], config: &Path) -> Result<()> {
    for plan in plans {
        let real_root = project::canon(&plan.env.path);
        let real = check_app_dir(plan, config)?;
        plan.envrc_rel = envrc_rel_in(&real_root, &real);
    }
    Ok(())
}

/// The files an app writes (env file, `.envrc`), as the checkout resolves
/// them: a folder linked to another app's makes two apps write one file,
/// which the config's text comparison cannot see.
fn resolved_outputs(app: &AppConfig, root: &Path) -> Vec<(String, PathBuf)> {
    let resolve = |rel: PathBuf| -> PathBuf {
        let path = root.join(rel);
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) if parent.exists() => project::canon(parent).join(name),
            _ => path,
        }
    };
    let mut out = vec![(
        app.id.key("app.env_file"),
        resolve(PathBuf::from(&app.app.env_file)),
    )];
    if app.app.envrc && !app.app.port_env.is_empty() {
        out.push((app.id.key("app.envrc"), resolve(app.dir.join(".envrc"))));
    }
    out
}

/// Refuses a selected app whose env file or `.envrc` resolves to a file
/// another app of the config writes.
fn check_shared_outputs(cfg: &Config, plans: &[AppUp], root: &Path) -> Result<()> {
    for plan in plans {
        let mine = resolved_outputs(plan.app, root);
        for other in cfg.apps().filter(|a| a.id != plan.app.id) {
            let theirs = resolved_outputs(other, root);
            for (key, path) in &mine {
                if let Some((theirs, _)) = theirs.iter().find(|(_, p)| p == path) {
                    bail!(
                        "{key} resolves to {}, which {theirs} writes too (a folder links to another app's); give each app its own folder",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

/// Whether the registered worktree at `path` is this branch's to reuse: its
/// branch, or a detached HEAD, as `create_worktree` does. Another branch with
/// the same slug is refused here, before any other check.
fn reuses(entry: &git::Entry, path: &Path, branch: &str) -> Result<bool> {
    if let Some(other) = entry.branch.as_deref().filter(|b| *b != branch) {
        bail!(
            "{} already holds branch {other}, which has the same slug as {branch}",
            path.display()
        );
    }
    Ok(true)
}

/// Says which apps this run leaves alone, when the config has more than one.
fn note_other_apps(cfg: &Config, selected: &[&AppConfig]) {
    if cfg.extras.is_empty() {
        return;
    }
    let up: Vec<String> = selected.iter().map(|a| a.id.to_string()).collect();
    let others: Vec<String> = cfg
        .apps()
        .filter(|a| !selected.iter().any(|s| s.id == a.id))
        .map(|a| a.id.to_string())
        .collect();
    if others.is_empty() {
        println!("Bringing up {}", up.join(", "));
    } else {
        println!(
            "Bringing up {}; also in .wt.toml: {} (add --app <name>, or --all)",
            up.join(", "),
            others.join(", ")
        );
    }
}

/// Docker is needed as soon as one selected app has a service.
fn needs_docker(selected: &[&AppConfig]) -> bool {
    selected.iter().any(|a| !a.services.is_empty())
}

/// Resolves every service of one app against its sources.
fn resolve_services<'a>(
    app: &'a AppConfig,
    env: &Env,
    prior: &AppState,
) -> Result<Vec<ServiceUp<'a>>> {
    let mut out = Vec::new();
    for svc in &app.services {
        let resolved = resolve_service(svc, env, prior)
            .with_context(|| app.id.key(&format!("services.{}", svc.kind.name())))?;
        out.push(ServiceUp { svc, resolved });
    }
    Ok(out)
}

fn resolve_service(svc: &Service, env: &Env, prior: &AppState) -> Result<Resolved> {
    if services::copies_volume(svc) {
        return Ok(Resolved::Container {
            source: services::resolve_source(svc)?,
        });
    }
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
    let copies = database::plan(svc, &src, env, &prior.databases)?;
    Ok(Resolved::Database { src, port, copies })
}

/// One service's use of a source container, for the checks across apps.
#[derive(Debug, Clone)]
struct SourceUse {
    /// Whether this run brings its app up; two uses this run leaves alone are
    /// never compared.
    selected: bool,
    /// Its key in `.wt.toml` (`apps.b.services.postgres`).
    key: String,
    kind: Kind,
    mode: Mode,
    /// The container it resolved to.
    container: String,
    /// The source databases it copies (database mode).
    databases: Vec<String>,
}

/// The sources this run resolved.
fn selected_sources(plans: &[AppUp]) -> Vec<SourceUse> {
    plans
        .iter()
        .flat_map(|plan| {
            plan.services.iter().map(|up| SourceUse {
                selected: true,
                key: plan.app.id.key(&format!("services.{}", up.svc.kind.name())),
                kind: up.svc.kind,
                mode: up.svc.mode,
                container: up.container().to_string(),
                databases: up
                    .copies()
                    .iter()
                    .map(|(copy, _)| copy.source_db.clone())
                    .collect(),
            })
        })
        .collect()
}

/// The sources of the apps this run leaves alone: a clone in one app stops
/// the source another app's copies live in, whichever run made them. Only
/// looked up (`lookup` gives the container a `source` names); a source that
/// names no container any more is skipped with a warning. Their databases are
/// the listed ones, else the ones their copies record; an app that lists none
/// and has no copies yet counts none, though its source may hold the one it
/// would detect.
fn other_sources(
    cfg: &Config,
    selected: &[&AppConfig],
    previous: &State,
    lookup: impl Fn(&str) -> Result<Option<String>>,
) -> Result<Vec<SourceUse>> {
    let mut out = Vec::new();
    for app in cfg
        .apps()
        .filter(|a| !selected.iter().any(|s| s.id == a.id))
    {
        for svc in &app.services {
            let key = app.id.key(&format!("services.{}", svc.kind.name()));
            let Some(container) = lookup(&svc.source)
                .with_context(|| format!("{key}: could not look up {}", svc.source))?
            else {
                eprintln!(
                    "warning: {key} names {}, which is not a container now; not checked against this run",
                    svc.source
                );
                continue;
            };
            let databases = if svc.mode != Mode::Database {
                Vec::new()
            } else if !svc.databases.is_empty() {
                svc.databases.clone()
            } else {
                previous
                    .app(&app.id)
                    .map(|s| {
                        s.databases
                            .iter()
                            .filter(|c| c.service == svc.kind.name())
                            .map(|c| c.source_db.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            out.push(SourceUse {
                selected: false,
                key,
                kind: svc.kind,
                mode: svc.mode,
                container,
                databases,
            });
        }
    }
    Ok(out)
}

/// The config compares `source` as written; here each use is the container
/// it resolved to, so an ID in one app and a name in another meet. One
/// container is one service type, used in one mode (a clone stops the source
/// the copies live in), and each of its databases is copied once per
/// worktree.
fn check_sources(uses: &[SourceUse]) -> Result<()> {
    for (i, a) in uses.iter().enumerate() {
        for b in uses[i + 1..]
            .iter()
            .filter(|b| b.container == a.container && (a.selected || b.selected))
        {
            let name = &a.container;
            if a.kind != b.kind {
                bail!(
                    "{} and {} both resolve to container {name}; one container cannot be a {} and a {}",
                    a.key,
                    b.key,
                    a.kind.name(),
                    b.kind.name()
                );
            }
            if a.mode != b.mode {
                bail!(
                    "{} and {} both resolve to container {name} but in different modes; a clone (mode = \"container\") stops the source the copies (mode = \"database\") live in, so pick one mode",
                    a.key,
                    b.key
                );
            }
            let same = |x: &String, y: &String| a.kind.same_database(x, y);
            if let Some(db) = a
                .databases
                .iter()
                .find(|x| b.databases.iter().any(|y| same(x, y)))
            {
                bail!(
                    "{} and {} would both copy {db:?} from container {name}; a worktree gets one copy of each source database",
                    a.key,
                    b.key
                );
            }
        }
    }
    Ok(())
}

/// The commit a new worktree will check out, and how to name it to the user.
struct AppDirRev {
    /// What git is asked for.
    spec: String,
    /// Where it came from: the branch, `--base <ref>`, or HEAD.
    label: String,
}

/// The branch when it exists (as `refs/heads/…`, so a tag of the same name
/// cannot shadow it), else the base, else HEAD.
fn app_dir_rev(main: &Path, branch: &str, base: Option<&str>) -> AppDirRev {
    if git::branch_exists(main, branch) {
        return AppDirRev {
            spec: format!("refs/heads/{branch}"),
            label: branch.to_string(),
        };
    }
    match base {
        Some(base) => AppDirRev {
            spec: base.to_string(),
            label: format!("--base {base}"),
        },
        None => AppDirRev {
            spec: "HEAD".into(),
            label: "HEAD".into(),
        },
    }
}

/// Every selected app's `dir` other than the root must be a folder in `rev`,
/// before the checkout exists: a missing one, a file or a symlink would leave
/// a worktree whose app has nowhere to live.
fn check_app_dirs_on(
    main: &Path,
    rev: &AppDirRev,
    selected: &[&AppConfig],
    config: &Path,
) -> Result<()> {
    // Checked on every run, so a typo in --base reads the same whichever
    // apps come up.
    let (spec, label) = (&rev.spec, &rev.label);
    if !git::is_commit(main, spec) {
        if spec == "HEAD" {
            bail!("HEAD has no commit yet; commit something, then run wt up again");
        }
        bail!("{label} is not a commit; nothing was created");
    }
    for app in selected.iter().filter(|a| a.dir != Path::new(".")) {
        let dir = app.dir.display().to_string();
        // A submodule is refused too: its folder is empty until a hook
        // initialises it, after the checkout.
        let what = match git::object_type(main, spec, &dir).as_deref() {
            Some("tree") => continue,
            Some("blob") => format!("is a file or a symlink on {label}, not a folder"),
            Some("commit") => format!("is a submodule on {label}, not a folder"),
            Some(other) => format!("is a {other} on {label}, not a folder"),
            None => format!("is not a folder on {label}"),
        };
        bail!(
            "{} = {dir:?} {what}; fix {} or the branch",
            app.id.key("dir"),
            config.display()
        );
    }
    Ok(())
}

/// The app's folder in the checkout, resolved: it must exist, be a
/// directory and stay inside the worktree, like a hook's `dir`. The caller
/// keeps the logical path for what it shows and exports.
fn check_app_dir(plan: &AppUp, config: &Path) -> Result<PathBuf> {
    let subject = format!("{} is", plan.app.id.key("dir"));
    hooks::inside(&subject, &plan.app.dir, &plan.env.path, config)
}

/// Starts or copies one app's services and checks they came up.
fn start_services(plan: &AppUp) -> Result<()> {
    let env = &plan.env;
    for up in &plan.services {
        let svc = up.svc;
        let started = || -> Result<()> {
            match &up.resolved {
                Resolved::Container { source } => {
                    services::prepare_volume(svc, source, env)?;
                    services::start(svc, source, env, plan.state.ports[svc.kind.name()])?;
                    services::wait_ready(svc, source, env)?;
                }
                Resolved::Database { src, copies, .. } => {
                    database::prepare(svc, src, env, copies)?;
                    let owned: Vec<DatabaseCopy> =
                        copies.iter().map(|(copy, _)| copy.clone()).collect();
                    let primary = database::primary(&owned).ok_or_else(|| {
                        anyhow::anyhow!("no database planned for {}", svc.kind.name())
                    })?;
                    database::verify(svc, src, &primary.target)?;
                }
            }
            Ok(())
        };
        started().with_context(|| plan.app.id.key(&format!("services.{}", svc.kind.name())))?;
    }
    Ok(())
}

/// A database-mode service must not run next to a container or volume of the
/// same slug and app, and a container-mode service must not silently abandon
/// copies a previous run registered. Both refuse before `resolve_ports`,
/// `state.save` and `create_worktree`.
fn refuse_mode_changes(
    app: &AppConfig,
    env: &Env,
    prior: &AppState,
    services: &[ServiceUp],
) -> Result<()> {
    let prefix = app.id.prefix();
    for up in services {
        let svc = up.svc;
        let container = docker::container(&env.container(svc.kind));
        let volume = docker::volume(&env.volume(svc.kind));
        match up.resolved {
            Resolved::Database { .. } => refuse_leftover(
                &prefix,
                svc.kind,
                svc.mode,
                container.as_ref(),
                volume.as_ref(),
                env,
            )?,
            Resolved::Container { .. } => refuse_copies(
                &prior.databases,
                &prefix,
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
    prefix: &str,
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
            "{} has a container or volume from container mode, but {prefix}services.{} is now mode = \"database\"; run: wt down {}",
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
    prefix: &str,
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
        "{what} registered for {} ({}), but {prefix}services.{} is now mode = \"container\"; run: wt down {}",
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

/// The state `up` saves: each app this run selected gets its fresh entry,
/// and every other app keeps what the previous run left, so a run for one app
/// never drops another's ports from the registry or its copies from `down`'s
/// list.
fn next_state(previous: &State, fresh: Vec<(AppId, AppState)>) -> State {
    let mut next = previous.clone();
    for (id, state) in fresh {
        *next.app_mut(&id) = state;
    }
    next
}

/// Every port already taken: the state files' registry, and every
/// container's published ports, stopped ones included, so a re-run never
/// hands out a port a stopped clone still owns. Without a selected service
/// Docker may not be installed at all, so a failure there is not fatal; it
/// is worth a warning only when some app of the config has services.
fn taken_ports(any_services: bool, config_has_services: bool) -> Result<HashSet<u16>> {
    let mut claimed = state::claimed_ports();
    match docker::claimed_host_ports() {
        Ok(ports) => claimed.extend(ports),
        Err(e) if any_services => return Err(e),
        Err(e) if config_has_services => eprintln!(
            "warning: could not list Docker's published ports, so they are not counted as taken: {e:#}"
        ),
        Err(_) => {}
    }
    Ok(claimed)
}

/// Re-runs must land on the same ports, or the .env and the containers
/// disagree: the published port wins, then the state file, then a free one.
/// A database-mode service has no port of its own: it uses the source's.
/// Every port handed out goes into `claimed`, so the next app gets another.
fn resolve_ports(plan: &mut AppUp, claimed: &mut HashSet<u16>) -> Result<()> {
    let app = plan.app;
    if !app.app.port_env.is_empty() {
        let port = match plan
            .prior
            .app_port
            .filter(|p| in_range(*p, app.app.port_range))
        {
            Some(p) => p,
            None => ports::find_free(app.app.port_range, claimed)?,
        };
        claimed.insert(port);
        plan.state.app_port = Some(port);
    }

    for up in &plan.services {
        let svc = up.svc;
        let name = svc.kind.name();
        let port = match &up.resolved {
            Resolved::Database { port, .. } => *port,
            Resolved::Container { .. } => {
                // The published port wins even if it is now outside the
                // configured range: the container already bound it. The state
                // file only counts while it still sits in the range.
                let published = docker::container(&plan.env.container(svc.kind))
                    .and_then(|c| c.host_ports().first().copied());
                let recorded = plan
                    .prior
                    .ports
                    .get(name)
                    .copied()
                    .filter(|p| in_range(*p, svc.port_range));
                match published.or(recorded) {
                    Some(p) => p,
                    None => ports::find_free(svc.port_range, claimed)?,
                }
            }
        };
        claimed.insert(port);
        plan.state.ports.insert(name.to_string(), port);
    }
    Ok(())
}

/// The app's copy list: what the config asks for, then the copies a previous
/// run left behind that the config no longer asks for. Those are never
/// dropped by `up`; `down` removes them.
fn planned_databases(
    prior: &AppState,
    services: &[ServiceUp],
) -> (Vec<DatabaseCopy>, Vec<DatabaseCopy>) {
    let desired: Vec<(DatabaseCopy, MarkerState)> = services
        .iter()
        .flat_map(|up| up.copies().iter().cloned())
        .collect();
    let reconciled = database::reconcile(&prior.databases, &desired);
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

fn warn_kept(plan: &AppUp) {
    let (app, env) = (plan.app, &plan.env);
    let file = env.path.join(&app.app.env_file);
    let prefix = app.id.prefix();
    for copy in &plan.kept {
        let key = key_pointing_at(&file, &copy.target)
            .map(|key| format!("; {key} in {} still points at it", app.app.env_file))
            .unwrap_or_default();
        eprintln!(
            "warning: keeping {} (no longer in {prefix}services.{}.databases){} (wt down {} removes it)",
            copy.target, copy.service, key, env.slug
        );
    }
}

fn in_range(port: u16, (first, last): (u16, u16)) -> bool {
    (first..=last).contains(&port)
}

fn env_values(plan: &AppUp) -> Result<Vec<(String, String)>> {
    let mut values = Vec::new();
    for up in &plan.services {
        let svc = up.svc;
        let port = plan.state.ports[svc.kind.name()];
        let targets: Vec<(String, String)> = up
            .copies()
            .iter()
            .map(|(copy, _)| (copy.source_db.clone(), copy.target.clone()))
            .collect();
        let pairs: Vec<(String, String)> = svc
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in database::render_env(&pairs, &targets, port)? {
            set_value(&mut values, k, v);
        }
    }
    if let Some(port) = plan.state.app_port {
        set_value(&mut values, plan.app.app.port_env.clone(), port.to_string());
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

/// The app's folder in the worktree: the root itself for the default app.
fn app_root(app: &AppConfig, env: &Env) -> PathBuf {
    if app.dir == Path::new(".") {
        env.path.clone()
    } else {
        env.path.join(&app.dir)
    }
}

/// `.envrc` in `real`, relative to the worktree's resolved root.
fn envrc_rel_in(real_root: &Path, real: &Path) -> String {
    match real.strip_prefix(real_root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".envrc".to_string(),
        Ok(rel) => rel.join(".envrc").display().to_string(),
        Err(_) => real.join(".envrc").display().to_string(),
    }
}

/// Where the app's `.envrc` goes, relative to the worktree root.
fn envrc_rel(app: &AppConfig) -> String {
    if app.dir == Path::new(".") {
        ".envrc".to_string()
    } else {
        app.dir.join(".envrc").display().to_string()
    }
}

/// Rewrites the worktree's own file on re-runs, so hand edits survive.
/// Returns whether it wrote the app's `.envrc`.
fn write_env(main: &Path, plan: &AppUp) -> Result<bool> {
    let (app, env) = (plan.app, &plan.env);
    let values = env_values(plan)?;
    let target = env.path.join(&app.app.env_file);
    let source = [target.clone(), main.join(&app.app.env_file)]
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

    let Some(port) = plan.state.app_port else {
        return Ok(false);
    };
    if !app.app.envrc {
        return Ok(false);
    }
    let rel = &plan.envrc_rel;
    if git::is_tracked(&env.path, rel) {
        println!("Skipping {rel}: the repo tracks its own");
        return Ok(false);
    }
    envfile::write_envrc(&plan.dir, &app.app.port_env, port)?;
    println!(
        "Wrote {}/.envrc ({}={port})",
        plan.dir.display(),
        app.app.port_env
    );
    Ok(true)
}

/// The env var name for an app's web port: `WT_APP_PORT_<NAME>`, upper case
/// with `-` as `_`, and `DEFAULT` for the default app.
fn app_port_var(id: &AppId) -> String {
    let name = id.label().unwrap_or("default");
    format!("WT_APP_PORT_{}", name.to_uppercase().replace('-', "_"))
}

/// The web port of every configured app this worktree has one for, so a hook
/// can reach another app: an app the config dropped, turned its web port off
/// or moved out of the recorded port's range is left out.
fn web_ports(cfg: &Config, state: &State) -> Vec<(AppId, u16)> {
    cfg.apps()
        .filter(|app| !app.app.port_env.is_empty())
        .filter_map(|app| {
            let port = state.app(&app.id)?.app_port?;
            in_range(port, app.app.port_range).then(|| (app.id.clone(), port))
        })
        .collect()
}

fn hook_env(
    main: &Path,
    plan: &AppUp,
    branch: &str,
    web_ports: &[(AppId, u16)],
) -> Vec<(String, String)> {
    let (app, env) = (plan.app, &plan.env);
    let mut vars = vec![
        ("WT_SLUG".to_string(), env.slug.clone()),
        ("WT_BRANCH".into(), branch.to_string()),
        ("WT_PATH".into(), env.path.display().to_string()),
        ("WT_MAIN_ROOT".into(), main.display().to_string()),
        ("WT_APP".into(), app.id.label().unwrap_or("").to_string()),
        ("WT_APP_DIR".into(), plan.dir.display().to_string()),
    ];
    if let Some(port) = plan.state.app_port {
        vars.push(("WT_APP_PORT".into(), port.to_string()));
    }
    // This app's own services only: another app's are its own business.
    for up in &plan.services {
        let svc = up.svc;
        vars.push((
            svc.kind.port_env_var(),
            plan.state.ports[svc.kind.name()].to_string(),
        ));
        if let Resolved::Database { copies, .. } = &up.resolved {
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
    for (id, port) in web_ports {
        vars.push((app_port_var(id), port.to_string()));
    }
    vars
}

fn report(worktree: &Env, branch: &str, plans: &[AppUp]) {
    println!();
    println!(
        "Worktree {} ready at {} (branch {branch})",
        worktree.slug,
        worktree.path.display()
    );
    for plan in plans {
        let app = plan.app;
        if let Some(name) = app.id.label() {
            println!("  app {name} ({})", app.dir.display());
        }
        for up in &plan.services {
            let name = up.svc.kind.name();
            let port = plan.state.ports[name];
            match &up.resolved {
                Resolved::Container { .. } => {
                    println!("  {name:<9} 127.0.0.1:{port}");
                }
                Resolved::Database { copies, .. } => {
                    let targets: Vec<&str> = copies
                        .iter()
                        .map(|(copy, _)| copy.target.as_str())
                        .collect();
                    println!(
                        "  {name:<9} 127.0.0.1:{port} (databases {})",
                        targets.join(", ")
                    );
                }
            }
        }
        if let Some(port) = plan.state.app_port {
            if plan.envrc_written {
                println!(
                    "  cd {} && source .envrc   # exports {}={port}",
                    plan.dir.display(),
                    app.app.port_env
                );
            }
            // Cookies are scoped by host and ignore the port, so worktrees
            // served on localhost share one session jar and sign each other
            // out.
            println!(
                "  open http://{}.localhost:{port}   # a subdomain per worktree keeps sessions apart",
                worktree.slug
            );
        }
    }
    println!(
        "  wt down {}   # removes the services and the checkout; keeps the branch",
        worktree.slug
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

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
            app: crate::config::AppId::Default,
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
            "",
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
                "",
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
                "",
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
                "",
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
            "",
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
                "",
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
                "",
                services::Kind::Postgres,
                Mode::Container,
                Some(&our_container("db-legacy2")),
                &env
            )
            .is_ok()
        );
        assert!(
            refuse_copies(
                &[],
                "",
                services::Kind::Postgres,
                Mode::Container,
                None,
                &env
            )
            .is_ok()
        );
    }

    #[test]
    fn a_default_up_keeps_the_extra_apps_state() {
        use crate::config::AppName;
        let mut previous = State::default();
        previous.default.app_port = Some(3001);
        let api = AppId::Extra(AppName::parse("coa-api").unwrap());
        previous.app_mut(&api).app_port = Some(8001);
        previous.app_mut(&api).ports.insert("postgres".into(), 5434);
        let default = crate::state::AppState {
            app_port: Some(3002),
            ..Default::default()
        };
        let next = next_state(&previous, vec![(AppId::Default, default)]);
        assert_eq!(next.default.app_port, Some(3002));
        assert_eq!(next.raw_apps(), previous.raw_apps());
        // Bringing up one extra replaces only its own entry.
        let fresh = crate::state::AppState {
            app_port: Some(8002),
            ..Default::default()
        };
        let next = next_state(&previous, vec![(api.clone(), fresh)]);
        assert_eq!(next.default, previous.default);
        assert_eq!(next.app(&api).unwrap().app_port, Some(8002));
        assert!(next.app(&api).unwrap().ports.is_empty());
    }

    fn monorepo() -> Config {
        Config::from_toml(
            r#"
            [services.postgres]
            source = "pg-1"
            [apps.coa-api]
            dir = "apps/coa-api"
            [apps.coa-api.app]
            port_env = "COA_API_PORT"
            [apps.coa-api.services.postgres]
            source = "1f2e3d4c"
            mode = "database"
            databases = ["app"]
            env = { DATABASE_URL = "postgres://x@127.0.0.1:{port}/{database}" }
            [apps.web]
            dir = "."
            [apps.web.app]
            env_file = "web.env"
            envrc = false
            "#,
        )
        .unwrap()
    }

    fn env_for(cfg: &Config, app: &AppConfig) -> Env {
        let mut env = Env::new(cfg, "feat-x");
        env.path = "/w/feat-x".into();
        env.with_app(&app.id)
    }

    fn named(name: &str) -> Container {
        let json = format!(
            r#"[{{"Name": "/{name}", "Config": {{"Image": "postgres"}},
            "State": {{"Running": true}}, "Mounts": [], "HostConfig": {{}}}}]"#
        );
        serde_json::from_str::<Vec<Container>>(&json)
            .unwrap()
            .remove(0)
    }

    fn cloned<'a>(svc: &'a crate::config::Service, container: &str) -> ServiceUp<'a> {
        ServiceUp {
            svc,
            resolved: Resolved::Container {
                source: Source {
                    container: named(container),
                    volume: "v".into(),
                    data_dir: "/d".into(),
                    container_port: 5432,
                },
            },
        }
    }

    fn copied<'a>(
        svc: &'a crate::config::Service,
        container: &str,
        dbs: &[(&str, &str)],
    ) -> ServiceUp<'a> {
        ServiceUp {
            svc,
            resolved: Resolved::Database {
                src: ContainerSource {
                    container: named(container),
                    container_port: 5432,
                },
                port: 5432,
                copies: dbs
                    .iter()
                    .map(|(source_db, target)| {
                        (
                            DatabaseCopy {
                                service: "postgres".into(),
                                source: container.into(),
                                source_db: source_db.to_string(),
                                target: target.to_string(),
                            },
                            MarkerState::Missing,
                        )
                    })
                    .collect(),
            },
        }
    }

    fn plan<'a>(cfg: &Config, app: &'a AppConfig, services: Vec<ServiceUp<'a>>) -> AppUp<'a> {
        let env = env_for(cfg, app);
        AppUp {
            app,
            dir: app_root(app, &env),
            envrc_rel: envrc_rel(app),
            env,
            services,
            prior: AppState::default(),
            state: AppState::default(),
            kept: Vec::new(),
            envrc_written: false,
        }
    }

    #[test]
    fn two_apps_never_get_one_web_port() {
        let cfg = monorepo();
        let mut claimed = HashSet::new();
        let mut a = plan(&cfg, &cfg.extras[0], Vec::new());
        let mut b = plan(&cfg, &cfg.extras[1], Vec::new());
        resolve_ports(&mut a, &mut claimed).unwrap();
        resolve_ports(&mut b, &mut claimed).unwrap();
        let (pa, pb) = (a.state.app_port.unwrap(), b.state.app_port.unwrap());
        assert_ne!(pa, pb);
        assert!(claimed.contains(&pa) && claimed.contains(&pb));
    }

    #[test]
    fn a_web_port_is_kept_only_while_in_range() {
        let cfg = monorepo();
        let mut kept = plan(&cfg, &cfg.extras[1], Vec::new());
        kept.prior.app_port = Some(3050);
        resolve_ports(&mut kept, &mut HashSet::new()).unwrap();
        assert_eq!(kept.state.app_port, Some(3050));
        let mut moved = plan(&cfg, &cfg.extras[1], Vec::new());
        moved.prior.app_port = Some(9999);
        resolve_ports(&mut moved, &mut HashSet::new()).unwrap();
        assert!(in_range(moved.state.app_port.unwrap(), (3001, 3099)));
    }

    #[test]
    fn hooks_see_their_own_services_and_every_apps_web_port() {
        let cfg = monorepo();
        let mut default = plan(
            &cfg,
            &cfg.default,
            vec![cloned(&cfg.default.services[0], "pg-1")],
        );
        default.state.app_port = Some(3001);
        default.state.ports.insert("postgres".into(), 5433);
        let api_app = &cfg.extras[0];
        let mut api = plan(
            &cfg,
            api_app,
            vec![copied(
                &api_app.services[0],
                "pg-1",
                &[("app", "wt_myapp_feat_x__coa_api__app")],
            )],
        );
        api.state.app_port = Some(3002);
        api.state.ports.insert("postgres".into(), 5432);
        let ports = vec![(AppId::Default, 3001), (api_app.id.clone(), 3002)];
        let vars: BTreeMap<String, String> = hook_env(Path::new("/main"), &api, "feat/x", &ports)
            .into_iter()
            .collect();
        assert_eq!(vars["WT_APP"], "coa-api");
        assert_eq!(vars["WT_APP_DIR"], "/w/feat-x/apps/coa-api");
        assert_eq!(vars["WT_PATH"], "/w/feat-x");
        assert_eq!(vars["WT_APP_PORT"], "3002");
        // Its own service, never the default app's clone.
        assert_eq!(vars["WT_POSTGRES_PORT"], "5432");
        assert_eq!(
            vars["WT_POSTGRES_DATABASE"],
            "wt_myapp_feat_x__coa_api__app"
        );
        assert_eq!(
            vars["WT_POSTGRES_DATABASES"],
            "app=wt_myapp_feat_x__coa_api__app"
        );
        assert_eq!(vars["WT_APP_PORT_DEFAULT"], "3001");
        assert_eq!(vars["WT_APP_PORT_COA_API"], "3002");
        let vars: BTreeMap<String, String> =
            hook_env(Path::new("/main"), &default, "feat/x", &ports)
                .into_iter()
                .collect();
        assert_eq!(vars["WT_APP"], "");
        assert_eq!(vars["WT_APP_DIR"], "/w/feat-x");
        assert_eq!(vars["WT_POSTGRES_PORT"], "5433");
        assert!(!vars.contains_key("WT_POSTGRES_DATABASE"));
    }

    #[test]
    fn web_ports_cover_configured_apps_that_have_one() {
        let cfg = monorepo();
        let mut state = State::default();
        state.default.app_port = Some(3001);
        state.app_mut(&cfg.extras[0].id).app_port = Some(3002);
        // An app removed from the config is not wired into hooks.
        state.raw_apps_mut().insert(
            "gone".into(),
            AppState {
                app_port: Some(3003),
                ..Default::default()
            },
        );
        let ids: Vec<(String, u16)> = web_ports(&cfg, &state)
            .into_iter()
            .map(|(id, p)| (id.to_string(), p))
            .collect();
        assert_eq!(
            ids,
            [("default".to_string(), 3001), ("coa-api".to_string(), 3002)]
        );
    }

    #[test]
    fn an_extras_env_file_gets_its_own_web_port() {
        let cfg = monorepo();
        let mut api = plan(&cfg, &cfg.extras[0], Vec::new());
        api.state.app_port = Some(3002);
        assert_eq!(
            env_values(&api).unwrap(),
            [("COA_API_PORT".to_string(), "3002".to_string())]
        );
    }

    #[test]
    fn an_extra_writes_its_env_and_envrc_in_its_dir() {
        let cfg = monorepo();
        let root = std::env::temp_dir().join(format!("wt-up-envrc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("apps/coa-api")).unwrap();
        let mut api = plan(&cfg, &cfg.extras[0], Vec::new());
        api.env.path = root.clone();
        api.dir = root.join("apps/coa-api");
        api.state.app_port = Some(3002);
        assert!(write_env(&root.join("main"), &api).unwrap());
        assert!(
            fs::read_to_string(root.join("apps/coa-api/.env"))
                .unwrap()
                .contains("COA_API_PORT=3002")
        );
        assert!(
            fs::read_to_string(root.join("apps/coa-api/.envrc"))
                .unwrap()
                .contains("export COA_API_PORT=3002")
        );
        assert!(!root.join(".envrc").exists());
        // envrc = false writes none, and says so.
        let mut web = plan(&cfg, &cfg.extras[1], Vec::new());
        web.env.path = root.clone();
        web.dir = root.clone();
        web.state.app_port = Some(3003);
        assert!(!write_env(&root.join("main"), &web).unwrap());
        assert!(!root.join(".envrc").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_extras_envrc_sits_in_its_dir() {
        let cfg = monorepo();
        assert_eq!(envrc_rel(&cfg.default), ".envrc");
        assert_eq!(envrc_rel(&cfg.extras[0]), "apps/coa-api/.envrc");
    }

    fn uses(plans: &[AppUp]) -> Result<()> {
        check_sources(&selected_sources(plans))
    }

    #[test]
    fn one_container_is_used_in_one_mode_whatever_it_is_called() {
        let cfg = monorepo();
        let api_app = &cfg.extras[0];
        // `1f2e3d4c` (an ID) and `pg-1` (a name) resolved to one container.
        let plans = [
            plan(
                &cfg,
                &cfg.default,
                vec![cloned(&cfg.default.services[0], "pg-1")],
            ),
            plan(
                &cfg,
                api_app,
                vec![copied(&api_app.services[0], "pg-1", &[("app", "t")])],
            ),
        ];
        let e = format!("{:#}", uses(&plans).unwrap_err());
        assert!(
            e.contains("pg-1")
                && e.contains("apps.coa-api.services.postgres")
                && e.contains("different modes"),
            "{e}"
        );
        let apart = [
            plan(
                &cfg,
                &cfg.default,
                vec![cloned(&cfg.default.services[0], "pg-1")],
            ),
            plan(
                &cfg,
                api_app,
                vec![copied(&api_app.services[0], "pg-2", &[("app", "t")])],
            ),
        ];
        assert!(uses(&apart).is_ok());
    }

    #[test]
    fn one_source_database_is_copied_once_per_worktree() {
        let svc = |kind: Kind, dbs: &[&str]| SourceUse {
            key: "x".into(),
            kind,
            mode: Mode::Database,
            container: "c".into(),
            databases: dbs.iter().map(|d| d.to_string()).collect(),
            selected: true,
        };
        let mut a = svc(Kind::Postgres, &["app"]);
        let mut b = svc(Kind::Postgres, &["other", "app"]);
        a.key = "services.postgres".into();
        b.key = "apps.b.services.postgres".into();
        let e = format!("{:#}", check_sources(&[a.clone(), b.clone()]).unwrap_err());
        assert!(
            e.contains("\"app\"") && e.contains("apps.b.services.postgres"),
            "{e}"
        );
        b.databases = vec!["App".into()];
        assert!(check_sources(&[a.clone(), b.clone()]).is_ok());
        let (mut ma, mut mb) = (svc(Kind::Mysql, &["app"]), svc(Kind::Mysql, &["App"]));
        ma.key = "services.mysql".into();
        mb.key = "apps.b.services.mysql".into();
        assert!(check_sources(&[ma, mb]).is_err());
    }

    #[test]
    fn apps_left_out_of_the_run_still_count() {
        let cfg = monorepo();
        let mut previous = State::default();
        previous
            .app_mut(&cfg.extras[0].id)
            .databases
            .push(DatabaseCopy {
                service: "postgres".into(),
                source: "1f2e3d4c".into(),
                source_db: "app".into(),
                target: "t".into(),
            });
        let lookup = |source: &str| -> Result<Option<String>> {
            Ok(match source {
                "pg-1" | "1f2e3d4c" => Some("pg-1".to_string()),
                _ => None,
            })
        };
        // Only the default app comes up; coa-api's copy lives in its source.
        let others = other_sources(&cfg, &[&cfg.default], &previous, lookup).unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].key, "apps.coa-api.services.postgres");
        assert_eq!(others[0].container, "pg-1");
        assert_eq!(others[0].databases, ["app"]);
        let plans = [plan(
            &cfg,
            &cfg.default,
            vec![cloned(&cfg.default.services[0], "pg-1")],
        )];
        let mut all = selected_sources(&plans);
        all.extend(others);
        assert!(check_sources(&all).is_err());
        // A source that resolves to nothing is skipped, not a blocker.
        assert!(
            other_sources(&cfg, &[&cfg.default], &previous, |_| Ok(None))
                .unwrap()
                .is_empty()
        );
        // Docker failing is not "no such container": the check must not be
        // skipped silently.
        let failing = |_: &str| -> Result<Option<String>> { bail!("daemon hiccup") };
        assert!(other_sources(&cfg, &[&cfg.default], &previous, failing).is_err());
    }

    #[test]
    fn docker_is_needed_when_any_selected_app_has_services() {
        let cfg = monorepo();
        assert!(needs_docker(&[&cfg.extras[1], &cfg.extras[0]]));
        assert!(!needs_docker(&[&cfg.extras[1]]));
    }

    #[test]
    fn an_apps_dir_must_be_a_folder_inside_the_worktree() {
        let cfg = monorepo();
        let root = std::env::temp_dir().join(format!("wt-up-appdir-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("wt-up-appdir-out-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(root.join("apps")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let config = Path::new("/c/.wt.toml");
        let mut api = plan(&cfg, &cfg.extras[0], Vec::new());
        api.env.path = root.clone();
        let e = format!("{:#}", check_app_dir(&api, config).unwrap_err());
        assert!(
            e.contains("apps.coa-api.dir")
                && e.contains("does not exist")
                && e.contains("/c/.wt.toml"),
            "{e}"
        );
        fs::write(root.join("apps/coa-api"), "").unwrap();
        let e = format!("{:#}", check_app_dir(&api, config).unwrap_err());
        assert!(e.contains("not a directory"), "{e}");
        fs::remove_file(root.join("apps/coa-api")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("apps/coa-api")).unwrap();
        let e = format!("{:#}", check_app_dir(&api, config).unwrap_err());
        assert!(e.contains("leaves the worktree"), "{e}");
        fs::remove_file(root.join("apps/coa-api")).unwrap();
        fs::create_dir_all(root.join("apps/coa-api")).unwrap();
        assert_eq!(
            check_app_dir(&api, config).unwrap(),
            root.canonicalize().unwrap().join("apps/coa-api")
        );
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn mode_change_errors_name_the_app() {
        let env = env();
        let e = format!(
            "{:#}",
            refuse_leftover(
                "apps.b.",
                Kind::Postgres,
                Mode::Database,
                Some(&our_container("db-legacy")),
                None,
                &env
            )
            .unwrap_err()
        );
        assert!(e.contains("apps.b.services.postgres is now mode"), "{e}");
        let e = format!(
            "{:#}",
            refuse_copies(
                &copies(),
                "apps.b.",
                Kind::Postgres,
                Mode::Container,
                None,
                &env
            )
            .unwrap_err()
        );
        assert!(e.contains("apps.b.services.postgres is now mode"), "{e}");
    }

    // ---- Round 2: sources across runs, app dirs on the branch ----

    fn db_config(other: &str) -> Config {
        Config::from_toml(&format!(
            r#"
            [services.postgres]
            source = "pg-1"
            mode = "database"
            databases = ["app"]
            env = {{ U = "x/{{database}}" }}
            [apps.b]
            dir = "b"
            [apps.b.services.postgres]
            source = "pgb"
            mode = "database"
            env = {{ U = "x/{{database}}" }}
            {other}
            "#
        ))
        .unwrap()
    }

    fn default_copying_app(cfg: &Config) -> AppUp<'_> {
        plan(
            cfg,
            &cfg.default,
            vec![copied(&cfg.default.services[0], "pg-1", &[("app", "t")])],
        )
    }

    fn pgb_is_pg1(source: &str) -> Result<Option<String>> {
        Ok(matches!(source, "pg-1" | "pgb").then(|| "pg-1".to_string()))
    }

    #[test]
    fn a_left_out_app_counts_the_databases_it_lists() {
        let cfg = db_config("databases = [\"app\"]");
        let plans = [default_copying_app(&cfg)];
        let mut all = selected_sources(&plans);
        all.extend(other_sources(&cfg, &[&cfg.default], &State::default(), pgb_is_pg1).unwrap());
        let e = format!("{:#}", check_sources(&all).unwrap_err());
        assert!(e.contains("would both copy \"app\""), "{e}");
    }

    #[test]
    fn a_left_out_app_counts_the_databases_its_copies_record() {
        let cfg = db_config("");
        let b = &cfg.extras[0].id;
        let recorded = |service: &str| {
            let mut previous = State::default();
            previous.app_mut(b).databases.push(DatabaseCopy {
                service: service.into(),
                source: "pgb".into(),
                source_db: "app".into(),
                target: "t".into(),
            });
            previous
        };
        let plans = [default_copying_app(&cfg)];
        let mut all = selected_sources(&plans);
        all.extend(
            other_sources(&cfg, &[&cfg.default], &recorded("postgres"), pgb_is_pg1).unwrap(),
        );
        assert!(check_sources(&all).is_err());
        // Another service's copy says nothing about this one.
        let mut all = selected_sources(&plans);
        all.extend(other_sources(&cfg, &[&cfg.default], &recorded("mysql"), pgb_is_pg1).unwrap());
        assert!(check_sources(&all).is_ok());
    }

    #[test]
    fn two_selected_apps_cannot_copy_one_database() {
        let cfg = db_config("databases = [\"app\"]");
        let b = &cfg.extras[0];
        let plans = [
            default_copying_app(&cfg),
            plan(
                &cfg,
                b,
                vec![copied(&b.services[0], "pg-1", &[("app", "t2")])],
            ),
        ];
        let e = format!("{:#}", uses(&plans).unwrap_err());
        assert!(
            e.contains("would both copy \"app\"") && e.contains("apps.b.services.postgres"),
            "{e}"
        );
    }

    #[test]
    fn one_container_is_one_service_type() {
        let cfg = Config::from_toml(
            "[services.postgres]\nsource = \"c\"\n[apps.b]\ndir = \"b\"\n[apps.b.services.mysql]\nsource = \"c2\"",
        )
        .unwrap();
        let b = &cfg.extras[0];
        let plans = [
            plan(
                &cfg,
                &cfg.default,
                vec![cloned(&cfg.default.services[0], "c")],
            ),
            plan(&cfg, b, vec![cloned(&b.services[0], "c")]),
        ];
        let e = format!("{:#}", uses(&plans).unwrap_err());
        assert!(e.contains("cannot be a postgres and a mysql"), "{e}");
    }

    #[test]
    fn every_pair_is_compared_and_left_out_pairs_are_not() {
        let at = |key: &str, mode: Mode, selected: bool| SourceUse {
            key: key.into(),
            kind: Kind::Postgres,
            mode,
            container: "c".into(),
            databases: Vec::new(),
            selected,
        };
        // The first and the third clash; the second is fine with both.
        let uses = [
            at("services.postgres", Mode::Container, true),
            at("apps.x.services.postgres", Mode::Container, false),
            at("apps.y.services.postgres", Mode::Database, false),
        ];
        assert!(check_sources(&uses).is_err());
        // Two apps this run leaves alone are not this run's business.
        let left_out = [
            at("services.postgres", Mode::Container, true),
            at("apps.x.services.postgres", Mode::Database, false),
        ];
        assert!(check_sources(&left_out).is_err());
        let others = [
            at("apps.w.services.redis", Mode::Container, true),
            at("apps.x.services.postgres", Mode::Container, false),
            at("apps.y.services.postgres", Mode::Database, false),
        ];
        let others: Vec<SourceUse> = others
            .into_iter()
            .map(|mut u| {
                if u.selected {
                    u.container = "elsewhere".into();
                }
                u
            })
            .collect();
        assert!(check_sources(&others).is_ok());
    }

    #[test]
    fn web_ports_skip_apps_without_a_usable_one() {
        let cfg = Config::from_toml(
            "[app]\nport_env = \"\"\n[apps.b]\ndir = \"b\"\n[apps.b.app]\nport_range = [8001, 8099]",
        )
        .unwrap();
        let mut state = State::default();
        state.default.app_port = Some(3001);
        state.app_mut(&cfg.extras[0].id).app_port = Some(3002);
        assert!(web_ports(&cfg, &state).is_empty());
        state.app_mut(&cfg.extras[0].id).app_port = Some(8001);
        assert_eq!(web_ports(&cfg, &state).len(), 1);
    }

    #[test]
    fn a_tracked_envrc_is_found_through_a_symlinked_dir() {
        let root = Path::new("/real/w");
        assert_eq!(
            envrc_rel_in(root, &root.join("apps/web")),
            "apps/web/.envrc"
        );
        assert_eq!(envrc_rel_in(root, root), ".envrc");
    }

    fn git(repo: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn app_dirs_are_checked_on_the_branch_not_a_tag() {
        let repo = std::env::temp_dir().join(format!("wt-up-appdirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        fs::write(repo.join("README"), "x").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "main"]);
        // A tag named like the branch, pointing at main, which lacks apps/x.
        git(&repo, &["tag", "feat"]);
        git(&repo, &["checkout", "--quiet", "-b", "feat"]);
        fs::create_dir_all(repo.join("apps/x")).unwrap();
        fs::write(repo.join("apps/x/keep"), "").unwrap();
        fs::write(repo.join("apps/f"), "").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "feat"]);
        git(&repo, &["checkout", "--quiet", "main"]);

        let cfg =
            Config::from_toml("[apps.x]\ndir = \"apps/x\"\n[apps.f]\ndir = \"apps/f\"").unwrap();
        let (x, f) = (&cfg.extras[0], &cfg.extras[1]);
        let config = Path::new("/c/.wt.toml");
        let branch = app_dir_rev(&repo, "feat", None);
        assert_eq!(
            (branch.spec.as_str(), branch.label.as_str()),
            ("refs/heads/feat", "feat")
        );
        let head = app_dir_rev(&repo, "new", None);
        assert_eq!((head.spec.as_str(), head.label.as_str()), ("HEAD", "HEAD"));
        let base = app_dir_rev(&repo, "new", Some("feat"));
        assert_eq!(
            (base.spec.as_str(), base.label.as_str()),
            ("feat", "--base feat")
        );
        assert!(check_app_dirs_on(&repo, &branch, &[x], config).is_ok());
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &head, &[x], config).unwrap_err()
        );
        assert!(
            e.contains("apps.x.dir") && e.contains("not a folder on HEAD"),
            "{e}"
        );
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &branch, &[f], config).unwrap_err()
        );
        assert!(e.contains("is a file or a symlink on feat"), "{e}");
        // Every app is checked, not just the first.
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &branch, &[x, f], config).unwrap_err()
        );
        assert!(e.contains("apps.f.dir"), "{e}");
        let typo = app_dir_rev(&repo, "new", Some("orgin/main"));
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &typo, &[x], config).unwrap_err()
        );
        assert!(
            e.contains("--base orgin/main is not a commit; nothing was created"),
            "{e}"
        );
        assert!(check_app_dirs_on(&repo, &head, &[&cfg.default], config).is_ok());
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn an_empty_repo_says_so() {
        let repo = std::env::temp_dir().join(format!("wt-up-unborn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        let cfg = Config::from_toml("[apps.x]\ndir = \"apps/x\"").unwrap();
        let head = app_dir_rev(&repo, "new", None);
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &head, &[&cfg.extras[0]], Path::new("/c")).unwrap_err()
        );
        assert!(e.contains("HEAD has no commit yet"), "{e}");
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn the_first_after_services_dir_is_relative_to_the_worktree() {
        let cfg = Config::from_toml(
            "[apps.b]\ndir = \"apps/b\"\n[apps.b.hooks]\nafter_services = [{ run = [\"true\"], dir = \"apps/b/x\" }]",
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!("wt-up-after-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("apps/b/x")).unwrap();
        let mut b = plan(&cfg, &cfg.extras[0], Vec::new());
        b.env.path = root.clone();
        assert!(check_first_after_services(&[b], Path::new("/c")).is_ok());
        fs::remove_dir_all(root.join("apps/b/x")).unwrap();
        let mut b = plan(&cfg, &cfg.extras[0], Vec::new());
        b.env.path = root.clone();
        let e = format!(
            "{:#}",
            check_first_after_services(&[b], Path::new("/c")).unwrap_err()
        );
        assert!(e.contains("apps.b.hooks.after_services[0]"), "{e}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolving_app_dirs_finds_a_tracked_envrc_through_a_symlink() {
        let cfg = Config::from_toml("[apps.b]\ndir = \"apps/link\"").unwrap();
        let root = std::env::temp_dir().join(format!("wt-up-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("apps/web")).unwrap();
        std::os::unix::fs::symlink("web", root.join("apps/link")).unwrap();
        let mut b = plan(&cfg, &cfg.extras[0], Vec::new());
        b.env.path = root.clone();
        b.dir = app_root(b.app, &b.env);
        let mut plans = [b];
        resolve_app_dirs(&mut plans, Path::new("/c")).unwrap();
        assert_eq!(plans[0].envrc_rel, "apps/web/.envrc");
        // What wt shows and exports stays the path as written.
        assert_eq!(plans[0].dir, root.join("apps/link"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_worktree_at_the_path_is_reused_only_for_this_branch() {
        let entry = |branch: Option<&str>| git::Entry {
            path: "/w/feat-x".into(),
            branch: branch.map(String::from),
        };
        let path = Path::new("/w/feat-x");
        assert!(reuses(&entry(Some("feat/x")), path, "feat/x").unwrap());
        // A detached HEAD at the path is reused too, as create_worktree does.
        assert!(reuses(&entry(None), path, "feat/x").unwrap());
        let e = format!(
            "{:#}",
            reuses(&entry(Some("feat-x")), path, "feat/x").unwrap_err()
        );
        assert!(e.contains("already holds branch feat-x"), "{e}");
    }

    #[test]
    fn an_app_dir_linked_to_another_apps_dir_is_refused() {
        let cfg = Config::from_toml(
            "[apps.c]\ndir = \"apps/c\"\n[apps.c.app]\nport_env = \"VITE_PORT\"\n[apps.e]\ndir = \"apps/e\"\n[apps.e.app]\nport_env = \"E_PORT\"",
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!("wt-up-twin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("apps/c")).unwrap();
        std::os::unix::fs::symlink("c", root.join("apps/e")).unwrap();
        let e_app = &cfg.extras[1];
        let mut e = plan(&cfg, e_app, Vec::new());
        e.env.path = root.clone();
        let err = format!("{:#}", check_shared_outputs(&cfg, &[e], &root).unwrap_err());
        assert!(
            err.contains("apps.e.app.env_file") && err.contains("apps.c.app.env_file"),
            "{err}"
        );
        // Two apps in one real folder with their own files are fine.
        fs::remove_file(root.join("apps/e")).unwrap();
        fs::create_dir_all(root.join("apps/e")).unwrap();
        let mut e = plan(&cfg, e_app, Vec::new());
        e.env.path = root.clone();
        assert!(check_shared_outputs(&cfg, &[e], &root).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bad_base_is_named_even_for_the_default_app() {
        let repo = std::env::temp_dir().join(format!("wt-up-basedefault-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        let cfg = Config::from_toml("").unwrap();
        let head = app_dir_rev(&repo, "new", None);
        let e = format!(
            "{:#}",
            check_app_dirs_on(&repo, &head, &[&cfg.default], Path::new("/c")).unwrap_err()
        );
        assert!(e.contains("HEAD has no commit yet"), "{e}");
        fs::remove_dir_all(&repo).unwrap();
    }
}
