//! Container-backed services: Postgres, MySQL/MariaDB and Redis/Valkey. Each
//! clone is a byte copy of the source container's data volume, booted with the
//! source's own image, command and env.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Service;
use crate::docker::{self, Container};
use crate::project::{Env, LABEL_SERVICE};

/// Written last, after the copy succeeded: a volume without it is a half-copy.
const CLONE_MARKER: &str = ".wt-clone-complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Postgres,
    Mysql,
    Redis,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Postgres, Kind::Mysql, Kind::Redis];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Mysql => "mysql",
            Kind::Redis => "redis",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// For `wt init`: which adapter an image belongs to.
    pub fn from_image(image: &str) -> Option<Kind> {
        let repo = image.rsplit('/').next().unwrap_or(image);
        let repo = repo.split([':', '@']).next().unwrap_or(repo).to_lowercase();
        match repo.as_str() {
            "postgres" | "postgis" | "pgvector" | "timescaledb" => Some(Kind::Postgres),
            "mysql" | "mariadb" | "percona" => Some(Kind::Mysql),
            "redis" | "valkey" | "keydb" | "redis-stack" | "redis-stack-server" => {
                Some(Kind::Redis)
            }
            _ => None,
        }
    }

    /// Where each image keeps its data, most specific first. Postgres 18 moved
    /// its default up a level.
    fn data_dirs(self) -> &'static [&'static str] {
        match self {
            Kind::Postgres => &["/var/lib/postgresql/data", "/var/lib/postgresql"],
            Kind::Mysql => &["/var/lib/mysql"],
            Kind::Redis => &["/data"],
        }
    }

    fn default_container_port(self) -> u16 {
        match self {
            Kind::Postgres => 5432,
            Kind::Mysql => 3306,
            Kind::Redis => 6379,
        }
    }

    /// Each range sits above the default port, so the main stack is never
    /// shadowed, and the ranges never overlap each other or the web range.
    pub fn default_port_range(self) -> (u16, u16) {
        match self {
            Kind::Postgres => (5433, 5499),
            Kind::Mysql => (3307, 3399),
            Kind::Redis => (6380, 6449),
        }
    }

    /// 127.0.0.1, not localhost: the clone publishes on IPv4 loopback only, and
    /// macOS resolves localhost to ::1 first.
    pub fn default_env(self) -> BTreeMap<String, String> {
        let pairs: &[(&str, &str)] = match self {
            Kind::Postgres | Kind::Mysql => &[("DB_HOST", "127.0.0.1"), ("DB_PORT", "{port}")],
            Kind::Redis => &[("REDIS_URL", "redis://127.0.0.1:{port}/0")],
        };
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    pub fn port_env_var(self) -> String {
        format!("WT_{}_PORT", self.name().to_uppercase())
    }

    /// Run inside the copy container after `cp -a`. A pid file left by an
    /// unclean stop makes the clone refuse to boot.
    fn cleanup_script(self) -> &'static str {
        match self {
            Kind::Postgres => {
                r#"find /to -name postmaster.pid -exec rm -f {} +
find /to -name postmaster.opts -exec rm -f {} +
for d in $(find /to -type d -name pg_stat_tmp); do
  owner="$(stat -c '%u:%g' "$(dirname "$d")")"
  rm -rf "$d" && mkdir "$d" && chown "$owner" "$d" && chmod 0700 "$d"
done"#
            }
            Kind::Mysql => {
                r#"find /to -maxdepth 2 \( -name '*.pid' -o -name '*.sock' -o -name '*.sock.lock' \) -exec rm -f {} +"#
            }
            Kind::Redis => ":",
        }
    }

    fn ready_timeout(self) -> Duration {
        match self {
            Kind::Mysql => Duration::from_secs(120),
            _ => Duration::from_secs(60),
        }
    }
}

/// What the clone copies from: the source container and its data volume.
pub struct Source {
    pub container: Container,
    pub volume: String,
    pub data_dir: String,
    pub container_port: u16,
}

