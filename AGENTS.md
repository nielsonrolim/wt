# wt

A CLI that gives each git worktree its own development databases and web port. A service is either a byte-copied container (`mode = "container"`, the default) or a set of logical databases copied inside the source instance (`mode = "database"`). `README.md` (en-US) is the single source for the `.wt.toml` format and user-facing behaviour, so update it alongside any change to either. `README.pt-BR.md` is its translation: carry every README change over to it in the same edit.

## Invariants

- **The source stack is only stopped and started.** A copy needs the source at rest, so `services::prepare_volume` stops it and registers it in `STOPPED`. Every exit path restarts it: right after the copy, `RestoreOnDrop` on error, and the Ctrl-C handler in `main.rs`. Any new step that stops a source registers it the same way. `mode = "database"` never stops the source: its copy is a dump taken while the server runs.
- **Labels are the proof of ownership.** `down` removes a container or volume only when its `wt.project` and `wt.slug` labels match, or when the `docker.slug_label` the repo configured holds the slug. `Env::owns` is the one place that decides. A matching name proves nothing and needs `--force`. Every resource `up` creates carries `wt.project`, `wt.slug` and `wt.service`, plus `docker.slug_label` when set. `up` also refuses to reuse a container or volume that `Env::owns` rejects. A copied database carries the same pair in its Postgres database comment or its MySQL `__wt_meta` table; `State.databases` is the registry `down` reads, so a copy whose service left the config is still removed.
- **The clone boots from the source's own config.** Image, entrypoint, cmd, env, user, working dir, container port, data dir, shm size, ulimits and tmpfs and read-only bind mounts all come from `docker inspect` of the source. The config holds only what cannot be discovered, so a clone can never drift from the stack it copied. In `mode = "database"` the copy inherits the source database's owner, encoding and locale instead.
- **Hooks are argv, run without a shell.** `Vec<Vec<String>>`, executed with `Command::new`. Anyone who wants shell features writes `["sh", "-c", …]` themselves.
- **Service keys are the four types.** `[services.postgres|mysql|redis|sqlite]`, at most one of each, with no `type` field. `deny_unknown_fields` turns any other key into an error. `redis` is refused in `mode = "database"` (no separate databases to copy) and `sqlite` has no mode (its file copy already is the database).
- **State lives outside the checkout.** Ports go in `$XDG_STATE_HOME/wt/<project>/<slug>.toml`, so `git status` stays clean, and those files are the cross-project registry `ports::find_free` checks. The same file lists the `[[databases]]` copies and their source containers.
- **Re-runs are idempotent.** The published port wins, then the state file, then a free port; a database-mode service uses the source's published port. The `.env` rewrite reproduces itself, and copies never overwrite, except the `app.refresh` paths, where the main checkout wins and a second run copies nothing. A volume finished by either `.wt-clone-complete` or the legacy `.worktree-clone-complete` marker is reused, never recopied. A database copy finished by `complete=1` in its marker is reused; `complete=0` is dropped and copied again, and only that copy. A database removed from `databases` is kept (with a warning) and dropped by `down`.
- **The branch outlives the worktree by default.** `down` asks about it and keeps it; only `--delete-branch` answers yes without a prompt, and `--yes`/`WT_YES=1` means the defaults.
- **A mode change is refused, not migrated.** `up` refuses, before `resolve_ports`, `state.save` and `create_worktree`, when a container-mode service has a container or volume of ours (it is now `mode = "database"`), or when a database-mode state has copies and the service is now `mode = "container"` with no container of ours. Both point at `wt down <slug>`.

## Database mode

`src/database.rs` owns it; `services.rs` stays container-only. `Kind` remains the one place the adapters differ.

- `databases` is the list of source databases; `database` is the one-item shorthand, and setting both is an error. With neither, detect the one the image's env names. `databases[0]` is primary.
- `env` values may use `{database}` (primary) and `{database:<source>}`. Config refuses a database-mode service whose `env` references no database, an unknown source, braces in a name, or `databases` outside database mode.
- Targets are `wt_<project>_<slug>__<source>`: sanitized, at most 63 bytes, order-independent, hash-suffixed on truncation, and refused when two sources collide. A target is never the source.
- The dump is written to a file inside the container and restored from it (`pg_dump > file && psql < file`, `mysqldump > file && mysql < file`), with a `trap` cleanup. Never a pipe between dump and restore, and no `set -o pipefail`: without it a failed dump would still report success. MySQL adds `--set-gtid-purged=OFF` only outside MariaDB, always dumps routines/triggers/events, and grants `MYSQL_USER` on the copy.
- Markers: `wt.project`, `wt.slug`, `wt.complete` in the Postgres comment or the `__wt_meta` table. `Missing` creates, `Reuse` reuses, `Recreate` drops and copies, `Foreign` is refused (and `down --force` may drop it).
- `up` plans and validates every source and target before `state.save` and before the checkout exists. `ls` precedence: `orphan`; `unknown` when Docker is unreachable and copies exist; container status; then the database verdict (`unknown` when a source is stopped or gone, else `shared`/`partial`/`no-db`); then the old `ready`/`no-db`.

## Adding a container adapter

1. Add a `Kind` variant in `src/services.rs` and a match arm in each `Kind` method plus `probe`. The `ranges_do_not_overlap` test enforces port ranges that are disjoint from the others and from 3001–3099.
2. Add the field to `Services` and the entry to the list in `config::resolve`.
3. Add a column to the adapter table in both READMEs.

A new adapter supports `mode = "container"` only. To accept `mode = "database"`, also add the marker, create/copy/drop and existence branches in `src/database.rs`, allow it in `config.rs`, and document it in the adapter table; `state.databases` resolves the service through `Kind::from_name`.

## Commands

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
tests/e2e.sh          # full cycle; needs Docker and the postgres:16.7-alpine, mariadb:11 and redis:7-alpine images
```

`tests/e2e.sh` builds its own compose project (`wte2e`) in a temp dir and cleans it up. Run `wt up` only against that disposable stack: on a real stack, it stops the developer's database during the copy.
