#!/usr/bin/env bash
# Full wt cycle against a throwaway compose stack (Postgres + Redis + SQLite)
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
mkdir -p "$REPO/storage" "$WORK/root"
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
  redis:
    image: redis:7-alpine
    volumes:
      - redis-data:/data
volumes:
  pg-data:
  redis-data:
EOF
printf '/.env*\n/storage/*\n!/storage/.keep\n' >.gitignore
touch storage/.keep
printf 'SECRET=main\nDB_PORT=5432\n' >.env
echo '{}' >package.json
git add -A && git commit --quiet -m init

docker compose up -d --wait >/dev/null 2>&1
docker compose exec -T postgres psql -qU app -d app -c "create table marker(v text); insert into marker values ('cloned');" >/dev/null
docker compose exec -T redis sh -c 'redis-cli set marker cloned >/dev/null && redis-cli save >/dev/null'
sqlite3 storage/development.sqlite3 "create table marker(v text); insert into marker values ('cloned');"
mkdir -p storage/ab && echo blob >storage/ab/blob

step "wt init"
"$WT" init
grep -q '^\[services.postgres\]' .wt.toml || fail "init missed postgres"
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

[services.redis]

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["sh", "-c", "echo setup \$WT_SLUG > hook-setup.out"]]
after_services = [["sh", "-c", "echo \$WT_POSTGRES_PORT \$WT_REDIS_PORT \$WT_APP_PORT > hook-after.out"]]
EOF

step "wt up feat/x"
"$WT" up feat/x
WTP="$WORK/root/feat-x"
[ -d "$WTP" ] || fail "no checkout"
[ "$(git -C "$WTP" rev-parse --abbrev-ref HEAD)" = feat/x ] || fail "wrong branch"

PG_PORT="$(sed -n 's/^DB_PORT=//p' "$WTP/.env")"
REDIS_URL="$(sed -n 's/^REDIS_URL=//p' "$WTP/.env")"
APP_PORT="$(sed -n 's/^PORT=//p' "$WTP/.env")"
echo "ports: pg=$PG_PORT redis=$REDIS_URL app=$APP_PORT"
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
[ "$(sqlite3 "$WTP/storage/development.sqlite3" 'select v from marker')" = cloned ] || fail "sqlite not copied"
[ "$(cat "$WTP/storage/ab/blob")" = blob ] || fail "storage not copied"
[ "$(cat "$WTP/hook-setup.out")" = "setup feat-x" ] || fail "setup hook"
read -r hp hr ha <"$WTP/hook-after.out"
[ "$hp" = "$PG_PORT" ] && [ "$ha" = "$APP_PORT" ] && [ -n "$hr" ] || fail "after_services hook env"
[ "$(docker inspect -f '{{.State.Running}}' "$PROJECT-postgres-1")" = true ] || fail "source not restarted"

step "wt up feat/x again (idempotent)"
echo "LOCAL=1" >>"$WTP/.env"
"$WT" up feat/x | tee "$WORK/rerun.log"
grep -q "Reusing $PROJECT-wt-feat-x-postgres-data" "$WORK/rerun.log" || fail "volume not reused"
[ "$(sed -n 's/^DB_PORT=//p' "$WTP/.env")" = "$PG_PORT" ] || fail "port changed on re-run"
[ "$(sed -n 's/^PORT=//p' "$WTP/.env")" = "$APP_PORT" ] || fail "app port changed on re-run"
grep -q '^LOCAL=1$' "$WTP/.env" || fail "hand edit lost"
[ "$(grep -c 'Written by wt up' "$WTP/.env")" = 1 ] || fail "banner stacked"

step "second worktree gets different ports"
"$WT" up feat/y >/dev/null
[ "$(sed -n 's/^DB_PORT=//p' "$WORK/root/feat-y/.env")" != "$PG_PORT" ] || fail "pg port reused"
[ "$(sed -n 's/^PORT=//p' "$WORK/root/feat-y/.env")" != "$APP_PORT" ] || fail "app port reused"

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

step "wt down feat-x"
rm -f "$WORK/root/feat-y"/hook-*.out
"$WT" down feat-x --yes
"$WT" down feat-y --yes >/dev/null
[ -z "$(docker ps -aq --filter "label=wt.project=$PROJECT")" ] || fail "containers left"
[ -z "$(docker volume ls -q --filter "label=wt.project=$PROJECT")" ] || fail "volumes left"
[ ! -e "$WTP" ] || fail "checkout left"
[ ! -e "$XDG_STATE_HOME/wt/$PROJECT/feat-x.toml" ] || fail "state left"
git show-ref --verify --quiet refs/heads/feat/x || fail "branch deleted"
docker volume inspect "${PROJECT}_pg-data" >/dev/null || fail "source volume touched"
"$WT" ls | grep -q 'No worktree environments' || fail "ls not empty"
