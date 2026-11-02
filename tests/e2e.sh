#!/usr/bin/env bash
# Full wt cycle against a throwaway compose stack (Postgres + MySQL + Redis + SQLite)
# in a temp repo. Touches nothing but resources named or labelled `wte2e`.
#
#   tests/e2e.sh            # builds, runs, cleans up
#   KEEP=1 tests/e2e.sh     # leaves the temp dir for inspection
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
PROJECT=wte2e
WORK="$(mktemp -d "${TMPDIR:-/tmp}/wt-e2e.XXXXXX")"
REPO="$WORK/$PROJECT"
WT="$HERE/target/debug/wt"

export XDG_STATE_HOME="$WORK/state"
export XDG_CONFIG_HOME="$WORK/config"
export WT_NO_HERDR=1

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() {
  echo "FAIL: $*" >&2
  exit 1
}

cleanup() {
  local rc=$?
  set +e
  # Drop copies inside the instances before their volumes go away, so a
  # leftover never outlives the fixture (and the final step can prove it).
  for db in $(docker compose exec -T postgres psql -qtAX -U app -d postgres \
    -c "select datname from pg_database where datname like 'wt_wte2e%'" 2>/dev/null); do
    docker compose exec -T postgres psql -qU app -d postgres \
      -c "drop database $db with (force)" >/dev/null 2>&1
  done
  for db in $(docker compose exec -T mariadb mariadb -uroot -papp -N -B \
    -e "select schema_name from information_schema.schemata where schema_name like 'wt_wte2e%'" 2>/dev/null); do
    docker compose exec -T mariadb mariadb -uroot -papp \
      -e "drop database $db" >/dev/null 2>&1
  done
  for id in $(docker ps -aq --filter "label=wt.project=$PROJECT"); do docker rm -f "$id" >/dev/null; done
docker rm -f "$PROJECT-helper" >/dev/null 2>&1
  for v in $(docker volume ls -q --filter "label=wt.project=$PROJECT"); do docker volume rm -f "$v" >/dev/null; done
  (cd "$REPO" 2>/dev/null && docker compose --profile extra down -v >/dev/null 2>&1)
  docker rmi "$PROJECT-slow:1" >/dev/null 2>&1
  if [ "${KEEP:-}" = 1 ]; then echo "kept $WORK"; else rm -rf "$WORK"; fi
  [ $rc -eq 0 ] && printf '\n\033[32mPASS\033[0m\n' || printf '\n\033[31mFAILED (%s)\033[0m\n' "$rc"
  exit $rc
}
trap cleanup EXIT

step "build"
(cd "$HERE" && cargo build --quiet)

step "temp repo with a compose stack"
mkdir -p "$REPO/storage" "$REPO/nested" "$WORK/root"
cd "$REPO"
git init --quiet -b main
cat >docker-compose.yaml <<'EOF'
services:
  postgres:
    image: postgres:16.7-alpine
    ports: ["127.0.0.1::5432"]
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: app
    command: postgres -c max_locks_per_transaction=256
    healthcheck:
      test: ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "app", "-d", "app"]
      interval: 1s
      retries: 60
    volumes:
      - pg-data:/var/lib/postgresql/data
  mariadb:
    image: mariadb:11
    ports: ["127.0.0.1::3306", "127.0.0.1::33060"]
    environment:
      MARIADB_ROOT_PASSWORD: app
      MARIADB_DATABASE: app
      MARIADB_USER: app
      MARIADB_PASSWORD: app
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 1s
      retries: 60
    volumes:
      - mariadb-data:/var/lib/mysql
  redis:
    image: redis:7-alpine
    volumes:
      - redis-data:/data
  # A source with no published port: database mode cannot reach it from the host.
  pg-noport:
    image: postgres:16.7-alpine
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: app
    healthcheck:
      test: ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "app", "-d", "app"]
      interval: 1s
      retries: 60
    volumes:
      - pg-noport-data:/var/lib/postgresql/data
  # A source whose data is split across mounts: wt must refuse to clone it.
  z-nested:
    image: postgres:16.7-alpine
    command: ["sleep", "infinity"]
    volumes:
      - z-nested-data:/var/lib/postgresql/data
      - ./nested:/var/lib/postgresql/data/pg_wal
  # Runs as a non-root user, so `docker exec` does too.
  mariadb-user:
    profiles: [extra]
    image: mariadb:11
    user: "999:999"
    ports: ["127.0.0.1::3306"]
    environment:
      MARIADB_ROOT_PASSWORD: app
      MARIADB_DATABASE: app
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 1s
      retries: 60
    volumes:
      - mariadb-user-data:/var/lib/mysql
  # An image whose `cp` is slow, so a copy can be interrupted while it runs.
  slow:
    profiles: [extra]
    image: wte2e-slow:1
    volumes:
      - slow-data:/data
volumes:
  mariadb-user-data:
  slow-data:
  pg-data:
  mariadb-data:
  redis-data:
  pg-noport-data:
  z-nested-data:
EOF
printf '/.env*\n/.envrc\n/hook-*.out\n/storage/*\n!/storage/.keep\n/agents/\n' >.gitignore
touch storage/.keep
printf 'SECRET=main\nDB_PORT=5432\n' >.env
echo '{}' >package.json
git add -A && git commit --quiet -m init

docker compose up -d --wait >/dev/null 2>&1
docker compose exec -T postgres psql -qU app -d app -c "create table marker(v text); insert into marker values ('cloned');" >/dev/null
# Extra logical databases, a non-default encoding and an object owned by another role.
docker compose exec -T postgres psql -qU app -d postgres -c "create database app_cache" >/dev/null
docker compose exec -T postgres psql -qU app -d app_cache -c "create table marker(v text); insert into marker values ('cache');" >/dev/null
docker compose exec -T postgres psql -qU app -d postgres -c "create database app_latin template template0 encoding 'LATIN1' lc_collate 'C' lc_ctype 'C'" >/dev/null
docker compose exec -T postgres psql -qU app -d app_latin -c "create table marker(v text); insert into marker values ('latin');" >/dev/null
docker compose exec -T postgres psql -qU app -d app -c "create table primary_only(v int); insert into primary_only values (1); create role app2; create table owned_by_app2(v int); alter table owned_by_app2 owner to app2;" >/dev/null
docker compose exec -T redis sh -c 'redis-cli set marker cloned >/dev/null && redis-cli save >/dev/null'
docker compose exec -T mariadb mariadb -uroot -papp app -e "create table marker(v text); insert into marker values ('cloned');" >/dev/null
docker compose exec -T mariadb mariadb -uroot -papp -e "create database app_aux character set utf8mb4 collate utf8mb4_bin" >/dev/null
docker compose exec -T mariadb mariadb -uroot -papp app_aux -e "create table marker(v text); insert into marker values ('aux');" >/dev/null
docker compose exec -T mariadb mariadb -uroot -papp app_aux -e "create table __wt_meta(k varchar(64), v varchar(255)); insert into __wt_meta values ('noise','x');" >/dev/null
docker compose exec -T mariadb mariadb -uroot -papp app -e "create procedure proc() select 1; create trigger trg before insert on marker for each row set new.v = new.v;" >/dev/null
sqlite3 storage/development.sqlite3 "create table marker(v text); insert into marker values ('cloned');"
mkdir -p storage/ab && echo blob >storage/ab/blob
mkdir -p agents && echo v1 >agents/a.md

step "wt init"
"$WT" init
grep -q '^\[services.postgres\]' .wt.toml || fail "init missed postgres"
grep -q '^\[services.mysql\]' .wt.toml || fail "init missed mysql"
grep -q "source = \"$PROJECT-mariadb-1\"" .wt.toml || fail "init missed the mysql source override"
grep -q '^\[services.redis\]' .wt.toml || fail "init missed redis"
grep -q 'storage/development.sqlite3' .wt.toml || fail "init missed sqlite"
grep -q 'copy = \["storage"\]' .wt.toml || fail "init missed storage"
grep -q 'mode =' .wt.toml && fail "init generated a mode = line"
"$WT" init 2>/dev/null && fail "init overwrote without --force"

cat >.wt.toml <<EOF
root = "$WORK/root"

[app]
copy = ["storage"]
refresh = ["agents"]

[services.postgres]
database = "app"
check_sql = "select count(*) from marker"

[services.mysql]
source = "$PROJECT-mariadb-1"
env = { MYSQL_HOST = "127.0.0.1", MYSQL_PORT = "{port}" }

[services.redis]

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["sh", "-c", "echo setup \$WT_SLUG > hook-setup.out"]]
after_services = [["sh", "-c", "echo \$WT_POSTGRES_PORT \$WT_MYSQL_PORT \$WT_REDIS_PORT \$WT_APP_PORT > hook-after.out"]]
EOF

step "wt up feat/x"
PG_STARTED="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
"$WT" up feat/x
WTP="$WORK/root/feat-x"
[ -d "$WTP" ] || fail "no checkout"
[ "$(git -C "$WTP" rev-parse --abbrev-ref HEAD)" = feat/x ] || fail "wrong branch"

