//! The `docker` CLI, not the API: it honours whatever context is active
//! (Docker Desktop, OrbStack, colima) without finding a socket ourselves.

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::sh;

fn docker() -> Command {
    Command::new("docker")
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Container {
    name: String,
    pub config: ContainerConfig,
    pub state: ContainerState,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    pub host_config: HostConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    pub image: String,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub user: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerState {
    pub running: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Mount {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    pub destination: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfig {
    #[serde(default)]
    pub port_bindings: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct PortBinding {
    #[serde(default)]
    pub host_port: String,
}

impl Container {
    pub fn name(&self) -> &str {
        self.name.trim_start_matches('/')
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.config.labels.as_ref()?.get(key).map(String::as_str)
    }

    pub fn env(&self) -> &[String] {
        self.config.env.as_deref().unwrap_or_default()
    }

    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env()
            .iter()
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
    }

    /// Read from the configuration, not from the live state: `docker ps` only
    /// reports ports of running containers, and a stopped one still owns its.
    pub fn host_ports(&self) -> Vec<u16> {
        self.bindings()
            .flat_map(|(_, bs)| bs)
            .filter_map(|b| b.host_port.parse().ok())
            .collect()
    }

    /// The published TCP container port the service listens on, e.g. 5432
    /// from "5432/tcp". With several (MySQL's 3306 and 33060), `preferred`
    /// wins when published; otherwise the lowest, so the pick never depends
    /// on map order.
    pub fn container_port(&self, preferred: u16) -> Option<u16> {
        let ports: Vec<u16> = self
            .bindings()
            .filter_map(|(key, _)| {
                let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
                (proto == "tcp").then(|| port.parse().ok()).flatten()
            })
            .collect();
        if ports.contains(&preferred) {
            return Some(preferred);
        }
        ports.into_iter().min()
    }

    fn bindings(&self) -> impl Iterator<Item = (&String, &Vec<PortBinding>)> {
        self.host_config
            .port_bindings
            .iter()
            .flatten()
            .filter_map(|(k, v)| v.as_ref().map(|bs| (k, bs)))
    }

    pub fn volume_mounts(&self) -> impl Iterator<Item = &Mount> {
        self.mounts.iter().filter(|m| m.kind == "volume")
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Volume {
    pub name: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
}

impl Volume {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.as_ref()?.get(key).map(String::as_str)
    }
}

pub fn ensure_running() -> Result<()> {
    if !sh::which("docker") {
        bail!("docker is required (Docker Desktop, OrbStack or colima)");
    }
    if !sh::succeeds(docker().args(["version", "--format", "{{.Server.Version}}"])) {
        bail!("the docker daemon is not reachable; start Docker and retry");
    }
    Ok(())
}

/// Tolerant: inspecting several ids returns whatever still exists.
fn inspect_containers(ids: &[String]) -> Vec<Container> {
    if ids.is_empty() {
        return Vec::new();
    }
    let out = sh::stdout_lossy(docker().args(["inspect", "--type", "container"]).args(ids));
    serde_json::from_str(&out).unwrap_or_default()
}

pub fn container(name: &str) -> Option<Container> {
    inspect_containers(&[name.to_string()]).into_iter().next()
}

fn filters(labels: &[(&str, String)]) -> Vec<String> {
    labels
        .iter()
        .flat_map(|(k, v)| ["--filter".to_string(), format!("label={k}={v}")])
        .collect()
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Containers carrying `key` with any value.
pub fn containers_with_label(key: &str) -> Result<Vec<Container>> {
    let ids = lines(&sh::output(docker().args([
        "ps",
        "-aq",
        "--filter",
        &format!("label={key}"),
    ]))?);
    Ok(inspect_containers(&ids))
}

pub fn containers_labelled(labels: &[(&str, String)]) -> Result<Vec<Container>> {
    let ids = lines(&sh::output(
        docker().args(["ps", "-aq"]).args(filters(labels)),
    )?);
    Ok(inspect_containers(&ids))
}

pub fn volume(name: &str) -> Option<Volume> {
    let out = sh::stdout_lossy(docker().args(["volume", "inspect", name]));
    serde_json::from_str::<Vec<Volume>>(&out)
        .ok()?
        .into_iter()
        .next()
}

pub fn volumes_labelled(labels: &[(&str, String)]) -> Result<Vec<Volume>> {
    let names = lines(&sh::output(
        docker().args(["volume", "ls", "-q"]).args(filters(labels)),
    )?);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let out = sh::stdout_lossy(docker().args(["volume", "inspect"]).args(&names));
    Ok(serde_json::from_str(&out).unwrap_or_default())
}

/// Every host port bound by any container, running or not.
pub fn claimed_host_ports() -> Result<Vec<u16>> {
    let ids = lines(&sh::output(docker().args(["ps", "-aq"]))?);
    Ok(inspect_containers(&ids)
        .iter()
        .flat_map(Container::host_ports)
        .collect())
}

/// One call for every volume, rather than a `du` container per row.
pub fn volume_sizes() -> HashMap<String, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Row {
        name: String,
        size: String,
    }
    let out =
        sh::stdout_lossy(docker().args(["system", "df", "-v", "--format", "{{json .Volumes}}"]));
    serde_json::from_str::<Option<Vec<Row>>>(&out)
        .ok()
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.name, r.size))
        .collect()
}

pub fn stop(name: &str) -> Result<()> {
    sh::output(docker().args(["stop", "-t", "60", name])).map(drop)
}

pub fn start(name: &str) -> Result<()> {
    sh::output(docker().args(["start", name])).map(drop)
}

pub fn remove_container(name: &str) -> Result<()> {
    sh::output(docker().args(["rm", "-f", name])).map(drop)
}

pub fn create_volume(name: &str, labels: &[(&str, String)]) -> Result<()> {
    let mut cmd = docker();
    cmd.args(["volume", "create"]);
    for (k, v) in labels {
        cmd.arg("--label").arg(format!("{k}={v}"));
    }
    sh::output(cmd.arg(name)).map(drop)
}

pub fn remove_volume(name: &str) -> Result<()> {
    sh::output(docker().args(["volume", "rm", "-f", name])).map(drop)
}

/// `docker run` with the given arguments; returns stdout.
pub fn run(args: &[String]) -> Result<String> {
    sh::output(docker().arg("run").args(args))
}

/// A throwaway shell in `image` with volumes mounted, for copying or probing.
pub fn run_shell(image: &str, mounts: &[String], script: &str) -> Result<String> {
    let mut args = vec!["--rm".to_string(), "--user".into(), "0:0".into()];
    for m in mounts {
        args.push("-v".into());
        args.push(m.clone());
    }
    args.extend(["--entrypoint".into(), "sh".into(), image.into()]);
    args.extend(["-euc".into(), script.into()]);
    run(&args)
}

pub fn exec_output(container: &str, argv: &[&str]) -> Option<String> {
    sh::output(docker().args(["exec", container]).args(argv)).ok()
}

pub fn logs_tail(container: &str) -> String {
    let out = Command::new("docker")
        .args(["logs", "--tail", "30", container])
        .output();
    match out {
        Ok(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSPECT: &str = r#"[{
        "Name": "/myapp-postgres-1",
        "Config": {
            "Image": "postgres:16.7-alpine",
            "Cmd": ["postgres", "-c", "max_locks_per_transaction=1024"],
            "Env": ["POSTGRES_USER=app", "POSTGRES_PASSWORD=app", "PGDATA=/var/lib/postgresql/data"],
            "Labels": {"com.docker.compose.service": "postgres"},
            "User": ""
        },
        "State": {"Running": true},
        "Mounts": [
            {"Type": "volume", "Name": "myapp_db-data", "Destination": "/var/lib/postgresql/data"},
            {"Type": "bind", "Source": "/x", "Destination": "/docker-entrypoint-initdb.d"}
        ],
        "HostConfig": {"PortBindings": {"5432/tcp": [{"HostIp": "", "HostPort": "5432"}]}}
    }]"#;

    #[test]
    fn parses_inspect() {
        let c: Vec<Container> = serde_json::from_str(INSPECT).unwrap();
        let c = &c[0];
        assert_eq!(c.name(), "myapp-postgres-1");
        assert_eq!(c.env_value("POSTGRES_USER"), Some("app"));
        assert_eq!(c.env_value("POSTGRES"), None);
        assert_eq!(c.host_ports(), vec![5432]);
        assert_eq!(c.container_port(5432), Some(5432));
        assert_eq!(c.label("com.docker.compose.service"), Some("postgres"));
        let vols: Vec<_> = c.volume_mounts().collect();
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name.as_deref(), Some("myapp_db-data"));
    }

    #[test]
    fn tolerates_null_fields() {
        let json = r#"[{"Name": "/x", "Config": {"Image": "redis", "Cmd": null, "Env": null, "Labels": null},
            "State": {"Running": false}, "Mounts": [], "HostConfig": {"PortBindings": null}}]"#;
        let c: Vec<Container> = serde_json::from_str(json).unwrap();
        assert!(c[0].host_ports().is_empty());
        assert!(c[0].env().is_empty());
        assert_eq!(c[0].container_port(6379), None);
    }

    fn with_bindings(bindings: &str) -> Container {
        let json = format!(
            r#"[{{"Name": "/x", "Config": {{"Image": "mysql"}}, "State": {{"Running": true}},
            "Mounts": [], "HostConfig": {{"PortBindings": {bindings}}}}}]"#
        );
        serde_json::from_str::<Vec<Container>>(&json)
            .unwrap()
            .remove(0)
    }

    #[test]
    fn container_port_prefers_the_service_port() {
        let c = with_bindings(
            r#"{"33060/tcp": [{"HostPort": "33060"}], "3306/tcp": [{"HostPort": "3306"}]}"#,
        );
        assert_eq!(c.container_port(3306), Some(3306));
        // Without the preferred one published, the pick is still stable.
        assert_eq!(c.container_port(9999), Some(3306));
        let c = with_bindings(
            r#"{"53/udp": [{"HostPort": "53"}], "3307/tcp": [{"HostPort": "3307"}]}"#,
        );
        assert_eq!(c.container_port(3306), Some(3307));
    }
}
