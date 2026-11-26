use std::collections::{BTreeSet, HashMap};

use anyhow::Result;

use crate::cmd::known_slugs;
use crate::config::{AppId, AppName, Config};
use crate::database;
use crate::docker::{self, Container, Volume};
use crate::project::{self, Env, LABEL_APP, LABEL_PROJECT};
use crate::state::{AppState, State};
use crate::{git, services::Kind};

/// One row: one app of one worktree, with only that app's resources.
#[derive(Debug)]
struct Row {
    env: Env,
    branch: String,
    containers: Vec<Container>,
    volumes: Vec<Volume>,
    /// This app's state, its copies as `down` would drop them: a copy listed
    /// under several apps belongs to the default app, else to the first extra
    /// by name. When `down` would refuse (a bad key with copies), each app
    /// keeps the copies its own table lists.
    state: AppState,
    /// The config gives this app no services; for an app the config dropped,
    /// the state recorded none.
    no_services: bool,
}

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

    // One listing for the whole project (two more with a slug label), split by
    // slug and app below.
    let (all_containers, all_volumes) = if docker_ok {
        project_resources(&cfg)?
    } else {
        (Vec::new(), Vec::new())
    };

    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for slug in &slugs {
        let worktree = Env::new(&cfg, slug);
        let state = State::load(&worktree.project, &worktree.slug);
        let branch = git::current_branch(&worktree.path).unwrap_or_else(|| "-".into());
        let (found, unclaimed) = worktree_rows(
            &cfg,
            &worktree,
            &branch,
            &all_containers,
            &all_volumes,
            &state,
        );
        rows.extend(found);
        notes.extend(unclaimed);
    }

    // Only where Docker could matter: a repo without services on a machine
    // without Docker hears nothing.
    if !docker_ok && docker_matters(&cfg, &rows) {
        notes.insert(
            0,
            "Docker is not reachable; container status and apps only a label names are not shown"
                .to_string(),
        );
    }
    let kinds = kinds(&cfg, &rows);
    let several = several(&cfg, &rows);
    let mut sources = Sources::default();
    let mut table = vec![header(&kinds, several)];
    for row in &rows {
        let env = &row.env;
        let mut cells = vec![env.slug.clone()];
        if several {
            cells.push(env.app.label().unwrap_or("-").to_string());
        }
        cells.push(row.branch.clone());
        cells.push(port_cell(row.state.app_port));
        for kind in &kinds {
            let published = row
                .containers
                .iter()
                .find(|c| env.service_of(c.name(), |k| c.label(k)) == Some(*kind))
                .and_then(|c| c.host_ports().first().copied());
            cells.push(port_cell(
                published.or(row.state.ports.get(kind.name()).copied()),
            ));
        }
        cells.push(status(row, docker_ok, &mut sources).into());
        let disk: Vec<&str> = row
            .volumes
            .iter()
            .filter_map(|v| sizes.get(&v.name).map(String::as_str))
            .collect();
        cells.push(if disk.is_empty() {
            "-".into()
        } else {
            disk.join("+")
        });
        table.push(cells);
    }

    print_table(&table);
    notes.extend(sources.reasons.into_values());
    for note in notes {
        eprintln!("warning: {note}");
    }
    Ok(())
}