PG_PORT="$(sed -n 's/^DB_PORT=//p' "$WTP/.env")"
MYSQL_PORT="$(sed -n 's/^MYSQL_PORT=//p' "$WTP/.env")"
REDIS_URL="$(sed -n 's/^REDIS_URL=//p' "$WTP/.env")"
APP_PORT="$(sed -n 's/^PORT=//p' "$WTP/.env")"
echo "ports: pg=$PG_PORT mysql=$MYSQL_PORT redis=$REDIS_URL app=$APP_PORT"
[ "$(grep -c '^DB_PORT=' "$WTP/.env")" = 1 ] || fail "DB_PORT duplicated"
grep -q '^SECRET=main$' "$WTP/.env" || fail ".env lost the main checkout's keys"
[ "$(stat -c %a "$WTP/.env" 2>/dev/null || stat -f %Lp "$WTP/.env")" = 600 ] || fail ".env is not 0600"
grep -q "export PORT=$APP_PORT" "$WTP/.envrc" || fail ".envrc"
[ -f "$XDG_STATE_HOME/wt/$PROJECT/feat-x.toml" ] || fail "no state file"

[ "$(docker exec "$PROJECT-wt-feat-x-postgres" psql -qtAX -U app -d app -c 'select v from marker')" = cloned ] ||
  fail "postgres data not cloned"
psql_port="$(docker port "$PROJECT-wt-feat-x-postgres" 5432/tcp)"
[ "$psql_port" = "127.0.0.1:$PG_PORT" ] || fail "postgres published on $psql_port, not $PG_PORT"
[ "$(docker exec "$PROJECT-wt-feat-x-redis" redis-cli get marker)" = cloned ] || fail "redis data not cloned"
[ "$(docker exec "$PROJECT-wt-feat-x-mysql" mariadb -uroot -papp app -N -B -e 'select v from marker')" = cloned ] || fail "mysql data not cloned"
mysql_port="$(docker port "$PROJECT-wt-feat-x-mysql" 3306/tcp)"
[ "$mysql_port" = "127.0.0.1:$MYSQL_PORT" ] || fail "mysql published on $mysql_port, not $MYSQL_PORT"
[ "$(sqlite3 "$WTP/storage/development.sqlite3" 'select v from marker')" = cloned ] || fail "sqlite not copied"
[ "$(cat "$WTP/storage/ab/blob")" = blob ] || fail "storage not copied"
[ "$(cat "$WTP/hook-setup.out")" = "setup feat-x" ] || fail "setup hook"
read -r hp hm hr ha <"$WTP/hook-after.out"
[ "$hp" = "$PG_PORT" ] && [ "$hm" = "$MYSQL_PORT" ] && [ "$ha" = "$APP_PORT" ] && [ -n "$hr" ] || fail "after_services hook env"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-postgres-1")" = true ] || fail "source not restarted"
[ "$PG_STARTED" != "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" ] ||
  fail "a byte copy must stop and restart the source"

[ "$(cat "$WTP/agents/a.md")" = v1 ] || fail "refresh path not copied"

step "wt up feat/x again (idempotent)"
echo "LOCAL=1" >>"$WTP/.env"
echo v2 >agents/a.md
echo mine >"$WTP/agents/local.md"
echo changed >"$WTP/storage/ab/blob"
RERUN_STARTED="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
"$WT" up feat/x | tee "$WORK/rerun.log"
[ "$(cat "$WTP/agents/a.md")" = v2 ] || fail "refresh did not follow the main checkout"
[ "$(cat "$WTP/agents/local.md")" = mine ] || fail "refresh removed a worktree-only file"
[ "$(cat "$WTP/storage/ab/blob")" = changed ] || fail "copy overwrote a worktree edit"
"$WT" up feat/x | grep -q 'Refreshed agents' && fail "refresh copied again with nothing to change"
grep -q "Reusing $PROJECT-wt-feat-x-postgres-data" "$WORK/rerun.log" || fail "volume not reused"
[ "$(sed -n 's/^DB_PORT=//p' "$WTP/.env")" = "$PG_PORT" ] || fail "port changed on re-run"
[ "$(sed -n 's/^PORT=//p' "$WTP/.env")" = "$APP_PORT" ] || fail "app port changed on re-run"
grep -q '^LOCAL=1$' "$WTP/.env" || fail "hand edit lost"
[ "$(grep -c 'Written by wt up' "$WTP/.env")" = 1 ] || fail "banner stacked"
[ "$RERUN_STARTED" = "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" ] ||
  fail "a re-run must not touch the source"

step "a half-copied volume held by a helper is discarded"
# Reproduce the Ctrl-C race without a signal: a helper still mounts the volume
# when the half-copy is discarded.
docker run --rm -v "$PROJECT-wt-feat-x-postgres-data:/v" --entrypoint sh \
  postgres:16.7-alpine -euc 'rm -f /v/.wt-clone-complete'
docker rm -f "$PROJECT-wt-feat-x-postgres" >/dev/null
docker run -d --name "$PROJECT-helper" -v "$PROJECT-wt-feat-x-postgres-data:/v" \
  --label wt.helper=1 --label "wt.project=$PROJECT" --label wt.slug=feat-x \
  postgres:16.7-alpine sleep 120 >/dev/null
"$WT" up feat/x >"$WORK/discard.log" 2>&1
grep -q 'Discarding half-copied' "$WORK/discard.log" || fail "half-copied volume was not discarded"
docker inspect "$PROJECT-helper" >/dev/null 2>&1 && fail "the helper holding the volume survived"
[ "$(docker run --rm -v "$PROJECT-wt-feat-x-postgres-data:/v" --entrypoint sh \
  postgres:16.7-alpine -euc 'test -f /v/.wt-clone-complete && echo yes')" = yes ] ||
  fail "the recopied volume has no marker"

step "a volume without a marker is not discarded while another container uses it"
# What `clone = false` seeded has no completion marker either, so turning `clone`
# on must not take the running clone and its data away.
docker run --rm -v "$PROJECT-wt-feat-x-postgres-data:/v" --entrypoint sh \
  postgres:16.7-alpine -euc 'rm -f /v/.wt-clone-complete'
docker run -d --name "$PROJECT-helper" -v "$PROJECT-wt-feat-x-postgres-data:/v" \
  postgres:16.7-alpine sleep 120 >/dev/null
CLONE_STARTED="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-wt-feat-x-postgres")"
if "$WT" up feat/x >"$WORK/in-use.log" 2>&1; then fail "up discarded a volume in use"; fi
grep -q 'will not discard it' "$WORK/in-use.log" || fail "wrong error for a volume in use"
[ "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-wt-feat-x-postgres")" = "$CLONE_STARTED" ] ||
  fail "the running clone was touched"
docker inspect "$PROJECT-helper" >/dev/null 2>&1 || fail "another tool's container was removed"
docker rm -f "$PROJECT-helper" >/dev/null
docker run --rm -v "$PROJECT-wt-feat-x-postgres-data:/v" --entrypoint sh \
  postgres:16.7-alpine -euc 'touch /v/.wt-clone-complete'

step "a branch with the same slug is refused"
"$WT" up feat-x 2>/dev/null && fail "feat-x reused feat/x's worktree"
[ "$(git -C "$WTP" rev-parse --abbrev-ref HEAD)" = feat/x ] || fail "feat/x checkout changed"

step "second worktree: different ports, and clone = false starts empty"
cat >.wt.toml <<EOF
root = "$WORK/root"

[app]
copy = ["storage"]

[services.postgres]
database = "app"

[services.mysql]
source = "$PROJECT-mariadb-1"
env = { MYSQL_HOST = "127.0.0.1", MYSQL_PORT = "{port}" }

[services.redis]
clone = false

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["sh", "-c", "echo setup \$WT_SLUG > hook-setup.out"]]
after_services = [["sh", "-c", "echo \$WT_POSTGRES_PORT \$WT_MYSQL_PORT \$WT_REDIS_PORT \$WT_APP_PORT > hook-after.out"]]
EOF
"$WT" up feat/y >/dev/null
[ "$(sed -n 's/^DB_PORT=//p' "$WORK/root/feat-y/.env")" != "$PG_PORT" ] || fail "pg port reused"
[ "$(sed -n 's/^PORT=//p' "$WORK/root/feat-y/.env")" != "$APP_PORT" ] || fail "app port reused"
[ -z "$(docker exec "$PROJECT-wt-feat-y-redis" redis-cli get marker)" ] || fail "clone=false copied redis data"

step "wt ls"
"$WT" ls | tee "$WORK/ls.log"
grep -q '^feat-x .*running' "$WORK/ls.log" || fail "ls feat-x"
grep -q '^feat-y .*running' "$WORK/ls.log" || fail "ls feat-y"

step "wt down refuses a dirty checkout"
echo dirty >"$WTP/new-file"
"$WT" down feat-x --yes 2>/dev/null && fail "down removed a dirty checkout"
rm "$WTP/new-file" "$WTP"/hook-*.out

