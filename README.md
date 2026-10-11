# wt

English · [Português (Brasil)](README.pt-BR.md)

Gives any app's git worktrees their own databases and web port. `wt up feat/x` creates the worktree and clones the main checkout's development databases into it (Postgres, MySQL/MariaDB, Redis/Valkey, SQLite). It starts each clone on a free port and writes the worktree's `.env` to point at them. `wt down` tears it all down and keeps the branch.

## Why this exists

Two branches sharing one database step on each other. One branch's migration breaks the other, their seeds get mixed, and one parallel test suite knocks over the other. A worktree isolates the code but not the database. `wt` gives each worktree its own byte-for-byte copy of the development database in seconds. On APFS, clonefile means the copy costs no disk space until something writes to it.

Whatever differs between apps lives in a `.wt.toml`. Everything `wt` can find out on its own (image, command, credentials, volume) is read from the source container.

## Installation

```bash
cargo install --path . --root ~/.local   # installs ~/.local/bin/wt
```

Requires `git` and `docker` (Docker Desktop, OrbStack or colima). Uses Herdr, `direnv` and `sqlite3` when they are installed.

## Quick start

```bash
cd ~/Projects/my-app               # the main checkout, with docker compose running
wt init                            # writes .wt.toml from the compose containers
wt up feat/x                       # worktree + cloned databases + web port
cd ~/.herdr/worktrees/my-app/feat-x && source .envrc
bin/dev                            # or whatever the app runs; it reads PORT and DB_PORT from the env/.env
wt ls                              # lists the worktrees and the state of each
wt down feat-x                     # removes containers, volumes and checkout; the branch stays
```

## Commands

### `wt up <branch> [--base REF]`

Run it from the main checkout. If the branch exists, `wt` checks it out; otherwise it creates the branch from `--base` (default `HEAD`). For a branch that already exists, `--base` is ignored with a warning. Running it again is safe. It reuses the worktree, the ports and any volumes it already cloned, and keeps your edits to the worktree's `.env`.

Steps, in order:

1. Creates the worktree at `<root>/<slug>`, through Herdr if it is available and with `git worktree add` otherwise. The slug is the branch name lowercased, with every other character turned into `-`, up to 40 characters: `feat/ENG-12_x` → `feat-eng-12-x`. A branch whose slug is already taken by another branch's worktree (`feat/x` and `feat-x`) is refused.
2. Picks the ports: the web port and one per service, each from its own range. It skips ports published by any container (stopped ones included), ports already given to another worktree of any project, and ports something is listening on.
3. Writes the worktree's `.env`, copied from the main checkout the first time, and `.envrc`.
4. Copies the SQLite files and the `app.copy` paths.
5. Runs `hooks.setup`.
6. For each service, clones the volume and starts the container. It then waits until the service is ready and checks that the data came across.
7. Runs `hooks.after_services`.

### `wt down [slug] [--keep-checkout] [--yes] [--force]`

Removes the worktree's containers and volumes, its checkout and its port record. It never deletes the branch. Without a slug, it uses the worktree you are in (and asks you to step out of it), or shows a list to pick from.

Safeguards:

- It removes only resources carrying this worktree's `wt.project` and `wt.slug` labels. It refuses a container or volume that merely has the expected name, unless you pass `--force`.
- It never removes a source container or a volume mounted in one.
- It refuses a checkout with uncommitted changes; `--force` overrides this.
- It asks for confirmation. Without a terminal, it requires `--yes` (or `WT_YES=1`).

`--keep-checkout` removes only the containers and volumes; the worktree and its web port stay.

### `wt ls`

```
SLUG    BRANCH  WEB   POSTGRES  REDIS  STATUS   DISK
feat-x  feat/x  3001  5433      6380   running  47.69MB+109B
```

| STATUS | Meaning |
|---|---|
| `running` | every container is up |
| `partial` | some containers are up |
| `stopped` | containers created, none running |
| `no-db` | checkout without containers (`wt up <branch>` creates them) |
| `ready` | project without services, checkout present |
| `orphan` | resources left behind, checkout gone (`wt down <slug>`) |

### `wt init [--force]`

Writes a `.wt.toml` from this repo's docker compose containers, matched by image. It also reads the repo's files:

- `Gemfile` → `bundle install`
- `package.json` with `bun.lock`, `pnpm-lock.yaml`, `yarn.lock` or none of them → the matching install
- `bin/rails` → `bin/rails db:prepare`
- git-ignored `*.sqlite3` files in `storage/` and `db/`, except test ones → `[services.sqlite]`
- a `storage/` holding more than its `.keep` → `app.copy`

