#!/usr/bin/env bash
# The production path, end to end (D-069), as docs/OPERATOR_GUIDE.md tells an operator to
# run it: a release build; a fresh database set up and migrated by `recon`; the server
# started the way deploy/recon-server.service starts it, with only the variables of a
# recon.env made from deploy/recon.env.example; a first statement imported through the
# API with its profile (and a refused one, with its report); the chains verified and
# anchored; a pg_dump backup restored into a new database and verified against the
# anchors and replayed; an upgrade (migrate again, stop on SIGTERM, start); a wrong
# setting refused with exit status 78; and, where Docker runs, the same upload through
# deploy/Caddyfile's reverse proxy, whose access log must hold neither the token nor the
# file's name. On Linux the systemd unit is checked with systemd-analyze. Functional, not
# a load run: a few requests. Exit codes are checked directly (set -e, no pipes).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${RECON_ADMIN_DATABASE_URL:-}" ]; then eval "$(scripts/stack.sh env)"; fi
port="${RECON_SMOKE_PORT:-62470}"
proxy_port="${RECON_SMOKE_PROXY_PORT:-62471}"
caddy_image="caddy:2.10.2-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d"
stamp="$(date -u +%Y%m%d%H%M%S)"
db="recon_prod_smoke_${stamp}"
restored="${db}_restored"
proxy="recon-prod-smoke-${stamp}"
work="$(mktemp -d)"
server_pid=""