/// One worktree's rows, one per app, from the project's resources (only this
/// slug's, helpers left out), and a note for everything `down` would act on
/// that no row shows: copies under a state key that is not an app name, and
/// resources whose `wt.app` label is not one.
fn worktree_rows(
    cfg: &Config,
    worktree: &Env,
    branch: &str,
    containers: &[Container],
    volumes: &[Volume],
    state: &State,
) -> (Vec<Row>, Vec<String>) {
    let containers: Vec<&Container> = containers
        .iter()
        .filter(|c| worktree.owns(|k| c.label(k)) && !worktree.owns_helper(|k| c.label(k)))
        .collect();
    let volumes: Vec<&Volume> = volumes
        .iter()
        .filter(|v| worktree.owns(|k| v.label(k)))
        .collect();
    let slug = &worktree.slug;
    let mut notes = BTreeSet::new();
    let labelled = containers
        .iter()
        .map(|c| ("container", c.name(), c.label(LABEL_APP)))
        .chain(
            volumes
                .iter()
                .map(|v| ("volume", v.name.as_str(), v.label(LABEL_APP))),
        );
    let mut labels = Vec::new();
    for (what, name, label) in labelled {
        let Some(label) = label else { continue };
        if AppName::parse(label).is_ok() {
            labels.push(label);
        } else {
            notes.insert(format!(
                "{what} {name} of {slug} is labelled wt.app={label:?}, which is not an app name; wt down {slug} removes it"
            ));
        }
    }
    for (key, id, app) in state.extras() {
        if id.is_err() && app.databases.is_empty() {
            notes.insert(format!(
                "{slug}: apps.{key:?} in its state file is not an app name; its ports stay taken until that table is removed"
            ));
        }
    }
    // The copies as `down` sees them, so ls and down agree on who owns each.
    let copies = match state.copies_by_app(worktree) {
        Ok(copies) => Some(copies),
        Err(e) => {
            notes.insert(format!(
                "{slug}: {e:#}; wt down {slug} refuses the whole worktree until that table is fixed"
            ));
            None
        }
    };

    let mut rows = Vec::new();
    for id in worktree_apps(cfg, state, &labels) {
        let env = worktree.clone().with_app(&id);
        let recorded = state.app(&id).cloned().unwrap_or_default();
        let mut app_state = recorded.clone();
        if let Some(copies) = &copies {
            app_state.databases = copies
                .iter()
                .filter(|(e, _)| e.app == id)
                .map(|(_, c)| (*c).clone())
                .collect();
        }
        let no_services = match cfg.app_by_id(&id) {
            Some(app) => app.services.is_empty(),
            // Its own record, before copies it shares move to another row.
            None => recorded.ports.is_empty() && recorded.databases.is_empty(),
        };
        rows.push(Row {
            branch: branch.to_string(),
            containers: containers
                .iter()
                .filter(|c| env.is_its_app(|k| c.label(k)))
                .map(|c| (*c).clone())
                .collect(),
            volumes: volumes
                .iter()
                .filter(|v| env.is_its_app(|k| v.label(k)))
                .map(|v| (*v).clone())
                .collect(),
            env,
            state: app_state,
            no_services,
        });
    }
    (rows, notes.into_iter().collect())
}

/// The apps of one worktree, one row each: the default app, then every extra
/// the state records or a resource is labelled with, the config's in its
/// order and the rest (apps the config dropped) by name. A key or label that
/// is not an app name gets no row.
fn worktree_apps(cfg: &Config, state: &State, labels: &[&str]) -> Vec<AppId> {
    let mut found: BTreeSet<AppName> = state
        .extras()
        .filter_map(|(_, id, _)| id.ok()?.name().cloned())
        .collect();
    found.extend(labels.iter().filter_map(|l| AppName::parse(l).ok()));
    let mut apps = vec![AppId::Default];
    for app in &cfg.extras {
        if let Some(name) = app.id.name()
            && found.remove(name)
        {
            apps.push(app.id.clone());
        }
    }
    apps.extend(found.into_iter().map(AppId::Extra));
    apps
}

/// A column for each configured service of any app, and for any a row still
/// has (a container, a copy or a recorded port) after the config dropped it.
fn kinds(cfg: &Config, rows: &[Row]) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = Vec::new();
    let mut add = |kind: Kind| {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    };
    cfg.services().for_each(|s| add(s.kind));
    for row in rows {
        for c in &row.containers {
            if let Some(kind) = row.env.service_of(c.name(), |k| c.label(k)) {
                add(kind);
            }
        }
        for copy in &row.state.databases {
            if let Some(kind) = Kind::from_name(&copy.service) {
                add(kind);
            }
        }
        for name in row.state.ports.keys() {
            if let Some(kind) = Kind::from_name(name) {
                add(kind);
            }
        }
    }
    kinds
}

