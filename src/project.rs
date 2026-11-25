//! Where things live and what they are called. Every resource `wt` creates is
//! named from (project, slug), so this is the one place those names are built.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::{AppId, Config};
use crate::git;
use crate::services::Kind;

pub const LABEL_PROJECT: &str = "wt.project";
pub const LABEL_SLUG: &str = "wt.slug";
pub const LABEL_SERVICE: &str = "wt.service";
/// The `[apps.<name>]` a resource belongs to. The default app's resources have
/// none, as they did before apps existed.
pub const LABEL_APP: &str = "wt.app";
/// Marks a throwaway copy or probe container, never a service of the worktree.
pub const LABEL_HELPER: &str = "wt.helper";

/// One worktree environment: its slug, checkout path and resource names.
#[derive(Debug, Clone)]
pub struct Env {
    pub project: String,
    pub slug: String,
    pub path: PathBuf,
    /// `docker.legacy_names`: the Postgres clone drops the service from its
    /// name (the default app's only).
    pub legacy_names: bool,
    /// `docker.slug_label`: an extra label that carries the slug.
    pub slug_label: Option<String>,
    /// The app these names are for; every app of a worktree shares its path.
    pub app: AppId,
}

impl Env {
    pub fn new(cfg: &Config, slug: &str) -> Env {
        Env {
            project: cfg.project.clone(),
            slug: slug.to_string(),
            path: canon(&cfg.root).join(slug),
            legacy_names: cfg.docker.legacy_names,
            slug_label: cfg.docker.slug_label.clone(),
            app: AppId::Default,
        }
    }

    /// The same worktree, seen as one of its apps. It takes an `AppId`, not
    /// the app's config, so `down` can reach an app the config dropped.
    pub fn with_app(self, app: &AppId) -> Env {
        Env {
            app: app.clone(),
            ..self
        }
    }

    pub fn prefix(&self) -> String {
        format!("{}-wt-", self.project)
    }

    /// An extra app's names put the app after a `.`. No project, slug or app
    /// name contains a `.`, so an extra's names never match a default app's.
    pub fn container(&self, kind: Kind) -> String {
        if let Some(app) = self.app.label() {
            return format!("{}{}.{app}-{}", self.prefix(), self.slug, kind.name());
        }
        if self.legacy_names && kind == Kind::Postgres {
            return format!("{}{}", self.prefix(), self.slug);
        }
        format!("{}{}-{}", self.prefix(), self.slug, kind.name())
    }

    pub fn volume(&self, kind: Kind) -> String {
        if self.legacy_names && kind == Kind::Postgres && self.app == AppId::Default {
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
        if let Some(app) = self.app.label() {
            labels.push((LABEL_APP, app.to_string()));
        }
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

    /// A throwaway helper of this environment. Only these may be removed to
    /// free a half-copied volume: anything else on it may hold real data.
    pub fn owns_helper<'a>(&self, label: impl Fn(&str) -> Option<&'a str>) -> bool {
        label(LABEL_HELPER).is_some() && self.owns(label)
    }

    /// Whether a resource of this worktree belongs to this app: its `wt.app`
    /// names it, or it has none and this is the default app.
    pub fn is_its_app<'a>(&self, label: impl Fn(&str) -> Option<&'a str>) -> bool {
        label(LABEL_APP) == self.app.label()
    }