It won't overwrite an existing `.wt.toml` without `--force`. Review the file before the first `wt up`.

## The `.wt.toml`

`wt` looks, in order, at the path in `WT_CONFIG`, then `<main checkout>/.wt.toml`, then `~/.config/wt/<project>.toml`. The last one is for repos where you can't commit the file. With none of them, `wt up` creates only the worktree, the web port and the `.env`.

Every option, with its default:

```toml
project = "<main checkout's directory name, normalized the way compose does>"
root = "~/.herdr/worktrees/<project>"     # where worktrees live

[app]
port_env = "PORT"           # key that receives the web port; "" turns the web port off
port_range = [3001, 3099]
env_file = ".env"           # copied from the main checkout the first time, then rewritten in place (0600)
envrc = true                # writes an .envrc exporting the web port (skipped if the repo tracks one)
copy = []                   # untracked paths copied from the main checkout

[services.postgres]         # the same options apply to [services.mysql] and [services.redis]
source = "<project>-postgres-1"   # the source container; defaults to the name compose gives it
volume = "<detected>"             # the source's data volume
clone = true                      # false: empty volume, same image/command/env
port_range = [5433, 5499]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}" }
database = "postgres"             # postgres only: the database used to check the clone

[services.sqlite]
files = []                  # required: files relative to the repo root

[hooks]
setup = []                  # before the services
after_services = []         # once the services are ready
```

### Services

The key of `[services.<type>]` is the type itself: `postgres`, `mysql`, `redis` or `sqlite`. Any other key is an error, and each app has at most one service of each type. An empty table such as `[services.postgres]` is enough when compose follows its default naming.

The config repeats nothing from compose. `wt` reads the image, entrypoint, command, environment variables, user, working directory, container port, data directory, shared-memory size and ulimits from the source container with `docker inspect`, so the clone always boots exactly like the source. The clone also gets the source's bind mounts (config files, init scripts), always read-only, and its tmpfs mounts. Its other named volumes and anything mounted inside the data directory are left out, so the clone never writes into state the source owns. The data volume is the one mounted at the type's default data directory. Failing that, it is the container's only volume. If there are several and none sits in the default place, set `volume`. Data in a bind mount is not supported. Neither is data split across mounts: `wt up` refuses a source with anything mounted inside its data directory (a `pg_wal` bind, say) or a Postgres with `POSTGRES_INITDB_WALDIR`, because only the data volume is copied and the clone would boot without the rest. Set `clone = false` to give such a service an empty database instead. When the source publishes several ports (MySQL's 3306 and 33060), the clone uses the type's standard port if it is among them, and the lowest TCP port otherwise.

| | Postgres | MySQL/MariaDB | Redis/Valkey | SQLite |
|---|---|---|---|---|
| Images (`wt init`) | `postgres`, `postgis`, `pgvector`, `timescaledb` | `mysql`, `mariadb`, `percona` | `redis`, `valkey`, `keydb`, `redis-stack`, `redis-stack-server` | `*.sqlite3` files |
| Data directory | `/var/lib/postgresql/data`, `/var/lib/postgresql` (18+) | `/var/lib/mysql` | `/data` | — |
| Port range | 5433–5499 | 3307–3399 | 6380–6449 | — |
| Default `env` | `DB_HOST`, `DB_PORT` | `DB_HOST`, `DB_PORT` | `REDIS_URL=redis://127.0.0.1:{port}/0` | — |
| Ready when | `pg_isready` over TCP | `mysqladmin ping` | `redis-cli ping` | — |
| Check | `select 1` on `database` | ping | ping | — |

In `env`, `{port}` becomes the clone's host port. The values use `127.0.0.1`, not `localhost`, because the clone publishes on IPv4 only and macOS resolves `localhost` to `::1` first.

The ranges don't overlap each other or the web port. Each sits above its database's default port, so a clone never takes the main stack's port.

### The main database during the clone

A byte-for-byte copy needs the database at rest, because a live data directory holds half-written pages. So `wt up` stops the source container (`docker stop`), copies the volume and starts it again. The source is down only for the seconds the copy takes, and comes back even if the copy fails or you press Ctrl-C. This happens only the first time, since re-runs reuse a cloned volume. A half-copied volume (one without the `.wt-clone-complete` marker) is discarded and copied again. If the marker check itself cannot run, `wt up` stops with an error and leaves the volume alone.