step "wt down refuses an unlabelled lookalike"
docker volume create "$PROJECT-wt-intruder-postgres-data" >/dev/null
"$WT" down intruder --yes 2>/dev/null && fail "down removed an unlabelled volume"
docker volume inspect "$PROJECT-wt-intruder-postgres-data" >/dev/null || fail "unlabelled volume is gone"
docker volume rm "$PROJECT-wt-intruder-postgres-data" >/dev/null

step "wt down from inside is refused"
(cd "$WTP" && "$WT" down --yes 2>/dev/null) && fail "down ran from inside"

step "wt down feat-y --keep-checkout"
"$WT" down feat-y --keep-checkout --yes
[ -d "$WORK/root/feat-y" ] || fail "keep-checkout removed the checkout"
[ -z "$(docker ps -aq --filter "label=wt.slug=feat-y")" ] || fail "keep-checkout left containers"
[ -z "$(docker volume ls -q --filter "label=wt.slug=feat-y")" ] || fail "keep-checkout left volumes"
grep -q '^app_port' "$XDG_STATE_HOME/wt/$PROJECT/feat-y.toml" || fail "keep-checkout dropped the web port"
git show-ref --verify --quiet refs/heads/feat/y || fail "keep-checkout went after the branch"

step "wt down refuses --keep-checkout with --delete-branch"
rm -f "$WORK/root/feat-y"/hook-*.out
"$WT" down feat-y --keep-checkout --delete-branch --yes 2>"$WORK/clash.log" && fail "down accepted contradictory flags"
grep -q 'contradict each other' "$WORK/clash.log" || fail "wrong error for contradictory flags"
[ -d "$WORK/root/feat-y" ] || fail "the refused run removed the checkout"

step "wt down feat-x"
"$WT" down feat-x --yes
"$WT" down feat-y --yes --delete-branch >/dev/null
[ -z "$(docker ps -aq --filter "label=wt.project=$PROJECT")" ] || fail "containers left"
[ -z "$(docker volume ls -q --filter "label=wt.project=$PROJECT")" ] || fail "volumes left"
[ ! -e "$WTP" ] || fail "checkout left"
[ ! -e "$XDG_STATE_HOME/wt/$PROJECT/feat-x.toml" ] || fail "state left"
git show-ref --verify --quiet refs/heads/feat/x || fail "branch deleted without --delete-branch"
git show-ref --verify --quiet refs/heads/feat/y && fail "--delete-branch kept the branch"
docker volume inspect "${PROJECT}_pg-data" >/dev/null || fail "source volume touched"
"$WT" ls | grep -q 'No worktree environments' || fail "ls not empty"

# ---------------------------------------------------------------------------
# Database mode: copies logical databases inside the source instance.
# ---------------------------------------------------------------------------
STATE_DIR="$XDG_STATE_HOME/wt/$PROJECT"
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
MYSQL_HOST="$(docker port "$PROJECT-mariadb-1" 3306/tcp | sed 's/.*://')"
pg() { docker exec "$PROJECT-postgres-1" psql -qtAX -U app -d postgres -c "$1"; }
pg_of() { docker exec "$PROJECT-postgres-1" psql -qtAX -U app -d "$1" -c "$2"; }
my() {
  if [ "$#" -ge 2 ]; then
    docker exec "$PROJECT-mariadb-1" mariadb -uroot -papp -N -B -e "$1" "$2"
  else
    docker exec "$PROJECT-mariadb-1" mariadb -uroot -papp -N -B -e "$1"
  fi
}

step "database mode: up copies two logical databases without a container"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}", DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
check_sql = "select count(*) from primary_only"

[hooks]
after_services = [["sh", "-c", "printf '%s|%s|%s' \"\$WT_POSTGRES_DATABASE\" \"\$WT_POSTGRES_DATABASES\" \"\$WT_POSTGRES_PORT\" > hook-db.out"]]
EOF
STARTED_AT="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
"$WT" up db/a | tee "$WORK/db-a.log"
DBA="$WORK/root/db-a"
[ -d "$DBA" ] || fail "no db/a checkout"
[ -z "$(docker ps -aq --filter "label=wt.slug=db-a")" ] || fail "database mode created a container"
[ -z "$(docker volume ls -q --filter "label=wt.slug=db-a")" ] || fail "database mode created a volume"
[ "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" = "$STARTED_AT" ] || fail "database mode stopped the source"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-postgres-1")" = true ] || fail "source not running"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_a__%'")" = 2 ] || fail "expected two copies"
[ "$(pg "select shobj_description(oid,'pg_database') from pg_database where datname='wt_wte2e_db_a__app'")" = "wt.project=wte2e;wt.slug=db-a;wt.complete=1" ] || fail "postgres marker"
[ "$(pg_of wt_wte2e_db_a__app 'select v from marker')" = cloned ] || fail "app data not copied"
[ "$(pg_of wt_wte2e_db_a__app_cache 'select v from marker')" = cache ] || fail "cache data not copied"
[ "$(pg_of wt_wte2e_db_a__app "select tableowner from pg_tables where tablename='owned_by_app2'")" = app2 ] || fail "object owner not preserved"
[ "$(pg "select pg_get_userbyid(datdba) from pg_database where datname='wt_wte2e_db_a__app'")" = "$(pg "select pg_get_userbyid(datdba) from pg_database where datname='app'")" ] || fail "database owner not preserved"
grep -q "^DATABASE_URL=postgres://app:app@127.0.0.1:$HOST/wt_wte2e_db_a__app$" "$DBA/.env" || fail "DATABASE_URL"
grep -q "^CACHE_URL=postgres://app:app@127.0.0.1:$HOST/wt_wte2e_db_a__app_cache$" "$DBA/.env" || fail "CACHE_URL"
[ "$(sed -n 's/^DB_PORT=//p' "$DBA/.env")" = "$HOST" ] || fail "DB_PORT is not the source port"
[ "$(stat -c %a "$DBA/.env" 2>/dev/null || stat -f %Lp "$DBA/.env")" = 600 ] || fail ".env mode"
[ "$(grep -c 'Written by wt up' "$DBA/.env")" = 1 ] || fail "banner"
[ "$(cat "$DBA/hook-db.out")" = "wt_wte2e_db_a__app|app=wt_wte2e_db_a__app,app_cache=wt_wte2e_db_a__app_cache|$HOST" ] || fail "hook env"
grep -qE "postgres +127.0.0.1:$HOST \(databases wt_wte2e_db_a__app, wt_wte2e_db_a__app_cache\)" "$WORK/db-a.log" || fail "report"
[ "$(sed -n 's/^postgres = //p' "$STATE_DIR/db-a.toml")" = "$HOST" ] || fail "state port"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-a.toml")" = 2 ] || fail "state copies"
grep -q 'target = "wt_wte2e_db_a__app"' "$STATE_DIR/db-a.toml" || fail "state target app"
grep -q 'target = "wt_wte2e_db_a__app_cache"' "$STATE_DIR/db-a.toml" || fail "state target cache"

step "database mode: encoding and collation follow the source"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app_latin"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/enc >/dev/null
ENC="$(pg "select pg_encoding_to_char(encoding)||'|'||datcollate||'|'||datctype from pg_database where datname='app_latin'")"
TARGET_ENC="$(pg "select pg_encoding_to_char(encoding)||'|'||datcollate||'|'||datctype from pg_database where datname='wt_wte2e_db_enc__app_latin'")"
[ "$ENC" = "$TARGET_ENC" ] || fail "encoding/collation not copied"
[ "$TARGET_ENC" = "LATIN1|C|C" ] || fail "fixture encoding wrong: $TARGET_ENC"
[ "$(pg_of wt_wte2e_db_enc__app_latin 'select v from marker')" = latin ] || fail "latin data not copied"
"$WT" down db/enc --yes >/dev/null

step "database mode: a second worktree shares the source port"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}", DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/b >/dev/null
DBA_URL="$(sed -n 's/^DATABASE_URL=//p' "$DBA/.env")"
DBB_URL="$(sed -n 's/^DATABASE_URL=//p' "$WORK/root/db-b/.env")"
[ "$DBA_URL" != "$DBB_URL" ] || fail "the two worktrees share a database"
[ "$(sed -n 's/^DB_PORT=//p' "$WORK/root/db-b/.env")" = "$HOST" ] || fail "db/b did not reuse the source port"
[ -z "$(docker ps -aq --filter "label=wt.slug=db-b")" ] || fail "db/b created a container"
"$WT" ls >"$WORK/ls-db.log"
awk -v s=db-a -v p="$HOST" '$1==s && $4==p {ok=1} END{exit !ok}' "$WORK/ls-db.log" || fail "ls db-a port"
awk -v s=db-b -v p="$HOST" '$1==s && $4==p {ok=1} END{exit !ok}' "$WORK/ls-db.log" || fail "ls db-b port"
"$WT" down db/b --yes >/dev/null

