//! Container-backed services: Postgres, MySQL/MariaDB and Redis/Valkey. Each
//! clone is a byte copy of the source container's data volume, booted with the
//! source's own image, command and env.

use std::collections::BTreeMap;
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
            "redis" | "valkey" | "keydb" | "redis-stack-server" => Some(Kind::Redis),
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

    Ok(Source {
        volume: mount.name.clone().unwrap_or_default(),
        data_dir: mount.destination.clone(),
        container_port: container
            .container_port()
            .unwrap_or(svc.kind.default_container_port()),
        container,
    })
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

fn marker_present(image: &str, volume: &str) -> bool {
    docker::run_shell(
        image,
        &[format!("{volume}:/v")],
        &format!("test -f /v/{CLONE_MARKER}"),
    )
    .is_ok()
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
        if marker_present(image, &volume) {
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
        docker::stop(&source)?;
        STOPPED.lock().expect("lock").push(source.clone());
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
        }
        println!("Started {name}");
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
    for kv in src.container.env() {
        args.extend(["-e".into(), kv.clone()]);
    }
    if !config.user.is_empty() {
        args.extend(["--user".into(), config.user.clone()]);
    }
    args.push(config.image.clone());
    args.extend(config.cmd.clone().unwrap_or_default());

    docker::run(&args)?;
    println!("Created {name} ({}) on 127.0.0.1:{port}", config.image);
    Ok(())
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
        Kind::Redis => {
            let script = format!("redis-cli -p {port} ping 2>&1 || valkey-cli -p {port} ping 2>&1");
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
}
