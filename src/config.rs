//! `.wt.toml`: how a repo describes its worktree environment. Everything the
//! clone needs to boot (image, command, env) is read from the source container
//! instead, so it can never drift from the stack it was copied from.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, de};

use crate::services::Kind;

pub const FILE_NAME: &str = ".wt.toml";

/// Where a worktree's data lives: its own container and copied volume, or a
/// database copied inside the source instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Container,
    Database,
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "container" => Ok(Mode::Container),
            "database" => Ok(Mode::Database),
            _ => Err(de::Error::custom(format!(
                "mode must be \"container\" or \"database\", not {raw:?}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    project: Option<String>,
    root: Option<String>,
    #[serde(default)]
    app: RawAppSection,
    #[serde(default)]
    services: Services,
    #[serde(default)]
    hooks: RawHooks,
    #[serde(default)]
    docker: Docker,
    #[serde(default)]
    apps: Apps,
}

/// `[apps.<name>]` tables, in file order (needs toml's `preserve_order`).
#[derive(Debug, Default)]
struct Apps(Vec<(String, RawApp)>);

impl<'de> Deserialize<'de> for Apps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Apps;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a table of apps")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Apps, A::Error> {
                let mut apps = Vec::new();
                while let Some(entry) = map.next_entry::<String, RawApp>()? {
                    apps.push(entry);
                }
                Ok(Apps(apps))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// One extra app. `project`, `root` and `[docker]` describe the worktree, so
/// they are unknown fields here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    dir: Option<String>,
    #[serde(default)]
    app: RawAppSection,
    #[serde(default)]
    services: Services,
    #[serde(default)]
    hooks: RawHooks,
}

/// `[app]` or `[apps.<name>.app]`. Every key is optional: `env_file` defaults
/// to `<dir>/.env` (`.env` for the default app); the others have fixed defaults.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAppSection {
    port_env: Option<String>,
    port_range: Option<(u16, u16)>,
    env_file: Option<String>,
    envrc: Option<bool>,
    #[serde(default)]
    copy: Vec<String>,
    #[serde(default)]
    refresh: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct App {
    /// The env key that carries the web port; empty turns the web port off.
    pub port_env: String,
    pub port_range: (u16, u16),
    /// Relative to the worktree root, normalized like every path below.
    pub env_file: String,
    pub envrc: bool,
    pub copy: Vec<String>,
    /// Like `copy`, but replaced on every run when the main checkout differs.
    pub refresh: Vec<String>,
}

/// One table per supported type; serde rejects any other key, so a typo or an
/// unsupported database fails loudly instead of being ignored.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Services {
    postgres: Option<ServiceFile>,
    mysql: Option<ServiceFile>,
    redis: Option<ServiceFile>,
    sqlite: Option<Sqlite>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ServiceFile {
    source: Option<String>,
    volume: Option<String>,
    clone: Option<bool>,
    port_range: Option<(u16, u16)>,
    mode: Option<Mode>,
    databases: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    database: Option<String>,
    check_sql: Option<String>,
}

/// How the clone's Docker resources are named and labelled, for repos that
/// already have environments from another tool.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Docker {
    /// Names the Postgres clone `<project>-wt-<slug>` and its volume
    /// `<project>-wt-<slug>-db`, without the service in between.
    pub legacy_names: bool,
    /// An extra label, set to the slug on everything `up` creates. `down` and
    /// `ls` also take it as proof of ownership.
    pub slug_label: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Sqlite {
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    dir: Option<String>,
    #[serde(default)]
    setup: Vec<RawHook>,
    #[serde(default)]
    after_services: Vec<RawHook>,
}

/// A hook is an argv, or `{ run = argv, dir = "…" }` to run it somewhere other
/// than `hooks.dir`.
#[derive(Debug)]
enum RawHook {
    Argv(Vec<String>),
    Table(HookTable),
}

// By hand rather than `untagged`, so an error inside the table form (an
// unknown key, no `run`) reaches the user instead of a generic mismatch.
impl<'de> Deserialize<'de> for RawHook {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = RawHook;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an argv list or { run = [...], dir = \"...\" }")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<RawHook, A::Error> {
                Vec::deserialize(de::value::SeqAccessDeserializer::new(seq)).map(RawHook::Argv)
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<RawHook, A::Error> {
                HookTable::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(RawHook::Table)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookTable {
    run: Vec<String>,
    dir: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Hooks {
    pub setup: Vec<Hook>,
    pub after_services: Vec<Hook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub argv: Vec<String>,
    /// Relative to the worktree root and normalized: no `.` or `..`
    /// components; the root is a lone `.`. `hooks::run` also refuses a symlink
    /// out of it.
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub kind: Kind,
    pub source: String,
    pub volume: Option<String>,
    pub clone: bool,
    pub port_range: (u16, u16),
    pub mode: Mode,
    /// Source databases to copy in `mode = "database"`; empty means detect the
    /// one the image's env names.
    pub databases: Vec<String>,
    /// Values may hold `{port}`, and in `mode = "database"` also `{database}`
    /// and `{database:<source>}`.
    pub env: BTreeMap<String, String>,
    pub database: Option<String>,
    /// A query that must succeed on the clone before `up` calls it good.
    pub check_sql: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    pub path: Option<PathBuf>,
    pub project: String,
    pub root: PathBuf,
    pub docker: Docker,
    /// The top-level `[app]`, `[services.*]` and `[hooks]`.
    pub default: AppConfig,
    /// `[apps.<name>]`, in file order.
    pub extras: Vec<AppConfig>,
}

/// Which app of the worktree: the top-level sections, or an `[apps.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppId {
    Default,
    Extra(AppName),
}

impl AppId {
    /// Where this app's keys live in `.wt.toml`, for error messages.
    pub fn prefix(&self) -> String {
        match self {
            AppId::Default => String::new(),
            AppId::Extra(name) => format!("apps.{name}."),
        }
    }

    /// The name in `wt.app` (and, sanitized, in target names): none for the
    /// default app, whose resources predate apps and carry no `wt.app`.
    pub fn label(&self) -> Option<&str> {
        self.name().map(AppName::as_str)
    }

    /// The extra app's name; none for the default app.
    pub fn name(&self) -> Option<&AppName> {
        match self {
            AppId::Default => None,
            AppId::Extra(name) => Some(name),
        }
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AppId::Default => f.write_str("default"),
            AppId::Extra(name) => write!(f, "{name}"),
        }
    }
}

/// An extra app's name, only built by [`AppName::parse`]: lowercase letters
/// and digits with single dashes between them, at most 30 characters, never
/// `default`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppName(String);

impl AppName {
    /// The bare rule, without a key: callers say where the name came from
    /// (`apps.<name>`, `--app`, a state file).
    pub(crate) fn parse(name: &str) -> Result<AppName> {
        if name == "default" {
            bail!("\"default\" is reserved for the top-level app");
        }
        let ok = !name.is_empty()
            && name.len() <= 30
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--");
        if !ok {
            bail!(
                "an app name is lowercase letters and digits, with single dashes between them, at most 30 characters"
            );
        }
        Ok(AppName(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `apps.<name>` as TOML would write the key: quoted only when the name could
/// not be a bare key (A-Z, a-z, 0-9, `-`, `_`).
fn app_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        format!("apps.{name}")
    } else {
        format!("apps.{name:?}")
    }
}

impl std::fmt::Display for AppName {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One app of a monorepo worktree. Every path is relative to the worktree
/// root, normalized, and checked to stay inside it.
#[derive(Debug)]
pub struct AppConfig {
    pub id: AppId,
    /// `.` for the default app.
    pub dir: PathBuf,
    pub app: App,
    pub services: Vec<Service>,
    pub sqlite: Option<Sqlite>,
    pub hooks: Hooks,
}

impl Config {
    pub fn load(main_root: &Path) -> Result<Config> {
        let path = locate(main_root);
        let file = match &path {
            Some(p) => {
                let text = fs::read_to_string(p)
                    .with_context(|| format!("could not read {}", p.display()))?;
                toml::from_str(&text).with_context(|| format!("invalid {}", p.display()))?
            }
            None => File::default(),
        };
        resolve(file, main_root, path)
    }

    /// The default app, then the extras in file order.
    pub fn apps(&self) -> impl Iterator<Item = &AppConfig> {
        std::iter::once(&self.default).chain(&self.extras)
    }

    /// The app `--app <name>` names; `default` is the top-level one.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wt up --app reads it in multi-app milestone 3")
    )]
    pub fn app(&self, name: &str) -> Result<&AppConfig> {
        if name == "default" {
            return Ok(&self.default);
        }
        if let Some(app) = self.extras.iter().find(|a| a.id.label() == Some(name)) {
            return Ok(app);
        }
        let known: Vec<String> = self.apps().map(|a| a.id.to_string()).collect();
        bail!("no app named {name:?}; the apps are: {}", known.join(", "))
    }

    /// Every app's services: what `down` must never remove as a clone, even
    /// for an app `up` has not brought up.
    pub fn services(&self) -> impl Iterator<Item = &Service> {
        self.apps().flat_map(|a| &a.services)
    }

    /// A config from TOML text, resolved as if the main checkout were
    /// `/work/My.App` (project `myapp`).
    #[cfg(test)]
    pub fn from_toml(text: &str) -> Result<Config> {
        let file: File = toml::from_str(text)?;
        resolve(file, Path::new("/work/My.App"), None)
    }

    #[cfg(test)]
    pub fn service(&self, kind: Kind) -> Option<&Service> {
        self.default.services.iter().find(|s| s.kind == kind)
    }
}

/// `WT_CONFIG`, then the repo's own file, then one kept outside the repo for
/// projects where the file cannot be committed.
fn locate(main_root: &Path) -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("WT_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let in_repo = main_root.join(FILE_NAME);
    if in_repo.is_file() {
        return Some(in_repo);
    }
    let outside = config_home()
        .join("wt")
        .join(format!("{}.toml", default_project(main_root)));
    outside.is_file().then_some(outside)
}

fn resolve(file: File, main_root: &Path, path: Option<PathBuf>) -> Result<Config> {
    let project = match file.project {
        Some(p) => {
            if compose_name(&p) != p || p.is_empty() {
                bail!("project '{p}' must be lowercase letters, digits, '-' or '_'");
            }
            p
        }
        None => default_project(main_root),
    };
    if project.is_empty() {
        bail!(
            "{} does not reduce to a project name; set `project` in {FILE_NAME}",
            main_root.display()
        );
    }

    let root = match file.root {
        Some(r) => expand_tilde(&r),
        None => home().join(".herdr/worktrees").join(&project),
    };

    if file.docker.slug_label.as_deref() == Some("") {
        bail!("docker.slug_label must not be empty");
    }

    let default = resolve_app(
        AppId::Default,
        PathBuf::from("."),
        file.app,
        file.services,
        file.hooks,
        &project,
    )?;
    let mut extras = Vec::new();
    for (name, raw) in file.apps.0 {
        extras.push(resolve_extra(&name, raw, &project)?);
    }
    check_across_apps(&default, &extras)?;

    Ok(Config {
        path,
        project,
        root,
        docker: file.docker,
        default,
        extras,
    })
}

/// One app's sections, with every default taken from `dir` and every error
/// keyed under the app's prefix (`apps.<name>.` for an extra).
fn resolve_app(
    id: AppId,
    dir: PathBuf,
    section: RawAppSection,
    services_file: Services,
    hooks: RawHooks,
    project: &str,
) -> Result<AppConfig> {
    let prefix = id.prefix();
    let prefix = prefix.as_str();
    let path = |field: &str, value: &str| -> Result<String> {
        Ok(inside_worktree(&format!("{prefix}app.{field}"), value)?
            .display()
            .to_string())
    };
    let paths = |field: &str, values: Vec<String>| -> Result<Vec<String>> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| path(&format!("{field}[{i}]"), v))
            .collect()
    };
    let app = App {
        port_env: section.port_env.unwrap_or_else(|| "PORT".into()),
        port_range: section.port_range.unwrap_or((3001, 3099)),
        env_file: match section.env_file {
            Some(file) => path("env_file", &file)?,
            None => under(&dir, ".env"),
        },
        envrc: section.envrc.unwrap_or(true),
        copy: paths("copy", section.copy)?,
        refresh: paths("refresh", section.refresh)?,
    };
    check_range(&format!("{prefix}app.port_range"), app.port_range)?;
    // Nested counts too: refresh would overwrite inside a tree copy protects.
    if let Some(path) = app
        .refresh
        .iter()
        .find(|r| app.copy.iter().any(|c| overlaps(c, r)))
    {
        bail!("{prefix}app.copy and {prefix}app.refresh both cover {path}; pick one");
    }
    let hooks = resolve_hooks(hooks, prefix, dir.clone())?;

    let mut services = Vec::new();
    for (kind, entry) in [
        (Kind::Postgres, services_file.postgres),
        (Kind::Mysql, services_file.mysql),
        (Kind::Redis, services_file.redis),
    ] {
        let Some(entry) = entry else { continue };
        let kind_name = kind.name();
        let svc = format!("{prefix}services.{kind_name}");
        if matches!(id, AppId::Extra(_)) && entry.source.is_none() {
            bail!(
                "{svc}.source is required: the compose default ({project}-{kind_name}-1) is the default app's container"
            );
        }
        let mode = entry.mode.unwrap_or_default();

        if kind == Kind::Redis && mode == Mode::Database {
            bail!(
                "{svc}.mode = \"database\" is not supported: Redis has no separate databases to copy (use mode = \"container\")"
            );
        }
        if mode == Mode::Database {
            if let Some(key) = &entry.volume {
                bail!("{svc}.volume = {key:?} is not used in mode = \"database\"");
            }
            if entry.port_range.is_some() {
                bail!("{svc}.port_range is not used in mode = \"database\"");
            }
        }
        if kind != Kind::Postgres && entry.database.is_some() && mode != Mode::Database {
            bail!("{svc}.database is only supported for postgres");
        }
        if entry.check_sql.is_some() && kind != Kind::Postgres {
            bail!("{svc}.check_sql is only supported for postgres");
        }

        let listed = entry.databases.is_some();
        let databases = match (entry.databases, entry.database.as_ref()) {
            (Some(_), Some(_)) => {
                bail!("{svc}: set either database or databases, not both")
            }
            (Some(list), None) => list,
            (None, Some(db)) => vec![db.clone()],
            (None, None) => Vec::new(),
        };
        if listed && mode != Mode::Database {
            bail!("{svc}.databases requires mode = \"database\"");
        }
        for db in &databases {
            if db.trim().is_empty() {
                bail!("{svc}.databases must not contain an empty database name");
            }
            if db.contains('}') {
                bail!("{svc}.databases: database name {db:?} must not contain \"}}\"");
            }
            if db.contains('{') {
                bail!("{svc}.databases: database name {db:?} must not contain \"{{\"");
            }
        }
        if listed {
            if databases.is_empty() {
                bail!("{svc}.databases must list at least one database");
            }
            for db in &databases {
                if databases.iter().filter(|d| *d == db).count() > 1 {
                    bail!("{svc}.databases must not contain {db:?} twice");
                }
            }
        }
        if entry.check_sql.is_some() && mode == Mode::Container && entry.database.is_none() {
            bail!("{svc}.check_sql needs {svc}.database");
        }
        // In container mode `database` is only the check target: the volume
        // copy already carries every logical database.
        let mut databases = databases;
        if mode != Mode::Database {
            databases.clear();
        }

        let env = entry.env.unwrap_or_else(|| kind.default_env());
        check_env(&svc, mode, &databases, &env)?;

        let port_range = entry.port_range.unwrap_or(kind.default_port_range());
        check_range(&format!("{svc}.port_range"), port_range)?;
        services.push(Service {
            kind,
            source: entry
                .source
                .unwrap_or_else(|| format!("{project}-{kind_name}-1")),
            volume: entry.volume,
            clone: entry.clone.unwrap_or(true),
            port_range,
            mode,
            databases,
            env,
            database: entry.database,
            check_sql: entry.check_sql,
        });
    }

    let sqlite = match services_file.sqlite {
        Some(sqlite) => {
            if sqlite.files.is_empty() {
                bail!("{prefix}services.sqlite.files must list at least one file");
            }
            let files = sqlite
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    inside_worktree(&format!("{prefix}services.sqlite.files[{i}]"), f)
                        .map(|p| p.display().to_string())
                })
                .collect::<Result<_>>()?;
            Some(Sqlite { files })
        }
        None => None,
    };

    let resolved = AppConfig {
        id,
        dir,
        app,
        services,
        sqlite,
        hooks,
    };
    // `up` writes these before it refreshes, so a refresh over them would put
    // the main checkout's copy back on every run. `copy` never overwrites.
    for path in &resolved.app.refresh {
        if let Some((owner, file)) = own_files(&resolved)
            .into_iter()
            .find(|(_, file)| overlaps(path, file))
        {
            bail!(
                "{prefix}app.refresh covers {path}, which holds {owner} ({file}); refresh would replace it with the main checkout's copy on every run"
            );
        }
    }
    Ok(resolved)
}