step "database mode: a re-run does not recopy"
pg_of wt_wte2e_db_a__app "insert into marker values ('edited')"
pg_of app "create table later(x int)"
RERUN_STARTED="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
"$WT" up db/a >"$WORK/db-a-rerun.log" 2>&1
grep -q 'Reusing wt_wte2e_db_a__app$' "$WORK/db-a-rerun.log" || fail "app copy not reused"
grep -q 'Reusing wt_wte2e_db_a__app_cache$' "$WORK/db-a-rerun.log" || fail "cache copy not reused"
! grep -q 'no longer in' "$WORK/db-a-rerun.log" || fail "a plain re-run warned about stale copies"
[ "$(pg_of wt_wte2e_db_a__app "select count(*) from marker where v='edited'")" = 1 ] || fail "re-run overwrote a copy"
[ "$(pg_of wt_wte2e_db_a__app "select count(*) from pg_tables where tablename='later'")" = 0 ] || fail "re-run recopied the source"
[ "$(pg_of app "select count(*) from pg_tables where tablename='later'")" = 1 ] || fail "fixture table missing in the source"
grep -q "^DATABASE_URL=postgres://app:app@127.0.0.1:$HOST/wt_wte2e_db_a__app$" "$DBA/.env" || fail ".env changed on re-run"
[ "$(grep -c 'Written by wt up' "$DBA/.env")" = 1 ] || fail "banner stacked"
[ "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" = "$RERUN_STARTED" ] || fail "re-run touched the source"
[ -z "$(git -C "$DBA" status --porcelain | grep -v '^?? hook-db.out$')" ] || fail "the worktree is dirty"

step "database mode: a half-copied database is recreated alone"
pg "comment on database wt_wte2e_db_a__app_cache is 'wt.project=wte2e;wt.slug=db-a;wt.complete=0'"
pg_of wt_wte2e_db_a__app_cache "delete from marker"
"$WT" up db/a >"$WORK/db-a-recreate.log" 2>&1
grep -q 'Recreating wt_wte2e_db_a__app_cache' "$WORK/db-a-recreate.log" || fail "cache copy not recreated"
grep -q 'Reusing wt_wte2e_db_a__app$' "$WORK/db-a-recreate.log" || fail "complete copy recreated"
[ "$(pg_of wt_wte2e_db_a__app_cache 'select v from marker')" = cache ] || fail "cache rows not restored"
[ "$(pg "select shobj_description(oid,'pg_database') from pg_database where datname='wt_wte2e_db_a__app_cache'")" = "wt.project=wte2e;wt.slug=db-a;wt.complete=1" ] || fail "marker not restored"
[ "$(pg_of wt_wte2e_db_a__app "select count(*) from marker where v='edited'")" = 1 ] || fail "the complete copy was recreated"

step "database mode: a foreign database is refused and left alone"
pg "create database wt_wte2e_db_foreign__app"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
if "$WT" up db/foreign >"$WORK/foreign.log" 2>&1; then fail "up adopted a foreign database"; fi
grep -q 'was not created by wt' "$WORK/foreign.log" || fail "wrong foreign error"
[ "$(pg "select count(*) from pg_database where datname='wt_wte2e_db_foreign__app'")" = 1 ] || fail "foreign database dropped"
[ "$(pg "select count(*) from pg_database where datname='wt_wte2e_db_foreign__app' and shobj_description(oid,'pg_database') is null")" = 1 ] || fail "foreign database marker changed"
[ ! -e "$STATE_DIR/db-foreign.toml" ] || fail "state written on foreign refusal"
[ ! -e "$WORK/root/db-foreign" ] || fail "worktree created on foreign refusal"
pg "drop database wt_wte2e_db_foreign__app"

step "database mode: a source without a published port is refused"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
source = "$PROJECT-pg-noport-1"
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
if "$WT" up noport/a >"$WORK/noport.log" 2>&1; then fail "up accepted a source without a published port"; fi
grep -q 'publishes no host port' "$WORK/noport.log" || fail "wrong noport error"
[ ! -e "$WORK/root/noport-a" ] || fail "worktree created on noport refusal"
[ ! -e "$STATE_DIR/noport-a.toml" ] || fail "state written on noport refusal"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-pg-noport-1")" = true ] || fail "noport source disturbed"

step "database mode: a stopped source is not started"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
docker stop "$PROJECT-postgres-1" >/dev/null
if "$WT" up db/stopped >"$WORK/stopped.log" 2>&1; then fail "up accepted a stopped source"; fi
grep -q "docker start $PROJECT-postgres-1" "$WORK/stopped.log" || fail "wrong stopped-source error"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-postgres-1")" = false ] || fail "wt started the source"
[ ! -e "$STATE_DIR/db-stopped.toml" ] || fail "state written on stopped-source refusal"
docker start "$PROJECT-postgres-1" >/dev/null
# An ephemeral published port can change across a stop/start.
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"

step "database mode: a typo in databases leaves no worktree or state"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["nope"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
if "$WT" up db/typo >"$WORK/typo.log" 2>&1; then fail "up accepted a missing source database"; fi
grep -q 'source database nope not found' "$WORK/typo.log" || fail "wrong missing-source error"
[ ! -e "$WORK/root/db-typo" ] || fail "worktree created for a missing source database"
[ ! -e "$STATE_DIR/db-typo.toml" ] || fail "state written for a missing source database"

step "database mode: redis is refused by the config"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }

[services.redis]
mode = "database"
EOF
if "$WT" up db/redis >"$WORK/redis.log" 2>&1; then fail "up accepted redis in database mode"; fi
grep -q 'Redis has no separate databases' "$WORK/redis.log" || fail "wrong redis error"
[ ! -e "$WORK/root/db-redis" ] || fail "worktree created on redis refusal"
[ ! -e "$STATE_DIR/db-redis.toml" ] || fail "state written on redis refusal"

step "database mode: two source names that map to the same target are refused"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "APP"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
if "$WT" up db/dup >"$WORK/dup.log" 2>&1; then fail "up accepted colliding database names"; fi
grep -q 'app' "$WORK/dup.log" && grep -q 'APP' "$WORK/dup.log" && grep -q 'wt_wte2e_db_dup__app' "$WORK/dup.log" || fail "wrong collision error"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_dup__%'")" = 0 ] || fail "collision created a database"
[ ! -e "$STATE_DIR/db-dup.toml" ] || fail "state written on collision"
[ ! -e "$WORK/root/db-dup" ] || fail "worktree created on collision"

step "database mode: mysql copies two databases, routines and grants"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.mysql]
mode = "database"
source = "$PROJECT-mariadb-1"
databases = ["app", "app_aux"]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}", DATABASE_URL = "mysql://app:app@127.0.0.1:{port}/{database}", AUX_URL = "mysql://app:app@127.0.0.1:{port}/{database:app_aux}" }
EOF
"$WT" up db/mysql >"$WORK/db-mysql.log" 2>&1
DBM="$WORK/root/db-mysql"
[ "$(sed -n 's/^DB_PORT=//p' "$DBM/.env")" = "$MYSQL_HOST" ] || fail "mysql port is not the source port"
[ "$(my "select count(*) from information_schema.schemata where schema_name like 'wt_wte2e_db_mysql__%'")" = 2 ] || fail "expected two mysql copies"
[ "$(my "select v from __wt_meta where k='wt.complete'" wt_wte2e_db_mysql__app)" = 1 ] || fail "mysql marker"
[ "$(my "select count(*) from __wt_meta where k='noise'" wt_wte2e_db_mysql__app_aux)" = 0 ] || fail "a source __wt_meta table leaked into the marker"
[ "$(my "select count(*) from marker" wt_wte2e_db_mysql__app)" = "$(my "select count(*) from marker" app)" ] || fail "mysql app data"
[ "$(my "select count(*) from marker" wt_wte2e_db_mysql__app_aux)" = "$(my "select count(*) from marker" app_aux)" ] || fail "mysql aux data"
AUX_CS="$(my "select default_character_set_name, default_collation_name from information_schema.schemata where schema_name='app_aux'")"
AUX_COPY_CS="$(my "select default_character_set_name, default_collation_name from information_schema.schemata where schema_name='wt_wte2e_db_mysql__app_aux'")"
[ "$AUX_CS" = "$AUX_COPY_CS" ] || fail "mysql charset/collation not copied"
FORIG="$(my "select count(*) from information_schema.routines where routine_schema='app'")"
FCOPY="$(my "select count(*) from information_schema.routines where routine_schema='wt_wte2e_db_mysql__app'")"
[ "$FORIG" = "$FCOPY" ] && [ "$FORIG" -ge 1 ] || fail "mysql routines not copied"
[ "$(docker exec "$PROJECT-mariadb-1" mariadb -uapp -papp -N -B -e 'select v from marker' wt_wte2e_db_mysql__app)" = cloned ] || fail "mysql grant missing"
"$WT" down db/mysql --yes >/dev/null

step "database mode: down drops every copy and never the source"
STARTED_AT="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
"$WT" down db/a --yes | tee "$WORK/db-a-down.log"
grep -q "database wt_wte2e_db_a__app (source $PROJECT-postgres-1, from app)" "$WORK/db-a-down.log" || fail "describe app copy"
grep -q "database wt_wte2e_db_a__app_cache (source $PROJECT-postgres-1, from app_cache)" "$WORK/db-a-down.log" || fail "describe cache copy"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_a__%'")" = 0 ] || fail "copies left"
[ "$(pg "select count(*) from pg_database where datname in ('app','app_cache')")" = 2 ] || fail "source databases dropped"
[ "$(pg_of app 'select v from marker')" = cloned ] || fail "source data lost"
[ "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" = "$STARTED_AT" ] || fail "down touched the source"
[ ! -e "$STATE_DIR/db-a.toml" ] || fail "state left"
[ ! -e "$DBA" ] || fail "checkout left"

