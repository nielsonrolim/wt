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
    app: App,
    #[serde(default)]
    services: Services,
    #[serde(default)]
    hooks: Hooks,
    #[serde(default)]
    docker: Docker,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct App {
    /// The env key that carries the web port; empty turns the web port off.
    pub port_env: String,
    pub port_range: (u16, u16),
    pub env_file: String,
    pub envrc: bool,
    pub copy: Vec<String>,
    /// Like `copy`, but replaced on every run when the main checkout differs.
    pub refresh: Vec<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            port_env: "PORT".into(),
            port_range: (3001, 3099),
            env_file: ".env".into(),
            envrc: true,
            copy: Vec::new(),
            refresh: Vec::new(),
        }
    }
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

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    #[serde(default)]
    pub after_services: Vec<Vec<String>>,
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
    pub app: App,
    pub services: Vec<Service>,
    pub sqlite: Option<Sqlite>,
    pub hooks: Hooks,
    pub docker: Docker,
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

    #[cfg(test)]
    pub fn service(&self, kind: Kind) -> Option<&Service> {
        self.services.iter().find(|s| s.kind == kind)
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

    check_range("app.port_range", file.app.port_range)?;
    // Nested counts too: refresh would overwrite inside a tree copy protects.
    if let Some(path) = file.app.refresh.iter().find(|r| {
        file.app
            .copy
            .iter()
            .any(|c| Path::new(c).starts_with(r) || Path::new(r).starts_with(c))
    }) {
        bail!("app.copy and app.refresh both cover {path}; pick one");
    }
    if file.docker.slug_label.as_deref() == Some("") {
        bail!("docker.slug_label must not be empty");
    }
    for (phase, hooks) in [
        ("setup", &file.hooks.setup),
        ("after_services", &file.hooks.after_services),
    ] {
        if let Some(i) = hooks.iter().position(|argv| argv.is_empty()) {
            bail!("hooks.{phase}[{i}] is an empty command");
        }
    }

    let mut services = Vec::new();
    for (kind, entry) in [
        (Kind::Postgres, file.services.postgres),
        (Kind::Mysql, file.services.mysql),
        (Kind::Redis, file.services.redis),
    ] {
        let Some(entry) = entry else { continue };
        let name = kind.name();
        let mode = entry.mode.unwrap_or_default();

        if kind == Kind::Redis && mode == Mode::Database {
            bail!(
                "services.{name}.mode = \"database\" is not supported: Redis has no separate databases to copy (use mode = \"container\")"
            );
        }
        if mode == Mode::Database {
            if let Some(key) = &entry.volume {
                bail!("services.{name}.volume = {key:?} is not used in mode = \"database\"");
            }
            if entry.port_range.is_some() {
                bail!("services.{name}.port_range is not used in mode = \"database\"");
            }
        }
        if kind != Kind::Postgres && entry.database.is_some() && mode != Mode::Database {
            bail!("services.{name}.database is only supported for postgres");
        }
        if entry.check_sql.is_some() && kind != Kind::Postgres {
            bail!("services.{name}.check_sql is only supported for postgres");
        }

        let listed = entry.databases.is_some();
        let databases = match (entry.databases, entry.database.as_ref()) {
            (Some(_), Some(_)) => {
                bail!("services.{name}: set either database or databases, not both")
            }
            (Some(list), None) => list,
            (None, Some(db)) => vec![db.clone()],
            (None, None) => Vec::new(),
        };
        if listed && mode != Mode::Database {
            bail!("services.{name}.databases requires mode = \"database\"");
        }
        for db in &databases {
            if db.trim().is_empty() {
                bail!("services.{name}.databases must not contain an empty database name");
            }
            if db.contains('}') {
                bail!("services.{name}.databases: database name {db:?} must not contain \"}}\"");
            }
            if db.contains('{') {
                bail!("services.{name}.databases: database name {db:?} must not contain \"{{\"");
            }
        }
        if listed {
            if databases.is_empty() {
                bail!("services.{name}.databases must list at least one database");
            }
            for db in &databases {
                if databases.iter().filter(|d| *d == db).count() > 1 {
                    bail!("services.{name}.databases must not contain {db:?} twice");
                }
            }
        }
        if entry.check_sql.is_some() && mode == Mode::Container && entry.database.is_none() {
            bail!("services.postgres.check_sql needs services.postgres.database");
        }
        // In container mode `database` is only the check target: the volume
        // copy already carries every logical database.
        let mut databases = databases;
        if mode != Mode::Database {
            databases.clear();
        }

        let env = entry.env.unwrap_or_else(|| kind.default_env());
        check_env(name, mode, &databases, &env)?;

        let port_range = entry.port_range.unwrap_or(kind.default_port_range());
        check_range(&format!("services.{name}.port_range"), port_range)?;
        services.push(Service {
            kind,
            source: entry
                .source
                .unwrap_or_else(|| format!("{project}-{name}-1")),
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

    if let Some(sqlite) = &file.services.sqlite
        && sqlite.files.is_empty()
    {
        bail!("services.sqlite.files must list at least one file");
    }

    Ok(Config {
        path,
        project,
        root,
        app: file.app,
        services,
        sqlite: file.services.sqlite,
        hooks: file.hooks,
        docker: file.docker,
    })
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
    name: &str,
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
                        bail!(
                            "services.{name}.env: {{database}} is only available in mode = \"database\""
                        );
                    }
                    references_a_database = true;
                }
                _ => {
                    if let Some(db) = token.strip_prefix("database:") {
                        if mode != Mode::Database {
                            bail!(
                                "services.{name}.env: {{{token}}} is only available in mode = \"database\""
                            );
                        }
                        if db.is_empty() {
                            bail!("services.{name}.env: placeholder {{{token}}} names no database");
                        }
                        if databases.is_empty() {
                            bail!(
                                "services.{name}.env references database {db:?} but databases is not set"
                            );
                        }
                        if !databases.iter().any(|d| d == db) {
                            bail!(
                                "services.{name}.env references database {db:?} which is not in databases"
                            );
                        }
                        references_a_database = true;
                    } else if token.starts_with("database") {
                        bail!("services.{name}.env has unknown placeholder {{{token}}}");
                    }
                    // Other braces are literal (Helm templates, JSON) and are
                    // left untouched, as they were before database mode.
                }
            }
        }
    }
    if mode == Mode::Database && !references_a_database {
        bail!(
            "services.{name}.env must reference a database ({{database}} or {{database:<source>}}) so the worktree does not write into the source"
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
        let file: File = toml::from_str(text)?;
        resolve(file, Path::new("/work/My.App"), None)
    }

    #[test]
    fn empty_file_gets_defaults() {
        let cfg = load("").unwrap();
        assert_eq!(cfg.project, "myapp");
        assert!(cfg.root.ends_with(".herdr/worktrees/myapp"));
        assert_eq!(cfg.app.port_env, "PORT");
        assert_eq!(cfg.app.port_range, (3001, 3099));
        assert!(cfg.services.is_empty());
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
        assert_eq!(cfg.hooks.setup[1], vec!["bun", "install"]);
        assert_eq!(cfg.hooks.after_services.len(), 1);
    }

    #[test]
    fn rejects_empty_or_string_hooks() {
        assert!(load("[hooks]\nsetup = [[]]").is_err());
        assert!(load("[hooks]\nsetup = [\"bun install\"]").is_err());
    }

    #[test]
    fn sqlite_needs_files() {
        assert!(load("[services.sqlite]\nfiles = []").is_err());
        assert!(load("[services.sqlite]").is_err());
        let cfg = load("[services.sqlite]\nfiles = [\"storage/dev.sqlite3\"]").unwrap();
        assert_eq!(cfg.sqlite.unwrap().files.len(), 1);
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
        assert_eq!(cfg.app.copy, vec!["storage"]);
        assert_eq!(cfg.hooks.after_services[0], vec!["bin/rails", "db:prepare"]);
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
        assert_eq!(cfg.app.copy, vec!["storage"]);
    }

    #[test]
    fn refresh_must_not_overlap_copy() {
        let cfg = load("[app]\ncopy = [\"storage\"]\nrefresh = [\".claude/agents\"]").unwrap();
        assert_eq!(cfg.app.refresh, vec![".claude/agents"]);
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
}
