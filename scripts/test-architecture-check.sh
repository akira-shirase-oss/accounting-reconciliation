#!/usr/bin/env bash
# Proves each architecture rule can fail: a check that cannot fail proves nothing.
set -euo pipefail
cd "$(dirname "$0")/.."

expect_failure() {
  local what="$1"; shift
  if env "$@" scripts/check-architecture.sh >/dev/null 2>&1; then
    echo "error: the architecture check did not catch $what" >&2
    exit 1
  fi
  echo "architecture check catches $what"
}

# recon-domain does depend on sha2: banning it must trip rule 1.
expect_failure "a banned dependency" ARCH_BANNED=sha2
# recon-domain's source does call Sha256: banning the name must trip rule 2.
expect_failure "a banned source pattern" "ARCH_SRC_PATTERNS=Sha256::new"
# recon-domain depends on serde: treating it as an HTTP client must trip rule 3.
expect_failure "an HTTP client in a production crate" ARCH_HTTP_CLIENTS=serde
echo "architecture self-test ok"