/// An `[apps.<name>]` table: its `dir` is required and supplies the defaults
/// the default app takes from the root.
fn resolve_extra(name: &str, raw: RawApp, project: &str) -> Result<AppConfig> {
    let id = AppId::Extra(AppName::parse(name).with_context(|| app_key(name))?);
    let Some(dir) = raw.dir else {
        bail!("apps.{name}.dir is required: the app's folder in the worktree");
    };
    let dir = inside_worktree(&format!("apps.{name}.dir"), &dir)?;
    resolve_app(id, dir, raw.app, raw.services, raw.hooks, project)
}

/// `dir/name`, or `name` alone for the root, so the default app's `.env` and
/// an extra's in "." compare equal.
fn under(dir: &Path, name: &str) -> String {
    if dir == Path::new(".") {
        name.to_string()
    } else {
        dir.join(name).display().to_string()
    }
}

/// Either normalized path contains the other; `.` is the whole worktree.
fn overlaps(a: &str, b: &str) -> bool {
    a == "." || b == "." || Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
}

/// Where the app writes its `.envrc`, if it writes one: only with a web port.
fn envrc_path(app: &AppConfig) -> Option<String> {
    (app.app.envrc && !app.app.port_env.is_empty()).then(|| under(&app.dir, ".envrc"))
}

