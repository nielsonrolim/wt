//! `mode = "database"`: copies logical databases inside the source instance,
//! with no container and no volume of the worktree's own. The database comment
//! (Postgres) or the `__wt_meta` table (MySQL) is the proof of ownership, the
//! same job the Docker labels do for containers and volumes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail};

use crate::config::{Service, map_placeholders};
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
    /// Its marker names another project or slug: never dropped without --force.
    Foreign,
    /// It exists with no marker at all. App tasks such as `db:reset` erase the
    /// marker, so this is ours only when the state also lists the target.
    Unmarked,
    /// Unmarked but registered, and it holds tables: keep it, restore the marker.
    Adopt,
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
        Some(_) => MarkerState::Foreign,
        // An empty or unrelated comment is not a claim by anyone else.
        None => MarkerState::Unmarked,
    }
}

/// The `__wt_meta` rows are the label of a MySQL copy. A missing table is
/// `None`, which is unmarked; an extra key is ignored.
pub fn classify_mysql_marker(
    rows: Option<&BTreeMap<String, String>>,
    project: &str,
    slug: &str,
) -> MarkerState {
    let Some(rows) = rows else {
        return MarkerState::Unmarked;
    };
    let (found_project, found_slug) = (rows.get("wt.project"), rows.get("wt.slug"));
    if found_project.is_some_and(|p| p != project) || found_slug.is_some_and(|s| s != slug) {
        return MarkerState::Foreign;
    }
    match (found_project, found_slug, rows.get("wt.complete")) {
        (Some(_), Some(_), Some(c)) => {
            if c == "1" {
                MarkerState::Reuse
            } else {
                MarkerState::Recreate
            }
        }
        _ => MarkerState::Unmarked,
    }
}

/// Whether the state lists exactly this copy. Together with `plan`, which never
/// lets a foreign target reach the state, that makes an unmarked target ours.
pub fn is_registered(previous: &[DatabaseCopy], copy: &DatabaseCopy) -> bool {
    previous.contains(copy)
}

