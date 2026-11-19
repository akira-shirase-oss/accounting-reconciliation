#!/usr/bin/env bash
# Runs a load scenario against a release build of the server on the local stack and keeps
# the evidence (workspace load-test standard).
#
#   MODE=full  scripts/load.sh r0-import   1,000,000 lines (200 accounts x 5 files x 1,000);
#                                          written to perf/results/ to be committed
#   MODE=smoke scripts/load.sh r0-import   50,000 lines; written under target/perf/
#                                          (KEEP=1 writes it to perf/results/ as well,
#                                          labelled smoke)
#   MODE=full  scripts/load.sh r1-import   1,000,000 lines (200 accounts, a 140-day
#                                          statement each cut into 5 files overlapping by
#                                          7 days: about 1,200,000 rows read, the overlap
#                                          rows duplicates); MODE=smoke: 20 accounts
#   MODE=full  scripts/load.sh r2-match    1,000,000 lines decided (200 clients x one
#                                          gen-v3 month of 5,000 lines + ~5,500 documents:
#                                          statement, documents, matching job, outcome
#                                          checked against the truth file); MODE=smoke:
#                                          10 clients x 1,000 lines
#
# Every result records the machine, the commit (and whether the tree was dirty) and the
# load average before and after: a run on a busy machine is visibly a busy-machine
# observation, never capacity. A threshold miss fails the run and is kept as evidence; it
# is never rerun until green. Each run uses its own fresh database (dropped afterwards).
set -euo pipefail
cd "$(dirname "$0")/.."

scenario="${1:?usage: MODE=full|smoke scripts/load.sh r0-import|r1-import|r2-match}"
case "$scenario" in
  r0-import | r1-import | r2-match) ;;
  *) echo "error: unknown scenario $scenario" >&2; exit 2 ;;
esac
mode="${MODE:-smoke}"
if [ -z "${RECON_ADMIN_DATABASE_URL:-}" ]; then
  eval "$(scripts/stack.sh env)"
fi

cargo build --release --locked -p recon-server -p recon-cli -p recon-loadgen >/dev/null

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
db="recon_load_$(date -u +%Y%m%d%H%M%S)"
host="$(sed -E 's#^postgres(ql)?://[^@]*@([^/]*)/.*#\2#' <<<"$RECON_ADMIN_DATABASE_URL")"
./target/release/recon db setup --database "$db" >/dev/null
./target/release/recon db migrate --database "$db" >/dev/null
cleanup() {
  docker compose -f deploy/compose.yaml exec -T postgres \
    psql -q -U recon_admin -d recon_admin -c "DROP DATABASE IF EXISTS ${db} WITH (FORCE)" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [ "$(uname -s)" = "Darwin" ]; then
  cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown cpu")"
  cores="$(sysctl -n hw.ncpu 2>/dev/null || echo "?")"
  mem="$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))"
  machine="$cpu, $cores cores, $mem GiB, macOS $(sw_vers -productVersion 2>/dev/null || echo "?")"
else
  machine="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs), $(nproc) cores, $(( $(grep MemTotal /proc/meminfo | awk '{print $2}') / 1048576 )) GiB, $(uname -sr)"
fi
machine="${machine}; PostgreSQL in Docker (deploy/compose.yaml) on the same machine"
commit="$(git rev-parse HEAD)"
# Result files are not code: an earlier run's result in perf/results/ leaves the tree clean.
if [ -n "$(git status --porcelain -- . ':!perf/results')" ]; then commit="${commit}-dirty"; fi
load_before="$(uptime)"

if [ "$mode" = "full" ] || [ "${KEEP:-}" = "1" ]; then outdir="perf/results"; else outdir="target/perf"; fi
mkdir -p "$outdir" target/perf
out="${outdir}/${scenario}-${mode}-${stamp}.json"

SCENARIO="$scenario" MODE="$mode" PORT="${RECON_LOAD_PORT:-62490}" OUT="$out" \
SERVER_BIN="./target/release/recon-server" MACHINE="$machine" COMMIT="$commit" LOAD_BEFORE="$load_before" \
RECON_DATABASE_URL="postgres://recon_app:${RECON_APP_PASSWORD}@${host}/${db}" \
  ./target/release/recon-loadgen
echo "result: $out"
