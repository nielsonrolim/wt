//! Where things live and what they are called. Every resource `wt` creates is
//! named from (project, slug), so this is the one place those names are built.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::Config;
use crate::git;
use crate::services::Kind;

pub const LABEL_PROJECT: &str = "wt.project";
pub const LABEL_SLUG: &str = "wt.slug";
pub const LABEL_SERVICE: &str = "wt.service";

/// One worktree environment: its slug, checkout path and resource names.
#[derive(Debug, Clone)]
pub struct Env {
    pub project: String,
    pub slug: String,
    pub path: PathBuf,
    /// `docker.legacy_names`: the Postgres clone drops the service from its name.
    pub legacy_names: bool,
    /// `docker.slug_label`: an extra label that carries the slug.
    pub slug_label: Option<String>,
}

impl Env {
    pub fn new(cfg: &Config, slug: &str) -> Env {
        Env {
            project: cfg.project.clone(),
            slug: slug.to_string(),
            path: canon(&cfg.root).join(slug),
            legacy_names: cfg.docker.legacy_names,
            slug_label: cfg.docker.slug_label.clone(),
        }
    }

    pub fn prefix(&self) -> String {
        format!("{}-wt-", self.project)
    }

    pub fn container(&self, kind: Kind) -> String {
        if self.legacy_names && kind == Kind::Postgres {
            return format!("{}{}", self.prefix(), self.slug);
        }
        format!("{}{}-{}", self.prefix(), self.slug, kind.name())
    }

    pub fn volume(&self, kind: Kind) -> String {
        if self.legacy_names && kind == Kind::Postgres {
            return format!("{}-db", self.container(kind));
        }
        format!("{}-data", self.container(kind))
    }

    /// The pair that identifies this environment in a Docker filter.
    pub fn labels(&self) -> [(&'static str, String); 2] {
        [
            (LABEL_PROJECT, self.project.clone()),
            (LABEL_SLUG, self.slug.clone()),
        ]
    }

    /// The configured slug label as a filter, for resources a tool without
    /// `wt.*` labels created.
    pub fn slug_filter(&self) -> Option<[(&str, String); 1]> {
        self.slug_label
            .as_deref()
            .map(|key| [(key, self.slug.clone())])
    }

    /// Everything a container or volume of `kind` is created with.
    pub fn creation_labels(&self, kind: Kind) -> Vec<(&str, String)> {
        let mut labels = self.labels().to_vec();
        labels.push((LABEL_SERVICE, kind.name().to_string()));
        labels.extend(self.slug_filter().into_iter().flatten());
        labels
    }

    /// Whether the labels prove this environment created the resource: the
    /// `wt.*` pair, or the configured slug label.
    pub fn owns<'a>(&self, label: impl Fn(&str) -> Option<&'a str>) -> bool {
        let wt = label(LABEL_PROJECT) == Some(self.project.as_str())
            && label(LABEL_SLUG) == Some(self.slug.as_str());
        // A resource another project's `wt` made carries a different
        // `wt.project`; the slug label alone must not claim it.
        let other_project = label(LABEL_PROJECT).is_some_and(|p| p != self.project);
        let legacy = !other_project
            && self
                .slug_label
                .as_deref()
                .is_some_and(|key| label(key) == Some(self.slug.as_str()));
        wt || legacy
    }

    /// The service a container of this environment runs: its `wt.service`
    /// label, or the one whose name it carries when a tool without that label
    /// created it.
    pub fn service_of<'a>(
        &self,
        name: &str,
        label: impl Fn(&str) -> Option<&'a str>,
    ) -> Option<Kind> {
        label(LABEL_SERVICE)
            .and_then(Kind::from_name)
            .or_else(|| Kind::ALL.into_iter().find(|k| self.container(*k) == name))
    }
}

/// The main checkout. Every worktree has its own copy of the files, so the
/// binary's location cannot tell; the shared .git can, from anywhere.
pub fn main_root() -> Result<PathBuf> {
    let common = git::common_dir()?;
    match common.parent() {
        Some(parent) => Ok(canon(parent)),
        None => bail!("could not find the main checkout from {}", common.display()),
    }
}