/// Whether Docker could change what `ls` shows: some app of the config has
/// services, or a worktree's state records service ports or copies.
fn docker_matters(cfg: &Config, rows: &[Row]) -> bool {
    cfg.services().next().is_some()
        || rows
            .iter()
            .any(|r| !r.state.ports.is_empty() || !r.state.databases.is_empty())
}

/// Whether the table needs an `APP` column: the config has several apps, or
/// a worktree still has an app the config dropped. A one-app config with
/// nothing else keeps the table it always had.
fn several(cfg: &Config, rows: &[Row]) -> bool {
    !cfg.extras.is_empty() || rows.iter().any(|r| r.env.app != AppId::Default)
}

/// The table's first line, with `APP` when `several` says so.
fn header(kinds: &[Kind], several: bool) -> Vec<String> {
    let mut header = vec!["SLUG"];
    if several {
        header.push("APP");
    }
    header.extend(["BRANCH", "WEB"]);
    header.extend(kinds.iter().map(|k| k.name()));
    header.extend(["STATUS", "DISK"]);
    header.iter().map(|s| s.to_uppercase()).collect()
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

/// Each source container looked up once for the whole table, and the first
/// reason it made a row `unknown`.
#[derive(Default)]
struct Sources {
    found: HashMap<String, Option<Container>>,
    reasons: std::collections::BTreeMap<String, String>,
}

impl Sources {
    fn get(&mut self, name: &str) -> Option<&Container> {
        self.found
            .entry(name.to_string())
            .or_insert_with(|| docker::container(name))
            .as_ref()
    }

    fn unknown(&mut self, source: &str, why: String) -> Option<&'static str> {
        self.reasons.entry(source.to_string()).or_insert(why);
        Some("unknown")
    }
}

/// One app's verdict, from its own containers, volumes and copies.
fn status(row: &Row, docker_ok: bool, sources: &mut Sources) -> &'static str {
    // A missing checkout is a filesystem fact and needs no daemon.
    if !row.env.path.is_dir() {
        return "orphan";
    }
    let copies = &row.state.databases;
    if !docker_ok && !copies.is_empty() {
        return "unknown";
    }
    if !row.containers.is_empty() {
        let running = row.containers.iter().filter(|c| c.state.running).count();
        return match (row.containers.len(), running) {
            (n, r) if n == r => "running",
            (_, 0) => "stopped",
            _ => "partial",
        };
    }
    if docker_ok
        && !copies.is_empty()
        && let Some(verdict) = database_status(&row.state, sources)
    {
        return verdict;
    }
    if row.no_services && row.volumes.is_empty() {
        "ready"
    } else {
        "no-db"
    }
}

