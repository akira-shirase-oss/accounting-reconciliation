#!/usr/bin/env bash
# The architecture rules of AGENTS.md (non-negotiable 10, D-024), checked on every commit:
#
#  1. The pure crates (recon-domain, recon-import; recon-match from R2) have no I/O, clock,
#     randomness or async runtime anywhere in their dependency tree (normal and build
#     edges, transitively).
#  2. Their sources never name the standard library's I/O, process, environment, thread
#     or clock APIs.
#  3. No production crate except recon-model (R4) depends on an HTTP client.
#
# Overrides, used by scripts/test-architecture-check.sh to prove each rule can fail:
#   ARCH_BANNED, ARCH_SRC_PATTERNS, ARCH_HTTP_CLIENTS, ARCH_PURE, ARCH_PRODUCTION.
set -euo pipefail
cd "$(dirname "$0")/.."

pure="${ARCH_PURE:-recon-domain recon-import}"
production="${ARCH_PRODUCTION:-recon-domain recon-import recon-db recon-server recon-cli}"
banned="${ARCH_BANNED:-tokio mio socket2 sqlx sqlx-core sqlx-postgres axum axum-core hyper hyper-util tower tower-http reqwest getrandom rand rand_core}"
src_patterns="${ARCH_SRC_PATTERNS:-std::fs std::net std::process std::env std::thread std::io::stdin std::io::stdout SystemTime Instant::now tokio::}"
http_clients="${ARCH_HTTP_CLIENTS:-reqwest ureq isahc curl attohttpc surf awc}"

members=$(cargo metadata --no-deps --format-version 1 --locked | grep -o '"name":"recon-[a-z-]*"' | sed 's/"name":"//; s/"//' | sort -u)
exists() { grep -qx -- "$1" <<<"$members"; }

tree_of() {
  cargo tree --locked -p "$1" -e normal,build --prefix none --format '{p}' | awk '{print $1}' | sort -u
}

fail=0
for crate in $pure; do
  exists "$crate" || continue
  tree=$(tree_of "$crate")
  found=()
  for name in $banned; do
    if grep -qx -- "$name" <<<"$tree"; then found+=("$name"); fi
  done
  if (( ${#found[@]} )); then
    echo "error: $crate must stay pure but depends on: ${found[*]}" >&2
    fail=1
  fi
  dir="crates/$crate/src"
  for pattern in $src_patterns; do
    if grep -rnF -- "$pattern" "$dir" >/dev/null; then
      echo "error: $crate must stay pure but its source uses $pattern:" >&2
      grep -rnF -- "$pattern" "$dir" | head -3 >&2
      fail=1
    fi
  done
  echo "pure ok: $crate ($(wc -l <<<"$tree" | tr -d ' ') crates, none of the banned)"
done

for crate in $production; do
  exists "$crate" || continue
  tree=$(tree_of "$crate")
  for name in $http_clients; do
    if grep -qx -- "$name" <<<"$tree"; then
      echo "error: $crate depends on the HTTP client $name; only recon-model may (D-024)" >&2
      fail=1
    fi
  done
  # hyper-util is the server's HTTP stack (through axum); its client half stays off.
  if cargo tree --locked -p "$crate" -e normal,build --prefix none --format '{p} {f}' \
      | grep -E '^hyper-util ' | grep -Eq '(^|[ ,])client(-legacy)?([ ,]|$)'; then
    echo "error: $crate enables hyper-util's HTTP client; only recon-model may (D-024)" >&2
    fail=1
  fi
done

if (( fail )); then exit 1; fi
echo "architecture ok"
