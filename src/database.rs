//! `mode = "database"`: copies logical databases inside the source instance,
//! with no container and no volume of the worktree's own. The database comment
//! (Postgres) or the `__wt_meta` table (MySQL) is the proof of ownership, the
//! same job the Docker labels do for containers and volumes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::config::Service;
use crate::docker::{self, Container};
use crate::project::Env;
use crate::services::ContainerSource;
use crate::state::DatabaseCopy;

/// Where the dump is written inside the source container. A file, never a
/// pipe: without `pipefail` the restore's exit code would hide a failed dump.
const DUMP_FILE: &str = "/tmp/wt-copy";
/// Postgres truncates identifiers past this; MySQL allows 64.
const MAX_NAME: usize = 63;

// ---------------------------------------------------------------------------
// Names and markers
// ---------------------------------------------------------------------------

/// Lowercase, anything outside `[a-z0-9]` becomes one `_`.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    let mut underscore = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            underscore = false;
        } else if !out.is_empty() && !underscore {
            out.push('_');
            underscore = true;
        }
    }
    out.trim_end_matches('_').to_string()
}

fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deterministic and independent of the list order: the suffix comes from the
/// source database's name, never its position.
pub fn target_name(project: &str, slug: &str, database: &str) -> String {
    let base = format!("wt_{}_{}", sanitize(project), sanitize(slug));
    let suffix = sanitize(database);
    let full = format!("{base}__{suffix}");
    if full.len() <= MAX_NAME {
        return full;
    }
    let hash = fnv1a(&format!("{project}\0{slug}\0{database}"));
    let prefix = format!("wt_{:08x}__", hash as u32);
    let budget = MAX_NAME - prefix.len();
    format!("{prefix}{}", shorten(&suffix, budget))
}

fn shorten(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let keep = budget.saturating_sub(9);
    let head = text[..keep].trim_end_matches('_');
    format!("{head}_{:08x}", fnv1a(text) as u32)
}

/// The target for every source database, refusing two names that sanitize to
/// the same target, a name with no usable characters, and a target that is
/// itself one of the sources.
pub fn targets(project: &str, slug: &str, databases: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for database in databases {
        if sanitize(database).is_empty() {
            bail!("database name {database:?} has no characters usable in a target");
        }
        let target = target_name(project, slug, database);
        check_not_self(database, &target)?;
        if let Some(other) = seen.get(&target) {
            bail!("databases {other:?} and {database:?} both map to {target}");
        }
        seen.insert(target.clone(), database.clone());
        out.push((database.clone(), target));
    }
    let sources: BTreeSet<&str> = databases.iter().map(String::as_str).collect();
    for (source, target) in &out {
        if sources.contains(target.as_str()) {
            bail!("target {target} for {source:?} is also a source database; choose another name");
        }
    }
    Ok(out)
}

/// A copy may never land on the database it copied from.
pub fn check_not_self(source: &str, target: &str) -> Result<()> {
    if source == target {
        bail!("refusing to copy database {source} onto itself");
    }
    Ok(())
}

/// The database's comment, the label of a Postgres copy.
pub fn pg_marker(project: &str, slug: &str, complete: bool) -> String {
    format!(
        "wt.project={project};wt.slug={slug};wt.complete={}",
        u8::from(complete)
    )
}

#[derive(Debug, PartialEq, Eq)]
pub struct Marker {
    pub project: String,
    pub slug: String,
    pub complete: bool,
}

pub fn parse_pg_marker(text: &str) -> Option<Marker> {
    let mut project = None;
    let mut slug = None;
    let mut complete = None;
    for part in text.split(';') {
        let (key, value) = part.split_once('=')?;
        match key {
            "wt.project" => project = Some(value.to_string()),
            "wt.slug" => slug = Some(value.to_string()),
            "wt.complete" => {
                complete = Some(match value {
                    "1" => true,
                    "0" => false,
                    _ => return None,
                })
            }
            _ => {}
        }
    }
    Some(Marker {
        project: project?,
        slug: slug?,
        complete: complete?,
    })
}

/// What the marker on an existing target says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    /// The target does not exist.
    Missing,
    /// It exists but carries no marker of ours: never dropped without --force.
    Foreign,
    /// Ours, but the copy never finished: drop and copy again.
    Recreate,
    /// Ours and finished: reuse.
    Reuse,
}

pub fn classify_marker(comment: Option<&str>, project: &str, slug: &str) -> MarkerState {
    match comment.and_then(parse_pg_marker) {
        Some(marker) if marker.project == project && marker.slug == slug => {
            if marker.complete {
                MarkerState::Reuse
            } else {
                MarkerState::Recreate
            }
        }
        _ => MarkerState::Foreign,
    }
}

/// The `__wt_meta` rows are the label of a MySQL copy. A missing table is
/// `None`, which is foreign; an extra key is ignored.
pub fn classify_mysql_marker(
    rows: Option<&BTreeMap<String, String>>,
    project: &str,
    slug: &str,
) -> MarkerState {
    let Some(rows) = rows else {
        return MarkerState::Foreign;
    };
    match (
        rows.get("wt.project"),
        rows.get("wt.slug"),
        rows.get("wt.complete"),
    ) {
        (Some(p), Some(s), Some(c)) if p == project && s == slug => {
            if c == "1" {
                MarkerState::Reuse
            } else {
                MarkerState::Recreate
            }
        }
        _ => MarkerState::Foreign,
    }
}

