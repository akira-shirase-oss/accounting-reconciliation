#!/usr/bin/env bash
# The fuzz gate (failure case 6, D-066): every cargo-fuzz target in fuzz/ runs for
# FUZZ_SECONDS (default 60) from a corpus seeded with fuzz/seeds/ and the fixtures. A
# crash, panic, an outcome that changes with chunking, a leak or a 10 s input fails the
# run, and libFuzzer leaves the input under fuzz/artifacts/ (kept as a fixture once fixed).
# Needs rustup's cargo, the pinned nightly (FUZZ_TOOLCHAIN) and cargo-fuzz; CI's fuzz job
# installs both. Exit codes are read directly, never piped.
set -euo pipefail
cd "$(dirname "$0")/.."

toolchain="${FUZZ_TOOLCHAIN:-nightly-2026-09-06}"
seconds="${FUZZ_SECONDS:-60}"
targets=(statement-csv statement-profile ofx documents amounts-and-dates decode model-answer anchor-file rule-predicate)

for target in "${targets[@]}"; do
  corpus="fuzz/corpus/${target}"
  mkdir -p "$corpus"
  if [ -d "fuzz/seeds/${target}" ]; then cp fuzz/seeds/"${target}"/* "$corpus"/; fi
  case "$target" in
    ofx) cp fixtures/ofx/*.ofx fixtures/hostile-files/ofx-*.ofx "$corpus"/ ;;
    statement-csv | decode) cp fixtures/hostile-files/csv-*.csv "$corpus"/ ;;
    model-answer) cp crates/recon-model/fixtures/model-wrong-match/*.json "$corpus"/ ;;
  esac
done

for target in "${targets[@]}"; do
  echo "fuzz: ${target} for ${seconds} s"
  cargo +"$toolchain" fuzz run --fuzz-dir fuzz "$target" "fuzz/corpus/${target}" -- \
    -max_total_time="$seconds" -rss_limit_mb=1024 -max_len=262144 -timeout=10 -print_final_stats=1
done
echo "fuzz ok: ${#targets[@]} targets, ${seconds} s each"