/// The files `up` writes or copies for an app, which another app's `copy` or
/// `refresh` must not cover.
fn own_files(app: &AppConfig) -> Vec<(String, String)> {
    let prefix = app.id.prefix();
    let mut files = vec![(format!("{prefix}app.env_file"), app.app.env_file.clone())];
    if let Some(envrc) = envrc_path(app) {
        files.push((format!("{prefix}app.envrc"), envrc));
    }
    // `files::copy_sqlite` copies the WAL companions with each database.
    for file in app.sqlite.iter().flat_map(|s| &s.files) {
        for path in [file.clone(), format!("{file}-wal"), format!("{file}-shm")] {
            files.push((format!("{prefix}services.sqlite.files"), path));
        }
    }
    files
}

/// `copy` and `refresh`, each keyed for error messages.
fn copied(app: &AppConfig) -> Vec<(String, String)> {
    let prefix = app.id.prefix();
    let copy = app
        .app
        .copy
        .iter()
        .map(|p| (format!("{prefix}app.copy"), p.clone()));
    let refresh = app
        .app
        .refresh
        .iter()
        .map(|p| (format!("{prefix}app.refresh"), p.clone()));
    copy.chain(refresh).collect()
}

/// What two apps of one worktree must not share: a file they both write, a
/// path they both copy, a file one writes and the other copies over, or a
/// source they use in different modes or copy the same database from.
fn check_across_apps(default: &AppConfig, extras: &[AppConfig]) -> Result<()> {
    let apps: Vec<&AppConfig> = std::iter::once(default).chain(extras).collect();
    // One path, one writer: env files, .envrc files and SQLite copies, within
    // an app and across apps.
    let written: Vec<(String, String)> = apps.iter().flat_map(|a| own_files(a)).collect();
    for (i, (key_a, file)) in written.iter().enumerate() {
        if let Some((key_b, _)) = written[i + 1..]
            .iter()
            .find(|(key_b, other)| overlaps(other, file) && key_b != key_a)
        {
            bail!("{key_a} and {key_b} both write {file}; give each its own path");
        }
    }
    for (i, a) in apps.iter().enumerate() {
        for b in &apps[i + 1..] {
            for (key_a, pa) in copied(a) {
                if let Some((key_b, pb)) = copied(b).into_iter().find(|(_, pb)| overlaps(&pa, pb)) {
                    bail!("{key_a} covers {pa} and {key_b} covers {pb}; pick one app");
                }
            }
            for (x, y) in [(a, b), (b, a)] {
                for (key, path) in copied(x) {
                    if let Some((owner, file)) = own_files(y)
                        .into_iter()
                        .find(|(_, file)| overlaps(&path, file))
                    {
                        bail!(
                            "{key} covers {path}, which holds {owner} ({file}); `up` would write over another app's file"
                        );
                    }
                }
            }
            check_shared_sources(a, b)?;
        }
    }
    Ok(())
}

/// Two apps on one source: a clone stops it while copies live inside it, so
/// both must use one mode; and a worktree copies each source database once.
fn check_shared_sources(a: &AppConfig, b: &AppConfig) -> Result<()> {
    let (ka, kb) = (a.id.prefix(), b.id.prefix());
    for sa in &a.services {
        for sb in b.services.iter().filter(|sb| sb.source == sa.source) {
            let (na, nb, source) = (sa.kind.name(), sb.kind.name(), &sa.source);
            if sa.kind != sb.kind {
                bail!(
                    "{ka}services.{na} and {kb}services.{nb} both name {source} as their source; one container cannot be a {na} and a {nb}"
                );
            }
            if sa.mode != sb.mode {
                bail!(
                    "{ka}services.{na} and {kb}services.{nb} both use {source} but in different modes; a clone (mode = \"container\") stops the source the copies (mode = \"database\") live in, so pick one mode"
                );
            }
            if sa.mode != Mode::Database {
                continue;
            }
            if sa.databases.is_empty() || sb.databases.is_empty() {
                bail!(
                    "{ka}services.{na} and {kb}services.{nb} both copy from {source}; name their databases (database or databases) in both, so wt can check they do not copy the same one"
                );
            }
            for (key, svc) in [
                (format!("{ka}services.{na}"), sa),
                (format!("{kb}services.{nb}"), sb),
            ] {
                if let Some(db) = svc
                    .databases
                    .iter()
                    .find(|db| crate::database::sanitize(db).is_empty())
                {
                    bail!(
                        "{key}.databases: database name {db:?} has no characters usable in a target"
                    );
                }
            }
            // An extra app's targets carry its name, so the copies would not
            // clash; one copy per source database and worktree is policy.
            // With lower_case_table_names set, MySQL stores names lowercased
            // (1, the Windows default) or compares them without case (2, the
            // macOS default), so `App` is `app` there. Only MySQL folds case
            // here; Postgres names are case-sensitive.
            let same = |x: &String, y: &String| match sa.kind {
                Kind::Mysql => x.eq_ignore_ascii_case(y),
                _ => x == y,
            };
            if let Some((da, db)) = sa.databases.iter().find_map(|da| {
                sb.databases
                    .iter()
                    .find(|db| same(da, db))
                    .map(|db| (da, db))
            }) {
                let what = if da == db {
                    format!("{da:?}")
                } else {
                    format!("{da:?} (as {db:?})")
                };
                bail!(
                    "{ka}services.{na} and {kb}services.{nb} would both copy {what} from {source}; a worktree gets one copy of each source database"
                );
            }
        }
    }
    Ok(())
}

fn resolve_hooks(raw: RawHooks, prefix: &str, app_dir: PathBuf) -> Result<Hooks> {
    let default = match &raw.dir {
        Some(dir) => inside_worktree(&format!("{prefix}hooks.dir"), dir)?,
        None => app_dir,
    };
    let phase = |name: &str, hooks: Vec<RawHook>| -> Result<Vec<Hook>> {
        hooks
            .into_iter()
            .enumerate()
            .map(|(i, hook)| {
                let (argv, dir) = match hook {
                    RawHook::Argv(argv) => (argv, default.clone()),
                    RawHook::Table(t) => {
                        let dir = match &t.dir {
                            Some(d) => {
                                inside_worktree(&format!("{prefix}hooks.{name}[{i}].dir"), d)?
                            }
                            None => default.clone(),
                        };
                        (t.run, dir)
                    }
                };
                if argv.is_empty() {
                    bail!("{prefix}hooks.{name}[{i}] is an empty command");
                }
                Ok(Hook { argv, dir })
            })
            .collect()
    };
    Ok(Hooks {
        setup: phase("setup", raw.setup)?,
        after_services: phase("after_services", raw.after_services)?,
    })
}

/// A relative path whose `..` never climbs above the worktree root, returned
/// normalized. The check is lexical: `..` is resolved here, before symlinks,
/// so the OS never sees it; `hooks::run` then resolves symlinks.
fn inside_worktree(key: &str, dir: &str) -> Result<PathBuf> {
    use std::path::Component;
    if dir.is_empty() {
        if key.ends_with("dir") {
            bail!("{key} must not be empty; use \".\" for the worktree root");
        }
        bail!("{key} must not be empty");
    }
    let mut path = PathBuf::new();
    for part in Path::new(dir).components() {
        match part {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !path.pop() {
                    bail!("{key} = {dir:?} must stay inside the worktree");
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("{key} = {dir:?} must be relative to the worktree root")
            }
        }
    }
    if path.as_os_str().is_empty() {
        path.push(".");
    }
    Ok(path)
}

fn check_range(name: &str, (first, last): (u16, u16)) -> Result<()> {
    if first == 0 || first > last {
        bail!("{name} must be [first, last] with 0 < first <= last");
    }
    Ok(())
}

/// Every `{...}` a value uses must be a placeholder the mode understands, and
/// a database-mode service must point the app at a copy or it would write into
/// the source.
fn check_env(
    svc: &str,
    mode: Mode,
    databases: &[String],
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let mut references_a_database = false;
    for value in env.values() {
        for token in placeholders(value) {
            match token.as_str() {
                "port" => {}
                "database" => {
                    if mode != Mode::Database {
                        bail!("{svc}.env: {{database}} is only available in mode = \"database\"");
                    }
                    references_a_database = true;
                }
                _ => {
                    if let Some(db) = token.strip_prefix("database:") {
                        if mode != Mode::Database {
                            bail!(
                                "{svc}.env: {{{token}}} is only available in mode = \"database\""
                            );
                        }
                        if db.is_empty() {
                            bail!("{svc}.env: placeholder {{{token}}} names no database");
                        }
                        if databases.is_empty() {
                            bail!("{svc}.env references database {db:?} but databases is not set");
                        }
                        if !databases.iter().any(|d| d == db) {
                            bail!("{svc}.env references database {db:?} which is not in databases");
                        }
                        references_a_database = true;
                    } else if token.starts_with("database") {
                        bail!("{svc}.env has unknown placeholder {{{token}}}");
                    }
                    // Other braces are literal (Helm templates, JSON) and are
                    // left untouched, as they were before database mode.
                }
            }
        }
    }
    if mode == Mode::Database && !references_a_database {
        bail!(
            "{svc}.env must reference a database ({{database}} or {{database:<source>}}) so the worktree does not write into the source"
        );
    }
    Ok(())
}

