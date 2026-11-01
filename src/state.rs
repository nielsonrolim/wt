//! The ports each worktree was given, kept outside the checkout so it never
//! shows in `git status`. It is what a re-run reuses, what `ls` reports, and
//! the registry that stops two worktrees created while neither server runs
//! from both being handed the same web port.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Databases copied inside the source instance (`mode = "database"`). A
    /// list, not a map: `down` must be able to drop a copy whose service no
    /// longer appears in the config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<DatabaseCopy>,
}

/// One logical database copied into a worktree's own database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseCopy {
    pub service: String,
    pub source: String,
    pub source_db: String,
    pub target: String,
}

fn dir() -> PathBuf {
    config::state_home().join("wt")
}

fn path(project: &str, slug: &str) -> PathBuf {
    dir().join(project).join(format!("{slug}.toml"))
}

impl State {
    pub fn load(project: &str, slug: &str) -> State {
        fs::read_to_string(path(project, slug))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, project: &str, slug: &str) -> Result<()> {
        let file = path(project, slug);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, toml::to_string(self)?)
            .with_context(|| format!("could not write {}", file.display()))
    }

    pub fn remove(project: &str, slug: &str) {
        let _ = fs::remove_file(path(project, slug));
    }

    fn all_ports(&self) -> impl Iterator<Item = u16> + '_ {
        self.app_port
            .into_iter()
            .chain(self.ports.values().copied())
    }
}

/// Every port any worktree of any project has been given.
pub fn claimed_ports() -> HashSet<u16> {
    let mut ports = HashSet::new();
    let Ok(projects) = fs::read_dir(dir()) else {
        return ports;
    };
    for project in projects.flatten() {
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            if let Some(state) = fs::read_to_string(file.path())
                .ok()
                .and_then(|t| toml::from_str::<State>(&t).ok())
            {
                ports.extend(state.all_ports());
            }
        }
    }
    ports
}

pub fn slugs(project: &str) -> Vec<String> {
    let Ok(files) = fs::read_dir(dir().join(project)) else {
        return Vec::new();
    };
    files
        .flatten()
        .filter_map(|f| {
            f.file_name()
                .to_str()?
                .strip_suffix(".toml")
                .map(String::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let state = State {
            app_port: Some(3001),
            ports: BTreeMap::from([("postgres".into(), 5433), ("redis".into(), 6380)]),
            databases: vec![DatabaseCopy {
                service: "postgres".into(),
                source: "myapp-postgres-1".into(),
                source_db: "app".into(),
                target: "wt_myapp_feat_x__app".into(),
            }],
        };
        let text = toml::to_string(&state).unwrap();
        assert_eq!(toml::from_str::<State>(&text).unwrap(), state);
        assert_eq!(
            state.all_ports().collect::<Vec<_>>(),
            vec![3001, 5433, 6380]
        );
    }

    #[test]
    fn reads_a_state_file_from_before_databases() {
        let state: State = toml::from_str("app_port = 3001\n\n[ports]\npostgres = 5433\n").unwrap();
        assert_eq!(state.ports["postgres"], 5433);
        assert!(state.databases.is_empty());
    }

    #[test]
    fn databases_serialize_as_a_list() {
        let state = State {
            databases: vec![
                DatabaseCopy {
                    service: "postgres".into(),
                    source: "myapp-postgres-1".into(),
                    source_db: "app".into(),
                    target: "wt_myapp_feat_x__app".into(),
                },
                DatabaseCopy {
                    service: "postgres".into(),
                    source: "myapp-postgres-1".into(),
                    source_db: "app_cache".into(),
                    target: "wt_myapp_feat_x__app_cache".into(),
                },
            ],
            ..State::default()
        };
        let text = toml::to_string(&state).unwrap();
        assert_eq!(text.matches("[[databases]]").count(), 2);
        let parsed: State = toml::from_str(&text).unwrap();
        assert_eq!(parsed.databases, state.databases);
    }
}
