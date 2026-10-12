# wt

A CLI that gives each git worktree its own byte-copied development databases and web port. `README.md` (en-US) is the single source for the `.wt.toml` format and user-facing behaviour, so update it alongside any change to either. `README.pt-BR.md` is its translation: carry every README change over to it in the same edit.

## Invariants

- **The source stack is only stopped and started.** A copy needs the source at rest, so `services::prepare_volume` stops it and registers it in `STOPPED`. Every exit path restarts it: right after the copy, `RestoreOnDrop` on error, and the Ctrl-C handler in `main.rs`. Any new step that stops a source registers it the same way.
- **Labels are the proof of ownership.** `down` removes a container or volume only when its `wt.project` and `wt.slug` labels match, or when the `docker.slug_label` the repo configured holds the slug. `Env::owns` is the one place that decides. A matching name proves nothing and needs `--force`. Every resource `up` creates carries `wt.project`, `wt.slug` and `wt.service`, plus `docker.slug_label` when set. `up` also refuses to reuse a container or volume that `Env::owns` rejects.
- **The clone boots from the source's own config.** Image, entrypoint, cmd, env, user, working dir, container port, data dir, shm size, ulimits and tmpfs and read-only bind mounts all come from `docker inspect` of the source. The config holds only what cannot be discovered, so a clone can never drift from the stack it copied.
- **Hooks are argv, run without a shell.** `Vec<Vec<String>>`, executed with `Command::new`. Anyone who wants shell features writes `["sh", "-c", …]` themselves.
- **Service keys are the four types.** `[services.postgres|mysql|redis|sqlite]`, at most one of each, with no `type` field. `deny_unknown_fields` turns any other key into an error.
- **State lives outside the checkout.** Ports go in `$XDG_STATE_HOME/wt/<project>/<slug>.toml`, so `git status` stays clean, and those files are the cross-project registry `ports::find_free` checks.
- **Re-runs are idempotent.** The published port wins, then the state file, then a free port. The `.env` rewrite reproduces itself, and copies never overwrite, except the `app.refresh` paths, where the main checkout wins and a second run copies nothing. A volume finished by either `.wt-clone-complete` or the legacy `.worktree-clone-complete` marker is reused, never recopied.
- **The branch outlives the worktree by default.** `down` asks about it and keeps it; only `--delete-branch` answers yes without a prompt, and `--yes`/`WT_YES=1` means the defaults.

## Adding a container adapter

1. Add a `Kind` variant in `src/services.rs` and a match arm in each `Kind` method plus `probe`. The `ranges_do_not_overlap` test enforces port ranges that are disjoint from the others and from 3001–3099.
2. Add the field to `Services` and the entry to the list in `config::resolve`.
3. Add a column to the adapter table in both READMEs.

## Commands

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
tests/e2e.sh          # full cycle; needs Docker and the postgres:16.7-alpine, mariadb:11 and redis:7-alpine images
```

`tests/e2e.sh` builds its own compose project (`wte2e`) in a temp dir and cleans it up. Run `wt up` only against that disposable stack: on a real stack, it stops the developer's database during the copy.
