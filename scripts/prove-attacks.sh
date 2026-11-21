#!/usr/bin/env bash
# Mutation proof for the attack suite (workspace rule, 2026-11-20): every
# attack test must be proved to fail without the protection it asserts.
#
# For each of the EXPECTED mutants, `attacks-mutants.py` applies a named mutant
# that removes or weakens exactly that protection; the test binary must compile
# (a mutant that breaks the build is a broken mutant, not a proof), the named
# test itself must report FAILED (any other failure — a crash, a harness error,
# a different test's failure — is not proof), and the mutated files are
# restored byte-identically. A test that stays green under its mutant is a
# survivor — the suite does not actually watch that protection — and a mutant
# that never ran is a broken one. This script can only exit 0 when the mutant
# list is exactly what the suite expects and every one proved red.

set -uo pipefail
cd "$(dirname "$0")/.."

EXPECTED=18
if ! MUTANT_LIST="$(python3 scripts/attacks-mutants.py list)"; then
  echo "prove-attacks: could not list mutants" >&2
  exit 1
fi
MUTANTS=()
while IFS= read -r line; do
  [ -n "$line" ] && MUTANTS+=("$line")
done <<< "$MUTANT_LIST"
if [ "${#MUTANTS[@]}" -ne "$EXPECTED" ]; then
  echo "prove-attacks: expected $EXPECTED mutants, got ${#MUTANTS[@]} — an empty or stale list proves nothing" >&2
  exit 1
fi

SNAP=$(mktemp -d)

files_of() { python3 scripts/attacks-mutants.py files "$1"; }

# Snapshot only the files one mutant touches — never whole trees, so a restore
# can't overwrite work a mutant never touched.
snapshot() {
  local f
  while read -r f; do
    [ -n "$f" ] || continue
    mkdir -p "$SNAP/$(dirname "$f")"
    cp -a "$f" "$SNAP/$f"
  done < <(files_of "$1")
}

restore() {
  local f
  while read -r f; do
    [ -n "$f" ] || continue
    cp "$SNAP/$f" "$f" || return 1
    # `cp` gives the restored file a fresh mtime on purpose: `cp -a` would preserve the
    # snapshot's older one, cargo's staleness check would then see the sources as
    # unchanged, and the mutant-built binary would survive into the next suite run —
    # that is how `metadata_…` once refused `://` narrations with the tree unmutated.
    touch "$f" || return 1
    if ! cmp -s "$SNAP/$f" "$f"; then
      echo "prove-attacks: restore of $f is not byte-identical" >&2
      return 1
    fi
    if [ ! "$f" -nt "$SNAP/$f" ]; then
      echo "prove-attacks: restore of $f kept a stale mtime — cargo would skip the rebuild" >&2
      return 1
    fi
  done < <(files_of "$1")
}

on_exit() {
  local m
  for m in "${MUTANTS[@]}"; do restore "$m" >/dev/null 2>&1 || true; done
  rm -rf "$SNAP"
}
trap on_exit EXIT

for m in "${MUTANTS[@]}"; do
  snapshot "$m" || { echo "prove-attacks: could not snapshot $m's files" >&2; exit 1; }
done

if [ -z "${RECON_TEST_ADMIN_URL:-}" ]; then
  eval "$(scripts/stack.sh env)"
  hostport=${RECON_TEST_ADMIN_URL#*@}; hostport=${hostport%%/*}
  if ! (exec 3<>"/dev/tcp/${hostport%:*}/${hostport#*:}") 2>/dev/null; then
    scripts/stack.sh up >/dev/null
  fi
fi
export RECON_TEST_ADMIN_URL

# A mutant proves a test watches its protection only if the suite is green
# without it — prove the baseline first.
if ! cargo test -p recon-server --test attacks --locked \
     > "target/prove-baseline.log" 2>&1; then
  echo "prove-attacks: the unmutated attack suite is not green — nothing can be proved:" >&2
  grep -E 'FAILED|^error' "target/prove-baseline.log" | head -20 >&2
  exit 1
fi

survivors=()
broken=()
proved=0
for mutant in "${MUTANTS[@]}"; do
  if python3 scripts/attacks-mutants.py apply "$mutant" \
       > "target/prove-$mutant-apply.log" 2>&1; then
    if cargo test -p recon-server --test attacks --locked --no-run \
         > "target/prove-$mutant-build.log" 2>&1; then
      cargo test -p recon-server --test attacks --locked -- "$mutant" \
        > "target/prove-$mutant-test.log" 2>&1
      rc=$?
      if grep -q "running 0 tests" "target/prove-$mutant-test.log"; then
        broken+=("$mutant (the named test did not run)")
      elif [ $rc -eq 0 ]; then
        survivors+=("$mutant")
      elif grep -qF "$mutant ... FAILED" "target/prove-$mutant-test.log"; then
        proved=$((proved + 1))
      else
        broken+=("$mutant (failed but the named test did not report FAILED)")
      fi
    else
      broken+=("$mutant (build)")
    fi
  else
    broken+=("$mutant (apply)")
  fi
  restore "$mutant" \
    || { echo "prove-attacks: could not restore $mutant — the tree may be mutated" >&2; exit 2; }
done

echo "prove-attacks: $proved/${#MUTANTS[@]} attacks proved red under their mutant"
[ ${#broken[@]} -gt 0 ] && echo "broken mutants (did not apply/compile/prove — fix the mutant): ${broken[*]}"
if [ ${#survivors[@]} -gt 0 ]; then
  echo "SURVIVORS — these tests pass without their protection:" >&2
  printf '  %s\n' "${survivors[@]}" >&2
  exit 1
fi
[ ${#broken[@]} -eq 0 ]