/// The contents of every `{...}` in a value, in order.
fn placeholders(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    map_placeholders(value, |token| {
        out.push(token.to_string());
        None
    });
    out
}

/// Rewrites every `{...}` the callback answers for and leaves the rest as it
/// was. The one tokenizer for values, so what validation reads as literal text
/// is exactly what rendering leaves alone. An unterminated `{` is literal.
pub fn map_placeholders(value: &str, mut replace: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        out.push_str(&rest[..start]);
        match replace(&after[..end]) {
            Some(text) => out.push_str(&text),
            None => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The name docker compose gives the project, so `{project}-postgres-1` finds
/// the container compose created.
pub fn default_project(main_root: &Path) -> String {
    compose_name(
        &main_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

fn compose_name(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

pub fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

pub fn state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Result<Config> {
        Config::from_toml(text)
    }

    #[test]
    fn empty_file_gets_defaults() {
        let cfg = load("").unwrap();
        assert_eq!(cfg.project, "myapp");
        assert!(cfg.root.ends_with(".herdr/worktrees/myapp"));
        assert_eq!(cfg.default.app.port_env, "PORT");
        assert_eq!(cfg.default.app.port_range, (3001, 3099));
        assert!(cfg.default.services.is_empty());
    }

    #[test]
    fn empty_service_table_resolves_compose_defaults() {
        let cfg = load("[services.postgres]").unwrap();
        let pg = cfg.service(Kind::Postgres).unwrap();
        assert_eq!(pg.source, "myapp-postgres-1");
        assert!(pg.clone);
        assert_eq!(pg.mode, Mode::Container);
        assert!(pg.databases.is_empty());
        assert!(pg.database.is_none());
        assert_eq!(pg.port_range, (5433, 5499));
        assert_eq!(pg.env["DB_PORT"], "{port}");
        assert_eq!(pg.env["DB_HOST"], "127.0.0.1");
    }

    #[test]
    fn mode_is_container_or_database() {
        assert_eq!(
            load("[services.postgres]")
                .unwrap()
                .service(Kind::Postgres)
                .unwrap()
                .mode,
            Mode::Container
        );
        assert_eq!(
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { U = \"x/{database}\" }")
                .unwrap()
                .service(Kind::Postgres)
                .unwrap()
                .mode,
            Mode::Database
        );
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"dbs\"").unwrap_err()
        );
        assert!(
            err.contains("must be \"container\" or \"database\""),
            "{err}"
        );
    }

    #[test]
    fn databases_and_the_database_shortcut() {
        let cfg = load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\",\"b\"]\nenv = { U = \"x/{database}\" }").unwrap();
        assert_eq!(
            cfg.service(Kind::Postgres).unwrap().databases,
            vec!["a".to_string(), "b".to_string()]
        );
        let cfg = load("[services.postgres]\nmode = \"database\"\ndatabase = \"a\"\nenv = { U = \"x/{database}\" }").unwrap();
        assert_eq!(cfg.service(Kind::Postgres).unwrap().databases, vec!["a"]);
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabase = \"a\"\ndatabases = [\"b\"]\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("either database or databases"), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = []\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("must list at least one"), "{err}");
    }

    #[test]
    fn rejects_duplicate_databases() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\",\"a\"]\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("must not contain \"a\" twice"), "{err}");
    }

    #[test]
    fn rejects_braces_in_database_names() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a}b\"]\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("must not contain \"}\""), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a{b\"]\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("must not contain \"{\""), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabase = \"a}b\"\nenv = { U = \"x/{database}\" }")
                .unwrap_err()
        );
        assert!(err.contains("must not contain \"}\""), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { U = \"x/{database:}\" }")
                .unwrap_err()
        );
        assert!(err.contains("names no database"), "{err}");
    }

    #[test]
    fn databases_are_database_mode_only_and_mysql_takes_database() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\ndatabases = [\"a\"]").unwrap_err()
        );
        assert!(err.contains("requires mode = \"database\""), "{err}");
        assert!(load("[services.mysql]\ndatabase = \"a\"").is_err());
        let cfg = load("[services.mysql]\nmode = \"database\"\ndatabase = \"a\"\nenv = { U = \"x/{database}\" }").unwrap();
        assert_eq!(cfg.service(Kind::Mysql).unwrap().databases, vec!["a"]);
        let cfg = load(
            "[services.postgres]\nmode = \"database\"\ndatabase = \"a\"\ncheck_sql = \"select 1\"\nenv = { U = \"x/{database}\" }",
        )
        .unwrap();
        assert!(cfg.service(Kind::Postgres).unwrap().check_sql.is_some());
    }

    #[test]
    fn redis_is_container_only() {
        let err = format!(
            "{:#}",
            load("[services.redis]\nmode = \"database\"").unwrap_err()
        );
        assert!(err.contains("Redis has no separate databases"), "{err}");
        assert!(err.contains("mode = \"container\""), "{err}");
    }

    #[test]
    fn database_mode_takes_no_volume_or_range() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\nvolume = \"x\"").unwrap_err()
        );
        assert!(err.contains("is not used in mode = \"database\""), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\nport_range = [5433, 5499]")
                .unwrap_err()
        );
        assert!(err.contains("is not used in mode = \"database\""), "{err}");
    }

    #[test]
    fn database_mode_env_names_a_database() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { DB_HOST = \"127.0.0.1\" }")
                .unwrap_err()
        );
        assert!(err.contains("must reference a database"), "{err}");
        assert!(
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { URL = \"x/{database}\" }")
                .is_ok()
        );
    }

    #[test]
    fn unknown_database_placeholders_are_errors() {
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { U = \"x/{database:nope}\" }")
                .unwrap_err()
        );
        assert!(
            err.contains("nope") && err.contains("is not in databases"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            load("[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { U = \"x/{databases}\" }")
                .unwrap_err()
        );
        assert!(err.contains("unknown placeholder"), "{err}");
        let err = format!(
            "{:#}",
            load("[services.postgres]\nenv = { U = \"x/{database}\" }").unwrap_err()
        );
        assert!(
            err.contains("only available in mode = \"database\""),
            "{err}"
        );
    }

    #[test]
    fn overrides_win() {
        let cfg = load(
            r#"
            project = "plat"
            root = "/tmp/wts"
            [services.redis]
            source = "cache-1"
            clone = false
            env = { REDIS_URL = "redis://127.0.0.1:{port}/1" }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.root, PathBuf::from("/tmp/wts"));
        let redis = cfg.service(Kind::Redis).unwrap();
        assert_eq!(redis.source, "cache-1");
        assert!(!redis.clone);
        assert_eq!(redis.env.len(), 1);
    }

    #[test]
    fn rejects_unknown_service_key() {
        let err = format!("{:#}", load("[services.cache]").unwrap_err());
        assert!(err.contains("cache"), "{err}");
    }

    #[test]
    fn rejects_type_field() {
        assert!(load("[services.postgres]\ntype = \"postgres\"").is_err());
    }

    #[test]
    fn database_in_mysql_needs_database_mode() {
        assert!(load("[services.mysql]\ndatabase = \"x\"").is_err());
        assert!(
            load("[services.mysql]\nmode = \"database\"\ndatabase = \"x\"\nenv = { U = \"x/{database}\" }")
                .is_ok()
        );
        assert!(load("[services.postgres]\ndatabase = \"x\"").is_ok());
    }

    #[test]
    fn hooks_are_argv_lists() {
        let cfg = load(
            r#"
            [hooks]
            setup = [["bundle", "install"], ["bun", "install"]]
            after_services = [["bin/rails", "db:prepare"]]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.default.hooks.setup[1].argv, vec!["bun", "install"]);
        assert_eq!(cfg.default.hooks.setup[1].dir, PathBuf::from("."));
        assert_eq!(cfg.default.hooks.after_services.len(), 1);
    }

    #[test]
    fn rejects_empty_or_string_hooks() {
        assert!(load("[hooks]\nsetup = [[]]").is_err());
        assert!(load("[hooks]\nsetup = [\"bun install\"]").is_err());
        let err = |text: &str| format!("{:#}", load(text).unwrap_err());
        let e = err("[hooks]\nsetup = [{ run = [] }]");
        assert!(e.contains("hooks.setup[0] is an empty command"), "{e}");
        let e = err("[hooks]\nsetup = [{ dir = \"a\" }]");
        assert!(e.contains("missing field `run`"), "{e}");
        let e = err("[hooks]\nsetup = [{ run = [\"x\"], cwd = \"a\" }]");
        assert!(e.contains("unknown field `cwd`"), "{e}");
        let e = err("[hooks]\nsetup = [\"bun install\"]");
        assert!(e.contains("{ run = [...], dir = \"...\" }"), "{e}");
    }

    #[test]
    fn hooks_dir_applies_to_every_hook_unless_overridden() {
        let cfg = load(
            r#"
            [hooks]
            dir = "apps/platform"
            setup = [["bundle", "install"], { run = ["bun", "install"], dir = "." }]
            after_services = [["bin/rails", "db:migrate"]]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.default.hooks.setup[0].argv, vec!["bundle", "install"]);
        assert_eq!(
            cfg.default.hooks.setup[0].dir,
            PathBuf::from("apps/platform")
        );
        assert_eq!(cfg.default.hooks.setup[1].argv, vec!["bun", "install"]);
        assert_eq!(cfg.default.hooks.setup[1].dir, PathBuf::from("."));
        assert_eq!(
            cfg.default.hooks.after_services[0].dir,
            PathBuf::from("apps/platform")
        );
        let cfg = load("[hooks]\ndir = \"apps/platform\"\nsetup = [{ run = [\"x\"] }]").unwrap();
        assert_eq!(
            cfg.default.hooks.setup[0].dir,
            PathBuf::from("apps/platform")
        );
    }

    #[test]
    fn hooks_load_as_an_array_of_tables() {
        let cfg = load(
            "[hooks]\ndir = \"apps/web\"\n[[hooks.setup]]\nrun = [\"bun\", \"install\"]\ndir = \".\"\n[[hooks.setup]]\nrun = [\"bundle\"]",
        )
        .unwrap();
        assert_eq!(cfg.default.hooks.setup[0].argv, vec!["bun", "install"]);
        assert_eq!(cfg.default.hooks.setup[0].dir, PathBuf::from("."));
        assert_eq!(cfg.default.hooks.setup[1].dir, PathBuf::from("apps/web"));
    }

    #[test]
    fn hooks_dir_is_stored_normalized() {
        for (raw, want) in [
            ("a/./b", "a/b"),
            ("./a/", "a"),
            ("a/..", "."),
            ("a/../b", "b"),
            (".", "."),
        ] {
            let cfg = load(&format!("[hooks]\ndir = {raw:?}\nsetup = [[\"x\"]]")).unwrap();
            assert_eq!(cfg.default.hooks.setup[0].dir, PathBuf::from(want), "{raw}");
        }
    }

    #[test]
    fn hook_dir_alone_overrides_the_root() {
        let cfg = load("[hooks]\nsetup = [{ run = [\"bundle\"], dir = \"apps/web\" }]").unwrap();
        assert_eq!(cfg.default.hooks.setup[0].dir, PathBuf::from("apps/web"));
    }

    #[test]
    fn hooks_dir_stays_inside_the_worktree() {
        for (bad, why) in [
            ("/abs", "must be relative"),
            ("..", "must stay inside"),
            ("../x", "must stay inside"),
            ("a/../../x", "must stay inside"),
            ("", "must not be empty"),
        ] {
            let err = format!(
                "{:#}",
                load(&format!("[hooks]\ndir = {bad:?}")).unwrap_err()
            );
            assert!(
                err.contains("hooks.dir") && err.contains(why),
                "{bad}: {err}"
            );
            let err = format!(
                "{:#}",
                load(&format!(
                    "[hooks]\nafter_services = [{{ run = [\"x\"], dir = {bad:?} }}]"
                ))
                .unwrap_err()
            );
            assert!(
                err.contains("hooks.after_services[0].dir") && err.contains(why),
                "{bad}: {err}"
            );
        }
        assert!(load("[hooks]\ndir = \"a/./b\"").is_ok());
    }

    #[test]
    fn sqlite_needs_files() {
        assert!(load("[services.sqlite]\nfiles = []").is_err());
        assert!(load("[services.sqlite]").is_err());
        let cfg = load("[services.sqlite]\nfiles = [\"storage/dev.sqlite3\"]").unwrap();
        assert_eq!(cfg.default.sqlite.unwrap().files.len(), 1);
    }

    #[test]
    fn other_braces_are_literal() {
        let cfg = load("[services.postgres]\nenv = { U = \"{{.Values.url}}\" }").unwrap();
        assert_eq!(
            cfg.service(Kind::Postgres).unwrap().env["U"],
            "{{.Values.url}}"
        );
    }

    #[test]
    fn rails_postgres_example_loads() {
        let cfg = load(include_str!("../examples/rails-postgres.wt.toml")).unwrap();
        assert_eq!(
            cfg.service(Kind::Postgres).unwrap().source,
            "myapp-postgres-1"
        );
        assert_eq!(cfg.default.app.copy, vec!["storage"]);
        assert_eq!(
            cfg.default.hooks.after_services[0].argv,
            vec!["bin/rails", "db:prepare"]
        );
    }

    #[test]
    fn rails_database_example_loads() {
        let cfg = load(include_str!("../examples/rails-postgres-database.wt.toml")).unwrap();
        let pg = cfg.service(Kind::Postgres).unwrap();
        assert_eq!(pg.mode, Mode::Database);
        assert_eq!(
            pg.databases,
            vec!["myapp_development".to_string(), "myapp_cache".to_string()]
        );
        assert_eq!(cfg.default.app.copy, vec!["storage"]);
    }

    #[test]
    fn refresh_must_not_overlap_copy() {
        let cfg = load("[app]\ncopy = [\"storage\"]\nrefresh = [\".claude/agents\"]").unwrap();
        assert_eq!(cfg.default.app.refresh, vec![".claude/agents"]);
        assert!(load("[app]\ncopy = [\"a\"]\nrefresh = [\"a\"]").is_err());
        assert!(load("[app]\ncopy = [\"a\"]\nrefresh = [\"a/b\"]").is_err());
        assert!(load("[app]\ncopy = [\"ab\"]\nrefresh = [\"a\"]").is_ok());
    }

    #[test]
    fn check_sql_is_postgres_only_and_needs_a_database() {
        let ok = "[services.postgres]\ndatabase = \"d\"\ncheck_sql = \"select 1\"";
        assert_eq!(
            load(ok).unwrap().service(Kind::Postgres).unwrap().check_sql,
            Some("select 1".into())
        );
        // `databases[0]` is the primary, so the check needs no `database` key.
        let ok = "[services.postgres]\nmode = \"database\"\ndatabases = [\"a\"]\ncheck_sql = \"select 1\"\nenv = { U = \"x/{database}\" }";
        assert_eq!(
            load(ok).unwrap().service(Kind::Postgres).unwrap().check_sql,
            Some("select 1".into())
        );
        assert!(load("[services.postgres]\ncheck_sql = \"select 1\"").is_err());
        assert!(load("[services.mysql]\ncheck_sql = \"select 1\"").is_err());
    }

    #[test]
    fn docker_table_defaults_and_rejects_unknown_keys() {
        let cfg = load("").unwrap();
        assert!(!cfg.docker.legacy_names);
        assert!(cfg.docker.slug_label.is_none());
        let cfg = load("[docker]\nlegacy_names = true\nslug_label = \"x.slug\"").unwrap();
        assert!(cfg.docker.legacy_names);
        assert_eq!(cfg.docker.slug_label.as_deref(), Some("x.slug"));
        assert!(load("[docker]\nnaming = \"legacy\"").is_err());
        assert!(load("[docker]\nslug_label = \"\"").is_err());
    }

    #[test]
    fn rejects_bad_ranges_and_projects() {
        assert!(load("[app]\nport_range = [3099, 3001]").is_err());
        assert!(load("project = \"My App\"").is_err());
    }

    // ---- Several apps (`[apps.<name>]`) ----

    fn err(text: &str) -> String {
        format!("{:#}", load(text).unwrap_err())
    }

    fn hook(argv: &[&str], dir: &str) -> Hook {
        Hook {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            dir: PathBuf::from(dir),
        }
    }

    /// platform's `.wt.toml` as of wt 0.4.0: it must keep loading as one app.
    const PLATFORM_0_4: &str = r#"
