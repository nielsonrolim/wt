pub mod down;
pub mod init;
pub mod ls;
pub mod up;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::project::{self, LABEL_PROJECT, LABEL_SLUG};
use crate::{docker, git, state};

/// Every environment of this project, including half-torn-down ones: what
/// Docker was told is ours, what git has checked out under the root, and what
/// the state files remember.
pub fn known_slugs(main: &Path, cfg: &Config) -> Result<Vec<String>> {
    let mut slugs = BTreeSet::new();

    if docker::ensure_running().is_ok() {
        let label = [(LABEL_PROJECT, cfg.project.clone())];
        for c in docker::containers_labelled(&label)? {
            slugs.extend(c.label(LABEL_SLUG).map(String::from));
        }
        for v in docker::volumes_labelled(&label)? {
            slugs.extend(v.label(LABEL_SLUG).map(String::from));
        }
        // Environments another tool made carry only the configured slug label.
        if let Some(key) = &cfg.docker.slug_label {
            let prefix = format!("{}-wt-", cfg.project);
            for c in docker::containers_with_label(key)? {
                if c.name().starts_with(&prefix) {
                    slugs.extend(c.label(key).map(String::from));
                }
            }
            for v in docker::volumes_with_label(key)? {
                if v.name.starts_with(&prefix) {
                    slugs.extend(v.label(key).map(String::from));
                }
            }
        }
    }

    let root = project::canon(&cfg.root);
    for entry in git::list(main)? {
        if entry.path.parent() == Some(root.as_path())
            && let Some(name) = entry.path.file_name()
        {
            slugs.insert(name.to_string_lossy().into_owned());
        }
    }

    slugs.extend(state::slugs(&cfg.project));
    Ok(slugs.into_iter().collect())
}