step "database mode: down refuses another owner's marker and never drops the source"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/look >/dev/null
pg "comment on database wt_wte2e_db_look__app is 'wt.project=wte2e;wt.slug=other;wt.complete=1'"
"$WT" down db/look --yes 2>"$WORK/look.log" && fail "down dropped an untagged copy"
grep -q 'not tagged for wte2e/db-look' "$WORK/look.log" || fail "wrong untagged error"
[ "$(pg "select count(*) from pg_database where datname='wt_wte2e_db_look__app'")" = 1 ] || fail "untagged copy dropped"
[ -d "$WORK/root/db-look" ] || fail "the refused down removed the checkout"
[ -e "$STATE_DIR/db-look.toml" ] || fail "the refused down removed the state"
"$WT" down db/look --yes --force >/dev/null
[ "$(pg "select count(*) from pg_database where datname='wt_wte2e_db_look__app'")" = 0 ] || fail "--force did not drop the untagged copy"

# A stale or edited state does not make any unmarked database ours.
"$WT" up db/look3 >/dev/null
pg "create database wte2e_not_ours"
sed -i 's/^target = .*/target = "wte2e_not_ours"/' "$STATE_DIR/db-look3.toml"
"$WT" down db/look3 --yes 2>"$WORK/look3.log" && fail "down dropped an unmarked database the state merely names"
grep -q 'not tagged for wte2e/db-look3' "$WORK/look3.log" || fail "wrong error for an edited state"
[ "$(pg "select count(*) from pg_database where datname='wte2e_not_ours'")" = 1 ] || fail "the edited state dropped a database"
pg "drop database wte2e_not_ours"
sed -i 's/^target = .*/target = "wt_wte2e_db_look3__app"/' "$STATE_DIR/db-look3.toml"
"$WT" down db/look3 --yes >/dev/null

"$WT" up db/look2 >/dev/null
sed -i 's/^target = .*/target = "app"/' "$STATE_DIR/db-look2.toml"
"$WT" down db/look2 --yes --force 2>"$WORK/look2.log" && fail "down dropped the source database"
grep -q 'refusing to drop app' "$WORK/look2.log" || fail "wrong source guard error"
[ "$(pg "select count(*) from pg_database where datname='app'")" = 1 ] || fail "source database dropped"
[ "$(pg_of app 'select v from marker')" = cloned ] || fail "source data lost"
# Even when source_db is rewritten, a target that names a configured source is
# never dropped.
sed -i 's/^source_db = .*/source_db = "other"/' "$STATE_DIR/db-look2.toml"
"$WT" down db/look2 --yes --force 2>"$WORK/look2b.log" && fail "down dropped a configured source database"
grep -q 'refusing to drop app' "$WORK/look2b.log" || fail "protected source guard missing"
sed -i 's/^source_db = "other"/source_db = "app"/' "$STATE_DIR/db-look2.toml"
sed -i 's/^target = "app"/target = "wt_wte2e_db_look2__app"/' "$STATE_DIR/db-look2.toml"
"$WT" up db/look2 >/dev/null
"$WT" down db/look2 --yes >/dev/null

step "database mode: down with the source stopped keeps the state"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/off >/dev/null
docker stop "$PROJECT-postgres-1" >/dev/null
"$WT" down db/off --yes 2>"$WORK/db-off.log" && fail "down dropped with the source stopped"
grep -q "docker start $PROJECT-postgres-1" "$WORK/db-off.log" || fail "wrong stopped-source down error"
[ -d "$WORK/root/db-off" ] || fail "the refused down removed the checkout"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-off.toml")" = 2 ] || fail "state lost on the refused down"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-postgres-1")" = false ] || fail "down started the source"
"$WT" down db/off --yes --force >/dev/null
[ ! -e "$WORK/root/db-off" ] || fail "checkout left after --force"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-off.toml")" = 2 ] || fail "--force dropped the state"
docker start "$PROJECT-postgres-1" >/dev/null
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
"$WT" down db/off --yes --force >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_off__%'")" = 0 ] || fail "copies left after the source came back"
[ ! -e "$STATE_DIR/db-off.toml" ] || fail "state left after the source came back"

step "database mode: --keep-checkout drops the copies and keeps the checkout"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}", DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/keep >/dev/null
"$WT" down db/keep --keep-checkout --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_keep__%'")" = 0 ] || fail "copies left after --keep-checkout"
[ -d "$WORK/root/db-keep" ] || fail "--keep-checkout removed the checkout"
grep -q '^app_port' "$STATE_DIR/db-keep.toml" || fail "app_port lost"
grep -q '^\[ports\]' "$STATE_DIR/db-keep.toml" && fail "ports not cleared"
grep -q '^\[\[databases\]\]' "$STATE_DIR/db-keep.toml" && fail "databases not cleared"
git show-ref --verify --quiet refs/heads/db/keep || fail "--keep-checkout deleted the branch"
"$WT" up db/keep >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_keep__%'")" = 2 ] || fail "copies not recreated"
[ "$(sed -n 's/^DB_PORT=//p' "$WORK/root/db-keep/.env")" = "$HOST" ] || fail "port after recreate"

step "database mode: a database removed from the config is kept and warned about"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/mix >/dev/null
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/mix >/dev/null 2>"$WORK/stale.log"
grep -qF 'no longer in services.postgres.databases' "$WORK/stale.log" || fail "no stale warning"
grep -qE 'CACHE_URL.*wt_wte2e_db_mix__app_cache|wt_wte2e_db_mix__app_cache.*CACHE_URL' "$WORK/stale.log" || fail "the warning does not name the key and the target"
[ "$(pg "select count(*) from pg_database where datname='wt_wte2e_db_mix__app_cache'")" = 1 ] || fail "the kept copy was dropped"
grep -q '^CACHE_URL=' "$WORK/root/db-mix/.env" || fail "CACHE_URL removed silently"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-mix.toml")" = 2 ] || fail "state lost the kept copy"
"$WT" down db/mix --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_mix__%'")" = 0 ] || fail "down left the kept copy"

step "database mode: a worktree without a checkout can still be torn down"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/orphan >/dev/null
git worktree remove --force "$WORK/root/db-orphan" >/dev/null
git worktree prune
"$WT" ls >"$WORK/ls-orphan.log"
awk -v s=db-orphan '$1==s {for(i=1;i<=NF;i++) if($i=="orphan") ok=1} END{exit !ok}' "$WORK/ls-orphan.log" || fail "ls orphan"
"$WT" down db/orphan --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_orphan__%'")" = 0 ] || fail "orphan copies left"
[ ! -e "$STATE_DIR/db-orphan.toml" ] || fail "orphan state left"

step "database mode: ls reports shared, partial and no-db"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/ls1 >/dev/null
for status in shared partial no-db; do
  if [ "$status" = partial ]; then pg "drop database wt_wte2e_db_ls1__app_cache"; fi
  if [ "$status" = no-db ]; then pg "drop database wt_wte2e_db_ls1__app"; fi
  "$WT" ls >"$WORK/ls1.log"
  awk -v s=db-ls1 -v st="$status" -v p="$HOST" \
    '$1==s && $4==p {for(i=1;i<=NF;i++) if($i==st) ok=1} END{exit !ok}' "$WORK/ls1.log" ||
    fail "ls did not report $status"
done
"$WT" down db/ls1 --yes >/dev/null

step "database mode: ls keeps its columns when the service left the config"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/nosvc >/dev/null
printf 'root = "%s"\n' "$WORK/root" >.wt.toml
"$WT" ls | awk -v s=db-nosvc -v p="$HOST" '$1==s && $4==p {ok=1} END{exit !ok}' || fail "ls columns broke when the service left the config"
"$WT" down db/nosvc --yes >/dev/null

step "database mode: ls says unknown while the source is stopped"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/uncertain >/dev/null
docker stop "$PROJECT-postgres-1" >/dev/null
"$WT" ls >"$WORK/ls-source-down.log"
awk -v s=db-uncertain '$1==s {for(i=1;i<=NF;i++) if($i=="unknown") ok=1} END{exit !ok}' "$WORK/ls-source-down.log" || fail "ls must be unknown with the source stopped"
docker start "$PROJECT-postgres-1" >/dev/null
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
"$WT" ls >"$WORK/ls-source-up.log"
awk -v s=db-uncertain '$1==s {for(i=1;i<=NF;i++) if($i=="shared") ok=1} END{exit !ok}' "$WORK/ls-source-up.log" || fail "ls must be shared after the source returns"
"$WT" down db/uncertain --yes >/dev/null