    /// The service a container of this environment runs: its `wt.service`
    /// label, or the one whose name it carries when a tool without that label
    /// created it, which in practice is the default app's unlabelled clone.
    pub fn service_of<'a>(
        &self,
        name: &str,
        label: impl Fn(&str) -> Option<&'a str>,
    ) -> Option<Kind> {
        // Another app's resource, or the default app's when this is an extra.
        if !self.is_its_app(&label) {
            return None;
        }
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
            app: AppId::Default,
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
            app: AppId::Default,
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

    // ---- Several apps ----

    use crate::config::AppName;

    fn monorepo() -> Config {
        Config::from_toml(include_str!("../examples/monorepo.wt.toml")).unwrap()
    }

    fn app_env(legacy: bool) -> Env {
        let mut env = legacy_env();
        env.legacy_names = legacy;
        env.with_app(&AppId::Extra(AppName::parse("coa-api").unwrap()))
    }

    #[test]
    fn an_extra_apps_names_carry_the_app_after_a_dot() {
        let env = app_env(false);
        assert_eq!(
            env.container(Kind::Postgres),
            "platform-wt-feat-x.coa-api-postgres"
        );
        assert_eq!(
            env.volume(Kind::Postgres),
            "platform-wt-feat-x.coa-api-postgres-data"
        );
        // legacy_names only ever renames the default app's Postgres.
        assert_eq!(
            app_env(true).container(Kind::Postgres),
            env.container(Kind::Postgres)
        );
        assert_eq!(
            app_env(true).volume(Kind::Postgres),
            env.volume(Kind::Postgres)
        );
    }

    #[test]
    fn an_extra_never_shares_a_name_with_another_worktree() {
        // slug feat + app coa-api vs the default app of slug feat-coa-api.
        let mut extra = app_env(false);
        extra.slug = "feat".into();
        let mut default = legacy_env();
        default.legacy_names = false;
        default.slug = "feat-coa-api".into();
        for kind in Kind::ALL {
            assert_ne!(extra.container(kind), default.container(kind));
            assert_ne!(extra.volume(kind), default.volume(kind));
        }
    }

    #[test]
    fn two_apps_of_the_monorepo_get_their_own_clone() {
        let cfg = monorepo();
        let default = Env::new(&cfg, "feat-x");
        let api = Env::new(&cfg, "feat-x").with_app(&cfg.extras[0].id);
        assert_ne!(
            default.container(Kind::Postgres),
            api.container(Kind::Postgres)
        );
        assert_eq!(
            default.container(Kind::Postgres),
            "myapp-wt-feat-x-postgres"
        );
        assert_eq!(
            api.container(Kind::Postgres),
            "myapp-wt-feat-x.coa-api-postgres"
        );
        assert_eq!(api.path, default.path);
        assert_eq!(api.app.label(), Some("coa-api"));
        assert_eq!(default.app.label(), None);
    }

    #[test]
    fn an_extras_resources_carry_wt_app() {
        let api = app_env(false);
        let labels = api.creation_labels(Kind::Postgres);
        assert!(labels.contains(&(LABEL_APP, "coa-api".to_string())));
        assert!(labels.contains(&(LABEL_SLUG, "feat-x".to_string())));
        let default = legacy_env();
        let labels = default.creation_labels(Kind::Postgres);
        assert!(!labels.iter().any(|(k, _)| *k == LABEL_APP));
    }

    #[test]
    fn ownership_is_per_worktree_whatever_the_app() {
        let wt_app = |k: &str| match k {
            LABEL_PROJECT => Some("platform"),
            LABEL_SLUG => Some("feat-x"),
            LABEL_APP => Some("coa-api"),
            _ => None,
        };
        assert!(legacy_env().owns(wt_app));
        assert!(app_env(false).owns(wt_app));
    }

    #[test]
    fn service_of_tells_two_apps_apart() {
        let default = legacy_env();
        let api = app_env(false);
        let api_pg = |k: &str| match k {
            LABEL_SERVICE => Some("postgres"),
            LABEL_APP => Some("coa-api"),
            _ => None,
        };
        let default_pg = |k: &str| (k == LABEL_SERVICE).then_some("postgres");
        let other_app = |k: &str| match k {
            LABEL_SERVICE => Some("postgres"),
            LABEL_APP => Some("other"),
            _ => None,
        };
        let name = api.container(Kind::Postgres);
        assert_eq!(api.service_of(&name, api_pg), Some(Kind::Postgres));
        assert_eq!(default.service_of(&name, api_pg), None);
        assert_eq!(default.service_of("x", default_pg), Some(Kind::Postgres));
        assert_eq!(api.service_of("x", default_pg), None);
        assert_eq!(api.service_of("x", other_app), None);
        // The name fallback still finds the default app's legacy clone.
        assert_eq!(
            default.service_of("platform-wt-feat-x", |_| None),
            Some(Kind::Postgres)
        );
        assert_eq!(api.service_of("platform-wt-feat-x", |_| None), None);
    }

    #[test]
    fn an_extra_keeps_the_worktrees_slug_label() {
        let labels_of = |env: &Env| {
            env.creation_labels(Kind::Redis)
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<Vec<_>>()
        };
        let labels = labels_of(&app_env(false));
        assert!(labels.contains(&("platform.worktree.slug".to_string(), "feat-x".to_string())));
    }

    #[test]
    fn a_resource_belongs_to_one_app() {
        let api = app_env(false);
        let default = legacy_env();
        let api_label = |k: &str| (k == LABEL_APP).then_some("coa-api");
        assert!(api.is_its_app(api_label));
        assert!(!default.is_its_app(api_label));
        assert!(default.is_its_app(|_| None));
        assert!(!api.is_its_app(|_| None));
    }
}