[app]
env_file = "apps/platform/.env"
copy = ["apps/platform/storage"]
refresh = ["apps/platform/.bundle", ".claude/agents", ".claude/skills"]

[services.postgres]
database = "platform_development"
check_sql = "select count(*) from schema_migrations"

[docker]
legacy_names = true
slug_label = "platform.worktree.slug"

[hooks]
dir = "apps/platform"
setup = [
  ["mise", "exec", "--", "bundle", "install"],
  { run = ["mise", "exec", "--", "bun", "install"], dir = "." },
]
after_services = [["mise", "exec", "--", "bin/rails", "db:migrate"]]
"#;

    #[test]
    fn a_config_without_apps_is_one_app() {
        let cfg = load("").unwrap();
        assert!(cfg.extras.is_empty());
        assert_eq!(cfg.default.id, AppId::Default);
        assert_eq!(cfg.default.dir, PathBuf::from("."));
        assert_eq!(cfg.apps().count(), 1);

        // platform as of 0.4.0, resolved field by field.
        let cfg = load(PLATFORM_0_4).unwrap();
        assert!(cfg.extras.is_empty());
        let app = &cfg.default.app;
        assert_eq!(app.port_env, "PORT");
        assert_eq!(app.port_range, (3001, 3099));
        assert_eq!(app.env_file, "apps/platform/.env");
        assert!(app.envrc);
        assert_eq!(app.copy, ["apps/platform/storage"]);
        assert_eq!(
            app.refresh,
            ["apps/platform/.bundle", ".claude/agents", ".claude/skills"]
        );
        assert_eq!(cfg.default.services.len(), 1);
        let pg = &cfg.default.services[0];
        assert_eq!(pg.kind, Kind::Postgres);
        assert_eq!(pg.source, "myapp-postgres-1");
        assert_eq!(pg.volume, None);
        assert!(pg.clone);
        assert_eq!(pg.port_range, (5433, 5499));
        assert_eq!(pg.mode, Mode::Container);
        assert!(pg.databases.is_empty());
        assert_eq!(pg.env, Kind::Postgres.default_env());
        assert_eq!(pg.database.as_deref(), Some("platform_development"));
        assert_eq!(
            pg.check_sql.as_deref(),
            Some("select count(*) from schema_migrations")
        );
        assert!(cfg.default.sqlite.is_none());
        assert_eq!(
            cfg.default.hooks.setup,
            [
                hook(
                    &["mise", "exec", "--", "bundle", "install"],
                    "apps/platform"
                ),
                hook(&["mise", "exec", "--", "bun", "install"], "."),
            ]
        );
        assert_eq!(
            cfg.default.hooks.after_services,
            [hook(
                &["mise", "exec", "--", "bin/rails", "db:migrate"],
                "apps/platform"
            )]
        );
        assert!(cfg.docker.legacy_names);
        assert_eq!(
            cfg.docker.slug_label.as_deref(),
            Some("platform.worktree.slug")
        );

        let cfg = load(include_str!("../examples/rails-postgres.wt.toml")).unwrap();
        assert!(cfg.extras.is_empty());
        assert_eq!(cfg.default.app.env_file, ".env");
        assert_eq!(cfg.default.app.copy, ["storage"]);
        assert_eq!(cfg.default.services[0].source, "myapp-postgres-1");
        assert_eq!(cfg.default.services[0].mode, Mode::Container);
        assert_eq!(
            cfg.default.hooks.setup,
            [
                hook(&["bundle", "install"], "."),
                hook(&["bun", "install"], ".")
            ]
        );
        assert_eq!(
            cfg.default.hooks.after_services,
            [hook(&["bin/rails", "db:prepare"], ".")]
        );

        let cfg = load(include_str!("../examples/rails-postgres-database.wt.toml")).unwrap();
        assert!(cfg.extras.is_empty());
        let pg = &cfg.default.services[0];
        assert_eq!(pg.mode, Mode::Database);
        assert_eq!(pg.databases, ["myapp_development", "myapp_cache"]);
        assert_eq!(pg.env.len(), 2);
    }

    #[test]
    fn extras_keep_file_order_and_take_defaults_from_dir() {
        let cfg = load(
            r#"
            [apps.zeta]
            dir = "apps/zeta"
            [apps.zeta.hooks]
            setup = [["uv", "sync"], { run = ["bun", "install"], dir = "." }]

            [apps.alpha]
            dir = "./apps/alpha/"
            [apps.alpha.app]
            env_file = "config/alpha.env"
            port_env = "ALPHA_PORT"
            port_range = [8001, 8099]
            envrc = false
            "#,
        )
        .unwrap();
        let names: Vec<String> = cfg.apps().map(|a| a.id.to_string()).collect();
        assert_eq!(names, ["default", "zeta", "alpha"]);

        let zeta = &cfg.extras[0];
        assert_eq!(zeta.dir, PathBuf::from("apps/zeta"));
        assert_eq!(zeta.app.env_file, "apps/zeta/.env");
        assert_eq!(zeta.app.port_env, "PORT");
        assert_eq!(zeta.app.port_range, (3001, 3099));
        assert!(zeta.app.envrc);
        assert_eq!(zeta.hooks.setup[0].dir, PathBuf::from("apps/zeta"));
        assert_eq!(zeta.hooks.setup[1].dir, PathBuf::from("."));
        assert!(zeta.services.is_empty());

        let alpha = &cfg.extras[1];
        assert_eq!(alpha.dir, PathBuf::from("apps/alpha"));
        assert_eq!(alpha.app.env_file, "config/alpha.env");
        assert_eq!(alpha.app.port_env, "ALPHA_PORT");
        assert_eq!(alpha.app.port_range, (8001, 8099));
        assert!(!alpha.app.envrc);
    }

    #[test]
    fn extra_hooks_dir_overrides_the_app_dir() {
        let cfg = load(
            "[apps.b]\ndir = \"apps/b\"\n[apps.b.hooks]\ndir = \"apps/b/web\"\nsetup = [[\"x\"]]",
        )
        .unwrap();
        assert_eq!(
            cfg.extras[0].hooks.setup[0].dir,
            PathBuf::from("apps/b/web")
        );
    }

    #[test]
    fn extra_services_resolve_like_the_default_app() {
        let cfg = load(
            r#"
            [apps.coa-api]
            dir = "apps/coa-api"
            [apps.coa-api.services.postgres]
            source = "coa-api-postgres-1"
            database = "coa_references"
            [apps.coa-api.services.sqlite]
            files = ["apps/coa-api/dev.sqlite3"]
            "#,
        )
        .unwrap();
        let app = &cfg.extras[0];
        assert_eq!(app.services.len(), 1);
        assert_eq!(app.services[0].kind, Kind::Postgres);
        assert_eq!(app.services[0].source, "coa-api-postgres-1");
        assert_eq!(app.services[0].port_range, (5433, 5499));
        assert_eq!(app.sqlite.as_ref().unwrap().files.len(), 1);
        // An extra names its source: the compose default is the default app's.
        let e = err("[apps.b]\ndir = \"b\"\n[apps.b.services.redis]");
        assert!(
            e.contains("apps.b.services.redis.source is required"),
            "{e}"
        );
    }

    #[test]
    fn extra_errors_name_the_app() {
        let base = "[apps.b]\ndir = \"b\"\n";
        let cases = [
            (
                "[apps.b.services.mysql]\nsource = \"m\"\ncheck_sql = \"x\"",
                "apps.b.services.mysql.check_sql",
            ),
            (
                "[apps.b.services.redis]\nsource = \"r\"\nmode = \"database\"",
                "apps.b.services.redis.mode",
            ),
            (
                "[apps.b.services.postgres]\nsource = \"p\"\nmode = \"database\"\ndatabases = [\"a\"]\nenv = { H = \"x\" }",
                "apps.b.services.postgres.env",
            ),
            (
                "[apps.b.services.postgres]\nsource = \"p\"\ncheck_sql = \"x\"",
                "apps.b.services.postgres.check_sql needs apps.b.services.postgres.database",
            ),
            ("[apps.b.app]\nport_range = [9, 1]", "apps.b.app.port_range"),
            (
                "[apps.b.app]\ncopy = [\"s\"]\nrefresh = [\"s\"]",
                "apps.b.app.copy",
            ),
            ("[apps.b.app]\nfoo = 1", "foo"),
            (
                "[apps.b.hooks]\nsetup = [[]]",
                "apps.b.hooks.setup[0] is an empty command",
            ),
            ("[apps.b.hooks]\ndir = \"../x\"", "apps.b.hooks.dir"),
            (
                "[apps.b.hooks]\nsetup = [{ run = [\"x\"], dir = \"/a\" }]",
                "apps.b.hooks.setup[0].dir",
            ),
            (
                "[apps.b.services.sqlite]\nfiles = []",
                "apps.b.services.sqlite.files",
            ),
        ];
        for (text, key) in cases {
            let e = err(&format!("{base}{text}"));
            assert!(e.contains(key), "{text}: {e}");
        }
    }

    #[test]
    fn app_names_are_checked() {
        for bad in [
            "",
            "Bad",
            "a--b",
            "-a",
            "a-",
            "a_b",
            "a.b",
            "default",
            &"a".repeat(31),
        ] {
            let e = err(&format!("[apps.\"{bad}\"]\ndir = \"x\""));
            // Quoted only when the name could not be a bare TOML key.
            let odd = bad.is_empty()
                || bad
                    .chars()
                    // TOML bare keys: A-Z, a-z, 0-9, `-` and `_`.
                    .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
            let key = if odd {
                format!("apps.{bad:?}")
            } else {
                format!("apps.{bad}")
            };
            assert!(e.contains(&format!("{key}:")), "{bad}: {e}");
        }
        assert!(load("[apps.coa-api2]\ndir = \"x\"").is_ok());
        assert!(load(&format!("[apps.{}]\ndir = \"x\"", "a".repeat(30))).is_ok());
    }

    #[test]
    fn an_extra_needs_a_dir_inside_the_worktree() {
        let e = err("[apps.b]");
        assert!(e.contains("apps.b.dir is required"), "{e}");
        for bad in ["", "/abs", "../x"] {
            let e = err(&format!("[apps.b]\ndir = {bad:?}"));
            assert!(e.contains("apps.b.dir"), "{bad}: {e}");
        }
    }

    #[test]
    fn worktree_keys_are_refused_inside_an_app() {
        for (key, field) in [
            ("project = \"x\"", "project"),
            ("root = \"/x\"", "root"),
            ("[apps.b.docker]\nlegacy_names = true", "docker"),
        ] {
            let e = err(&format!("[apps.b]\ndir = \"b\"\n{key}"));
            assert!(e.contains(field), "{key}: {e}");
        }
    }

    #[test]
    fn paths_are_normalized_and_stay_in_the_worktree() {
        let cfg = load(
            "[app]\nenv_file = \"./a/../.env\"\ncopy = [\"./storage/\"]\n[services.sqlite]\nfiles = [\"./db/dev.sqlite3\"]",
        )
        .unwrap();
        assert_eq!(cfg.default.app.env_file, ".env");
        assert_eq!(cfg.default.app.copy, ["storage"]);
        assert_eq!(
            cfg.default.sqlite.as_ref().unwrap().files,
            ["db/dev.sqlite3"]
        );
        for (text, key) in [
            ("[app]\nenv_file = \"/etc/x\"", "app.env_file"),
            ("[app]\ncopy = [\"../x\"]", "app.copy"),
            ("[app]\nrefresh = [\"a/../../x\"]", "app.refresh"),
            (
                "[services.sqlite]\nfiles = [\"/x.sqlite3\"]",
                "services.sqlite.files",
            ),
            (
                "[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"/x\"]",
                "apps.b.app.copy",
            ),
        ] {
            let e = err(text);
            assert!(e.contains(key), "{text}: {e}");
        }
        // Normalized before comparing: these name the same file.
        let e = err("[app]\nenv_file = \"apps/b/../c/.env\"\n[apps.c]\ndir = \"apps/c\"");
        assert!(e.contains("apps/c/.env"), "{e}");
    }

    #[test]
    fn two_apps_cannot_write_the_same_env_file() {
        // The default app writes .env at the root; an extra in "." would too.
        let e = err("[apps.b]\ndir = \".\"");
        assert!(
            e.contains("env_file") && e.contains(".env") && e.contains("apps.b"),
            "{e}"
        );
        let e = err("[apps.b]\ndir = \"x\"\n[apps.c]\ndir = \"x\"\n[apps.c.app]\nenvrc = false");
        assert!(
            e.contains("apps.b") && e.contains("apps.c") && e.contains("x/.env"),
            "{e}"
        );
        let e = err("[app]\nenv_file = \"x/.env\"\n[apps.b]\ndir = \"x\"");
        assert!(e.contains("x/.env"), "{e}");
    }

    #[test]
    fn two_apps_cannot_write_the_same_envrc() {
        let two = "[apps.b]\ndir = \"x\"\n[apps.b.app]\nenv_file = \"b.env\"\n[apps.c]\ndir = \"x\"\n[apps.c.app]\nenv_file = \"c.env\"";
        let e = err(two);
        assert!(
            e.contains(".envrc") && e.contains("apps.b") && e.contains("apps.c"),
            "{e}"
        );
        assert!(load(&format!("{two}\nenvrc = false")).is_ok());
        // No web port means no .envrc.
        assert!(load(&format!("{two}\nport_env = \"\"")).is_ok());
    }

    #[test]
    fn an_extra_in_the_root_collides_with_the_default_envrc() {
        let extra = "[apps.b]\ndir = \".\"\n[apps.b.app]\nenv_file = \"b.env\"";
        let e = err(extra);
        assert!(
            e.contains("app.envrc and apps.b.app.envrc") && e.contains(".envrc"),
            "{e}"
        );
        assert!(load(&format!("{extra}\nenvrc = false")).is_ok());
    }

    #[test]
    fn copy_and_refresh_do_not_overlap_across_apps() {
        let cases = [
            "[app]\ncopy = [\"storage\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\nrefresh = [\"storage/x\"]",
            "[app]\ncopy = [\"./storage\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\nrefresh = [\"storage\"]",
            "[app]\ncopy = [\".\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"x\"]",
        ];
        for text in cases {
            let e = err(text);
            assert!(
                e.contains("app.") && e.contains("apps.b.app."),
                "{text}: {e}"
            );
        }
        let e = err(
            "[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"s\"]\n[apps.c]\ndir = \"c\"\n[apps.c.app]\ncopy = [\"s\"]",
        );
        assert!(e.contains("apps.b") && e.contains("apps.c"), "{e}");
        assert!(
            load("[app]\ncopy = [\"a\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"ab\"]")
                .is_ok()
        );
    }

    #[test]
    fn copy_and_refresh_do_not_cover_another_apps_files() {
        let e = err("[app]\nrefresh = [\"apps\"]\n[apps.b]\ndir = \"apps/b\"");
        assert!(
            e.contains("app.refresh") && e.contains("apps.b.app.env_file"),
            "{e}"
        );
        let e = err(
            "[app]\nenv_file = \"apps/platform/.env\"\nenvrc = false\n[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"apps/platform\"]",
        );
        assert!(
            e.contains("apps.b.app.copy") && e.contains("app.env_file"),
            "{e}"
        );
        // The default app's .envrc sits at the root.
        let e = err("[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\".envrc\"]");
        assert!(e.contains("apps.b.app.copy") && e.contains(".envrc"), "{e}");
        let e = err(
            "[services.sqlite]\nfiles = [\"db/dev.sqlite3\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\nrefresh = [\"db\"]",
        );
        assert!(
            e.contains("apps.b.app.refresh") && e.contains("services.sqlite.files"),
            "{e}"
        );
        // An app's own files are its own business.
        assert!(load("[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"b/storage\"]").is_ok());
    }

    fn pg_db(prefix: &str, source: &str, dbs: &str) -> String {
        format!(
            "[{prefix}services.postgres]\nmode = \"database\"\nsource = \"{source}\"\n{dbs}env = {{ U = \"x/{{database}}\" }}\n"
        )
    }

    #[test]
    fn two_apps_cannot_copy_the_same_database() {
        let b = "[apps.b]\ndir = \"b\"\n";
        let list = |db: &str| format!("databases = [\"{db}\"]\n");
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", &list("app")),
            pg_db("apps.b.", "pg-1", &list("app"))
        ));
        assert!(
            e.contains("pg-1") && e.contains("app") && e.contains("apps.b"),
            "{e}"
        );
        assert!(
            load(&format!(
                "{}{b}{}",
                pg_db("", "pg-1", &list("app")),
                pg_db("apps.b.", "pg-1", &list("other"))
            ))
            .is_ok()
        );
        // Different sources never collide.
        assert!(
            load(&format!(
                "{}{b}{}",
                pg_db("", "pg-1", &list("app")),
                pg_db("apps.b.", "pg-2", &list("app"))
            ))
            .is_ok()
        );
        // A detected database cannot be told apart at load.
        for (a, bb) in [("", ""), (list("app").as_str(), "")] {
            let e = err(&format!(
                "{}{b}{}",
                pg_db("", "pg-1", a),
                pg_db("apps.b.", "pg-1", bb)
            ));
            assert!(
                e.contains("pg-1") && e.contains("name their databases"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_source_is_cloned_or_copied_in_one_mode() {
        let b = "[apps.b]\ndir = \"b\"\n";
        let e = err(&format!(
            "{}{b}[apps.b.services.postgres]\nsource = \"pg-1\"",
            pg_db("", "pg-1", "databases = [\"a\"]\n")
        ));
        assert!(e.contains("pg-1") && e.contains("mode"), "{e}");
        // Two clones of one source, both in container mode, are fine.
        assert!(
            load(&format!(
                "[services.postgres]\nsource = \"pg-1\"\ndatabase = \"app\"\n{b}[apps.b.services.postgres]\nsource = \"pg-1\"\ndatabase = \"app\""
            ))
            .is_ok()
        );
    }

    // ---- Cross-app checks and shared sources ----

    #[test]
    fn services_lists_every_apps_services() {
        let cfg = load(
            "[services.postgres]\n[apps.b]\ndir = \"b\"\n[apps.b.services.redis]\nsource = \"b-redis-1\"",
        )
        .unwrap();
        let sources: Vec<&str> = cfg.services().map(|s| s.source.as_str()).collect();
        assert_eq!(sources, ["myapp-postgres-1", "b-redis-1"]);
    }

    #[test]
    fn an_apps_refresh_must_not_cover_its_own_files() {
        for (text, own) in [
            ("[app]\nrefresh = [\".\"]", "app.env_file"),
            (
                "[app]\nenv_file = \"web/.env\"\nrefresh = [\"web\"]",
                "app.env_file",
            ),
            ("[app]\nrefresh = [\".envrc\"]", "app.envrc"),
            (
                "[services.sqlite]\nfiles = [\"db/dev.sqlite3\"]\n[app]\nrefresh = [\"db\"]",
                "services.sqlite.files",
            ),
            (
                "[apps.b]\ndir = \"apps/b\"\n[apps.b.app]\nrefresh = [\"apps/b\"]",
                "apps.b.app.env_file",
            ),
        ] {
            let e = err(text);
            assert!(e.contains("refresh") && e.contains(own), "{text}: {e}");
        }
        // No .envrc is written without a web port, and copy never overwrites.
        assert!(load("[app]\nport_env = \"\"\nrefresh = [\".envrc\"]").is_ok());
        assert!(load("[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"b/storage\", \"b\"]").is_ok());
    }

    #[test]
    fn two_apps_never_write_one_file() {
        let cases = [
            (
                "[apps.b]\ndir = \"b\"\n[apps.b.app]\nenv_file = \".envrc\"",
                "app.envrc",
            ),
            (
                "[services.sqlite]\nfiles = [\"db/dev.sqlite3\"]\n[apps.b]\ndir = \"b\"\n[apps.b.services.sqlite]\nfiles = [\"./db/dev.sqlite3\"]",
                "apps.b.services.sqlite.files",
            ),
            // One app, two outputs on one path.
            ("[app]\nenv_file = \".envrc\"", "app.envrc"),
        ];
        for (text, key) in cases {
            let e = err(text);
            assert!(e.contains(key), "{text}: {e}");
        }
        assert!(load("[app]\nenv_file = \".envrc\"\nenvrc = false").is_ok());
    }

    #[test]
    fn a_source_mode_clash_is_refused_both_ways() {
        let b = "[apps.b]\ndir = \"b\"\n";
        let e = err(&format!(
            "[services.postgres]\nsource = \"pg-1\"\n{b}{}",
            pg_db("apps.b.", "pg-1", "databases = [\"a\"]\n")
        ));
        assert!(e.contains("different modes"), "{e}");
    }

    #[test]
    fn overlap_is_checked_from_either_side() {
        for text in [
            "[app]\ncopy = [\"storage/x\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\nrefresh = [\"storage\"]",
            "[app]\ncopy = [\"x\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\".\"]",
        ] {
            let e = err(text);
            assert!(e.contains("apps.b.app."), "{text}: {e}");
        }
        let e = err("[app]\ncopy = [\"x\"]\nrefresh = [\".\"]");
        assert!(e.contains("app.copy and app.refresh"), "{e}");
    }

    #[test]
    fn shared_source_databases_are_compared_whole() {
        let b = "[apps.b]\ndir = \"b\"\n";
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", ""),
            pg_db("apps.b.", "pg-1", "databases = [\"app\"]\n")
        ));
        assert!(e.contains("name their databases"), "{e}");
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", "databases = [\"x\", \"app\"]\n"),
            pg_db("apps.b.", "pg-1", "databases = [\"app\"]\n")
        ));
        assert!(e.contains("\"app\""), "{e}");
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", "databases = [\"--\"]\n"),
            pg_db("apps.b.", "pg-1", "databases = [\"__\"]\n")
        ));
        assert!(
            e.contains("services.postgres.databases") && e.contains("no characters usable"),
            "{e}"
        );
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", "databases = [\"app\"]\n"),
            pg_db("apps.b.", "pg-1", "databases = [\"x\", \"app\"]\n")
        ));
        assert!(e.contains("would both copy \"app\""), "{e}");
    }

    #[test]
    fn an_empty_path_hint_fits_the_key() {
        let e = err("[app]\nenv_file = \"\"");
        assert!(
            e.contains("app.env_file must not be empty") && !e.contains("use \".\""),
            "{e}"
        );
        let e = err("[hooks]\ndir = \"\"");
        assert!(e.contains("use \".\""), "{e}");
    }

    #[test]
    fn the_monorepo_example_loads() {
        let cfg = load(include_str!("../examples/monorepo.wt.toml")).unwrap();
        let names: Vec<String> = cfg.apps().map(|a| a.id.to_string()).collect();
        assert_eq!(names, ["default", "coa-api"]);
        let api = &cfg.extras[0];
        assert_eq!(api.dir, PathBuf::from("apps/coa-api"));
        assert_eq!(api.app.env_file, "apps/coa-api/.env");
        assert_eq!(api.app.port_env, "COA_API_PORT");
        assert_eq!(api.services[0].source, "coa-api-postgres-1");
        assert_eq!(api.hooks.setup, [hook(&["uv", "sync"], "apps/coa-api")]);
        assert_eq!(cfg.default.hooks.setup[1], hook(&["bun", "install"], "."));
        // The README shows this file without its header comment.
        let file = include_str!("../examples/monorepo.wt.toml");
        let body = file.split_once("\n\n").unwrap().1;
        let readme = include_str!("../README.md");
        assert!(
            readme.contains(&format!("```toml\n{body}```")),
            "README block differs from the example"
        );
    }

    #[test]
    fn sqlite_companions_are_the_apps_own_files() {
        let e = err(
            "[services.sqlite]\nfiles = [\"storage/dev.sqlite3\"]\n[app]\nrefresh = [\"storage/dev.sqlite3-wal\"]",
        );
        assert!(
            e.contains("services.sqlite.files") && e.contains("-wal"),
            "{e}"
        );
        let e = err(
            "[services.sqlite]\nfiles = [\"db/dev.sqlite3\"]\n[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"db/dev.sqlite3-shm\"]",
        );
        assert!(e.contains("-shm") && e.contains("would write"), "{e}");
    }

    #[test]
    fn one_writer_compares_nested_paths() {
        let e = err(
            "[app]\nenv_file = \"apps/x/config\"\n[apps.b]\ndir = \"b\"\n[apps.b.services.sqlite]\nfiles = [\"apps/x/config/db.sqlite3\"]",
        );
        assert!(
            e.contains("app.env_file") && e.contains("apps.b.services.sqlite.files"),
            "{e}"
        );
    }

    #[test]
    fn list_errors_name_the_item() {
        for (text, key) in [
            (
                "[app]\ncopy = [\"storage\", \"\"]",
                "app.copy[1] must not be empty",
            ),
            ("[app]\nrefresh = [\"a\", \"/b\"]", "app.refresh[1]"),
            (
                "[services.sqlite]\nfiles = [\"a.sqlite3\", \"../b\"]",
                "services.sqlite.files[1]",
            ),
            (
                "[apps.b]\ndir = \"b\"\n[apps.b.app]\ncopy = [\"/x\"]",
                "apps.b.app.copy[0]",
            ),
        ] {
            let e = err(text);
            assert!(e.contains(key), "{text}: {e}");
        }
    }

    #[test]
    fn an_app_cannot_nest_apps() {
        let e = err("[apps.b]\ndir = \"b\"\n[apps.b.apps.c]\ndir = \"c\"");
        assert!(e.contains("apps"), "{e}");
    }

    #[test]
    fn one_container_cannot_be_two_service_types() {
        let e = err(
            "[services.postgres]\nsource = \"c\"\n[apps.b]\ndir = \"b\"\n[apps.b.services.mysql]\nsource = \"c\"",
        );
        assert!(
            e.contains("postgres") && e.contains("mysql") && e.contains("one container"),
            "{e}"
        );
    }

    #[test]
    fn apps_are_found_by_name() {
        let cfg = load(include_str!("../examples/monorepo.wt.toml")).unwrap();
        assert_eq!(cfg.app("default").unwrap().id, AppId::Default);
        assert_eq!(cfg.app("coa-api").unwrap().id.label(), Some("coa-api"));
        let e = format!("{:#}", cfg.app("nope").unwrap_err());
        assert!(
            e.contains("\"nope\"") && e.contains("default, coa-api"),
            "{e}"
        );
    }

    #[test]
    fn an_app_name_parses_on_its_own() {
        let name = AppName::parse("coa-api").unwrap();
        assert_eq!(name.as_str(), "coa-api");
        let e = format!("{:#}", AppName::parse("Bad").unwrap_err());
        assert!(e.starts_with("an app name is"), "{e}");
        assert!(AppId::Default.label().is_none());
    }

    #[test]
    fn two_apps_may_copy_names_that_only_meet_once_sanitized() {
        // Their targets carry the app, so app-db and app_db do not collide.
        let b = "[apps.b]\ndir = \"b\"\n";
        assert!(
            load(&format!(
                "{}{b}{}",
                pg_db("", "pg-1", "databases = [\"app-db\"]\n"),
                pg_db("apps.b.", "pg-1", "databases = [\"app_db\"]\n")
            ))
            .is_ok()
        );
        // The same database twice from one source is still refused, as policy.
        let e = err(&format!(
            "{}{b}{}",
            pg_db("", "pg-1", "databases = [\"app\"]\n"),
            pg_db("apps.b.", "pg-1", "databases = [\"app\"]\n")
        ));
        assert!(e.contains("would both copy \"app\" from pg-1"), "{e}");
    }

    #[test]
    fn an_empty_app_name_is_not_an_app() {
        let cfg = load(include_str!("../examples/monorepo.wt.toml")).unwrap();
        assert!(cfg.app("").is_err());
    }

    #[test]
    fn mysql_database_names_compare_without_case() {
        let my = |prefix: &str, db: &str| {
            format!(
                "[{prefix}services.mysql]\nmode = \"database\"\nsource = \"my-1\"\ndatabases = [\"{db}\"]\nenv = {{ U = \"x/{{database}}\" }}\n"
            )
        };
        let e = err(&format!(
            "{}[apps.b]\ndir = \"b\"\n{}",
            my("", "App"),
            my("apps.b.", "app")
        ));
        // Both spellings, so the clash is visible.
        assert!(e.contains("\"App\"") && e.contains("\"app\""), "{e}");
        // Postgres names are case-sensitive.
        let b = "[apps.b]\ndir = \"b\"\n";
        assert!(
            load(&format!(
                "{}{b}{}",
                pg_db("", "pg-1", "databases = [\"App\"]\n"),
                pg_db("apps.b.", "pg-1", "databases = [\"app\"]\n")
            ))
            .is_ok()
        );
    }
}