psql_admin() {
  docker compose -f deploy/compose.yaml exec -T postgres psql -q -U recon_admin -d recon_admin "$@"
}
cleanup() {
  if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; fi
  docker rm -f "$proxy" >/dev/null 2>&1 || true
  psql_admin -c "DROP DATABASE IF EXISTS ${db} WITH (FORCE)" >/dev/null 2>&1 || true
  psql_admin -c "DROP DATABASE IF EXISTS ${restored} WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "production smoke FAILED: $*" >&2; exit 1; }
json() { python3 -I -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

echo "1. release build"
cargo build --release --locked -p recon-server -p recon-cli >/dev/null
bin="$PWD/target/release"

echo "2. database setup and migrations"
"$bin/recon" db setup --database "$db" >/dev/null
"$bin/recon" db migrate --database "$db" >/dev/null
host="$(sed -E 's#^postgres(ql)?://[^@]*@([^/]*)/.*#\2#' <<<"$RECON_ADMIN_DATABASE_URL")"
app_url="postgres://recon_app:${RECON_APP_PASSWORD}@${host}/${db}?sslmode=require"

echo "3. recon.env from the template"
token="$(openssl rand -hex 32)"
sed -e "s#^RECON_DATABASE_URL=.*#RECON_DATABASE_URL=${app_url}#" \
    -e "s#^RECON_OPERATOR_TOKEN=.*#RECON_OPERATOR_TOKEN=${token}#" \
    -e "s#^RECON_LISTEN=.*#RECON_LISTEN=127.0.0.1:${port}#" \
    deploy/recon.env.example >"$work/recon.env"
chmod 600 "$work/recon.env"
grep -q CHANGE-ME "$work/recon.env" && fail "the env file still holds a placeholder"

start_server() {
  # Exactly the file's variables, as systemd's EnvironmentFile= gives them.
  # shellcheck disable=SC2046
  env -i PATH=/usr/bin:/bin $(grep -E '^RECON_[A-Z_]+=' "$work/recon.env") \
    "$bin/recon-server" >>"$work/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 150); do
    if curl -fsS "http://127.0.0.1:${port}/readyz" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  cat "$work/server.log" >&2
  fail "the server never became ready"
}
base="http://127.0.0.1:${port}"
api() { # METHOD PATH [curl args...] -> body on stdout, status in $work/status
  local method="$1" path="$2"; shift 2
  curl -sS -o "$work/body" -w '%{http_code}' -X "$method" -H "Authorization: Bearer ${token}" \
    "$@" "${API_BASE:-$base}${path}" >"$work/status"
  cat "$work/body"
}
expect() { [ "$(cat "$work/status")" = "$1" ] || fail "$2: HTTP $(cat "$work/status"): $(cat "$work/body")"; }

echo "4. start as the service does"
start_server

echo "5. a first statement, as the guide imports it"
firm="$(api POST /v1/firms -H 'content-type: application/json' -d '{"name":"Smoke Firm"}' | json 'd["firm"]["id"]')"
expect 201 "create the firm"
client="$(api POST "/v1/firms/${firm}/clients" -H 'content-type: application/json' -d '{"name":"Smoke Client"}' | json 'd["client"]["id"]')"
account="$(api POST "/v1/firms/${firm}/clients/${client}/accounts" -H 'content-type: application/json' \
  -d '{"name":"Savings","currency":"INR"}' | json 'd["account"]["id"]')"
expect 201 "create the account"
api POST "/v1/firms/${firm}/profiles" -H 'content-type: application/json' \
  --data-binary @examples/profiles/indian-bank-savings.json >/dev/null
expect 201 "store the profile"
lines="$(api POST "/v1/firms/${firm}/accounts/${account}/imports?profile=indian-bank-savings" \
  -H 'content-type: text/csv' -H 'X-File-Name: march-2026.csv' \
  --data-binary @examples/statements/indian-bank-march-2026.csv | json 'd["import"]["lines_created"]')"
expect 201 "import the statement"
[ "$lines" = "12" ] || fail "12 lines expected, got $lines"
problems="$(api POST "/v1/firms/${firm}/accounts/${account}/imports?profile=indian-bank-savings" \
  -H 'content-type: text/csv' -H 'X-File-Name: march-2026-with-errors.csv' \
  --data-binary @examples/statements/indian-bank-march-2026-with-errors.csv \
  | json 'len(d["error"]["report"]["problems"])')"
expect 422 "refuse the file with errors"
[ "$problems" = "3" ] || fail "3 problems expected in the report, got $problems"

echo "6. verify and anchor the chains"
key="$work/anchor.key"
openssl rand -hex 32 >"$key"
chmod 600 "$key"
RECON_DATABASE_URL="$app_url" "$bin/recon" audit verify "$firm" >/dev/null
RECON_DATABASE_URL="$app_url" RECON_ANCHOR_KEY_FILE="$key" \
  "$bin/recon" audit anchor "$firm" --out "$work/anchors" >/dev/null

echo "7. back up, restore into a new database, verify against the anchors, replay"
docker compose -f deploy/compose.yaml exec -T postgres pg_dump -U recon_admin -Fc -d "$db" >"$work/backup.dump"
"$bin/recon" db setup --database "$restored" >/dev/null
docker compose -f deploy/compose.yaml exec -T postgres \
  pg_restore -U recon_admin -d "$restored" --exit-on-error <"$work/backup.dump"
restored_url="postgres://recon_app:${RECON_APP_PASSWORD}@${host}/${restored}?sslmode=require"
RECON_DATABASE_URL="$restored_url" RECON_ANCHOR_KEY_FILE="$key" \
  "$bin/recon" audit verify "$firm" --anchors "$work/anchors" >/dev/null
RECON_DATABASE_URL="$restored_url" "$bin/recon" audit replay "$firm" >/dev/null

echo "8. upgrade: migrate again, stop on SIGTERM, start"
"$bin/recon" db migrate --database "$db" >/dev/null
kill -TERM "$server_pid"
status=0
wait "$server_pid" || status=$?
server_pid=""
[ "$status" = "0" ] || fail "SIGTERM should stop the server with status 0, got $status"
start_server

echo "9. a wrong setting is refused with 78 (and systemd does not restart it)"
status=0
env -i PATH=/usr/bin:/bin RECON_DATABASE_URL="$app_url" RECON_OPERATOR_TOKEN=short \
  RECON_LISTEN="127.0.0.1:$((port + 2))" "$bin/recon-server" >"$work/bad.log" 2>&1 || status=$?
[ "$status" = "78" ] || fail "a short token should exit 78, got $status"

if docker info >/dev/null 2>&1; then
  echo "10. the same upload through deploy/Caddyfile"
  if [ "$(uname -s)" = "Linux" ]; then
    network=(--network host); upstream="127.0.0.1:${port}"
  else
    network=(-p "127.0.0.1:${proxy_port}:${proxy_port}"); upstream="host.docker.internal:${port}"
  fi
  sed -e "s#^recon.example.com {#http://:${proxy_port} {#" -e "s#reverse_proxy 127.0.0.1:62480#reverse_proxy ${upstream}#" \
    deploy/Caddyfile >"$work/Caddyfile"
  docker run -d --name "$proxy" "${network[@]}" -v "$work/Caddyfile:/etc/caddy/Caddyfile:ro" \
    "$caddy_image" >/dev/null
  ready=""
  for _ in $(seq 1 100); do
    if curl -fsS "http://127.0.0.1:${proxy_port}/readyz" >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.2
  done
  [ -n "$ready" ] || { docker logs "$proxy" >&2; fail "the proxy never answered"; }
  API_BASE="http://127.0.0.1:${proxy_port}" api POST "/v1/firms/${firm}/clients/${client}/document-imports?profile=r1-generic-documents" \
    -H 'content-type: text/csv' -H 'X-File-Name: proxy-secret-name-QX7.csv' \
    --data-binary @fixtures/hostile-files/csv-header-mismatch.csv >/dev/null
  expect 422 "a refusal through the proxy"
  docker logs "$proxy" >"$work/proxy.log" 2>&1
  grep -q '/v1/firms/' "$work/proxy.log" || fail "the proxy logged no access"
  if grep -q -e "$token" -e 'proxy-secret-name-QX7' "$work/proxy.log"; then
    fail "the proxy's access log holds the token or the file name"
  fi
else
  echo "10. skipped: Docker is not available for the proxy check"
fi

if command -v systemd-analyze >/dev/null 2>&1; then
  echo "11. systemd-analyze verify deploy/recon-server.service"
  me="$(id -un)"
  sed -e "s#^ExecStart=.*#ExecStart=${bin}/recon-server#" -e "s#^EnvironmentFile=.*#EnvironmentFile=${work}/recon.env#" \
      -e "s#^User=.*#User=${me}#" -e "s#^Group=.*#Group=$(id -gn)#" \
      deploy/recon-server.service >"$work/recon-server.service"
  systemd-analyze verify "$work/recon-server.service"
else
  echo "11. skipped: no systemd-analyze on this machine"
fi

# The server's own log holds no token, file name or content either.
if grep -q -e "$token" -e 'march-2026' -e 'ACME TRADERS' "$work/server.log"; then
  fail "the server log holds the token, a file name or file content"
fi
echo "production smoke ok"
