//! The ports each worktree was given, kept outside the checkout so it never
//! shows in `git status`. It is what a re-run reuses, what `ls` reports, and
//! the registry that stops two worktrees created while neither server runs
//! from both being handed the same web port.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, AppId, AppName};

/// One worktree's state. The default app's fields sit at the top level, as in
/// a state file from before apps, so 0.4 files read and write unchanged.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(flatten)]
    pub default: AppState,
    /// What `wt up` gave each extra app it brought up, keyed by its name. Keys
    /// are parsed in [`State::extras`], not here: one bad key must not make
    /// `load` drop the whole registry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, AppState>,
}

/// One app's share of a worktree's state.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppState {
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

/// The state file of one worktree.
pub fn path(project: &str, slug: &str) -> PathBuf {
    dir().join(project).join(format!("{slug}.toml"))
}

/// The state, or why the file cannot be read. `up` and `down` rewrite or
/// remove the file, and it is the only registry of the copies, so they must
/// never mistake an unreadable file for an empty one.
fn load_strict_from(file: &Path) -> Result<State> {
    match fs::read_to_string(file) {
        Ok(text) => parse_strict(&text, file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("could not read {}", file.display())),
    }
}

/// A worktree's state file, not an editor's leftover next to it.
fn is_state_file(file: &Path) -> bool {
    file.extension().is_some_and(|e| e == "toml")
        && !file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn parse_strict(text: &str, file: &Path) -> Result<State> {
    toml::from_str(text).with_context(|| {
        format!(
            "could not read {}; fix or remove it (it lists this worktree's ports and database copies)",
            file.display()
        )
    })
}

impl State {
    /// For `up` and `down`: a missing file is an empty state, an unreadable
    /// one an error.
    pub fn load_strict(project: &str, slug: &str) -> Result<State> {
        load_strict_from(&path(project, slug))
    }

    /// For reports such as `ls`: an unreadable file is a warning.
    pub fn load(project: &str, slug: &str) -> State {
        let file = path(project, slug);
        let Ok(text) = fs::read_to_string(&file) else {
            return State::default();
        };
        match toml::from_str(&text) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("warning: ignoring unreadable {}: {error}", file.display());
                State::default()
            }
        }
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

    /// One app's state, if it has any; the default app always has one.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wt up --app uses it in multi-app milestone 3")
    )]
    pub fn app(&self, id: &AppId) -> Option<&AppState> {
        match id {
            AppId::Default => Some(&self.default),
            AppId::Extra(name) => self.apps.get(name.as_str()),
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wt up --app uses it in multi-app milestone 3")
    )]
    pub fn app_mut(&mut self, id: &AppId) -> &mut AppState {
        match id {
            AppId::Default => &mut self.default,
            AppId::Extra(name) => self.apps.entry(name.as_str().to_string()).or_default(),
        }
    }

    /// The extra apps' states: each key as written, then its parsed name or
    /// why it is not one.
    pub fn extras(&self) -> impl Iterator<Item = (&str, Result<AppId>, &AppState)> {
        self.apps
            .iter()
            .map(|(key, app)| (key.as_str(), AppName::parse(key).map(AppId::Extra), app))
    }

    /// Every app's ports, so the cross-project registry never hands out one
    /// an extra app holds.
    fn all_ports(&self) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(&self.default)
            .chain(self.apps.values())
            .flat_map(|a| a.app_port.into_iter().chain(a.ports.values().copied()))
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
            let file = file.path();
            if !is_state_file(&file) {
                continue;
            }
            match load_strict_from(&file) {
                Ok(state) => ports.extend(state.all_ports()),
                Err(error) => eprintln!(
                    "warning: ports in {} are not counted as taken: {error:#}",
                    file.display()
                ),
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
            default: AppState {
                app_port: Some(3001),
                ports: BTreeMap::from([("postgres".into(), 5433), ("redis".into(), 6380)]),
                databases: vec![DatabaseCopy {
                    service: "postgres".into(),
                    source: "myapp-postgres-1".into(),
                    source_db: "app".into(),
                    target: "wt_myapp_feat_x__app".into(),
                }],
            },
            apps: BTreeMap::new(),
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
        assert_eq!(state.default.ports["postgres"], 5433);
        assert!(state.default.databases.is_empty());
    }

    #[test]
    fn databases_serialize_as_a_list() {
        let state = State {
            default: AppState {
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
                ..AppState::default()
            },
            ..State::default()
        };
        let text = toml::to_string(&state).unwrap();
        assert_eq!(text.matches("[[databases]]").count(), 2);
        let parsed: State = toml::from_str(&text).unwrap();
        assert_eq!(parsed.default.databases, state.default.databases);
    }

    /// A 0.4 state file, as 0.4 wrote it.
    const STATE_0_4: &str = r#"app_port = 3001

[ports]
postgres = 5433
redis = 6380

