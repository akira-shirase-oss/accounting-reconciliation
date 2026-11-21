#!/usr/bin/env bash
# Mutation proof for the attack suite (workspace rule, 2026-11-20): every
# attack test must be proved to fail without the protection it asserts.
#
# For each of the 17 tests, `attacks-mutants.py` applies a named mutant that
# removes or weakens exactly that protection; the test binary must compile
# (a mutant that breaks the build is a broken mutant, not a proof), and the
# test must fail. A test that stays green under its mutant is a survivor —
# the suite does not actually watch that protection, and this script fails.
#
# The source tree is restored after every mutant and on exit.

set -uo pipefail
cd "$(dirname "$0")/.."

MUTANTS=( $(python3 scripts/attacks-mutants.py list) )
SRC="crates/recon-server/src crates/recon-db/src crates/recon-import/src"

restore() { git checkout -- $SRC 2>/dev/null || true; }
trap restore EXIT
restore

if [ -z "${RECON_TEST_ADMIN_URL:-}" ]; then
  eval "$(scripts/stack.sh env)"
  hostport=${RECON_TEST_ADMIN_URL#*@}; hostport=${hostport%%/*}
  if ! (exec 3<>"/dev/tcp/${hostport%:*}/${hostport#*:}") 2>/dev/null; then
    scripts/stack.sh up >/dev/null
  fi
fi
export RECON_TEST_ADMIN_URL

survivors=()
broken=()
proved=0
for mutant in "${MUTANTS[@]}"; do
  python3 scripts/attacks-mutants.py apply "$mutant" \
    || { broken+=("$mutant (apply)"); restore; continue; }
  if ! cargo test -p recon-server --test attacks --locked --no-run \
       > "target/prove-$mutant-build.log" 2>&1; then
    broken+=("$mutant (build)"); restore; continue
  fi
  if cargo test -p recon-server --test attacks --locked -- "$mutant" \
      > "target/prove-$mutant-test.log" 2>&1; then
    survivors+=("$mutant")
  else
    proved=$((proved + 1))
  fi
  restore
done

echo "prove-attacks: $proved/${#MUTANTS[@]} attacks proved red under their mutant"
[ ${#broken[@]} -gt 0 ] && echo "broken mutants (did not apply/compile — fix the mutant): ${broken[*]}"
if [ ${#survivors[@]} -gt 0 ]; then
  echo "SURVIVORS — these tests pass without their protection:" >&2
  printf '  %s\n' "${survivors[@]}" >&2
  exit 1
fi
[ ${#broken[@]} -eq 0 ]
