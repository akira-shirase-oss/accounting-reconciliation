#!/usr/bin/env bash
# The local stack (compose project `accounting-reconciliation`, PostgreSQL on
# 127.0.0.1:62432).
#
#   scripts/stack.sh up      start PostgreSQL and wait until it is healthy
#   scripts/stack.sh setup   create the roles and the dev database `recon`, then migrate it
#   scripts/stack.sh down    stop it
#   scripts/stack.sh reset   stop it and delete its volume
#   scripts/stack.sh env     print the variables the server, CLI and tests read
set -euo pipefail
cd "$(dirname "$0")/.."

compose=(docker compose -f deploy/compose.yaml)
port="${RECON_PG_PORT:-62432}"
host="127.0.0.1:${port}"

env_vars() {
  echo "export RECON_ADMIN_DATABASE_URL=postgres://recon_admin:recon-admin-dev@${host}/recon_admin"
  echo "export RECON_TEST_ADMIN_URL=postgres://recon_admin:recon-admin-dev@${host}/recon_admin"
  echo "export RECON_MIGRATE_DATABASE_URL=postgres://recon_owner:recon-owner-dev@${host}/recon"
  echo "export RECON_DATABASE_URL=postgres://recon_app:recon-app-dev@${host}/recon"
  echo "export RECON_OWNER_PASSWORD=recon-owner-dev"
  echo "export RECON_APP_PASSWORD=recon-app-dev"
}

case "${1:-}" in
  up)
    "${compose[@]}" up -d --wait postgres
    echo "stack ok: postgres on ${host}"
    ;;
  setup)
    eval "$(env_vars)"
    cargo run --quiet --locked -p recon-cli -- db setup --database recon
    cargo run --quiet --locked -p recon-cli -- db migrate
    ;;
  down)
    "${compose[@]}" down
    ;;
  reset)
    "${compose[@]}" down --volumes
    ;;
  env)
    env_vars
    ;;
  *)
    echo "usage: scripts/stack.sh up|setup|down|reset|env" >&2
    exit 2
    ;;
esac