[[databases]]
service = "postgres"
source = "myapp-postgres-1"
source_db = "app"
target = "wt_myapp_feat_x__app"

[[databases]]
service = "postgres"
source = "myapp-postgres-1"
source_db = "app_cache"
target = "wt_myapp_feat_x__app_cache"
"#;

    fn copy(target: &str) -> DatabaseCopy {
        DatabaseCopy {
            service: "postgres".into(),
            source: "coa-api-postgres-1".into(),
            source_db: "coa".into(),
            target: target.into(),
        }
    }

    #[test]
    fn a_0_4_state_file_round_trips_byte_for_byte() {
        let state: State = toml::from_str(STATE_0_4).unwrap();
        assert_eq!(state.default.app_port, Some(3001));
        assert_eq!(state.default.ports["redis"], 6380);
        assert_eq!(state.default.databases.len(), 2);
        assert!(state.apps.is_empty());
        assert_eq!(toml::to_string(&state).unwrap(), STATE_0_4);
    }

    #[test]
    fn extras_keep_their_own_table() {
        let mut state: State = toml::from_str(STATE_0_4).unwrap();
        let api = AppId::Extra(AppName::parse("coa-api").unwrap());
        assert!(state.app(&api).is_none());
        let entry = state.app_mut(&api);
        entry.app_port = Some(8001);
        entry.ports.insert("postgres".into(), 5434);
        entry.databases.push(copy("wt_p_s__coa_api__coa"));
        let text = toml::to_string(&state).unwrap();
        assert!(text.starts_with(STATE_0_4), "{text}");
        assert!(text.contains("[apps.coa-api]"), "{text}");
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back, state);
        assert_eq!(back.app(&api).unwrap().app_port, Some(8001));
        assert_eq!(back.app(&AppId::Default).unwrap().app_port, Some(3001));
        let mut ports: Vec<u16> = state.all_ports().collect();
        ports.sort();
        assert_eq!(ports, [3001, 5433, 5434, 6380, 8001]);
    }

    #[test]
    fn extras_parse_their_keys_one_by_one() {
        let state: State =
            toml::from_str("[apps.coa-api]\napp_port = 8001\n[apps.Bad_Key]\napp_port = 8002\n")
                .unwrap();
        let mut seen: Vec<(bool, Option<u16>)> = state
            .extras()
            .map(|(_, id, a)| (id.is_ok(), a.app_port))
            .collect();
        seen.sort();
        // A bad key is reported, never a reason to lose the rest.
        assert_eq!(seen, [(false, Some(8002)), (true, Some(8001))]);
    }

    #[test]
    fn an_app_state_writes_only_what_it_has() {
        let mut state = State::default();
        state
            .app_mut(&AppId::Extra(AppName::parse("b").unwrap()))
            .app_port = Some(8001);
        assert_eq!(
            toml::to_string(&state).unwrap(),
            "[apps.b]\napp_port = 8001\n"
        );
    }

    #[test]
    fn a_strict_parse_refuses_what_the_lenient_load_would_drop() {
        let file = Path::new("/state/p/s.toml");
        let e = format!(
            "{:#}",
            parse_strict("[apps.b]\napp_port = 70000\n", file).unwrap_err()
        );
        assert!(
            e.contains("/state/p/s.toml") && e.contains("fix or remove it"),
            "{e}"
        );
        assert_eq!(
            parse_strict(STATE_0_4, file).unwrap().default.app_port,
            Some(3001)
        );
        assert_eq!(parse_strict("", file).unwrap(), State::default());
    }

    #[test]
    fn a_strict_load_tells_missing_from_unreadable() {
        let dir = std::env::temp_dir().join(format!("wt-state-strict-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            load_strict_from(&dir.join("missing.toml")).unwrap(),
            State::default()
        );
        let bad = dir.join("bad.toml");
        fs::write(&bad, "app_port = \"x\"\n").unwrap();
        let e = format!("{:#}", load_strict_from(&bad).unwrap_err());
        assert!(
            e.contains(&bad.display().to_string()) && e.contains("fix or remove it"),
            "{e}"
        );
        // A directory where the file should be is a read error, not "missing".
        let not_a_file = dir.join("dir.toml");
        fs::create_dir_all(&not_a_file).unwrap();
        assert!(load_strict_from(&not_a_file).is_err());
        fs::write(dir.join("good.toml"), STATE_0_4).unwrap();
        assert_eq!(
            load_strict_from(&dir.join("good.toml"))
                .unwrap()
                .default
                .app_port,
            Some(3001)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_state_files_count_as_state() {
        assert!(is_state_file(Path::new("/s/p/feat-x.toml")));
        for other in [
            "/s/p/feat-x.toml~",
            "/s/p/.feat-x.toml.swp",
            "/s/p/.DS_Store",
        ] {
            assert!(!is_state_file(Path::new(other)), "{other}");
        }
    }
}