step "database mode: ls says unknown and down keeps the state without Docker"
mkdir -p "$WORK/fakebin"
printf '#!/bin/sh\nexit 1\n' >"$WORK/fakebin/docker"
chmod +x "$WORK/fakebin/docker"
PATH="$WORK/fakebin:$PATH" "$WT" ls >"$WORK/ls-nodocker.log"
awk -v s=db-keep '$1==s {for(i=1;i<=NF;i++) if($i=="unknown") ok=1} END{exit !ok}' "$WORK/ls-nodocker.log" || fail "ls unknown"
PATH="$WORK/fakebin:$PATH" "$WT" down db/keep --yes --force >"$WORK/nodocker.log" 2>&1 || fail "down with no docker"
grep -q 'Docker is not reachable; only the checkout will be removed' "$WORK/nodocker.log" || fail "wrong no-docker message"
[ ! -e "$WORK/root/db-keep" ] || fail "checkout kept with no docker"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-keep.toml")" = 2 ] || fail "state dropped with no docker"
PATH="$WORK/fakebin:$PATH" "$WT" down db/keep --yes >/dev/null 2>&1 || fail "second down with no docker"
PATH="$WORK/fakebin:$PATH" "$WT" ls >"$WORK/ls-nodocker.log"
awk -v s=db-keep '$1==s {for(i=1;i<=NF;i++) if($i=="orphan") ok=1} END{exit !ok}' "$WORK/ls-nodocker.log" || fail "orphan must win over unknown"
"$WT" down db/keep --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_keep__%'")" = 0 ] || fail "copies left after the real down"

step "database mode: down without a terminal and without --yes drops nothing"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/tty >/dev/null
"$WT" down db/tty </dev/null 2>"$WORK/tty.log" && fail "down ran without a terminal"
grep -q 'refusing to destroy db-tty without a terminal; pass --yes' "$WORK/tty.log" || fail "wrong no-tty error"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_tty__%'")" = 2 ] || fail "copies dropped without confirmation"
[ -d "$WORK/root/db-tty" ] || fail "checkout dropped without confirmation"
[ -e "$STATE_DIR/db-tty.toml" ] || fail "state dropped without confirmation"
"$WT" down db/tty --yes >/dev/null

step "database mode: two branches with the same slug"
"$WT" up db/c >/dev/null
"$WT" up db-c 2>"$WORK/slug.log" && fail "db-c reused db/c's worktree"
grep -q 'same slug' "$WORK/slug.log" || fail "wrong slug collision error"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_c__%'")" = 2 ] || fail "the collision created copies"
[ "$(git -C "$WORK/root/db-c" rev-parse --abbrev-ref HEAD)" = db/c ] || fail "db/c checkout changed"
"$WT" down db/c --yes >/dev/null

step "database mode: switching container -> database is refused until down"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
database = "app"
EOF
"$WT" up db/legacy >/dev/null
OLD_PORT="$(sed -n 's/^DB_PORT=//p' "$WORK/root/db-legacy/.env")"
[ "$OLD_PORT" -ge 5433 ] && [ "$OLD_PORT" -le 5499 ] || fail "container clone outside the range"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/legacy 2>"$WORK/legacy.log" && fail "up migrated silently to database mode"
grep -q 'is now mode = "database"' "$WORK/legacy.log" || fail "wrong forward mode-change error"
grep -q 'wt down db-legacy' "$WORK/legacy.log" || fail "no down hint"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-wt-db-legacy-postgres")" = true ] || fail "the leftover container was touched"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_legacy__%'")" = 0 ] || fail "copies created on the refusal"
[ "$(sed -n 's/^postgres = //p' "$STATE_DIR/db-legacy.toml")" = "$OLD_PORT" ] || fail "state changed on the refusal"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-legacy.toml" || true)" = 0 ] || fail "state gained copies on the refusal"
"$WT" down db/legacy --yes >/dev/null
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
"$WT" up db/legacy >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_legacy__%'")" = 1 ] || fail "copy not created after down"
grep -q "^DATABASE_URL=postgres://app:app@127.0.0.1:$HOST/wt_wte2e_db_legacy__app$" "$WORK/root/db-legacy/.env" || fail "database URL after the transition"
"$WT" ls | awk -v s=db-legacy -v p="$HOST" '$1==s && $4==p {ok=1} END{exit !ok}' || fail "ls after the transition"
"$WT" down db/legacy --yes >/dev/null

step "database mode: a container wins over the database verdict in ls"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
database = "app"
EOF
"$WT" up feat/prec >/dev/null
PREC_PORT="$(sed -n 's/^DB_PORT=//p' "$WORK/root/feat-prec/.env")"
cat >>"$STATE_DIR/feat-prec.toml" <<EOF

[[databases]]
service = "postgres"
source = "$PROJECT-postgres-1"
source_db = "app"
target = "wt_wte2e_feat_prec__app"
EOF
"$WT" ls | awk -v s=feat-prec '$1==s {for(i=1;i<=NF;i++) if($i=="running") ok=1} END{exit !ok}' || fail "a container must win over the copy verdict"
docker stop "$PROJECT-wt-feat-prec-postgres" >/dev/null
"$WT" ls | awk -v s=feat-prec '$1==s {for(i=1;i<=NF;i++) if($i=="stopped") ok=1} END{exit !ok}' || fail "ls stopped"
docker rm -f "$PROJECT-wt-feat-prec-postgres" >/dev/null
"$WT" ls | awk -v s=feat-prec '$1==s {for(i=1;i<=NF;i++) if($i=="no-db") ok=1} END{exit !ok}' || fail "ls no-db after the container was removed"
[ "$(sed -n 's/^postgres = //p' "$STATE_DIR/feat-prec.toml")" = "$PREC_PORT" ] || fail "port lost"
"$WT" down feat/prec --yes >/dev/null

step "database mode: switching database -> container is refused until down"
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/legacy2 >/dev/null
cp "$STATE_DIR/db-legacy2.toml" "$WORK/state.before"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-legacy2.toml")" = 2 ] || fail "legacy2 state"
[ "$(sed -n 's/^postgres = //p' "$STATE_DIR/db-legacy2.toml")" = "$HOST" ] || fail "legacy2 port"
[ -z "$(docker ps -aq --filter 'name=wte2e-wt-db-legacy2-postgres')" ] || fail "database mode created a container"
STARTED_AT="$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
database = "app"
EOF
"$WT" up db/legacy2 2>"$WORK/migrate.log" && fail "up migrated back to container silently"
grep -qF 'is now mode = "container"' "$WORK/migrate.log" || fail "wrong reverse mode-change error"
grep -qF '2 database copies' "$WORK/migrate.log" || fail "copy count missing"
grep -qE 'wt_wte2e_db_legacy2__app.*wt_wte2e_db_legacy2__app_cache.*wt down db-legacy2' "$WORK/migrate.log" || fail "targets or hint missing"
cmp -s "$WORK/state.before" "$STATE_DIR/db-legacy2.toml" || fail "state changed on the refusal"
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_legacy2__%'")" = 2 ] || fail "copies dropped on the refusal"
[ "$(docker inspect -f '{{.State.StartedAt}}' "$PROJECT-postgres-1")" = "$STARTED_AT" ] || fail "source touched on the refusal"
git worktree remove --force "$WORK/root/db-legacy2" >/dev/null
git worktree prune
"$WT" up db/legacy2 2>>"$WORK/migrate.log" && fail "up recreated the worktree on the refusal"
[ ! -d "$WORK/root/db-legacy2" ] || fail "worktree recreated on the refusal"
"$WT" down db/legacy2 --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_legacy2__%'")" = 0 ] || fail "down did not drop the copies from the state"
[ ! -e "$STATE_DIR/db-legacy2.toml" ] || fail "state left"
git show-ref --verify --quiet refs/heads/db/legacy2 || fail "branch lost"
"$WT" up db/legacy2 >/dev/null
HOST="$(docker port "$PROJECT-postgres-1" 5432/tcp | sed 's/.*://')"
NEW_PORT="$(docker port "$PROJECT-wt-db-legacy2-postgres" 5432/tcp | sed 's/.*://')"
[ "$NEW_PORT" -ge 5433 ] && [ "$NEW_PORT" -le 5499 ] || fail "new container port outside the range"
[ "$NEW_PORT" != "$HOST" ] || fail "the container took the source port"
[ "$(sed -n 's/^DB_PORT=//p' "$WORK/root/db-legacy2/.env")" = "$NEW_PORT" ] || fail ".env has the wrong port"
"$WT" ls | awk -v s=db-legacy2 -v p="$NEW_PORT" '$1==s && $4==p {ok=1} END{exit !ok}' || fail "ls after switching back"
"$WT" down db/legacy2 --yes >/dev/null