pub fn assert_main_checkout(main_root: &Path) -> Result<()> {
    match git::toplevel() {
        Some(top) if canon(&top) == main_root => Ok(()),
        _ => bail!(
            "run this from the main checkout: cd {}",
            main_root.display()
        ),
    }
}

/// Lowercase, runs of anything else collapsed to one dash, at most 40 chars.
pub fn slugify(raw: &str) -> String {
    let mut slug = String::new();
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(40);
    slug.trim_end_matches('-').to_string()
}

/// Resolves symlinks when the path exists, so paths from git, the config and
/// the cwd compare equal.
pub fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_branch_names() {
        assert_eq!(slugify("feat/ENG-123_Add Thing"), "feat-eng-123-add-thing");
        assert_eq!(slugify("--Hello--World--"), "hello-world");
        assert_eq!(slugify("///"), "");
    }

    #[test]
    fn slugify_cuts_at_40_without_trailing_dash() {
        let long = format!("{}-tail", "a".repeat(39));
        let slug = slugify(&long);
        assert_eq!(slug, "a".repeat(39));
        assert!(slugify(&"b".repeat(60)).len() == 40);
    }

    #[test]
    fn names_are_namespaced() {
        let env = Env {
            project: "myapp".into(),
            slug: "feat-x".into(),
            path: PathBuf::from("/w/feat-x"),
            legacy_names: false,
            slug_label: None,
        };
        assert_eq!(env.container(Kind::Postgres), "myapp-wt-feat-x-postgres");
        assert_eq!(env.volume(Kind::Redis), "myapp-wt-feat-x-redis-data");
        assert!(env.container(Kind::Mysql).starts_with(&env.prefix()));
    }

    fn legacy_env() -> Env {
        Env {
            project: "platform".into(),
            slug: "feat-x".into(),
            path: PathBuf::from("/w/feat-x"),
            legacy_names: true,
            slug_label: Some("platform.worktree.slug".into()),
        }
    }

    #[test]
    fn legacy_names_only_change_postgres() {
        let env = legacy_env();
        assert_eq!(env.container(Kind::Postgres), "platform-wt-feat-x");
        assert_eq!(env.volume(Kind::Postgres), "platform-wt-feat-x-db");
        assert_eq!(env.container(Kind::Redis), "platform-wt-feat-x-redis");
        assert_eq!(env.volume(Kind::Redis), "platform-wt-feat-x-redis-data");
    }

    #[test]
    fn creation_labels_carry_both_sets() {
        let env = legacy_env();
        let labels = env.creation_labels(Kind::Postgres);
        assert!(labels.contains(&(LABEL_SLUG, "feat-x".to_string())));
        assert!(labels.contains(&(LABEL_SERVICE, "postgres".to_string())));
        assert!(labels.contains(&("platform.worktree.slug", "feat-x".to_string())));
    }

    #[test]
    fn ownership_takes_either_label_set() {
        let env = legacy_env();
        let wt = |k: &str| match k {
            LABEL_PROJECT => Some("platform"),
            LABEL_SLUG => Some("feat-x"),
            _ => None,
        };
        let legacy = |k: &str| (k == "platform.worktree.slug").then_some("feat-x");
        let other_slug = |k: &str| (k == "platform.worktree.slug").then_some("feat-y");
        let other_project = |k: &str| match k {
            LABEL_PROJECT => Some("other"),
            LABEL_SLUG => Some("feat-x"),
            _ => None,
        };
        assert!(env.owns(wt));
        assert!(env.owns(legacy));
        assert!(!env.owns(other_slug));
        assert!(!env.owns(other_project));
        let both = |k: &str| match k {
            LABEL_PROJECT => Some("other"),
            "platform.worktree.slug" => Some("feat-x"),
            _ => None,
        };
        assert!(!env.owns(both));
        assert!(!env.owns(|_| None));
    }

    #[test]
    fn legacy_container_is_a_postgres_without_a_service_label() {
        let env = legacy_env();
        assert_eq!(
            env.service_of("platform-wt-feat-x", |_| None),
            Some(Kind::Postgres)
        );
        assert_eq!(env.service_of("platform-wt-feat-y", |_| None), None);
    }
}
