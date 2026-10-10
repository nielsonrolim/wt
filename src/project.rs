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
}

impl Env {
    pub fn new(cfg: &Config, slug: &str) -> Env {
        Env {
            project: cfg.project.clone(),
            slug: slug.to_string(),
            path: canon(&cfg.root).join(slug),
        }
    }

    pub fn prefix(&self) -> String {
        format!("{}-wt-", self.project)
    }

    pub fn container(&self, kind: Kind) -> String {
        format!("{}{}-{}", self.prefix(), self.slug, kind.name())
    }

    pub fn volume(&self, kind: Kind) -> String {
        format!("{}-data", self.container(kind))
    }

    pub fn labels(&self) -> [(&'static str, String); 2] {
        [
            (LABEL_PROJECT, self.project.clone()),
            (LABEL_SLUG, self.slug.clone()),
        ]
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
        };
        assert_eq!(env.container(Kind::Postgres), "myapp-wt-feat-x-postgres");
        assert_eq!(env.volume(Kind::Redis), "myapp-wt-feat-x-redis-data");
        assert!(env.container(Kind::Mysql).starts_with(&env.prefix()));
    }
}