step "database mode: a reset database keeps its data and gets its marker back"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", LITERAL = "{{database}}", OPEN = "{database" }

[services.mysql]
source = "$PROJECT-mariadb-1"
mode = "database"
databases = ["app"]
env = { MYSQL_URL = "mysql://root:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/reset >/dev/null
PG_RESET=wt_wte2e_db_reset__app
# Braces validation read as literal text stay literal in the file.
grep -q '^LITERAL=.*{{database}}' "$WORK/root/db-reset/.env" || fail "a literal value was rewritten"
grep -q '^OPEN=.*{database' "$WORK/root/db-reset/.env" || fail "an unterminated brace was rewritten"
# A drop and create followed by a migrate: no comment, the developer's tables.
pg "select pg_terminate_backend(pid) from pg_stat_activity where datname='$PG_RESET'" >/dev/null
pg "drop database $PG_RESET"
pg "create database $PG_RESET"
pg_of "$PG_RESET" "create table mine(v text); insert into mine values ('dev')"
# Laravel's migrate:fresh on MySQL: every table goes, then the migrations run.
my "drop table $PG_RESET.__wt_meta"
"$WT" up db/reset >"$WORK/reset.log" 2>&1 || fail "up refused a reset copy: $(cat "$WORK/reset.log")"
[ "$(grep -c 'lost its wt marker' "$WORK/reset.log")" = 2 ] || fail "no warning for the two reset copies"
[ "$(pg_of "$PG_RESET" 'select v from mine')" = dev ] || fail "the reset postgres copy lost its data"
[ "$(pg_of "$PG_RESET" "select count(*) from pg_tables where tablename='marker'")" = 0 ] || fail "the reset postgres copy was recopied"
[ "$(my "select count(*) from $PG_RESET.marker")" = 1 ] || fail "the reset mysql copy lost its data"
[ "$(pg "select shobj_description(oid,'pg_database') from pg_database where datname='$PG_RESET'")" = "wt.project=wte2e;wt.slug=db-reset;wt.complete=1" ] ||
  fail "postgres marker not restored"
[ "$(my "select v from $PG_RESET.__wt_meta where k='wt.complete'")" = 1 ] || fail "mysql marker not restored"
"$WT" up db/reset >"$WORK/reset2.log" 2>&1
grep -q 'lost its wt marker' "$WORK/reset2.log" && fail "the marker did not stick"

step "database mode: an empty unmarked copy is copied again"
# What an interruption right after CREATE DATABASE would leave, on both engines.
pg "drop database $PG_RESET"
pg "create database $PG_RESET"
my "drop database $PG_RESET"
my "create database $PG_RESET"
"$WT" up db/reset >"$WORK/empty.log" 2>&1 || fail "up refused an empty copy: $(cat "$WORK/empty.log")"
[ "$(grep -c "Recreating $PG_RESET" "$WORK/empty.log")" = 2 ] || fail "empty copies not recreated"
[ "$(pg_of "$PG_RESET" 'select v from marker')" = cloned ] || fail "postgres copy not refilled"
[ "$(my "select v from $PG_RESET.marker")" = cloned ] || fail "mysql copy not refilled"

step "database mode: a refused down drops nothing, whichever copy refuses"
# The mysql copy refuses after the postgres one was checked.
my "update $PG_RESET.__wt_meta set v='other' where k='wt.slug'"
"$WT" down db/reset --yes 2>"$WORK/atomic.log" && fail "down accepted another owner's marker"
grep -q 'not tagged for wte2e/db-reset' "$WORK/atomic.log" || fail "wrong refusal"
[ "$(pg "select count(*) from pg_database where datname='$PG_RESET'")" = 1 ] || fail "the refused down dropped the postgres copy"
[ -d "$WORK/root/db-reset" ] || fail "the refused down removed the checkout"
my "update $PG_RESET.__wt_meta set v='db-reset' where k='wt.slug'"

step "database mode: down needs no --force after a reset, and still refuses a dirty checkout"
pg "comment on database $PG_RESET is null"
my "drop table $PG_RESET.__wt_meta"
echo dirty >"$WORK/root/db-reset/untracked-file"
"$WT" down db/reset --yes 2>"$WORK/reset-dirty.log" && fail "down removed a dirty checkout"
grep -q 'uncommitted work' "$WORK/reset-dirty.log" || fail "wrong error for a dirty checkout"
rm "$WORK/root/db-reset/untracked-file"

step "database mode: down wins against a client that keeps reconnecting"
# The race itself is too narrow to fail reliably, so the server's log proves the
# drop that closes it was the one that ran.
pg "alter system set log_statement = 'ddl'" >/dev/null
pg "select pg_reload_conf()" >/dev/null
# The clients end by themselves: killing them mid-connect can take the server
# through crash recovery, which has nothing to do with the drop.
docker exec "$PROJECT-postgres-1" touch /tmp/wte2e-race
for _ in $(seq 1 5); do
  docker exec -d "$PROJECT-postgres-1" sh -c "while [ -e /tmp/wte2e-race ]; do psql -qtAX -U app -d $PG_RESET -c 'select pg_sleep(0)' >/dev/null 2>&1; done"
done
sleep 1
"$WT" down db/reset --yes >"$WORK/race.log" 2>&1 || fail "down lost the race: $(cat "$WORK/race.log")"
[ "$(pg "select count(*) from pg_database where datname='$PG_RESET'")" = 0 ] || fail "postgres copy left"
[ "$(my "select count(*) from information_schema.schemata where schema_name='$PG_RESET'")" = 0 ] || fail "mysql copy left"
docker exec "$PROJECT-postgres-1" rm -f /tmp/wte2e-race
sleep 1
docker logs "$PROJECT-postgres-1" >"$WORK/pg-ddl.log" 2>&1
grep -q 'terminating any other active server processes' "$WORK/pg-ddl.log" && fail "the drop took the source through crash recovery"
grep -q "DROP DATABASE \"$PG_RESET\" WITH (FORCE)" "$WORK/pg-ddl.log" || fail "the drop did not use WITH (FORCE)"
pg "alter system reset log_statement" >/dev/null
pg "select pg_reload_conf()" >/dev/null

step "database mode: Ctrl-C stops the copy running inside the source"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}" }
EOF
PG_INT=wt_wte2e_db_int__app
DUMP_FILE=/tmp/wt-copy-wt_wte2e_db_int_app
PID_FILE="$DUMP_FILE.pid"
in_pg() { docker exec "$PROJECT-postgres-1" sh -c "$1"; }
wait_for_pid_file() {
  for _ in $(seq 1 100); do
    in_pg "test -f $PID_FILE" && return 0
    sleep 0.2
  done
  fail "the copy script never started: $(cat "$WORK"/int*.log)"
}
in_pg 'mv /usr/local/bin/pg_dump /usr/local/bin/pg_dump.real && printf "#!/bin/sh\nexec sleep 120\n" >/usr/local/bin/pg_dump && chmod +x /usr/local/bin/pg_dump'
"$WT" up db/int >"$WORK/int.log" 2>&1 &
WT_PID=$!
wait_for_pid_file
RC=0
kill -INT "$WT_PID"
wait "$WT_PID" || RC=$?
[ "$RC" = 130 ] || fail "up exited $RC after Ctrl-C"
in_pg "test ! -e $PID_FILE" || fail "the copy script survived Ctrl-C"
in_pg "test ! -e $DUMP_FILE" || fail "the dump file survived Ctrl-C"
in_pg 'pgrep -f "sleep 120"' >/dev/null && fail "the dump step survived Ctrl-C"
[ "$(pg "select shobj_description(oid,'pg_database') from pg_database where datname='$PG_INT'")" = "wt.project=wte2e;wt.slug=db-int;wt.complete=0" ] ||
  fail "an interrupted copy must stay incomplete"

step "database mode: a re-run ends a copy script the last run left behind"
"$WT" up db/int >"$WORK/int2.log" 2>&1 &
WT_PID=$!
wait_for_pid_file
kill -9 "$WT_PID"
wait "$WT_PID" || true
in_pg "test -f $PID_FILE" || fail "the fixture should leave the script running"
in_pg 'mv /usr/local/bin/pg_dump.real /usr/local/bin/pg_dump'
"$WT" up db/int >"$WORK/int3.log" 2>&1 || fail "the re-run failed: $(cat "$WORK/int3.log")"
grep -q "Recreating $PG_INT" "$WORK/int3.log" || fail "the half copy was not recreated"
in_pg "test ! -e $PID_FILE" || fail "the orphan or its pid file survived"
in_pg 'pgrep -f "sleep 120"' >/dev/null && fail "the orphan's dump step survived"
[ "$(pg_of "$PG_INT" 'select v from marker')" = cloned ] || fail "the re-run did not copy the data"
[ "$(pg "select shobj_description(oid,'pg_database') from pg_database where datname='$PG_INT'")" = "wt.project=wte2e;wt.slug=db-int;wt.complete=1" ] ||
  fail "the re-run did not finish the copy"
"$WT" down db/int --yes >/dev/null

step "database mode: mysql works on a source that runs as a non-root user"
docker compose --profile extra up -d --wait mariadb-user >/dev/null 2>&1
docker exec "$PROJECT-mariadb-user-1" mariadb -uroot -papp app -e "create table marker(v text); insert into marker values ('nonroot');"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.mysql]
source = "$PROJECT-mariadb-user-1"
mode = "database"
databases = ["app"]
env = { MYSQL_URL = "mysql://root:app@127.0.0.1:{port}/{database}" }
EOF
"$WT" up db/nonroot >"$WORK/nonroot.log" 2>&1 || fail "up failed on a non-root source: $(cat "$WORK/nonroot.log")"
[ "$(docker exec "$PROJECT-mariadb-user-1" mariadb -uroot -papp -N -B -e "select v from wt_wte2e_db_nonroot__app.marker")" = nonroot ] ||
  fail "the non-root source was not copied"