// ---------------------------------------------------------------------------
// Pure helpers: quoting, scripts, env rendering, reconciliation
// ---------------------------------------------------------------------------

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn sql_lit(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// MySQL treats backslash as an escape inside a single-quoted string, unlike
/// Postgres.
fn mysql_lit(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

fn sql_ident(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn sql_backtick(text: &str) -> String {
    format!("`{}`", text.replace('`', "``"))
}

/// One dump path per target: two worktrees brought up at the same time from
/// the same source container must not share a temporary file.
fn dump_file(target: &str) -> String {
    format!("{DUMP_FILE}-{}", sanitize(target))
}

/// Dump to a file and restore from it, so `set -e` sees the dump's exit code.
/// No pipe and no `set -o pipefail`: the shell inside the image may not have it,
/// and a failed dump would otherwise still report success.
pub fn pg_copy_script(origin: &str, target: &str, user: &str) -> String {
    let file = dump_file(target);
    format!(
        "f={file}\ntrap 'rm -f {file}' 0 1 2 15\npg_dump -U {user} -d {origin} > \"$f\" && psql -v ON_ERROR_STOP=1 -U {user} -d {target} < \"$f\"",
        user = shell_quote(user),
        origin = shell_quote(origin),
        target = shell_quote(target),
    )
}

/// `--set-gtid-purged` is MySQL-only; MariaDB rejects it, which would make the
/// dump fail and leave an empty copy.
pub fn mysql_dump_args(version: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--single-transaction",
        "--quick",
        "--routines",
        "--triggers",
        "--events",
    ]
    .iter()
    .map(|a| (*a).to_string())
    .collect();
    if !version.to_lowercase().contains("mariadb") {
        args.push("--set-gtid-purged=OFF".into());
    }
    args
}

pub fn mysql_copy_script(origin: &str, target: &str, version: &str) -> String {
    let args = mysql_dump_args(version).join(" ");
    let file = dump_file(target);
    format!(
        "f={file}\ntrap 'rm -f {file}' 0 1 2 15\ndump=$(command -v mysqldump || command -v mariadb-dump)\ncli=$(command -v mysql || command -v mariadb)\n\"$dump\" {args} {origin} > \"$f\" && \"$cli\" {target} < \"$f\"",
        origin = shell_quote(origin),
        target = shell_quote(target),
    )
}

/// Replaces `{port}` and, in database mode, `{database}` and
/// `{database:<source>}`. Config validation already rejected anything else.
pub fn render_env(
    values: &[(String, String)],
    targets: &[(String, String)],
    port: u16,
) -> Result<Vec<(String, String)>> {
    let primary = targets
        .first()
        .map(|(_, target)| target.as_str())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (key, value) in values {
        let mut text = value.replace("{port}", &port.to_string());
        for (source, target) in targets {
            text = text.replace(&format!("{{database:{source}}}"), target);
        }
        text = text.replace("{database}", primary);
        if text.contains("{database") {
            bail!("services.env value {key} has an unresolved {{database}} placeholder");
        }
        out.push((key.clone(), text));
    }
    Ok(out)
}

#[derive(Debug, Default)]
pub struct Reconciled {
    pub create: Vec<DatabaseCopy>,
    pub reuse: Vec<DatabaseCopy>,
    pub recreate: Vec<DatabaseCopy>,
    /// Copies a previous run left behind that the config no longer asks for.
    /// Never dropped by `up`; `down` removes them.
    pub keep: Vec<DatabaseCopy>,
}

pub fn reconcile(previous: &[DatabaseCopy], desired: &[(DatabaseCopy, MarkerState)]) -> Reconciled {
    let mut out = Reconciled::default();
    for (copy, state) in desired {
        match state {
            MarkerState::Missing => out.create.push(copy.clone()),
            MarkerState::Reuse => out.reuse.push(copy.clone()),
            MarkerState::Recreate => out.recreate.push(copy.clone()),
            MarkerState::Foreign => {}
        }
    }
    let wanted: BTreeSet<(&str, &str, &str)> = desired
        .iter()
        .map(|(copy, _)| {
            (
                copy.service.as_str(),
                copy.source.as_str(),
                copy.target.as_str(),
            )
        })
        .collect();
    let mut seen = BTreeSet::new();
    for copy in previous {
        let key = (
            copy.service.as_str(),
            copy.source.as_str(),
            copy.target.as_str(),
        );
        if wanted.contains(&key) {
            continue;
        }
        if seen.insert((
            copy.service.clone(),
            copy.source.clone(),
            copy.target.clone(),
        )) {
            out.keep.push(copy.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Talking to the source instance
// ---------------------------------------------------------------------------

fn pg_user(container: &Container) -> &str {
    container
        .env_value("POSTGRES_USER")
        .filter(|user| !user.is_empty())
        .unwrap_or("postgres")
}

fn mysql_password(container: &Container) -> Option<&str> {
    container
        .env_value("MYSQL_ROOT_PASSWORD")
        .or_else(|| container.env_value("MARIADB_ROOT_PASSWORD"))
        .filter(|password| !password.is_empty())
}

fn mysql_env(container: &Container) -> Vec<(&str, &str)> {
    mysql_password(container)
        .map(|password| vec![("MYSQL_PWD", password)])
        .unwrap_or_default()
}

fn pg_env(container: &Container) -> Vec<(&str, &str)> {
    container
        .env_value("POSTGRES_PASSWORD")
        .filter(|password| !password.is_empty())
        .map(|password| vec![("PGPASSWORD", password)])
        .unwrap_or_default()
}

fn pg_query(container: &Container, user: &str, database: &str, sql: &str) -> Result<String> {
    docker::exec_env(
        container.name(),
        &pg_env(container),
        &[
            "psql",
            "-qtAX",
            "-F",
            "\t",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            user,
            "-d",
            database,
            "-c",
            sql,
        ],
    )
}

fn pg_exec(container: &Container, user: &str, database: &str, sql: &str) -> Result<()> {
    pg_query(container, user, database, sql).map(drop)
}

fn mysql_exec(container: &Container, sql: &str) -> Result<()> {
    docker::exec_env(
        container.name(),
        &mysql_env(container),
        &[
            "sh",
            "-c",
            "cli=$(command -v mysql || command -v mariadb); \"$cli\" -N -B -e \"$1\"",
            "wt",
            sql,
        ],
    )
    .map(drop)
}

fn mysql_query(container: &Container, database: &str, sql: &str) -> Result<String> {
    docker::exec_env(
        container.name(),
        &mysql_env(container),
        &[
            "sh",
            "-c",
            "cli=$(command -v mysql || command -v mariadb); \"$cli\" -N -B \"$1\" -e \"$2\"",
            "wt",
            database,
            sql,
        ],
    )
}

fn mysql_version(container: &Container) -> Result<String> {
    docker::exec(
        container.name(),
        &[
            "sh",
            "-c",
            "mysql --version 2>/dev/null || mariadb --version",
        ],
    )
    .map(|out| out.trim().to_string())
}

fn pg_state(container: &Container, target: &str, project: &str, slug: &str) -> Result<MarkerState> {
    let user = pg_user(container);
    let sql = format!(
        "SELECT datname, coalesce(shobj_description(oid, 'pg_database'), '') FROM pg_database WHERE datname = {}",
        sql_lit(target)
    );
    let out = pg_query(container, user, "postgres", &sql)?;
    let line = out.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return Ok(MarkerState::Missing);
    }
    let (_, comment) = line.split_once('\t').unwrap_or((line, ""));
    Ok(classify_marker(Some(comment), project, slug))
}

fn mysql_exists(container: &Container, database: &str) -> Result<bool> {
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = {}",
        mysql_lit(database)
    );
    Ok(mysql_query(container, "information_schema", &sql)?.trim() == "1")
}

/// The source database's charset and collation, so the copy keeps the same
/// defaults for tables the app creates later.
fn mysql_metadata(container: &Container, source: &str) -> Result<(String, String)> {
    let sql = format!(
        "SELECT default_character_set_name, default_collation_name FROM information_schema.schemata WHERE schema_name = {}",
        mysql_lit(source)
    );
    let out = mysql_query(container, "information_schema", &sql)?;
    let line = out.trim_end_matches(['\n', '\r']);
    let (charset, collation) = line.split_once('\t').with_context(|| {
        format!(
            "source database {source} not found in {}; set services.mysql.databases",
            container.name()
        )
    })?;
    Ok((charset.to_string(), collation.to_string()))
}

fn mysql_rows(container: &Container, database: &str) -> Result<Option<BTreeMap<String, String>>> {
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = {} AND table_name = '__wt_meta'",
        mysql_lit(database)
    );
    if mysql_query(container, "information_schema", &sql)?.trim() != "1" {
        return Ok(None);
    }
    let sql = format!("SELECT k, v FROM {}.__wt_meta", sql_backtick(database));
    let out = mysql_query(container, database, &sql)?;
    let rows = out
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Ok(Some(rows))
}

fn mysql_state(
    container: &Container,
    target: &str,
    project: &str,
    slug: &str,
) -> Result<MarkerState> {
    if !mysql_exists(container, target)? {
        return Ok(MarkerState::Missing);
    }
    let rows = mysql_rows(container, target)?;
    Ok(classify_mysql_marker(rows.as_ref(), project, slug))
}

/// The marker state of an existing copy, for `down` and `ls`.
pub fn state_of(
    copy: &DatabaseCopy,
    container: &Container,
    project: &str,
    slug: &str,
) -> Result<MarkerState> {
    match service_kind(copy)? {
        "postgres" => pg_state(container, &copy.target, project, slug),
        "mysql" => mysql_state(container, &copy.target, project, slug),
        other => bail!("unknown service {other:?} in the state file"),
    }
}

fn service_kind(copy: &DatabaseCopy) -> Result<&str> {
    match copy.service.as_str() {
        "postgres" | "mysql" => Ok(copy.service.as_str()),
        other => bail!("unknown service {other:?} in the state file"),
    }
}

/// Whether the copy still exists, for `ls`.
pub fn exists(copy: &DatabaseCopy, container: &Container) -> Result<bool> {
    match service_kind(copy)? {
        "postgres" => {
            let user = pg_user(container);
            let sql = format!(
                "SELECT COUNT(*) FROM pg_database WHERE datname = {}",
                sql_lit(&copy.target)
            );
            Ok(pg_query(container, user, "postgres", &sql)?.trim() == "1")
        }
        _ => mysql_exists(container, &copy.target),
    }
}

/// The source databases: the configured list, or the one the image's own env
/// names. All of them are copied.
pub fn source_databases(svc: &Service, container: &Container) -> Result<Vec<String>> {
    if !svc.databases.is_empty() {
        return Ok(svc.databases.clone());
    }
    let found = match svc.kind {
        crate::services::Kind::Postgres => container
            .env_value("POSTGRES_DB")
            .filter(|db| !db.is_empty())
            .or_else(|| {
                container
                    .env_value("POSTGRES_USER")
                    .filter(|db| !db.is_empty())
            })
            .unwrap_or("postgres")
            .to_string(),
        crate::services::Kind::Mysql => container
            .env_value("MYSQL_DATABASE")
            .filter(|db| !db.is_empty())
            .or_else(|| {
                container
                    .env_value("MARIADB_DATABASE")
                    .filter(|db| !db.is_empty())
            })
            .map(String::from)
            .with_context(|| {
                format!(
                    "cannot tell which database of {} to copy; set services.mysql.databases",
                    container.name()
                )
            })?,
        crate::services::Kind::Redis => unreachable!("redis is refused in database mode"),
    };
    Ok(vec![found])
}

// ---------------------------------------------------------------------------
// Planning and copying
// ---------------------------------------------------------------------------

/// Classifies every target before anything is created or written to the state:
/// a refused run must leave no worktree, state file or database behind.
pub fn plan(
    svc: &Service,
    src: &ContainerSource,
    env: &Env,
) -> Result<Vec<(DatabaseCopy, MarkerState)>> {
    if !src.container.state.running {
        bail!(
            "source container {} is not running; run: docker start {}",
            src.container.name(),
            src.container.name()
        );
    }
    let databases = source_databases(svc, &src.container)?;
    let targets = targets(&env.project, &env.slug, &databases)?;
    let mut out = Vec::new();
    for (source_db, target) in targets {
        // A typo or a renamed source must fail here, before the checkout and
        // the state exist, not inside the copy after both were written.
        match svc.kind {
            crate::services::Kind::Postgres => {
                pg_metadata(&src.container, pg_user(&src.container), &source_db)?;
            }
            crate::services::Kind::Mysql => {
                mysql_metadata(&src.container, &source_db)?;
            }
            crate::services::Kind::Redis => unreachable!(),
        }
        let state = match svc.kind {
            crate::services::Kind::Postgres => {
                pg_state(&src.container, &target, &env.project, &env.slug)?
            }
            crate::services::Kind::Mysql => {
                mysql_state(&src.container, &target, &env.project, &env.slug)?
            }
            crate::services::Kind::Redis => unreachable!(),
        };
        if state == MarkerState::Foreign {
            bail!(
                "{target} was not created by wt for {}/{}; drop it by hand before retrying",
                env.project,
                env.slug
            );
        }
        out.push((
            DatabaseCopy {
                service: svc.kind.name().to_string(),
                source: src.container.name().to_string(),
                source_db,
                target,
            },
            state,
        ));
    }
    Ok(out)
}

/// Creates the missing copies, copies the requested data and reuses the
/// finished ones. A half-copied target is dropped and copied again.
pub fn prepare(
    svc: &Service,
    src: &ContainerSource,
    env: &Env,
    planned: &[(DatabaseCopy, MarkerState)],
) -> Result<()> {
    for (copy, state) in planned {
        match state {
            MarkerState::Reuse => {
                // Idempotent: repairs a grant that failed after the copy was
                // finished, and keeps the app user able to reach the copy.
                if svc.kind == crate::services::Kind::Mysql {
                    grant_mysql(&src.container, &copy.target)?;
                }
                println!("Reusing {}", copy.target);
            }
            MarkerState::Recreate => {
                println!("Recreating {}", copy.target);
                drop_copy(copy, &src.container)?;
                create(svc, src, env, copy)?;
            }
            MarkerState::Missing => create(svc, src, env, copy)?,
            MarkerState::Foreign => bail!(
                "{} was not created by wt for {}/{}",
                copy.target,
                env.project,
                env.slug
            ),
        }
    }
    Ok(())
}

fn create(svc: &Service, src: &ContainerSource, env: &Env, copy: &DatabaseCopy) -> Result<()> {
    match svc.kind {
        crate::services::Kind::Postgres => create_postgres(svc, src, env, copy),
        crate::services::Kind::Mysql => create_mysql(svc, src, env, copy),
        crate::services::Kind::Redis => unreachable!(),
    }
}

fn create_postgres(
    svc: &Service,
    src: &ContainerSource,
    env: &Env,
    copy: &DatabaseCopy,
) -> Result<()> {
    let container = &src.container;
    let user = pg_user(container);
    let (owner, encoding, collate, ctype) = pg_metadata(container, user, &copy.source_db)?;
    let create = format!(
        "CREATE DATABASE {} WITH TEMPLATE template0 OWNER {} ENCODING {} LC_COLLATE {} LC_CTYPE {}",
        sql_ident(&copy.target),
        sql_ident(&owner),
        sql_lit(&encoding),
        sql_lit(&collate),
        sql_lit(&ctype)
    );
    pg_exec(container, user, "postgres", &create)?;
    mark_complete(container, copy, env, false)?;
    if svc.clone {
        let script = pg_copy_script(&copy.source_db, &copy.target, user);
        docker::exec_env(
            container.name(),
            &pg_env(container),
            &["sh", "-euc", &script],
        )
        .with_context(|| format!("copying {} into {}", copy.source_db, copy.target))?;
    }
    mark_complete(container, copy, env, true)?;
    println!(
        "{} {} into {}",
        if svc.clone { "Copied" } else { "Created" },
        copy.source_db,
        copy.target
    );
    Ok(())
}

fn pg_metadata(
    container: &Container,
    user: &str,
    source: &str,
) -> Result<(String, String, String, String)> {
    // `datlocprovider` exists from Postgres 15. ICU and builtin databases do
    // not accept plain LC_COLLATE/LC_CTYPE on CREATE DATABASE, so refuse them
    // instead of failing after a Recreate has already dropped the copy.
    let modern = format!(
        "SELECT pg_get_userbyid(datdba), pg_encoding_to_char(encoding), datcollate, datctype, datlocprovider FROM pg_database WHERE datname = {}",
        sql_lit(source)
    );
    if let Ok(out) = pg_query(container, user, "postgres", &modern) {
        let line = out.trim_end_matches(['\n', '\r']);
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() == 5 {
            if fields[4] != "c" {
                bail!(
                    "source database {source} uses locale provider {:?}; wt copies only libc (LC_COLLATE/LC_CTYPE) databases",
                    fields[4]
                );
            }
            return Ok((
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
                fields[3].to_string(),
            ));
        }
    }
    let legacy = format!(
        "SELECT pg_get_userbyid(datdba), pg_encoding_to_char(encoding), datcollate, datctype FROM pg_database WHERE datname = {}",
        sql_lit(source)
    );
    let out = pg_query(container, user, "postgres", &legacy)
        .with_context(|| format!("could not read {source} from {}", container.name()))?;
    let line = out.trim_end_matches(['\n', '\r']);
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 4 {
        bail!(
            "source database {source} not found in {}; set services.postgres.databases",
            container.name()
        );
    }
    Ok((
        fields[0].to_string(),
        fields[1].to_string(),
        fields[2].to_string(),
        fields[3].to_string(),
    ))
}

fn create_mysql(
    svc: &Service,
    src: &ContainerSource,
    env: &Env,
    copy: &DatabaseCopy,
) -> Result<()> {
    let container = &src.container;
    let target = sql_backtick(&copy.target);
    let (charset, collation) = mysql_metadata(container, &copy.source_db)?;
    mysql_exec(
        container,
        &format!("CREATE DATABASE {target} CHARACTER SET {charset} COLLATE {collation}"),
    )?;
    mysql_marker(container, env, copy, false)?;
    if svc.clone {
        let version = mysql_version(container)?;
        let script = mysql_copy_script(&copy.source_db, &copy.target, &version);
        docker::exec_env(
            container.name(),
            &mysql_env(container),
            &["sh", "-euc", &script],
        )
        .with_context(|| format!("copying {} into {}", copy.source_db, copy.target))?;
    }
    // Rewritten after the restore: a source `__wt_meta` table would come with
    // the dump and replace the marker created above.
    mysql_marker(container, env, copy, true)?;
    grant_mysql(container, &copy.target)?;
    println!(
        "{} {} into {}",
        if svc.clone { "Copied" } else { "Created" },
        copy.source_db,
        copy.target
    );
    Ok(())
}

/// Writes (or rewrites) the MySQL ownership marker. `__wt_meta` is reserved.
fn mysql_marker(
    container: &Container,
    env: &Env,
    copy: &DatabaseCopy,
    complete: bool,
) -> Result<()> {
    let target = sql_backtick(&copy.target);
    mysql_exec(
        container,
        &format!("DROP TABLE IF EXISTS {target}.__wt_meta"),
    )?;
    mysql_exec(
        container,
        &format!(
            "CREATE TABLE {target}.__wt_meta (k VARCHAR(64) PRIMARY KEY, v VARCHAR(255) NOT NULL)"
        ),
    )?;
    mysql_exec(
        container,
        &format!(
            "INSERT INTO {target}.__wt_meta (k, v) VALUES ('wt.project', {}), ('wt.slug', {}), ('wt.complete', {})",
            mysql_lit(&env.project),
            mysql_lit(&env.slug),
            mysql_lit(if complete { "1" } else { "0" })
        ),
    )
}

fn grant_mysql(container: &Container, target: &str) -> Result<()> {
    let user = container
        .env_value("MYSQL_USER")
        .or_else(|| container.env_value("MARIADB_USER"))
        .filter(|user| !user.is_empty() && *user != "root");
    let Some(user) = user else { return Ok(()) };
    mysql_exec(
        container,
        &format!(
            "GRANT ALL PRIVILEGES ON {}.* TO {}@'%'",
            sql_backtick(target),
            mysql_lit(user)
        ),
    )
}

/// Marks a Postgres copy complete (or incomplete) by rewriting its comment.
fn mark_complete(
    container: &Container,
    copy: &DatabaseCopy,
    env: &Env,
    complete: bool,
) -> Result<()> {
    let user = pg_user(container);
    pg_exec(
        container,
        user,
        "postgres",
        &format!(
            "COMMENT ON DATABASE {} IS {}",
            sql_ident(&copy.target),
            sql_lit(&pg_marker(&env.project, &env.slug, complete))
        ),
    )
}

/// Drops one copy. The caller has already decided that dropping is allowed.
pub fn drop_copy(copy: &DatabaseCopy, container: &Container) -> Result<()> {
    check_not_self(&copy.source_db, &copy.target)?;
    match service_kind(copy)? {
        "postgres" => {
            let user = pg_user(container);
            pg_exec(
                container,
                user,
                "postgres",
                &format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = {}",
                    sql_lit(&copy.target)
                ),
            )?;
            pg_exec(
                container,
                user,
                "postgres",
                &format!("DROP DATABASE {}", sql_ident(&copy.target)),
            )
        }
        _ => mysql_exec(
            container,
            &format!("DROP DATABASE {}", sql_backtick(&copy.target)),
        ),
    }
}

/// Proves the copy is readable: the check query on the primary, or `select 1`
/// on every copy.
pub fn verify(svc: &Service, src: &ContainerSource, target: &str) -> Result<()> {
    let container = &src.container;
    match svc.kind {
        crate::services::Kind::Postgres => {
            let user = pg_user(container);
            let sql = svc.check_sql.as_deref().unwrap_or("select 1");
            let out = pg_query(container, user, target, sql)
                .with_context(|| format!("checking {target}"))?;
            if svc.check_sql.is_none() && out.trim() != "1" {
                bail!("{target} answered {out:?} instead of 1");
            }
        }
        crate::services::Kind::Mysql => {
            mysql_query(container, target, "select 1")
                .with_context(|| format!("checking {target}"))?;
        }
        crate::services::Kind::Redis => unreachable!(),
    }
    Ok(())
}

/// Drops the copies the state records, with the source container in hand.
/// Returns false when it had to leave the state alone (Docker down, source
/// stopped or gone), so the caller can keep the registry for later.
pub fn drop_all(
    copies: &[DatabaseCopy],
    env: &Env,
    force: bool,
    protected: &BTreeSet<String>,
) -> Result<bool> {
    if copies.is_empty() {
        return Ok(true);
    }
    for copy in copies {
        // A tampered state must never make `down` drop a source database.
        if copy.source_db == copy.target
            || protected.contains(&copy.target)
            || copies.iter().any(|other| other.source_db == copy.target)
        {
            bail!(
                "refusing to drop {}: it is the source database",
                copy.target
            );
        }
        // A container that is gone may come back with its volume, and the
        // copies with it; keep the registry so a later run can finish.
        let Some(container) = docker::container(&copy.source) else {
            println!("Keeping {}: {} is gone", copy.target, copy.source);
            return Ok(false);
        };
        if !container.state.running {
            if !force {
                bail!(
                    "{} is not running; run: docker start {}",
                    copy.source,
                    copy.source
                );
            }
            println!("Keeping {}: {} is not running", copy.target, copy.source);
            return Ok(false);
        }
        match state_of(copy, &container, &env.project, &env.slug)? {
            MarkerState::Missing => {}
            MarkerState::Foreign if !force => bail!(
                "{} is not tagged for {}/{}; pass --force if it is really yours",
                copy.target,
                env.project,
                env.slug
            ),
            _ => {
                drop_copy(copy, &container)?;
                println!("Dropped database {}", copy.target);
            }
        }
    }
    Ok(true)
}

/// The primary copy, the one `{database}` and `check_sql` use.
pub fn primary(copies: &[DatabaseCopy]) -> Option<&DatabaseCopy> {
    copies.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(service: &str, source_db: &str, target: &str) -> DatabaseCopy {
        DatabaseCopy {
            service: service.into(),
            source: "src".into(),
            source_db: source_db.into(),
            target: target.into(),
        }
    }

    #[test]
    fn target_name_is_namespaced() {
        assert_eq!(
            target_name("wte2e", "feat-x", "app"),
            "wt_wte2e_feat_x__app"
        );
    }

    #[test]
    fn target_name_sanitizes() {
        assert_eq!(
            target_name("wte2e", "feat-x", "My-App.cache v2"),
            "wt_wte2e_feat_x__my_app_cache_v2"
        );
        assert_eq!(target_name("P_A", "feat-x", "a"), "wt_p_a_feat_x__a");
    }

    #[test]
    fn long_names_are_capped_and_stable() {
        let a = "a".repeat(80);
        let b = format!("{}b", "a".repeat(79));
        let first = target_name("wte2e", "feat-x", &a);
        let second = target_name("wte2e", "feat-x", &b);
        assert!(first.len() <= MAX_NAME, "{}", first.len());
        assert!(second.len() <= MAX_NAME, "{}", second.len());
        assert_eq!(first.len(), MAX_NAME);
        assert_eq!(second.len(), MAX_NAME);
        assert_ne!(first, second);
        assert!(first.starts_with("wt_"));
        assert_eq!(target_name("wte2e", "feat-x", &a), first);
    }

    #[test]
    fn names_do_not_depend_on_list_order() {
        let dbs = vec!["b".to_string(), "a".to_string()];
        let mut sorted = dbs.clone();
        sorted.sort();
        let one: BTreeMap<_, _> = targets("p", "s", &dbs).unwrap().into_iter().collect();
        let two: BTreeMap<_, _> = targets("p", "s", &sorted).unwrap().into_iter().collect();
        assert_eq!(one, two);
    }

    #[test]
    fn colliding_targets_are_refused() {
        let err = format!(
            "{:#}",
            targets("p", "s", &["app-db".into(), "app.db".into()]).unwrap_err()
        );
        assert!(err.contains("app-db") && err.contains("app.db"), "{err}");
        assert!(err.contains("wt_p_s__app_db"), "{err}");
        let err = format!(
            "{:#}",
            targets("p", "s", &["a;b".into(), "a b".into()]).unwrap_err()
        );
        assert!(err.contains("wt_p_s__a_b"), "{err}");
    }

    #[test]
    fn refuses_to_overwrite_the_source_database() {
        assert!(format!("{:#}", check_not_self("app", "app").unwrap_err()).contains("onto itself"));
        assert!(
            format!(
                "{:#}",
                check_not_self("wt_p_s__a", "wt_p_s__a").unwrap_err()
            )
            .contains("onto itself")
        );
        assert!(check_not_self("app", "wt_p_s__app").is_ok());
    }

    #[test]
    fn pg_marker_round_trips() {
        assert_eq!(
            pg_marker("wte2e", "feat-x", false),
            "wt.project=wte2e;wt.slug=feat-x;wt.complete=0"
        );
        assert_eq!(
            pg_marker("wte2e", "feat-x", true),
            "wt.project=wte2e;wt.slug=feat-x;wt.complete=1"
        );
        assert_eq!(
            parse_pg_marker(&pg_marker("wte2e", "feat-x", true)),
            Some(Marker {
                project: "wte2e".into(),
                slug: "feat-x".into(),
                complete: true
            })
        );
    }

    #[test]
    fn pg_marker_classification() {
        assert_eq!(
            classify_marker(None, "wte2e", "feat-x"),
            MarkerState::Foreign
        );
        assert_eq!(
            classify_marker(Some("owned by the app"), "wte2e", "feat-x"),
            MarkerState::Foreign
        );
        assert_eq!(
            classify_marker(
                Some("wt.project=other;wt.slug=feat-x;wt.complete=1"),
                "wte2e",
                "feat-x"
            ),
            MarkerState::Foreign
        );
        assert_eq!(
            classify_marker(
                Some("wt.project=wte2e;wt.slug=feat-x;wt.complete=0"),
                "wte2e",
                "feat-x"
            ),
            MarkerState::Recreate
        );
        assert_eq!(
            classify_marker(
                Some("wt.project=wte2e;wt.slug=feat-x;wt.complete=1"),
                "wte2e",
                "feat-x"
            ),
            MarkerState::Reuse
        );
    }

    #[test]
    fn mysql_marker_classification() {
        let rows = BTreeMap::from([
            ("wt.project".to_string(), "wte2e".to_string()),
            ("wt.slug".to_string(), "feat-x".to_string()),
            ("wt.complete".to_string(), "1".to_string()),
        ]);
        assert_eq!(
            classify_mysql_marker(Some(&rows), "wte2e", "feat-x"),
            MarkerState::Reuse
        );
        assert_eq!(
            classify_mysql_marker(None, "wte2e", "feat-x"),
            MarkerState::Foreign
        );
        let mut extra = rows.clone();
        extra.insert("other".into(), "x".into());
        assert_eq!(
            classify_mysql_marker(Some(&extra), "wte2e", "feat-x"),
            MarkerState::Reuse
        );
        let mut incomplete = rows;
        incomplete.insert("wt.complete".into(), "0".into());
        assert_eq!(
            classify_mysql_marker(Some(&incomplete), "wte2e", "feat-x"),
            MarkerState::Recreate
        );
    }

    #[test]
    fn env_placeholders_resolve_to_targets() {
        let values = vec![
            ("U".to_string(), "pg://h:{port}/{database}".to_string()),
            (
                "C".to_string(),
                "pg://h:{port}/{database:cache}".to_string(),
            ),
        ];
        let targets = vec![
            ("app".to_string(), "wt_p_s__app".to_string()),
            ("cache".to_string(), "wt_p_s__cache".to_string()),
        ];
        assert_eq!(
            render_env(&values, &targets, 5432).unwrap(),
            vec![
                ("U".to_string(), "pg://h:5432/wt_p_s__app".to_string()),
                ("C".to_string(), "pg://h:5432/wt_p_s__cache".to_string()),
            ]
        );
    }

    #[test]
    fn refuses_names_without_usable_characters() {
        assert!(targets("p", "s", &["日本".to_string()]).is_err());
        assert!(targets("p", "s", &[String::new()]).is_err());
    }

    #[test]
    fn refuses_a_target_that_is_another_source() {
        let err = format!(
            "{:#}",
            targets("p", "s", &["app".to_string(), "wt_p_s__app".to_string()]).unwrap_err()
        );
        assert!(err.contains("also a source database"), "{err}");
    }

    #[test]
    fn changing_the_source_keeps_the_old_copy() {
        let old = DatabaseCopy {
            service: "postgres".into(),
            source: "old".into(),
            source_db: "app".into(),
            target: "wt_p_s__app".into(),
        };
        let new = DatabaseCopy {
            source: "new".into(),
            ..old.clone()
        };
        let reconciled = reconcile(std::slice::from_ref(&old), &[(new, MarkerState::Reuse)]);
        assert_eq!(reconciled.keep, vec![old]);
    }

    #[test]
    fn first_database_is_the_primary() {
        let targets = targets("p", "s", &["cache".into(), "app".into()]).unwrap();
        let copies: Vec<DatabaseCopy> = targets
            .into_iter()
            .map(|(source_db, target)| db("postgres", &source_db, &target))
            .collect();
        assert_eq!(primary(&copies).unwrap().target, "wt_p_s__cache");
    }

    #[test]
    fn copy_script_writes_a_file_and_propagates_failure() {
        let script = pg_copy_script("app", "wt_p_s__app", "app");
        assert!(!script.contains("| psql"), "{script}");
        assert!(!script.contains("pipefail"), "{script}");
        assert!(script.matches(DUMP_FILE).count() >= 2, "{script}");
        assert!(script.contains("trap"), "{script}");
        assert!(script.contains("&&"), "{script}");

        let script = mysql_copy_script("app", "wt_p_s__app", "mysql  Ver 8.0.36");
        assert!(!script.contains("| mysql"), "{script}");
        assert!(!script.contains("pipefail"), "{script}");
        assert!(script.matches(DUMP_FILE).count() >= 2, "{script}");
        assert!(script.contains("trap"), "{script}");
        assert!(script.contains("&&"), "{script}");
    }

    #[test]
    fn gtid_flag_only_for_mysql() {
        let mariadb = mysql_dump_args("mysql  Ver 15.1 Distrib 10.11.6-MariaDB");
        assert!(!mariadb.iter().any(|a| a.contains("set-gtid-purged")));
        let mysql = mysql_dump_args("mysql  Ver 8.0.36 for Linux");
        assert!(mysql.iter().any(|a| a == "--set-gtid-purged=OFF"));
        for args in [mariadb, mysql] {
            for flag in [
                "--single-transaction",
                "--quick",
                "--routines",
                "--triggers",
                "--events",
            ] {
                assert!(args.iter().any(|a| a == flag), "{args:?}");
            }
        }
    }

    #[test]
    fn reconcile_keeps_and_recreates() {
        let app = db("postgres", "app", "wt_p_s__app");
        let cache = db("postgres", "cache", "wt_p_s__cache");

        let empty = reconcile(
            &[],
            &[
                (app.clone(), MarkerState::Missing),
                (cache.clone(), MarkerState::Missing),
            ],
        );
        assert_eq!(empty.create.len(), 2);

        let done = reconcile(
            &[app.clone(), cache.clone()],
            &[
                (app.clone(), MarkerState::Reuse),
                (cache.clone(), MarkerState::Reuse),
            ],
        );
        assert_eq!(done.reuse.len(), 2);
        assert!(done.keep.is_empty());

        let mixed = reconcile(
            &[app.clone(), cache.clone()],
            &[
                (app.clone(), MarkerState::Reuse),
                (cache.clone(), MarkerState::Recreate),
            ],
        );
        assert_eq!(mixed.reuse, vec![app.clone()]);
        assert_eq!(mixed.recreate, vec![cache.clone()]);

        let removed = reconcile(
            &[app.clone(), cache.clone()],
            &[(app.clone(), MarkerState::Reuse)],
        );
        assert_eq!(removed.keep, vec![cache]);
        assert!(removed.recreate.is_empty());

        let duplicated = reconcile(&[app.clone(), app.clone()], &[(app, MarkerState::Reuse)]);
        assert!(duplicated.keep.is_empty());
    }
}