pub fn resolve_source(svc: &Service) -> Result<Source> {
    let kind = svc.kind.name();
    let Some(container) = docker::container(&svc.source) else {
        bail!(
            "source container {} not found (docker compose up -d {kind}, or set services.{kind}.source)",
            svc.source
        );
    };

    let mount = match &svc.volume {
        Some(volume) => container
            .volume_mounts()
            .find(|m| m.name.as_deref() == Some(volume.as_str()))
            .with_context(|| format!("{volume} is not mounted in {}", svc.source))?,
        None => {
            let mounts: Vec<_> = container.volume_mounts().collect();
            let by_dir = svc
                .kind
                .data_dirs()
                .iter()
                .find_map(|dir| mounts.iter().copied().find(|m| m.destination == *dir));
            match (by_dir, mounts.as_slice()) {
                (Some(m), _) => m,
                (None, [only]) => *only,
                _ => bail!(
                    "cannot tell which volume of {} holds the data; set services.{kind}.volume (data in a bind mount is not supported)",
                    svc.source
                ),
            }
        }
    };

    if svc.clone {
        check_self_contained(svc.kind, &container, &mount.destination)?;
    }

    Ok(Source {
        volume: mount.name.clone().unwrap_or_default(),
        data_dir: mount.destination.clone(),
        container_port: container
            .container_port(svc.kind.default_container_port())
            .unwrap_or(svc.kind.default_container_port()),
        container,
    })
}

