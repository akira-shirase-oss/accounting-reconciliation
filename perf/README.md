# Load scenarios

`scripts/load.sh` runs a scenario against a release build of `recon-server`
on the local stack, with its own fresh database, and writes one JSON result.
Every result names the machine, the commit (`-dirty` when the tree was not
clean) and the load average before and after. Laptop numbers are development
observations, never production capacity; a run on a busy machine says so in
its load average and is a smoke observation. A threshold miss fails the run
and is kept as evidence, never rerun until green.

## `r0-import` (ROADMAP R0)

The unit (D-027, import half) is one statement line taken through import:
parsed, validated, normalized, staged, fingerprinted and published by the
flip. `perf/loadgen` starts the server under `/usr/bin/time` (exact peak
RSS), creates one firm with 200 clients and one USD account each, and eight
workers upload five `gen-v1` statements of 1,000 lines per account (disjoint
months) — 1,000 files, 1,000,000 lines. Every tenth file is then uploaded
again and must be answered with the earlier import.

Checks (all must hold): every line exactly once; fingerprints unique per
account; every file complete; nothing left in staging; repeats answered by
the earlier import; the decision log replays to the stored state.
Thresholds (full run): at least 10,000 lines/s end to end, file p99 at most
3,000 ms, server peak RSS at most 256 MiB, zero errors.

```sh
scripts/stack.sh up
MODE=full scripts/load.sh r0-import    # writes perf/results/r0-import-full-<stamp>.json
MODE=smoke scripts/load.sh r0-import   # 50,000 lines (20 accounts x 5 x 500), target/perf/
```

Run the full size only on a quiet machine (`uptime` load average below about
6 on this 12-core laptop); otherwise run the smoke and record the full run as
owed. CI runs the smoke on every push.
