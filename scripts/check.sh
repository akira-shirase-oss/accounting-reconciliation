#!/usr/bin/env bash
# The gate: what CI runs, run before every commit. Each step's exit code is checked
# directly (set -e, no pipes into grep): the toolchain pin, formatting, Clippy with
# warnings denied (plus the fault-injection build the drills use), the architecture rules
# and the proof that each can fail, every test against the local stack (started here
# when RECON_TEST_ADMIN_URL is unset), the drills, then cargo-deny when installed. Fuzzing
# is CI's own job (scripts/fuzz.sh, a minute per target), and so is the production path
# (scripts/production-smoke.sh, a release build and Docker); neither is part of this gate.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/check-toolchain.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
if cargo metadata --no-deps --format-version 1 --locked | grep -q '"fault-injection"'; then
  cargo clippy -p recon-server --features fault-injection --all-targets --locked -- -D warnings
fi
scripts/check-architecture.sh
scripts/test-architecture-check.sh

if [ -z "${RECON_TEST_ADMIN_URL:-}" ]; then
  scripts/stack.sh up >/dev/null
  eval "$(scripts/stack.sh env)"
fi
export RECON_TEST_ADMIN_URL
cargo test --workspace --locked
if cargo metadata --no-deps --format-version 1 --locked | grep -q '"fault-injection"'; then
  # The drills: the real server binary, killed at seeded points and cut off from its
  # database (failure case 4's import half, db-down).
  cargo test -p recon-server --features fault-injection --test drills --locked -- --test-threads=1
fi

# cargo installs tools into ~/.cargo/bin, which may be missing from PATH (after rustup's).
[ -d "$HOME/.cargo/bin" ] && PATH="$PATH:$HOME/.cargo/bin"
if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny --locked check
else
  echo "note: cargo-deny not installed; supply-chain check skipped locally (CI runs it)"
fi
echo "check ok"