/// A state file can be stale or edited, so an unmarked target also has to carry
/// the name wt would give this source database in this worktree.
pub fn has_own_name(copy: &DatabaseCopy, project: &str, slug: &str) -> bool {
    target_name(project, slug, &copy.source_db) == copy.target
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
///
/// `docker exec` does not end with its client, so a Ctrl-C leaves this running.
/// The script records its pid for `stop_script`, and runs each step in the
/// background so a signal reaches the `wait` and the trap ends the step too.
fn copy_script(target: &str, setup: &str, dump: &str, restore: &str) -> String {
    let file = dump_file(target);
    format!(
        "f={file}\necho $$ > \"$f.pid\"\nchild=\ntrap 'if [ -n \"$child\" ]; then kill \"$child\" 2>/dev/null || true; fi; rm -f \"$f\" \"$f.pid\"' 0\ntrap 'exit 143' 1 2 15\n{setup}{dump} > \"$f\" &\nchild=$!\nwait \"$child\"\n{restore} < \"$f\" &\nchild=$!\nwait \"$child\"\nchild=\n"
    )
}

pub fn pg_copy_script(origin: &str, target: &str, user: &str) -> String {
    let user = shell_quote(user);
    copy_script(
        target,
        "",
        &format!("pg_dump -U {user} -d {}", shell_quote(origin)),
        &format!(
            "psql -v ON_ERROR_STOP=1 -U {user} -d {}",
            shell_quote(target)
        ),
    )
}

/// Ends a copy script a previous run left behind and waits until it is gone:
/// it would otherwise delete the new run's dump or restore into the new target.
/// The command line is checked because a stale pid file may name a reused pid.
pub fn stop_script(target: &str) -> String {
    let file = dump_file(target);
    format!(
        r#"f={file}
p=$(cat "$f.pid" 2>/dev/null || true)
[ -n "$p" ] || exit 0
i=0
while grep -qF "$f" "/proc/$p/cmdline" 2>/dev/null; do
  [ "$i" -lt 30 ] || {{ echo "copy script $p did not stop" >&2; exit 1; }}
  kill -TERM "$p" 2>/dev/null || true
  i=$((i+1))
  sleep 1
done
rm -f "$f" "$f.pid"
"#
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
    copy_script(
        target,
        "dump=$(command -v mysqldump || command -v mariadb-dump)\ncli=$(command -v mysql || command -v mariadb)\n",
        // Never the source's `__wt_meta`: restoring it would replace the target's
        // `complete=0`, and an interrupted restore would pass for a reset copy.
        &format!(
            "\"$dump\" -uroot {args} {} {}",
            shell_quote(&format!("--ignore-table={origin}.__wt_meta")),
            shell_quote(origin)
        ),
        &format!("\"$cli\" -uroot {}", shell_quote(target)),
    )
}

/// Replaces `{port}` and, in database mode, `{database}` and
/// `{database:<source>}`. Config validation already rejected anything else, and
/// reads a value with the same tokenizer, so other braces stay literal here.
pub fn render_env(
    values: &[(String, String)],
    targets: &[(String, String)],
    port: u16,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, value) in values {
        let mut unresolved = None;
        let text = map_placeholders(value, |token| {
            let found = match token {
                "port" => return Some(port.to_string()),
                "database" => targets.first().map(|(_, target)| target.clone()),
                _ => {
                    let source = token.strip_prefix("database:")?;
                    targets
                        .iter()
                        .find(|(name, _)| name == source)
                        .map(|(_, target)| target.clone())
                }
            };
            if found.is_none() {
                unresolved = Some(token.to_string());
            }
            found
        });
        if let Some(token) = unresolved {
            bail!("services.env value {key} has an unresolved {{{token}}} placeholder");
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
            MarkerState::Reuse | MarkerState::Adopt => out.reuse.push(copy.clone()),
            MarkerState::Recreate => out.recreate.push(copy.clone()),
            MarkerState::Foreign | MarkerState::Unmarked => {}
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
    pg_session(container, user, database, &[sql])
}

/// One psql session running each statement on its own, outside a transaction,
/// which is what `CREATE DATABASE` and `DROP DATABASE` require.
fn pg_session(
    container: &Container,
    user: &str,
    database: &str,
    statements: &[&str],
) -> Result<String> {
    let mut argv = vec![
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
    ];
    for sql in statements {
        argv.extend(["-c", sql]);
    }
    docker::exec_env(container.name(), &pg_env(container), &argv)
}

fn pg_exec(container: &Container, user: &str, database: &str, sql: &str) -> Result<()> {
    pg_query(container, user, database, sql).map(drop)
}

/// `-uroot` because the env holds root's password: without it the client logs
/// in as the exec user, which fails on a source that sets `user:`.
fn mysql_exec(container: &Container, sql: &str) -> Result<()> {
    docker::exec_env(
        container.name(),
        &mysql_env(container),
        &[
            "sh",
            "-c",
            "cli=$(command -v mysql || command -v mariadb); \"$cli\" -uroot -N -B -e \"$1\"",
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
            "cli=$(command -v mysql || command -v mariadb); \"$cli\" -uroot -N -B \"$1\" -e \"$2\"",
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

/// Whether the target holds anything an app put there. `__wt_meta` does not count.
fn has_tables(svc: &Service, container: &Container, target: &str) -> Result<bool> {
    let count = match svc.kind {
        crate::services::Kind::Postgres => pg_query(
            container,
            pg_user(container),
            target,
            "SELECT COUNT(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%'",
        )?,
        crate::services::Kind::Mysql => mysql_query(
            container,
            "information_schema",
            &format!(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = {} AND table_name <> '__wt_meta'",
                mysql_lit(target)
            ),
        )?,
        crate::services::Kind::Redis => unreachable!(),
    };
    Ok(count.trim() != "0")
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
    previous: &[DatabaseCopy],
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
        let copy = DatabaseCopy {
            service: svc.kind.name().to_string(),
            source: src.container.name().to_string(),
            source_db,
            target,
        };
        let state = match state {
            MarkerState::Unmarked if is_registered(previous, &copy) => {
                // An empty target is an interrupted create or a recreated
                // database: nothing to lose, and stamping it complete would
                // pass an empty copy off as a finished one.
                if has_tables(svc, &src.container, &copy.target)? {
                    MarkerState::Adopt
                } else {
                    MarkerState::Recreate
                }
            }
            MarkerState::Unmarked | MarkerState::Foreign => bail!(
                "{} was not created by wt for {}/{}; drop it by hand before retrying",
                copy.target,
                env.project,
                env.slug
            ),
            other => other,
        };
        out.push((copy, state));
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
            MarkerState::Adopt => {
                eprintln!(
                    "warning: {} lost its wt marker (a database reset does that); keeping its data and restoring the marker",
                    copy.target
                );
                match svc.kind {
                    crate::services::Kind::Postgres => {
                        mark_complete(&src.container, copy, env, true)?;
                    }
                    crate::services::Kind::Mysql => {
                        mysql_marker(&src.container, env, copy, true)?;
                        grant_mysql(&src.container, &copy.target)?;
                    }
                    crate::services::Kind::Redis => unreachable!(),
                }
                println!("Reusing {}", copy.target);
            }
            MarkerState::Missing => create(svc, src, env, copy)?,
            MarkerState::Foreign | MarkerState::Unmarked => bail!(
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
    // One session, so an interrupted `wt` cannot split the two: the session
    // finishes inside the container even after its client is gone.
    let comment = pg_comment_sql(copy, env, false);
    pg_session(container, user, "postgres", &[&create, &comment])?;
    if svc.clone {
        let script = pg_copy_script(&copy.source_db, &copy.target, user);
        run_copy(container, &pg_env(container), &copy.target, &script)
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
    // One client run, so an interrupted `wt` cannot split the database from its
    // marker: the client finishes inside the container after `wt` is gone.
    mysql_exec(
        container,
        &format!(
            "CREATE DATABASE {target} CHARACTER SET {charset} COLLATE {collation}; {}",
            mysql_marker_sql(env, copy, false)
        ),
    )?;
    if svc.clone {
        let version = mysql_version(container)?;
        let script = mysql_copy_script(&copy.source_db, &copy.target, &version);
        run_copy(container, &mysql_env(container), &copy.target, &script)
            .with_context(|| format!("copying {} into {}", copy.source_db, copy.target))?;
    }
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
    mysql_exec(container, &mysql_marker_sql(env, copy, complete))
}

fn mysql_marker_sql(env: &Env, copy: &DatabaseCopy, complete: bool) -> String {
    let target = sql_backtick(&copy.target);
    format!(
        "DROP TABLE IF EXISTS {target}.__wt_meta; CREATE TABLE {target}.__wt_meta (k VARCHAR(64) PRIMARY KEY, v VARCHAR(255) NOT NULL); INSERT INTO {target}.__wt_meta (k, v) VALUES ('wt.project', {}), ('wt.slug', {}), ('wt.complete', {})",
        mysql_lit(&env.project),
        mysql_lit(&env.slug),
        mysql_lit(if complete { "1" } else { "0" })
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
    pg_exec(
        container,
        pg_user(container),
        "postgres",
        &pg_comment_sql(copy, env, complete),
    )
}

fn pg_comment_sql(copy: &DatabaseCopy, env: &Env, complete: bool) -> String {
    format!(
        "COMMENT ON DATABASE {} IS {}",
        sql_ident(&copy.target),
        sql_lit(&pg_marker(&env.project, &env.slug, complete))
    )
}

/// Copy scripts in flight, for the Ctrl-C handler.
struct Copies {
    aborted: bool,
    active: Vec<(String, String)>,
}

static COPIES: Mutex<Copies> = Mutex::new(Copies {
    aborted: false,
    active: Vec::new(),
});

fn copies() -> MutexGuard<'static, Copies> {
    COPIES.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs a copy script inside the source, registered so Ctrl-C can end it.
fn run_copy(container: &Container, env: &[(&str, &str)], target: &str, script: &str) -> Result<()> {
    let entry = (container.name().to_string(), target.to_string());
    {
        let mut copies = copies();
        if copies.aborted {
            bail!("interrupted");
        }
        copies.active.push(entry.clone());
    }
    let result = docker::exec_env(container.name(), env, &["sh", "-euc", script]);
    copies().active.retain(|active| active != &entry);
    result.map(drop)
}

/// Ends the copy scripts of this run. The target keeps `complete=0`, so the next
/// `up` drops and copies it again; that drop also catches a script this missed.
pub fn abort_copies() {
    let active: Vec<(String, String)> = {
        let mut copies = copies();
        copies.aborted = true;
        copies.active.drain(..).collect()
    };
    for (container, target) in active {
        if let Err(error) = stop_copy_script(&container, &target) {
            eprintln!("error: could not stop the copy into {target}: {error:#}");
        }
    }
}

fn stop_copy_script(container: &str, target: &str) -> Result<()> {
    docker::exec(container, &["sh", "-c", &stop_script(target)]).map(drop)
}

/// `WITH (FORCE)` (Postgres 13+) ends the sessions and drops atomically, so a
/// client that reconnects cannot slip in between. Older servers get the two
/// steps, in one session to keep the gap as small as it can be.
pub fn pg_drop_statements(version_num: Option<u32>, target: &str) -> Vec<String> {
    let drop = format!("DROP DATABASE {}", sql_ident(target));
    if version_num.is_some_and(|version| version >= 130000) {
        return vec![format!("{drop} WITH (FORCE)")];
    }
    vec![
        format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = {}",
            sql_lit(target)
        ),
        drop,
    ]
}

/// Drops one copy. The caller has already decided that dropping is allowed.
pub fn drop_copy(copy: &DatabaseCopy, container: &Container) -> Result<()> {
    check_not_self(&copy.source_db, &copy.target)?;
    stop_copy_script(container.name(), &copy.target)?;
    match service_kind(copy)? {
        "postgres" => {
            let user = pg_user(container);
            let version = pg_query(container, user, "postgres", "SHOW server_version_num")
                .ok()
                .and_then(|out| out.trim().parse().ok());
            let statements = pg_drop_statements(version, &copy.target);
            let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
            pg_session(container, user, "postgres", &statements).map(drop)
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
/// Every copy is checked before the first drop, so a refusal leaves them all in
/// place. Returns false when it had to leave the state alone (source stopped or
/// gone), so the caller can keep the registry for later.
pub fn drop_all(
    copies: &[DatabaseCopy],
    env: &Env,
    force: bool,
    protected: &BTreeSet<String>,
) -> Result<bool> {
    let mut droppable = Vec::new();
    let mut finished = true;
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
            finished = false;
            continue;
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
            finished = false;
            continue;
        }
        let state = state_of(copy, &container, &env.project, &env.slug)?;
        if may_drop(state, copy, env, force)? {
            droppable.push((copy, container));
        }
    }
    for (copy, container) in droppable {
        drop_copy(copy, &container)?;
        println!("Dropped database {}", copy.target);
    }
    Ok(finished)
}

/// The decision for one copy listed in the state: false when there is nothing
/// to drop, an error when it is not ours to drop.
fn may_drop(state: MarkerState, copy: &DatabaseCopy, env: &Env, force: bool) -> Result<bool> {
    let ours = match state {
        MarkerState::Missing => return Ok(false),
        MarkerState::Foreign => false,
        MarkerState::Unmarked => has_own_name(copy, &env.project, &env.slug),
        MarkerState::Adopt | MarkerState::Recreate | MarkerState::Reuse => true,
    };
    if !ours && !force {
        bail!(
            "{} is not tagged for {}/{}; pass --force if it is really yours",
            copy.target,
            env.project,
            env.slug
        );
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
            MarkerState::Unmarked
        );
        assert_eq!(
            classify_marker(Some("owned by the app"), "wte2e", "feat-x"),
            MarkerState::Unmarked
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
            MarkerState::Unmarked
        );
        let mut other = rows.clone();
        other.insert("wt.slug".into(), "feat-y".into());
        assert_eq!(
            classify_mysql_marker(Some(&other), "wte2e", "feat-x"),
            MarkerState::Foreign
        );
        let mut partial = rows.clone();
        partial.remove("wt.complete");
        assert_eq!(
            classify_mysql_marker(Some(&partial), "wte2e", "feat-x"),
            MarkerState::Unmarked
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
    fn braces_validation_reads_as_literal_stay_literal() {
        // Container mode has no targets, and these are not placeholders.
        let values = vec![
            ("A".to_string(), "x/{{database}}".to_string()),
            ("B".to_string(), "x/{database".to_string()),
            ("C".to_string(), "{{ .Values.x }}:{port}".to_string()),
        ];
        assert_eq!(
            render_env(&values, &[], 5432).unwrap(),
            vec![
                ("A".to_string(), "x/{{database}}".to_string()),
                ("B".to_string(), "x/{database".to_string()),
                ("C".to_string(), "{{ .Values.x }}:5432".to_string()),
            ]
        );
    }

    #[test]
    fn an_unknown_database_placeholder_is_an_error() {
        let targets = vec![("app".to_string(), "t".to_string())];
        let values = vec![("U".to_string(), "x/{database:other}".to_string())];
        let err = format!("{:#}", render_env(&values, &targets, 1).unwrap_err());
        assert!(err.contains("{database:other}"), "{err}");
        let values = vec![("U".to_string(), "x/{database}".to_string())];
        assert!(render_env(&values, &[], 1).is_err());
    }

    fn test_env() -> Env {
        Env {
            project: "p".into(),
            slug: "s".into(),
            path: "/w/s".into(),
            legacy_names: false,
            slug_label: None,
        }
    }

    #[test]
    fn an_unmarked_copy_needs_the_name_wt_would_give_it() {
        let env = test_env();
        let own = db("postgres", "app", "wt_p_s__app");
        assert!(may_drop(MarkerState::Unmarked, &own, &env, false).unwrap());
        // A state entry edited to point at some other unmarked database.
        let other = db("postgres", "app", "billing");
        assert!(may_drop(MarkerState::Unmarked, &other, &env, false).is_err());
        assert!(may_drop(MarkerState::Unmarked, &other, &env, true).unwrap());
    }

    #[test]
    fn a_marker_of_another_owner_always_needs_force() {
        let env = test_env();
        let own = db("postgres", "app", "wt_p_s__app");
        assert!(may_drop(MarkerState::Foreign, &own, &env, false).is_err());
        assert!(may_drop(MarkerState::Foreign, &own, &env, true).unwrap());
        assert!(!may_drop(MarkerState::Missing, &own, &env, false).unwrap());
        assert!(may_drop(MarkerState::Reuse, &own, &env, false).unwrap());
    }

    #[test]
    fn registration_is_the_exact_copy() {
        let copy = db("postgres", "app", "wt_p_s__app");
        assert!(is_registered(std::slice::from_ref(&copy), &copy));
        assert!(!is_registered(&[], &copy));
        let moved = DatabaseCopy {
            source: "other".into(),
            ..copy.clone()
        };
        assert!(!is_registered(&[moved], &copy));
    }

    #[test]
    fn drop_uses_force_where_the_server_has_it() {
        assert_eq!(
            pg_drop_statements(Some(160007), "t"),
            vec!["DROP DATABASE \"t\" WITH (FORCE)".to_string()]
        );
        for version in [Some(120000), None] {
            let statements = pg_drop_statements(version, "t");
            assert_eq!(statements.len(), 2);
            assert!(statements[0].contains("pg_terminate_backend"));
            assert_eq!(statements[1], "DROP DATABASE \"t\"");
        }
    }

    #[test]
    fn mysql_scripts_log_in_as_root() {
        let script = mysql_copy_script("app", "t", "mysql  Ver 8.0.36");
        assert!(script.contains("\"$dump\" -uroot"), "{script}");
        assert!(script.contains("\"$cli\" -uroot"), "{script}");
        assert!(
            script.contains("'--ignore-table=app.__wt_meta'"),
            "{script}"
        );
    }

    /// Runs a real copy script with stub tools on PATH.
    fn run_script(name: &str, stubs: &[(&str, &str)]) -> (std::process::ExitStatus, String) {
        let dir = std::env::temp_dir().join(format!("wt-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (tool, body) in stubs {
            let path = dir.join(tool);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let target = format!("wt_test_{}_{name}", std::process::id());
        let script = pg_copy_script("app", &target, "app");
        let out = std::process::Command::new("sh")
            .args(["-euc", &script])
            .env(
                "PATH",
                format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert!(!std::path::Path::new(&dump_file(&target)).exists());
        assert!(!std::path::Path::new(&format!("{}.pid", dump_file(&target))).exists());
        std::fs::remove_dir_all(&dir).unwrap();
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn a_failed_dump_fails_the_copy_and_skips_the_restore() {
        let (status, stdout) = run_script(
            "dumpfail",
            &[("pg_dump", "exit 3"), ("psql", "echo restored")],
        );
        assert!(!status.success());
        assert!(!stdout.contains("restored"), "{stdout}");
    }

    #[test]
    fn a_failed_restore_fails_the_copy() {
        let (status, _) = run_script("restfail", &[("pg_dump", "echo d"), ("psql", "exit 4")]);
        assert!(!status.success());
    }

    #[test]
    fn a_good_copy_restores_the_dump() {
        let (status, stdout) = run_script("good", &[("pg_dump", "echo dumped"), ("psql", "cat")]);
        assert!(status.success());
        assert_eq!(stdout.trim(), "dumped");
    }

    #[test]
    fn stopping_ends_a_running_copy_and_its_step() {
        let name = format!("wt_test_{}_stop", std::process::id());
        let dir = std::env::temp_dir().join(format!("wt-test-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("pg_dump");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 60\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-euc", &pg_copy_script("app", &name, "app")])
            .env(
                "PATH",
                format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
            )
            .spawn()
            .unwrap();
        let pid_file = format!("{}.pid", dump_file(&name));
        for _ in 0..100 {
            if std::path::Path::new(&pid_file).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let stopped = std::process::Command::new("sh")
            .args(["-c", &stop_script(&name)])
            .status()
            .unwrap();
        assert!(stopped.success());
        assert!(!child.wait().unwrap().success());
        assert!(!std::path::Path::new(&pid_file).exists());
        assert!(!std::path::Path::new(&dump_file(&name)).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stopping_ignores_a_stale_pid_file() {
        let name = format!("wt_test_{}_stale", std::process::id());
        let pid_file = format!("{}.pid", dump_file(&name));
        // Our own pid: alive, but its command line does not name the dump file.
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();
        let stopped = std::process::Command::new("sh")
            .args(["-c", &stop_script(&name)])
            .status()
            .unwrap();
        assert!(stopped.success());
        assert!(!std::path::Path::new(&pid_file).exists());
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
        assert!(script.contains(&dump_file("wt_p_s__app")), "{script}");
        assert!(script.contains("trap"), "{script}");

        let script = mysql_copy_script("app", "wt_p_s__app", "mysql  Ver 8.0.36");
        assert!(!script.contains("| mysql"), "{script}");
        assert!(!script.contains("pipefail"), "{script}");
        assert!(script.contains(&dump_file("wt_p_s__app")), "{script}");
        assert!(script.contains("trap"), "{script}");

        // One dump file per target: two worktrees cannot share a temporary.
        let a_file = dump_file("wt_p_s__a");
        let b_file = dump_file("wt_p_s__b");
        assert_ne!(a_file, b_file);
        for script in [
            pg_copy_script("app", "wt_p_s__a", "app"),
            md_script("wt_p_s__a"),
        ] {
            assert!(script.contains(&a_file), "{script}");
            assert!(!script.contains(&b_file), "{script}");
        }
        for script in [
            pg_copy_script("app", "wt_p_s__b", "app"),
            md_script("wt_p_s__b"),
        ] {
            assert!(script.contains(&b_file), "{script}");
        }
    }

    fn md_script(target: &str) -> String {
        mysql_copy_script("app", target, "mysql  Ver 8.0.36")
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
