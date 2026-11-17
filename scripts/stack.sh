#!/usr/bin/env bash
# The local stack (compose project `accounting-reconciliation`, PostgreSQL on
# 127.0.0.1:62432, and the TLS test database on 127.0.0.1:62433 with a certificate from
# the development CA, D-070).
#
#   scripts/stack.sh up      create the development CA if missing, start both databases
#                            and wait until they are healthy
#   scripts/stack.sh setup   create the roles and the dev database `recon`, then migrate it
#                            (as the administrator acting as the NOLOGIN owner, D-049)
#   scripts/stack.sh down    stop it
#   scripts/stack.sh reset   stop it and delete its volume
#   scripts/stack.sh env     print the variables the server, CLI and tests read
set -euo pipefail
cd "$(dirname "$0")/.."

compose=(docker compose -f deploy/compose.yaml)
port="${RECON_PG_PORT:-62432}"
host="127.0.0.1:${port}"
tls_port="${RECON_PG_TLS_PORT:-62433}"

env_vars() {
  echo "export RECON_ADMIN_DATABASE_URL=postgres://recon_admin:recon-admin-dev@${host}/recon_admin"
  echo "export RECON_TEST_ADMIN_URL=postgres://recon_admin:recon-admin-dev@${host}/recon_admin"
  echo "export RECON_DATABASE_URL=postgres://recon_app:recon-app-dev@${host}/recon"
  echo "export RECON_APP_PASSWORD=recon-app-dev"
  # The TLS test database: its certificate names localhost only, signed by this CA.
  echo "export RECON_TLS_TEST_URL=postgres://recon_admin:recon-admin-dev@localhost:${tls_port}/recon_admin"
  echo "export RECON_TLS_TEST_CA=$PWD/.dev-tls/ca.pem"
}

case "${1:-}" in
  up)
    scripts/dev-tls.sh
    "${compose[@]}" up -d --wait postgres postgres-tls
    echo "stack ok: postgres on ${host}, postgres-tls on 127.0.0.1:${tls_port}"
    ;;
  setup)
    eval "$(env_vars)"
    cargo run --quiet --locked -p recon-cli -- db setup --database recon
    cargo run --quiet --locked -p recon-cli -- db migrate --database recon
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
