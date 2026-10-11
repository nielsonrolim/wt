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
  for id in $(docker ps -aq --filter "label=wt.project=$PROJECT"); do docker rm -f "$id" >/dev/null; done
  for v in $(docker volume ls -q --filter "label=wt.project=$PROJECT"); do docker volume rm -f "$v" >/dev/null; done
  (cd "$REPO" 2>/dev/null && docker compose down -v >/dev/null 2>&1)
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
    environment:
      MARIADB_ROOT_PASSWORD: app
      MARIADB_DATABASE: app
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
  # A source whose data is split across mounts: wt must refuse to clone it.
  z-nested:
    image: postgres:16.7-alpine
    command: ["sleep", "infinity"]
    volumes:
      - z-nested-data:/var/lib/postgresql/data
      - ./nested:/var/lib/postgresql/data/pg_wal
volumes:
  pg-data:
  mariadb-data:
  redis-data:
  z-nested-data:
EOF
printf '/.env*\n/storage/*\n!/storage/.keep\n' >.gitignore
touch storage/.keep
printf 'SECRET=main\nDB_PORT=5432\n' >.env
echo '{}' >package.json
git add -A && git commit --quiet -m init

docker compose up -d --wait >/dev/null 2>&1
docker compose exec -T postgres psql -qU app -d app -c "create table marker(v text); insert into marker values ('cloned');" >/dev/null
docker compose exec -T redis sh -c 'redis-cli set marker cloned >/dev/null && redis-cli save >/dev/null'
docker compose exec -T mariadb mariadb -uroot -papp app -e "create table marker(v text); insert into marker values ('cloned');" >/dev/null
sqlite3 storage/development.sqlite3 "create table marker(v text); insert into marker values ('cloned');"
mkdir -p storage/ab && echo blob >storage/ab/blob

step "wt init"
"$WT" init
grep -q '^\[services.postgres\]' .wt.toml || fail "init missed postgres"
grep -q '^\[services.mysql\]' .wt.toml || fail "init missed mysql"
grep -q "source = \"$PROJECT-mariadb-1\"" .wt.toml || fail "init missed the mysql source override"
grep -q '^\[services.redis\]' .wt.toml || fail "init missed redis"
grep -q 'storage/development.sqlite3' .wt.toml || fail "init missed sqlite"
grep -q 'copy = \["storage"\]' .wt.toml || fail "init missed storage"
"$WT" init 2>/dev/null && fail "init overwrote without --force"

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

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["sh", "-c", "echo setup \$WT_SLUG > hook-setup.out"]]
after_services = [["sh", "-c", "echo \$WT_POSTGRES_PORT \$WT_MYSQL_PORT \$WT_REDIS_PORT \$WT_APP_PORT > hook-after.out"]]
EOF

step "wt up feat/x"
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
[ "$(stat -f %Lp "$WTP/.env" 2>/dev/null || stat -c %a "$WTP/.env")" = 600 ] || fail ".env is not 0600"
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

step "wt up feat/x again (idempotent)"
echo "LOCAL=1" >>"$WTP/.env"
"$WT" up feat/x | tee "$WORK/rerun.log"
grep -q "Reusing $PROJECT-wt-feat-x-postgres-data" "$WORK/rerun.log" || fail "volume not reused"
[ "$(sed -n 's/^DB_PORT=//p' "$WTP/.env")" = "$PG_PORT" ] || fail "port changed on re-run"
[ "$(sed -n 's/^PORT=//p' "$WTP/.env")" = "$APP_PORT" ] || fail "app port changed on re-run"
grep -q '^LOCAL=1$' "$WTP/.env" || fail "hand edit lost"
[ "$(grep -c 'Written by wt up' "$WTP/.env")" = 1 ] || fail "banner stacked"

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
"$WT" down nested-test --yes >/dev/null
"$WT" ls | grep -q 'No worktree environments' || fail "nested-test left behind"