"$WT" down db/nonroot --yes >/dev/null
docker compose --profile extra rm -sf mariadb-user >/dev/null 2>&1

step "no database copy survived"
[ -z "$(pg "select datname from pg_database where datname like 'wt_wte2e%'")" ] || fail "postgres copies left"
[ -z "$(my "select schema_name from information_schema.schemata where schema_name like 'wt_wte2e%'")" ] || fail "mysql copies left"
"$WT" ls | grep -q 'No worktree environments' || fail "ls not empty after database mode"

step "wt up refuses a source with a mount inside its data dir"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
source = "$PROJECT-z-nested-1"
EOF
if "$WT" up nested-test >"$WORK/nested.log" 2>&1; then
  fail "up accepted a source with a mount inside its data dir"
fi
grep -q 'inside its data dir' "$WORK/nested.log" || fail "wrong error for the nested mount"
grep -q 'mode = "database"' "$WORK/nested.log" || fail "the refusal does not suggest database mode"
# The source is resolved before the checkout exists: a refused run leaves nothing.
[ ! -e "$WORK/root/nested-test" ] || fail "a refused run created a worktree"
[ ! -e "$XDG_STATE_HOME/wt/$PROJECT/nested-test.toml" ] || fail "a refused run wrote state"
"$WT" ls | grep -q 'No worktree environments' || fail "nested-test left behind"

step "Ctrl-C during a volume copy stops the helper before the source comes back"
docker create --name "$PROJECT-slow-base" redis:7-alpine >/dev/null
printf '#!/bin/sh\nsleep 8\nexec /bin/cp "$@"\n' >"$WORK/slow-cp"
chmod +x "$WORK/slow-cp"
docker cp "$WORK/slow-cp" "$PROJECT-slow-base:/usr/local/bin/cp" >/dev/null
docker commit "$PROJECT-slow-base" "$PROJECT-slow:1" >/dev/null
docker rm -f "$PROJECT-slow-base" >/dev/null
docker compose --profile extra up -d slow >/dev/null 2>&1
SLOW="$PROJECT-slow-1"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.redis]
source = "$SLOW"
env = { REDIS_URL = "redis://127.0.0.1:{port}" }
EOF
wait_for_helper() {
  for _ in $(seq 1 150); do
    [ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-wt-$1-redis-copy" 2>/dev/null)" = true ] && return 0
    sleep 0.2
  done
  fail "the copy helper of $1 never started"
}
"$WT" up slow/a >"$WORK/slow-a.log" 2>&1 &
WT_PID=$!
wait_for_helper slow-a
[ "$(docker inspect -f '{{.State.Running}}' "$SLOW")" = false ] || fail "the source should be stopped for the copy"
RC=0
kill -INT "$WT_PID"
wait "$WT_PID" || RC=$?
[ "$RC" = 130 ] || fail "up exited $RC after Ctrl-C: $(cat "$WORK/slow-a.log")"
docker inspect "$PROJECT-wt-slow-a-redis-copy" >/dev/null 2>&1 && fail "the helper survived Ctrl-C"
[ "$(docker inspect -f '{{.State.Running}}' "$SLOW")" = true ] || fail "the source did not come back"
docker volume inspect "$PROJECT-wt-slow-a-redis-data" >/dev/null 2>&1 && fail "the unfinished volume survived Ctrl-C"
"$WT" down slow/a --yes >/dev/null

step "a helper left running does not stop ls or down"
"$WT" up slow/b >"$WORK/slow-b.log" 2>&1 &
WT_PID=$!
wait_for_helper slow-b
kill -9 "$WT_PID"
wait "$WT_PID" || true
docker inspect "$PROJECT-wt-slow-b-redis-copy" >/dev/null || fail "the fixture should leave the helper running"
"$WT" ls >"$WORK/slow-ls.log"
awk -v s=slow-b '$1==s {for(i=1;i<=NF;i++) if($i=="running"||$i=="partial") bad=1} END{exit bad}' "$WORK/slow-ls.log" ||
  fail "ls counted the helper as a service: $(cat "$WORK/slow-ls.log")"
# The same with the random name of a short-lived probe.
docker run -d --label wt.helper=1 --label "wt.project=$PROJECT" --label wt.slug=slow-b \
  --name "probe-$PROJECT" redis:7-alpine sleep 120 >/dev/null
"$WT" down slow/b --yes >"$WORK/slow-down.log" 2>&1 || fail "down refused a helper: $(cat "$WORK/slow-down.log")"
docker inspect "$PROJECT-wt-slow-b-redis-copy" >/dev/null 2>&1 && fail "down left the helper"
docker inspect "probe-$PROJECT" >/dev/null 2>&1 && fail "down left the probe"
docker volume inspect "$PROJECT-wt-slow-b-redis-data" >/dev/null 2>&1 && fail "down left the volume"
docker start "$SLOW" >/dev/null

step "an interrupted copy is copied again from scratch"
"$WT" up slow/c >"$WORK/slow-c.log" 2>&1 || fail "up after the interruptions failed: $(cat "$WORK/slow-c.log")"
[ "$(docker inspect -f '{{.State.Running}}' "$SLOW")" = true ] || fail "the source did not come back after a good copy"
"$WT" down slow/c --yes >/dev/null
docker compose --profile extra rm -sf slow >/dev/null 2>&1

step "legacy names and the slug label"
cat >.wt.toml <<EOF
root = "$WORK/root"

[docker]
legacy_names = true
slug_label = "$PROJECT.slug"

[services.postgres]
database = "app"
EOF
"$WT" up legacy-test
[ "$(docker inspect -f '{{index .Config.Labels "'"$PROJECT"'.slug"}}' "$PROJECT-wt-legacy-test")" = legacy-test ] || fail "legacy container name or slug label"
[ "$(docker volume inspect -f '{{index .Labels "'"$PROJECT"'.slug"}}' "$PROJECT-wt-legacy-test-db")" = legacy-test ] || fail "legacy volume name or slug label"
[ "$(docker inspect -f '{{index .Config.Labels "wt.slug"}}' "$PROJECT-wt-legacy-test")" = legacy-test ] || fail "wt.slug missing next to the legacy label"
"$WT" ls | grep legacy-test | grep -q running || fail "ls does not show the legacy-named environment"
"$WT" down legacy-test --yes >/dev/null
docker inspect "$PROJECT-wt-legacy-test" >/dev/null 2>&1 && fail "legacy container left"
docker volume inspect "$PROJECT-wt-legacy-test-db" >/dev/null 2>&1 && fail "legacy volume left"

step "down accepts the slug label alone, and still refuses a bare lookalike"
docker volume create --label "$PROJECT.slug=old-one" "$PROJECT-wt-old-one-db" >/dev/null
"$WT" down old-one --yes >/dev/null
docker volume inspect "$PROJECT-wt-old-one-db" >/dev/null 2>&1 && fail "down ignored the slug label"
docker volume create "$PROJECT-wt-bare-db" >/dev/null
if "$WT" down bare --yes >"$WORK/bare.log" 2>&1; then
  fail "down removed a volume with no label"
fi
grep -q 'not labelled' "$WORK/bare.log" || fail "wrong error for the unlabelled volume"
docker volume rm "$PROJECT-wt-bare-db" >/dev/null

step "database mode: down keeps the state when the source container is gone"
cat >.wt.toml <<EOF
root = "$WORK/root"

[services.postgres]
mode = "database"
databases = ["app", "app_cache"]
env = { DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}", CACHE_URL = "postgres://app:app@127.0.0.1:{port}/{database:app_cache}" }
EOF
"$WT" up db/gone >/dev/null
# Remove the container but keep its volume: the copies come back with it.
docker compose rm -sf postgres >/dev/null
"$WT" ls >"$WORK/ls-gone.log"
awk -v s=db-gone '$1==s {for(i=1;i<=NF;i++) if($i=="unknown") ok=1} END{exit !ok}' "$WORK/ls-gone.log" || fail "ls must be unknown with the source gone"
"$WT" down db/gone --yes >"$WORK/gone.log" 2>&1 || fail "down failed with a gone source"
grep -q 'is gone' "$WORK/gone.log" || fail "down did not say the source is gone"
[ "$(grep -c '^\[\[databases\]\]' "$STATE_DIR/db-gone.toml")" = 2 ] || fail "state dropped with a gone source"
[ ! -e "$WORK/root/db-gone" ] || fail "checkout kept with a gone source"
docker compose up -d --wait postgres >/dev/null 2>&1
"$WT" up db/gone >/dev/null
"$WT" ls >"$WORK/ls-back.log"
awk -v s=db-gone '$1==s {for(i=1;i<=NF;i++) if($i=="shared") ok=1} END{exit !ok}' "$WORK/ls-back.log" || fail "ls must be shared after the source returns"
"$WT" down db/gone --yes >/dev/null
[ "$(pg "select count(*) from pg_database where datname like 'wt_wte2e_db_gone__%'")" = 0 ] || fail "copies left after the source returned"
