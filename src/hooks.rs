use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::Hook;
use crate::sh;

/// Runs each argv in order, without a shell, in its `dir` under `root`; the
/// first failure aborts. Each dir is checked right before its own hook, so an
/// earlier hook may create it, and the resolved path is what the hook gets.
/// `phase` is the phase's key in `.wt.toml` (`hooks.setup`,
/// `apps.<name>.hooks.setup`) and `config` the file it came from, both named
/// in the errors.
pub fn run(
    phase: &str,
    hooks: &[Hook],
    root: &Path,
    env: &[(String, String)],
    config: &Path,
) -> Result<()> {
    for (i, hook) in hooks.iter().enumerate() {
        let Some((program, args)) = hook.argv.split_first() else {
            bail!("{phase}[{i}] is an empty command");
        };
        let cwd = dir(phase, i, hook, root, config)?;
        let line = hook.argv.join(" ");
        if hook.dir == Path::new(".") {
            println!("→ {line}");
        } else {
            println!("→ ({}) {line}", hook.dir.display());
        }
        sh::run(&mut command(program, args, &cwd, env))
            .with_context(|| format!("{phase}[{i}] failed: {line} (in {})", cwd.display()))?;
    }
    Ok(())
}

/// `PWD` follows the working directory: a child does not reset it, and tools
/// that read it would otherwise look where `wt up` was typed.
fn command(program: &str, args: &[String], cwd: &Path, env: &[(String, String)]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("PWD", cwd);
    cmd
}

/// The dirs of a phase, from index `skip` on, that are not there yet, as
/// notes for the user: a typo would otherwise surface only when the hook's
/// turn comes. A dir that exists but cannot be used is left to `run`.
fn not_yet_there(phase: &str, hooks: &[Hook], skip: usize, root: &Path) -> Vec<String> {
    hooks
        .iter()
        .enumerate()
        .skip(skip)
        .filter(|(_, hook)| {
            fs::metadata(root.join(&hook.dir))
                .is_err_and(|e| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory))
        })
        .map(|(i, hook)| {
            format!(
                "{phase}[{i}] runs in {}, which does not exist yet; an earlier hook must create it",
                hook.dir.display()
            )
        })
        .collect()
}

/// Warnings to print before the first hook, for dirs an earlier hook could
/// still create. setup[0] has no earlier hook, so `run` fails on it right
/// away; after_services[0] can only be created by a setup hook, so without
/// setup it is left to the check `up` makes once setup ends.
/// `prefix` is the app's key prefix (`apps.<name>.`, empty for the default).
pub fn early_notes(
    prefix: &str,
    setup: &[Hook],
    after_services: &[Hook],
    root: &Path,
) -> Vec<String> {
    let mut notes = not_yet_there(&format!("{prefix}hooks.setup"), setup, 1, root);
    let skip = usize::from(setup.is_empty());
    notes.extend(not_yet_there(
        &format!("{prefix}hooks.after_services"),
        after_services,
        skip,
        root,
    ));
    notes
}

/// The hook's working directory with symlinks resolved: an existing directory
/// still inside the worktree.
pub fn dir(phase: &str, i: usize, hook: &Hook, root: &Path, config: &Path) -> Result<PathBuf> {
    inside(&format!("{phase}[{i}] runs in"), &hook.dir, root, config)
}

