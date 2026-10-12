//! `.wt.toml`: how a repo describes its worktree environment. Everything the
//! clone needs to boot (image, command, env) is read from the source container
//! instead, so it can never drift from the stack it was copied from.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::services::Kind;

pub const FILE_NAME: &str = ".wt.toml";

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
    /// Values may hold `{port}`, replaced with the clone's host port.
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
        if entry.database.is_some() && kind != Kind::Postgres {
            bail!(
                "services.{}.database is only supported for postgres",
                kind.name()
            );
        }
        if entry.check_sql.is_some() && kind != Kind::Postgres {
            bail!(
                "services.{}.check_sql is only supported for postgres",
                kind.name()
            );
        }
        if entry.check_sql.is_some() && entry.database.is_none() {
            bail!("services.postgres.check_sql needs services.postgres.database");
        }
        let port_range = entry.port_range.unwrap_or(kind.default_port_range());
        check_range(&format!("services.{}.port_range", kind.name()), port_range)?;
        services.push(Service {
            kind,
            source: entry
                .source
                .unwrap_or_else(|| format!("{project}-{}-1", kind.name())),
            volume: entry.volume,
            clone: entry.clone.unwrap_or(true),
            port_range,
            env: entry.env.unwrap_or_else(|| kind.default_env()),
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
        assert_eq!(pg.port_range, (5433, 5499));
        assert_eq!(pg.env["DB_PORT"], "{port}");
        assert_eq!(pg.env["DB_HOST"], "127.0.0.1");
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
    fn database_is_postgres_only() {
        assert!(load("[services.mysql]\ndatabase = \"x\"").is_err());
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