SQLite stops nothing: `sqlite3 .backup` takes a consistent copy even while the app has the file open. Without the `sqlite3` CLI, `wt` copies the file along with its `-wal`/`-shm` files. Each file is copied only once; delete the worktree's copy to copy it again.

### `app.copy`

Copies from the main checkout whatever the worktree doesn't have yet, and never overwrites. A file created or changed inside the worktree survives re-runs. On APFS the copy is a clonefile, so a large directory costs no disk space until something writes to it. The typical case is Active Storage blobs in `storage/`, which the cloned database points at.

### Hooks

Each hook is a list of arguments, run directly in the worktree directory without a shell. Hooks run in order, and the first one to fail stops `wt up`.

```toml
[hooks]
setup = [["bundle", "install"], ["bun", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

Without a shell there are no pipes, `||`, globs or `$VAR`. For those, point at a script in the repo (`["bin/setup"]`) or call the shell explicitly (`["sh", "-c", "…"]`).

Hooks get these variables in their environment: `WT_SLUG`, `WT_BRANCH`, `WT_PATH`, `WT_MAIN_ROOT`, `WT_APP_PORT`, and `WT_POSTGRES_PORT`/`WT_MYSQL_PORT`/`WT_REDIS_PORT` for the configured services only.

## Examples

**Rails with Postgres** ([`examples/rails-postgres.wt.toml`](examples/rails-postgres.wt.toml)): Postgres in compose and Active Storage on disk.

```toml
[app]
copy = ["storage"]

[services.postgres]

[hooks]
setup = [["bundle", "install"], ["bun", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

**Rails 8 with SQLite:** no Docker.

```toml
[app]
copy = ["storage"]

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["bundle", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

**Node with MySQL and Redis:** the app reads `DATABASE_URL`, and the cache starts empty.

```toml
[services.mysql]
env = { DATABASE_URL = "mysql://root:root@127.0.0.1:{port}/app" }

[services.redis]
clone = false

[hooks]
setup = [["pnpm", "install"]]
after_services = [["pnpm", "prisma", "migrate", "deploy"]]
```

## What lives where

| What | Where |
|---|---|
| Worktree | `<root>/<slug>` (default `~/.herdr/worktrees/<project>/<slug>`) |
| Containers | `<project>-wt-<slug>-<type>`, labelled `wt.project`, `wt.slug`, `wt.service` |
| Volumes | `<project>-wt-<slug>-<type>-data`, with the same labels |
| Ports | `~/.local/state/wt/<project>/<slug>.toml` (honours `XDG_STATE_HOME`) |

Ports live outside the checkout so they never show up in `git status`. A re-run reuses the ports in that file. Because the file records every port handed out, two worktrees created while their servers are stopped never get the same web port.

## Integrations

**Herdr.** When `herdr` is installed and its server answers, `wt` creates and removes worktrees through it, closing the open workspace if there is one. The default `root` is Herdr's worktree directory. `WT_NO_HERDR=1` forces plain git, and `WT_TRUST_REPOSITORY=1` passes `--trust-repository` to Herdr.

**direnv.** The `.envrc` exports only the web port. Many servers (`bin/rails server` and `bin/dev`, for example) read `PORT` from the environment before they load `.env`. `wt` runs `direnv allow` on every rewrite. Leaving out the database port is deliberate: a shell that ran `source .envrc` by hand would keep it after returning to the main checkout, pointing the main checkout at the worktree's database.

**Browser sessions.** Cookies ignore the port, so two worktrees on `localhost` share one session and sign each other out. Open each one on its own subdomain: `http://feat-x.localhost:3001`.

## Environment variables

| Variable | Effect |
|---|---|
| `WT_CONFIG` | explicit path to the `.wt.toml` |
| `WT_NO_HERDR=1` | uses `git worktree` even when Herdr is available |
| `WT_TRUST_REPOSITORY=1` | passes `--trust-repository` to `herdr worktree create` |
| `WT_YES=1` | `wt down` without confirmation |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | where the out-of-repo config and the port registry live |

## Development

```bash
cargo test                          # unit tests
cargo clippy --all-targets -- -D warnings
tests/e2e.sh                        # full cycle against a throwaway compose stack (Postgres + Redis + SQLite)
```

## License

[MIT](LICENSE)