/// `root.join(rel)` with symlinks resolved: an existing directory still
/// inside the worktree. `subject` opens each error ("hooks.setup[0] runs in",
/// "apps.b.dir is"), and `config` is the file to fix.
pub fn inside(subject: &str, rel: &Path, root: &Path, config: &Path) -> Result<PathBuf> {
    let again = format!(
        "fix {} or the checkout and run wt up again; the worktree is kept",
        config.display()
    );
    let real_root = root
        .canonicalize()
        .with_context(|| format!("could not resolve the worktree {}", root.display()))?;
    let dir = root.join(rel);
    let shown = dir.display();
    let meta = match fs::metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            bail!("{subject} {shown}, which does not exist in the checkout; {again}")
        }
        Err(e) => {
            return Err(e).with_context(|| format!("{subject} {shown}, which could not be read"));
        }
    };
    if !meta.is_dir() {
        bail!("{subject} {shown}, which is not a directory; {again}");
    }
    let real = dir
        .canonicalize()
        .with_context(|| format!("{subject} {shown}, which could not be resolved"))?;
    if !real.starts_with(&real_root) {
        bail!(
            "{subject} {shown}, which leaves the worktree (it resolves to {}); {again}",
            real.display()
        );
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::config::Hook;

    const CFG: &str = "/cfg/.wt.toml";

    fn run(phase: &str, hooks: &[Hook], root: &Path, env: &[(String, String)]) -> Result<()> {
        super::run(&format!("hooks.{phase}"), hooks, root, env, Path::new(CFG))
    }

    fn hook(argv: &[&str], dir: &str) -> Hook {
        Hook {
            argv: argv.iter().map(|a| a.to_string()).collect(),
            dir: PathBuf::from(dir),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wt-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("apps/web")).unwrap();
        dir
    }

    #[test]
    fn each_hook_runs_in_its_dir() {
        let root = scratch("cwd");
        run(
            "setup",
            &[
                hook(&["touch", "in-web"], "apps/web"),
                hook(&["touch", "in-root"], "."),
            ],
            &root,
            &[],
        )
        .unwrap();
        assert!(root.join("apps/web/in-web").is_file());
        assert!(root.join("in-root").is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failing_hook_stops_the_rest_and_names_its_dir() {
        let root = scratch("fail");
        let err = format!(
            "{:#}",
            run(
                "setup",
                &[hook(&["false"], "apps/web"), hook(&["touch", "after"], ".")],
                &root,
                &[],
            )
            .unwrap_err()
        );
        assert!(err.contains("hooks.setup[0] failed: false"), "{err}");
        let shown = root.canonicalize().unwrap().join("apps/web");
        assert!(err.contains(&shown.display().to_string()), "{err}");
        assert!(!root.join("after").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hook_can_use_a_dir_an_earlier_hook_created() {
        let root = scratch("created");
        run(
            "setup",
            &[
                hook(&["mkdir", "-p", "tmp/build"], "."),
                hook(&["touch", "made"], "tmp/build"),
            ],
            &root,
            &[],
        )
        .unwrap();
        assert!(root.join("tmp/build/made").is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_dir_stops_at_its_own_hook() {
        let root = scratch("phase");
        let err = format!(
            "{:#}",
            run(
                "after_services",
                &[hook(&["touch", "x"], "."), hook(&["true"], "apps/nope")],
                &root,
                &[],
            )
            .unwrap_err()
        );
        assert!(err.contains("hooks.after_services[1]"), "{err}");
        assert!(err.contains("apps/nope"), "{err}");
        assert!(err.contains("does not exist in the checkout"), "{err}");
        assert!(
            err.contains("fix /cfg/.wt.toml or the checkout and run wt up again"),
            "{err}"
        );
        assert!(root.join("x").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_root_level_failure_shows_the_root_alone() {
        let root = scratch("rootfail");
        let err = format!(
            "{:#}",
            run("setup", &[hook(&["false"], ".")], &root, &[]).unwrap_err()
        );
        let shown = root.canonicalize().unwrap().display().to_string();
        assert!(err.contains(&format!("(in {shown})")), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_dir_swapped_for_a_symlink_by_an_earlier_hook_is_refused() {
        let root = scratch("swap");
        let outside =
            std::env::temp_dir().join(format!("wt-hooks-swap-out-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        let swap = format!("rm -r apps/web && ln -s {} apps/web", outside.display());
        let err = format!(
            "{:#}",
            run(
                "setup",
                &[
                    hook(&["sh", "-c", &swap], "."),
                    hook(&["touch", "x"], "apps/web")
                ],
                &root,
                &[],
            )
            .unwrap_err()
        );
        assert!(err.contains("leaves the worktree"), "{err}");
        assert!(!outside.join("x").exists());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn a_symlinked_root_and_an_in_tree_symlink_are_fine() {
        let real = scratch("inlink");
        std::os::unix::fs::symlink("web", real.join("apps/current")).unwrap();
        let root = std::env::temp_dir().join(format!("wt-hooks-rootlink-{}", std::process::id()));
        let _ = fs::remove_file(&root);
        std::os::unix::fs::symlink(&real, &root).unwrap();
        run(
            "setup",
            &[hook(&["touch", "here"], "apps/current")],
            &root,
            &[],
        )
        .unwrap();
        assert!(real.join("apps/web/here").is_file());
        fs::remove_file(&root).unwrap();
        fs::remove_dir_all(&real).unwrap();
    }

    #[test]
    fn an_unreadable_dir_does_not_blame_the_config() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("eacces");
        fs::create_dir_all(root.join("locked/sub")).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::metadata(root.join("locked/sub")).is_ok(); // running as root
        let result = run("setup", &[hook(&["true"], "locked/sub")], &root, &[]);
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("skipped: running as root, so EACCES cannot be produced");
        } else {
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.contains("could not be read"), "{err}");
            assert!(!err.contains(".wt.toml"), "{err}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dirs_that_do_not_exist_yet_are_listed() {
        let root = scratch("later");
        fs::write(root.join("afile"), "").unwrap();
        let hooks = [
            hook(&["true"], "first"),
            hook(&["true"], "apps/web"),
            hook(&["true"], "afile"),
            hook(&["true"], "gen"),
            hook(&["true"], "afile/sub"),
        ];
        let notes = not_yet_there("hooks.after_services", &hooks, 1, &root);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes[0].contains("hooks.after_services[3]") && notes[0].contains("gen"),
            "{notes:?}"
        );
        assert!(notes[1].contains("hooks.after_services[4]"), "{notes:?}");
        assert_eq!(not_yet_there("hooks.setup", &hooks, 0, &root).len(), 3);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn early_notes_skip_hooks_nothing_could_create_a_dir_for() {
        let root = scratch("early");
        let missing = [hook(&["true"], "gen"), hook(&["true"], "gen2")];
        // setup[0] has no earlier hook; setup[1] may be created by setup[0].
        let notes = early_notes("", &missing, &[], &root);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("hooks.setup[1]"), "{notes:?}");
        // With setup, after_services[0] may still be created by it.
        let notes = early_notes("", &[hook(&["true"], ".")], &missing, &root);
        assert_eq!(notes.len(), 2, "{notes:?}");
        // Without setup, nothing can create after_services[0]'s dir.
        let notes = early_notes("", &[], &missing, &root);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("hooks.after_services[1]"), "{notes:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dir_resolves_a_single_hook_with_the_same_rules() {
        let root = scratch("single");
        let cfg = Path::new(CFG);
        let ok = dir(
            "hooks.after_services",
            0,
            &hook(&["x"], "apps/web"),
            &root,
            cfg,
        )
        .unwrap();
        assert_eq!(ok, root.canonicalize().unwrap().join("apps/web"));
        let err = format!(
            "{:#}",
            dir("hooks.after_services", 0, &hook(&["x"], "nope"), &root, cfg).unwrap_err()
        );
        assert!(
            err.contains("hooks.after_services[0]") && err.contains("does not exist"),
            "{err}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_hook_sees_its_dir_as_pwd() {
        let cmd = command("true", &[], Path::new("/w/apps/web"), &[]);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "PWD" && v == Some(std::ffi::OsStr::new("/w/apps/web"))),
            "{:?}",
            cmd.get_envs().collect::<Vec<_>>()
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/w/apps/web")));
    }

    #[test]
    fn a_dir_that_is_a_file_reports_why() {
        let root = scratch("file");
        fs::write(root.join("notadir"), "").unwrap();
        let err = format!(
            "{:#}",
            run("setup", &[hook(&["true"], "notadir")], &root, &[]).unwrap_err()
        );
        assert!(err.contains("is not a directory"), "{err}");
        let err = format!(
            "{:#}",
            run("setup", &[hook(&["true"], "notadir/sub")], &root, &[]).unwrap_err()
        );
        assert!(err.contains("does not exist in the checkout"), "{err}");
        let err = format!(
            "{:#}",
            run("setup", &[hook(&["true"], "gone")], &root, &[]).unwrap_err()
        );
        assert!(err.contains("does not exist in the checkout"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlink_out_of_the_worktree_is_refused() {
        let root = scratch("link");
        let outside = std::env::temp_dir().join(format!("wt-hooks-out-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        let err = format!(
            "{:#}",
            run("setup", &[hook(&["touch", "x"], "out")], &root, &[]).unwrap_err()
        );
        assert!(err.contains("leaves the worktree"), "{err}");
        assert!(!outside.join("x").exists());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn an_empty_argv_is_an_error_not_a_panic() {
        let root = scratch("empty");
        assert!(run("setup", &[hook(&[], ".")], &root, &[]).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_apps_notes_carry_its_prefix() {
        let root = scratch("prefix");
        let notes = early_notes(
            "apps.b.",
            &[hook(&["true"], "."), hook(&["true"], "gen")],
            &[],
            &root,
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("apps.b.hooks.setup[1]"), "{notes:?}");
        let notes = early_notes(
            "apps.b.",
            &[hook(&["true"], ".")],
            &[hook(&["true"], "gen")],
            &root,
        );
        assert!(
            notes[0].starts_with("apps.b.hooks.after_services[0]"),
            "{notes:?}"
        );
        let e = format!(
            "{:#}",
            dir(
                "apps.b.hooks.after_services",
                0,
                &hook(&["x"], "nope"),
                &root,
                Path::new(CFG)
            )
            .unwrap_err()
        );
        assert!(
            e.starts_with("apps.b.hooks.after_services[0] runs in"),
            "{e}"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