/// Whether the app's registered copies still exist. One query per copy, and
/// only when the app has no container of its own. A stopped source (or a
/// probe that fails) means "cannot tell", never "the copies are gone"; the
/// first such reason per source is kept for a warning.
fn database_status(state: &AppState, sources: &mut Sources) -> Option<&'static str> {
    if state.databases.is_empty() {
        return None;
    }
    let mut existing = 0;
    for copy in &state.databases {
        let source = copy.source.as_str();
        // The volume may outlive the container, so a missing source is
        // "cannot tell", not "the copies are gone".
        let Some(container) = sources.get(source).cloned() else {
            return sources.unknown(
                source,
                format!("source {source} is gone, so its copies are unknown"),
            );
        };
        if !container.state.running {
            return sources.unknown(
                source,
                format!("source {source} is stopped, so its copies are unknown"),
            );
        }
        match database::exists(copy, &container) {
            Ok(true) => existing += 1,
            Ok(false) => {}
            Err(e) => {
                return sources.unknown(
                    source,
                    format!("could not check the copies in {source}: {e:#}"),
                );
            }
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::state::DatabaseCopy;

    /// Removes a test's folder even when an assert fails.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wt-ls-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn cfg() -> Config {
        Config::from_toml(
            "[services.postgres]\n[apps.b]\ndir = \"b\"\n[apps.c]\ndir = \"c\"\n[apps.c.services.redis]\nsource = \"r\"",
        )
        .unwrap()
    }

    fn id(name: &str) -> AppId {
        AppId::Extra(AppName::parse(name).unwrap())
    }

    fn copy(target: &str) -> DatabaseCopy {
        DatabaseCopy {
            service: "postgres".into(),
            source: "pg".into(),
            source_db: "app".into(),
            target: target.into(),
        }
    }

    fn labelled(name: &str, labels: &str, up: bool) -> Container {
        let json = format!(
            r#"[{{"Name": "/{name}", "Config": {{"Image": "redis", "Labels": {labels}}},
            "State": {{"Running": {up}}}, "Mounts": [], "HostConfig": {{}}}}]"#
        );
        serde_json::from_str::<Vec<Container>>(&json)
            .unwrap()
            .remove(0)
    }

    fn volume(name: &str, labels: &str) -> Volume {
        let json = format!(r#"[{{"Name": "{name}", "Labels": {labels}}}]"#);
        serde_json::from_str::<Vec<Volume>>(&json)
            .unwrap()
            .remove(0)
    }

    fn rows_of(
        cfg: &Config,
        state: &State,
        containers: &[Container],
        volumes: &[Volume],
    ) -> (Vec<Row>, Vec<String>) {
        let worktree = Env::new(cfg, "feat-x");
        worktree_rows(cfg, &worktree, "feat/x", containers, volumes, state)
    }

    #[test]
    fn rows_cover_every_app_the_worktree_has() {
        let cfg = cfg();
        let mut state = State::default();
        state.app_mut(&id("c")).app_port = Some(3002);
        // An app the config dropped still has a row while it has anything.
        state.raw_apps_mut().insert(
            "gone".into(),
            AppState {
                app_port: Some(3003),
                ..Default::default()
            },
        );
        state
            .raw_apps_mut()
            .insert("Bad Key".into(), AppState::default());
        let apps = worktree_apps(&cfg, &state, &["zzz", "b", "Bad"]);
        let names: Vec<String> = apps.iter().map(|a| a.to_string()).collect();
        assert_eq!(names, ["default", "b", "c", "gone", "zzz"]);
        assert_eq!(
            worktree_apps(&cfg, &State::default(), &[]),
            [AppId::Default]
        );
    }

    #[test]
    fn rows_follow_the_config_order_not_the_alphabet() {
        let cfg = Config::from_toml("[apps.zeta]\ndir = \"z\"\n[apps.alpha]\ndir = \"a\"").unwrap();
        let mut state = State::default();
        state.app_mut(&id("alpha")).app_port = Some(1);
        state.app_mut(&id("zeta")).app_port = Some(2);
        state.app_mut(&id("beta")).app_port = Some(3);
        let names: Vec<String> = worktree_apps(&cfg, &state, &[])
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(names, ["default", "zeta", "alpha", "beta"]);
    }

    #[test]
    fn each_row_holds_only_its_own_containers_and_volumes() {
        let cfg = cfg();
        let own = r#"{"wt.project": "myapp", "wt.slug": "feat-x"}"#;
        let of_c = r#"{"wt.project": "myapp", "wt.slug": "feat-x", "wt.app": "c"}"#;
        let (rows, notes) = rows_of(
            &cfg,
            &State::default(),
            &[labelled("d", own, true), labelled("c1", of_c, false)],
            &[volume("dv", own), volume("cv", of_c)],
        );
        assert!(notes.is_empty(), "{notes:?}");
        let names = |r: &Row| -> (Vec<String>, Vec<String>) {
            (
                r.containers.iter().map(|c| c.name().to_string()).collect(),
                r.volumes.iter().map(|v| v.name.clone()).collect(),
            )
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            names(&rows[0]),
            (vec!["d".to_string()], vec!["dv".to_string()])
        );
        assert_eq!(rows[1].env.app, id("c"));
        assert_eq!(
            names(&rows[1]),
            (vec!["c1".to_string()], vec!["cv".to_string()])
        );
        assert_eq!(rows[0].branch, "feat/x");
    }

    #[test]
    fn no_services_comes_from_the_config_or_the_state() {
        let cfg = cfg();
        let mut state = State::default();
        state.app_mut(&id("b")).app_port = Some(1);
        state.app_mut(&id("c")).app_port = Some(2);
        state.app_mut(&id("web")).app_port = Some(3);
        state
            .app_mut(&id("api"))
            .ports
            .insert("postgres".into(), 5434);
        let (rows, _) = rows_of(&cfg, &state, &[], &[]);
        let flag = |name: &str| {
            rows.iter()
                .find(|r| r.env.app.to_string() == name)
                .unwrap()
                .no_services
        };
        // The default app has services; b (configured) has none; c has redis.
        assert!(!flag("default"));
        assert!(flag("b"));
        assert!(!flag("c"));
        // Dropped apps: only a web port means no services; a service port does not.
        assert!(flag("web"));
        assert!(!flag("api"));
    }

    #[test]
    fn a_copy_listed_twice_shows_once_where_down_drops_it() {
        let cfg = cfg();
        let mut state = State::default();
        state.default.databases.push(copy("t"));
        state.app_mut(&id("b")).databases.push(copy("t"));
        state.app_mut(&id("b")).databases.push(copy("t-b"));
        let (rows, _) = rows_of(&cfg, &state, &[], &[]);
        let targets = |r: &Row| -> Vec<String> {
            r.state.databases.iter().map(|c| c.target.clone()).collect()
        };
        assert_eq!(targets(&rows[0]), ["t"]);
        assert_eq!(targets(&rows[1]), ["t-b"]);
    }

    #[test]
    fn what_down_would_remove_never_hides() {
        let cfg = cfg();
        let mut state = State::default();
        state.raw_apps_mut().insert(
            "Bad Key".into(),
            AppState {
                databases: vec![copy("x")],
                ..Default::default()
            },
        );
        let odd = r#"{"wt.project": "myapp", "wt.slug": "feat-x", "wt.app": "Not An App"}"#;
        let mut with_own = state.clone();
        with_own.default.databases.push(copy("own"));
        let (rows, notes) = rows_of(
            &cfg,
            &with_own,
            &[labelled("odd", odd, true)],
            &[volume("odd-data", odd)],
        );
        // One note per resource, naming it, and one for the bad key.
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("container odd ") && n.contains("wt down feat-x removes it")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("volume odd-data ")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("Bad Key") && n.contains("refuses the whole worktree")),
            "{notes:?}"
        );
        // The fallback still shows the copies it can place.
        let targets: Vec<&str> = rows[0]
            .state
            .databases
            .iter()
            .map(|c| c.target.as_str())
            .collect();
        assert_eq!(targets, ["own"]);
        // A bad key without copies still holds ports: say so too.
        let mut ports_only = State::default();
        ports_only.raw_apps_mut().insert(
            "Bad Key".into(),
            AppState {
                app_port: Some(3005),
                ..Default::default()
            },
        );
        let (_, notes) = rows_of(&cfg, &ports_only, &[], &[]);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("Bad Key") && n.contains("ports stay taken")),
            "{notes:?}"
        );
    }

    #[test]
    fn the_app_column_shows_only_with_several_apps() {
        assert_eq!(
            header(&[Kind::Postgres], false),
            ["SLUG", "BRANCH", "WEB", "POSTGRES", "STATUS", "DISK"]
        );
        assert_eq!(
            header(&[Kind::Postgres], true),
            ["SLUG", "APP", "BRANCH", "WEB", "POSTGRES", "STATUS", "DISK"]
        );
        let one = Config::from_toml("").unwrap();
        let (rows, _) = rows_of(&one, &State::default(), &[], &[]);
        assert!(!several(&one, &rows));
        // A one-app config whose worktree still has a dropped app.
        let mut state = State::default();
        state.app_mut(&id("gone")).app_port = Some(1);
        let (rows, _) = rows_of(&one, &state, &[], &[]);
        assert!(several(&one, &rows));
        // A multi-app config with only the default row.
        let (rows, _) = rows_of(&cfg(), &State::default(), &[], &[]);
        assert_eq!(rows.len(), 1);
        assert!(several(&cfg(), &rows));
    }

    #[test]
    fn an_extras_service_gets_a_column() {
        let cfg =
            Config::from_toml("[apps.c]\ndir = \"c\"\n[apps.c.services.redis]\nsource = \"r\"")
                .unwrap();
        let (rows, _) = rows_of(&cfg, &State::default(), &[], &[]);
        assert_eq!(kinds(&cfg, &rows), [Kind::Redis]);
    }

    fn row(
        env: Env,
        containers: Vec<Container>,
        volumes: Vec<Volume>,
        state: AppState,
        no_services: bool,
    ) -> Row {
        Row {
            env,
            branch: "-".into(),
            containers,
            volumes,
            state,
            no_services,
        }
    }

    #[test]
    fn an_apps_status_comes_from_its_own_resources() {
        let scratch = Scratch::new("status");
        let cfg = cfg();
        let mut env = Env::new(&cfg, "feat-x").with_app(&id("c"));
        env.path = scratch.0.clone();
        let none = AppState::default();
        let copies = AppState {
            databases: vec![copy("t")],
            ..Default::default()
        };
        let mut sources = Sources::default();
        let mut verdict = |r: Row, docker: bool| status(&r, docker, &mut sources);
        let up = || labelled("x", "{}", true);
        assert_eq!(
            verdict(
                row(env.clone(), vec![up()], vec![], none.clone(), false),
                true
            ),
            "running"
        );
        assert_eq!(
            verdict(
                row(
                    env.clone(),
                    vec![labelled("x", "{}", false)],
                    vec![],
                    none.clone(),
                    false
                ),
                true
            ),
            "stopped"
        );
        assert_eq!(
            verdict(
                row(
                    env.clone(),
                    vec![up(), labelled("y", "{}", false)],
                    vec![],
                    none.clone(),
                    false
                ),
                true
            ),
            "partial"
        );
        // Services configured, nothing of them here: wt up --app c creates them.
        assert_eq!(
            verdict(row(env.clone(), vec![], vec![], none.clone(), false), true),
            "no-db"
        );
        assert_eq!(
            verdict(row(env.clone(), vec![], vec![], none.clone(), true), true),
            "ready"
        );
        // A volume left behind is not "ready", even without services.
        assert_eq!(
            verdict(
                row(
                    env.clone(),
                    vec![],
                    vec![volume("v", "{}")],
                    none.clone(),
                    true
                ),
                true
            ),
            "no-db"
        );
        assert_eq!(
            verdict(
                row(env.clone(), vec![], vec![], copies.clone(), false),
                false
            ),
            "unknown"
        );
        // Without Docker and without copies there is nothing to be unsure about.
        assert_eq!(
            verdict(row(env.clone(), vec![], vec![], none.clone(), true), false),
            "ready"
        );
        let mut gone = env.clone();
        gone.path = scratch.0.join("gone");
        // The filesystem fact wins over "cannot tell".
        assert_eq!(
            verdict(row(gone, vec![], vec![], copies, false), false),
            "orphan"
        );
    }

    #[test]
    fn a_volume_alone_still_has_its_app_and_another_slugs_does_not() {
        let cfg = cfg();
        let gone = r#"{"wt.project": "myapp", "wt.slug": "feat-x", "wt.app": "gone"}"#;
        let other = r#"{"wt.project": "myapp", "wt.slug": "feat-y"}"#;
        let (rows, notes) = rows_of(
            &cfg,
            &State::default(),
            &[],
            &[volume("v", gone), volume("y", other)],
        );
        assert!(notes.is_empty(), "{notes:?}");
        let apps: Vec<String> = rows.iter().map(|r| r.env.app.to_string()).collect();
        assert_eq!(apps, ["default", "gone"]);
        assert!(
            rows[0].volumes.is_empty(),
            "another slug's volume landed here"
        );
        assert_eq!(rows[1].volumes.len(), 1);
        let bad = r#"{"wt.project": "myapp", "wt.slug": "feat-x", "wt.app": "Bad Label"}"#;
        let (_, notes) = rows_of(&cfg, &State::default(), &[], &[volume("b", bad)]);
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn kinds_list_each_service_once_wherever_it_shows() {
        let cfg = cfg();
        let mut state = State::default();
        // postgres again, as a copy of a dropped app.
        state.app_mut(&id("api")).databases.push(copy("t"));
        let mut mysql = copy("m");
        mysql.service = "mysql".into();
        state.app_mut(&id("old")).databases.push(mysql);
        let redis = r#"{"wt.project": "myapp", "wt.slug": "feat-x", "wt.service": "redis", "wt.app": "zed"}"#;
        state
            .app_mut(&id("ports"))
            .ports
            .insert("mysql".into(), 3307);
        let (rows, _) = rows_of(
            &Config::from_toml("[services.postgres]").unwrap(),
            &state,
            &[labelled("z", redis, true)],
            &[],
        );
        let kinds = kinds(&Config::from_toml("[services.postgres]").unwrap(), &rows);
        assert_eq!(kinds, [Kind::Postgres, Kind::Mysql, Kind::Redis]);
        let _ = cfg;
    }

    #[test]
    fn a_dropped_apps_recorded_port_gets_its_column() {
        let one = Config::from_toml("").unwrap();
        let mut state = State::default();
        state
            .app_mut(&id("api"))
            .ports
            .insert("postgres".into(), 5434);
        let (rows, _) = rows_of(&one, &state, &[], &[]);
        assert_eq!(kinds(&one, &rows), [Kind::Postgres]);
    }

    #[test]
    fn a_dropped_apps_services_come_from_its_own_record() {
        let cfg = cfg();
        let mut state = State::default();
        state.default.databases.push(copy("t"));
        // Its only copy is also the default's: shown there, but it still had one.
        state.app_mut(&id("gone")).databases.push(copy("t"));
        let (rows, _) = rows_of(&cfg, &state, &[], &[]);
        let gone = rows.iter().find(|r| r.env.app == id("gone")).unwrap();
        assert!(gone.state.databases.is_empty());
        assert!(!gone.no_services);
    }

    #[test]
    fn docker_matters_only_with_services_or_copies() {
        let none = Config::from_toml("").unwrap();
        let (rows, _) = rows_of(&none, &State::default(), &[], &[]);
        assert!(!docker_matters(&none, &rows));
        assert!(docker_matters(&cfg(), &rows));
        let mut state = State::default();
        state.default.databases.push(copy("t"));
        let (rows, _) = rows_of(&none, &state, &[], &[]);
        assert!(docker_matters(&none, &rows));
        let mut state = State::default();
        state
            .app_mut(&id("gone"))
            .ports
            .insert("redis".into(), 6380);
        let (rows, _) = rows_of(&none, &state, &[], &[]);
        assert!(docker_matters(&none, &rows));
    }
}