/// Refuses a source whose data does not all live in the data volume. Only
/// that volume is copied, and the clone gets none of the source's other
/// writable mounts, so it would boot with part of its data missing: with
/// Postgres, a WAL that no longer matches the copied data files.
fn check_self_contained(kind: Kind, container: &Container, data_dir: &str) -> Result<()> {
    let source = container.name();
    let inside = format!("{}/", data_dir.trim_end_matches('/'));
    if let Some(m) = container
        .mounts
        .iter()
        .find(|m| m.destination.starts_with(&inside))
    {
        bail!(
            "{source} mounts {} inside its data dir {data_dir}; wt copies only the data volume, so the clone would miss it (set clone = false to start empty)",
            m.destination
        );
    }
    if kind == Kind::Postgres
        && let Some(dir) = container
            .env_value("POSTGRES_INITDB_WALDIR")
            .filter(|d| !d.is_empty())
        // A WAL dir inside the data volume is copied along with it. One moved
        // by other means (a `-c` flag, a config file) is not detected here.
        && !Path::new(dir).starts_with(data_dir)
    {
        bail!(
            "{source} keeps its WAL in {dir}, outside the data volume; wt copies only the data volume, so the clone would boot without it (set clone = false to start empty)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stopping the source
// ---------------------------------------------------------------------------

/// Sources stopped for a copy. Restarted as soon as the copy ends, and also on
/// any error (via `RestoreOnDrop`) or Ctrl-C (via the handler in main).
static STOPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn restore_sources() {
    let names: Vec<String> = STOPPED
        .lock()
        .map(|mut v| v.drain(..).collect())
        .unwrap_or_default();
    for name in names {
        if docker::start(&name).is_err() {
            eprintln!("error: could not restart {name}; run: docker start {name}");
        }
    }
}

pub struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        restore_sources();
    }
}

// ---------------------------------------------------------------------------
// Volume, container, readiness
// ---------------------------------------------------------------------------

/// Whether a finished copy left its marker. An error means the check itself
/// failed (daemon, image, no `sh`), which says nothing about the volume, so
/// the caller must not treat it as a half-copy and discard the data.
fn marker_present(image: &str, volume: &str) -> Result<bool> {
    let out = docker::run_shell(
        image,
        &[format!("{volume}:/v")],
        &format!("if [ -f /v/{CLONE_MARKER} ]; then echo yes; else echo no; fi"),
    )
    .with_context(|| format!("could not check whether {volume} is a finished copy"))?;
    parse_marker(&out)
}

fn parse_marker(out: &str) -> Result<bool> {
    match out.trim() {
        "yes" => Ok(true),
        "no" => Ok(false),
        other => bail!("unexpected marker check output: {other:?}"),
    }
}

/// Creates the clone's volume: a byte copy of the source, or empty when the
/// service has `clone = false`. A finished copy is reused on re-runs.
pub fn prepare_volume(svc: &Service, src: &Source, env: &Env) -> Result<()> {
    let volume = env.volume(svc.kind);
    let image = &src.container.config.image;
    let mut labels = env.labels().to_vec();
    labels.push((LABEL_SERVICE, svc.kind.name().to_string()));

    if !svc.clone {
        if docker::volume(&volume).is_none() {
            docker::create_volume(&volume, &labels)?;
            println!("Created empty {volume}");
        }
        return Ok(());
    }

    if docker::volume(&volume).is_some() {
        if marker_present(image, &volume)? {
            println!("Reusing {volume}");
            return Ok(());
        }
        println!("Discarding half-copied {volume}");
        docker::remove_volume(&volume)?;
    }

    // A byte copy needs the source at rest: a live data dir has torn pages.
    let source = src.container.name().to_string();
    if src.container.state.running {
        println!("Stopping {source} (it is down for the few seconds of the copy)");
        // Registered before the stop: a Ctrl-C while `docker stop` waits still
        // lets the daemon finish stopping it, and the handler must restart it.
        match STOPPED.lock() {
            Ok(mut v) => v.push(source.clone()),
            Err(poisoned) => poisoned.into_inner().push(source.clone()),
        }
        docker::stop(&source)?;
    }

    docker::create_volume(&volume, &labels)?;
    // `/from/.`, not `/from/*`, or dotfiles are missed. The volume root is
    // created root:root 0755, which Postgres refuses, so it takes the owner
    // and mode of the source root.
    let script = format!(
        r#"cp -a /from/. /to/
{cleanup}
chown "$(stat -c '%u:%g' /from)" /to
chmod "$(stat -c '%a' /from)" /to
touch /to/{CLONE_MARKER}
chown "$(stat -c '%u:%g' /from)" /to/{CLONE_MARKER}"#,
        cleanup = svc.kind.cleanup_script()
    );
    let copied = docker::run_shell(
        image,
        &[format!("{}:/from:ro", src.volume), format!("{volume}:/to")],
        &script,
    );
    restore_sources();
    copied.with_context(|| format!("copying {} into {volume} failed", src.volume))?;
    println!("Cloned {} into {volume}", src.volume);
    Ok(())
}

/// Boots the clone with the source's image, command, env and user.
pub fn start(svc: &Service, src: &Source, env: &Env, port: u16) -> Result<()> {
    let name = env.container(svc.kind);
    if let Some(existing) = docker::container(&name) {
        let published = existing.host_ports();
        if !published.contains(&port) {
            bail!(
                "{name} is published on {published:?}, not {port} (wt down {})",
                env.slug
            );
        }
        if !existing.state.running {
            docker::start(&name)?;
            println!("Started {name}");
        } else {
            println!("Reusing {name}");
        }
        return Ok(());
    }

    let config = &src.container.config;
    let mut args: Vec<String> = vec!["-d".into(), "--name".into(), name.clone()];
    for (k, v) in env.labels() {
        args.extend(["--label".into(), format!("{k}={v}")]);
    }
    args.extend([
        "--label".into(),
        format!("{LABEL_SERVICE}={}", svc.kind.name()),
        "--restart".into(),
        "unless-stopped".into(),
        "-p".into(),
        format!("127.0.0.1:{port}:{}", src.container_port),
        "-v".into(),
        format!("{}:{}", env.volume(svc.kind), src.data_dir),
    ]);
    args.extend(extra_mounts(src));
    args.extend(resource_args(&src.container));
    for kv in src.container.env() {
        args.extend(["-e".into(), kv.clone()]);
    }
    if !config.user.is_empty() {
        args.extend(["--user".into(), config.user.clone()]);
    }
    if !config.working_dir.is_empty() {
        args.extend(["--workdir".into(), config.working_dir.clone()]);
    }
    // Inspect reports the effective entrypoint, so an empty one means the
    // source cleared the image's and the clone must clear it too.
    // `--entrypoint` takes one word; the rest of a multi-word entrypoint goes
    // before the command, which is how docker joins them anyway.
    let entrypoint = config.entrypoint.clone().unwrap_or_default();
    let first = entrypoint.first().cloned().unwrap_or_default();
    args.extend(["--entrypoint".into(), first]);
    args.push(config.image.clone());
    args.extend(entrypoint.into_iter().skip(1));
    args.extend(config.cmd.clone().unwrap_or_default());

    docker::run(&args)?;
    println!("Created {name} ({}) on 127.0.0.1:{port}", config.image);
    Ok(())
}

/// The source's bind mounts (config files, init scripts), always read-only,
/// and its tmpfs mounts. Other named volumes, and anything inside the data
/// dir, are left out: the clone must never write into state the source owns,
/// such as a bind-mounted WAL directory both servers would append to.
fn extra_mounts(src: &Source) -> Vec<String> {
    let inside_data = format!("{}/", src.data_dir.trim_end_matches('/'));
    let mut args = Vec::new();
    for m in &src.container.mounts {
        if m.destination == src.data_dir || m.destination.starts_with(&inside_data) {
            continue;
        }
        match m.kind.as_str() {
            "bind" if !m.source.is_empty() => {
                args.extend(["-v".into(), format!("{}:{}:ro", m.source, m.destination)]);
            }
            "tmpfs" => args.extend(["--tmpfs".into(), m.destination.clone()]),
            _ => {}
        }
    }
    args
}

/// The source's `HostConfig` values that change how the container runs and are
/// not in the image or command: its shared-memory size and ulimits. Without
/// them the clone falls back to the daemon defaults, which for Postgres means
/// 64 MiB of `/dev/shm` and parallel queries that fail under load.
fn resource_args(container: &Container) -> Vec<String> {
    let mut args = Vec::new();
    if container.shm_size() > 0 {
        args.extend(["--shm-size".into(), container.shm_size().to_string()]);
    }
    for u in container.ulimits() {
        args.extend([
            "--ulimit".into(),
            format!("{}={}:{}", u.name, u.soft, u.hard),
        ]);
    }
    args
}

/// Waits until the server answers, then proves the data survived the copy.
pub fn wait_ready(svc: &Service, src: &Source, env: &Env) -> Result<()> {
    let name = env.container(svc.kind);
    let deadline = Instant::now() + svc.kind.ready_timeout();
    loop {
        match docker::container(&name) {
            Some(c) if c.state.running => {}
            _ => bail!(
                "{name} exited during startup:\n{}",
                docker::logs_tail(&name)
            ),
        }
        if probe(svc, src, &name, Probe::Ready) {
            break;
        }
        if Instant::now() > deadline {
            bail!("{name} never became ready:\n{}", docker::logs_tail(&name));
        }
        thread::sleep(Duration::from_millis(500));
    }
    if !probe(svc, src, &name, Probe::Verify) {
        bail!(
            "{name} is up but its data is not readable (wt down {})",
            env.slug
        );
    }
    println!("{name} is ready");
    Ok(())
}

#[derive(PartialEq)]
enum Probe {
    Ready,
    Verify,
}

fn probe(svc: &Service, src: &Source, name: &str, probe: Probe) -> bool {
    let port = src.container_port.to_string();
    match svc.kind {
        // Over TCP: during first-boot init the image runs a socket-only
        // server, which must not count as ready. Verify goes through the
        // socket, where the image trusts local connections.
        Kind::Postgres => {
            let user = src
                .container
                .env_value("POSTGRES_USER")
                .unwrap_or("postgres");
            if probe == Probe::Ready {
                docker::exec_output(
                    name,
                    &[
                        "pg_isready",
                        "-q",
                        "-h",
                        "127.0.0.1",
                        "-p",
                        &port,
                        "-U",
                        user,
                    ],
                )
                .is_some()
            } else {
                let db = svc.database.as_deref().unwrap_or("postgres");
                docker::exec_output(
                    name,
                    &["psql", "-qtAX", "-U", user, "-d", db, "-c", "select 1"],
                )
                .is_some_and(|out| out.trim() == "1")
            }
        }
        // mysqladmin ping exits 0 whenever the server runs, even on access
        // denied, so it needs no credentials.
        Kind::Mysql => {
            let script = format!(
                "mysqladmin ping -h 127.0.0.1 -P {port} --silent || mariadb-admin ping -h 127.0.0.1 -P {port} --silent"
            );
            docker::exec_output(name, &["sh", "-c", &script]).is_some()
        }
        // The reply decides, not the exit code: whether an error reply such as
        // NOAUTH exits non-zero varies between client versions.
        Kind::Redis => {
            let script = format!(
                "cli=redis-cli; command -v $cli >/dev/null || cli=valkey-cli; $cli -p {port} ping 2>&1 || true"
            );
            docker::exec_output(name, &["sh", "-c", &script])
                .is_some_and(|out| out.contains("PONG") || out.contains("NOAUTH"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_images() {
        assert_eq!(
            Kind::from_image("postgres:16.7-alpine"),
            Some(Kind::Postgres)
        );
        assert_eq!(
            Kind::from_image("postgis/postgis:16-3.4"),
            Some(Kind::Postgres)
        );
        assert_eq!(
            Kind::from_image("docker.io/library/mariadb:11"),
            Some(Kind::Mysql)
        );
        assert_eq!(
            Kind::from_image("valkey/valkey:8-alpine"),
            Some(Kind::Redis)
        );
        assert_eq!(Kind::from_image("redis@sha256:abc"), Some(Kind::Redis));
        assert_eq!(
            Kind::from_image("redis/redis-stack:latest"),
            Some(Kind::Redis)
        );
        assert_eq!(Kind::from_image("localstack/localstack:4"), None);
    }

    #[test]
    fn ranges_do_not_overlap() {
        let mut ranges: Vec<_> = Kind::ALL.iter().map(|k| k.default_port_range()).collect();
        ranges.push((3001, 3099));
        ranges.sort();
        for pair in ranges.windows(2) {
            assert!(pair[0].1 < pair[1].0, "{pair:?}");
        }
    }

    #[test]
    fn marker_check_only_reports_what_it_saw() {
        assert!(parse_marker("yes\n").unwrap());
        assert!(!parse_marker("no\n").unwrap());
        // Anything else, e.g. an image that prints a banner, is not "no".
        assert!(parse_marker("").is_err());
        assert!(parse_marker("Welcome\nno").is_err());
    }

    #[test]
    fn clone_gets_read_only_binds_and_tmpfs_but_no_shared_state() {
        let json = r#"[{"Name": "/x", "Config": {"Image": "postgres"}, "State": {"Running": true},
            "Mounts": [
                {"Type": "volume", "Name": "app_db", "Destination": "/var/lib/postgresql/data", "RW": true},
                {"Type": "volume", "Name": "app_sock", "Destination": "/run/postgresql", "RW": true},
                {"Type": "bind", "Source": "/repo/pg.conf", "Destination": "/etc/pg.conf", "RW": false},
                {"Type": "bind", "Source": "/repo/logs", "Destination": "/logs", "RW": true},
                {"Type": "bind", "Source": "/repo/wal", "Destination": "/var/lib/postgresql/data/pg_wal", "RW": true},
                {"Type": "tmpfs", "Source": "", "Destination": "/tmp", "RW": true}
            ],
            "HostConfig": {"PortBindings": null}}]"#;
        let container = serde_json::from_str::<Vec<Container>>(json)
            .unwrap()
            .remove(0);
        let src = Source {
            container,
            volume: "app_db".into(),
            data_dir: "/var/lib/postgresql/data".into(),
            container_port: 5432,
        };
        assert_eq!(
            extra_mounts(&src),
            [
                "-v",
                "/repo/pg.conf:/etc/pg.conf:ro",
                "-v",
                "/repo/logs:/logs:ro",
                "--tmpfs",
                "/tmp"
            ]
        );
    }

    #[test]
    fn clone_gets_the_source_shm_size_and_ulimits() {
        let json = r#"[{"Name": "/x", "Config": {"Image": "postgres"}, "State": {"Running": true},
            "Mounts": [], "HostConfig": {"ShmSize": 1073741824,
            "Ulimits": [{"Name": "nofile", "Soft": 1024, "Hard": 65536},
                        {"Name": "nproc", "Soft": 2048, "Hard": 4096}]}}]"#;
        let container = serde_json::from_str::<Vec<Container>>(json)
            .unwrap()
            .remove(0);
        assert_eq!(
            resource_args(&container),
            [
                "--shm-size",
                "1073741824",
                "--ulimit",
                "nofile=1024:65536",
                "--ulimit",
                "nproc=2048:4096"
            ]
        );
        // A source that sets neither adds nothing.
        assert!(resource_args(&container_with("[]", "[]")).is_empty());
    }

    fn container_with(mounts: &str, env: &str) -> Container {
        let json = format!(
            r#"[{{"Name": "/db", "Config": {{"Image": "postgres", "Env": {env}}},
            "State": {{"Running": true}}, "Mounts": {mounts}, "HostConfig": {{}}}}]"#
        );
        serde_json::from_str::<Vec<Container>>(&json)
            .unwrap()
            .remove(0)
    }

    #[test]
    fn refuses_data_outside_the_data_volume() {
        let dir = "/var/lib/postgresql/data";
        let data = r#"{"Type": "volume", "Name": "db", "Destination": "/var/lib/postgresql/data"}"#;
        let conf = r#"{"Type": "bind", "Source": "/r/pg.conf", "Destination": "/etc/pg.conf"}"#;
        let wal = r#"{"Type": "bind", "Source": "/r/wal", "Destination": "/var/lib/postgresql/data/pg_wal"}"#;

        let ok = container_with(&format!("[{data}, {conf}]"), "[]");
        assert!(check_self_contained(Kind::Postgres, &ok, dir).is_ok());

        let nested = container_with(&format!("[{data}, {wal}]"), "[]");
        assert!(check_self_contained(Kind::Postgres, &nested, dir).is_err());

        let waldir = container_with(&format!("[{data}]"), r#"["POSTGRES_INITDB_WALDIR=/pgwal"]"#);
        assert!(check_self_contained(Kind::Postgres, &waldir, dir).is_err());
        // A WAL dir inside the data volume travels with the copy, so it is fine.
        let inside = container_with(
            &format!("[{data}]"),
            r#"["POSTGRES_INITDB_WALDIR=/var/lib/postgresql/data/wal"]"#,
        );
        assert!(check_self_contained(Kind::Postgres, &inside, dir).is_ok());
        // The variable means nothing to other images.
        assert!(check_self_contained(Kind::Mysql, &waldir, dir).is_ok());
    }
}
