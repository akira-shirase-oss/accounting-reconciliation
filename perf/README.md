# Load scenarios

`scripts/load.sh` runs a scenario against a release build of `recon-server`
on the local stack, with its own fresh database, and writes one JSON result.
Every result names the machine, the commit (`-dirty` when the tree, result
files aside, was not clean) and the load average before and after. Laptop numbers are development
observations, never production capacity; a run on a busy machine says so in
its load average and is a smoke observation. A threshold miss fails the run
and is kept as evidence, never rerun until green.

## `r0-import` (ROADMAP R0)

The unit (D-027, import half) is one statement line taken through import:
parsed, validated, normalized, staged, fingerprinted and published by the
flip. `perf/loadgen` starts the server as a direct child (peak RSS read
from `/proc`, unknown off Linux), creates one firm with 200 clients and one
USD account each, and eight
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

## `r1-import` (ROADMAP R1, D-067)

The same unit, through R1's overlap handling. Each account gets one continuous
140-day `gen-v2` statement cut into five files whose day windows overlap by
seven days, so every file after the first repeats the last week of the one
before byte for byte (about 1,200,000 rows read for 1,000,000 lines in the full
run: 200 accounts x 5,000 lines). Every tenth file is uploaded again. An upload
that meets a busy account (`409 account_busy`) is retried after a pause and
counted.

Checks (all must hold, every run): every line exactly once; fingerprints
unique; every file complete; no revision held; the overlap rows are duplicates
(lines created equal the lines, lines plus duplicates equal the rows read);
nothing staged; repeats answered; the replay check. Thresholds — full: as
`r0-import`; smoke (20 accounts, CI on every push, gating): at least 1,000
lines/s, file p99 at most 10,000 ms, peak RSS at most 256 MiB, zero errors.
`CLIENTS=N` runs fewer accounts as a functional check, never a result.

```sh
MODE=full scripts/load.sh r1-import    # perf/results/r1-import-full-20261117T142528Z.json
MODE=smoke scripts/load.sh r1-import
```

## `r2-match` (ROADMAP R2)

The unit (D-027, matching half) is one statement line decided by a matching
job: pinned snapshot, engine, atomically committed chunks. Each client gets
one `gen-v3` month — a 5,000-line statement plus its documents (about 1.1 per
line plus 500 distractors) and a truth file; the full run is 200 clients,
1,000,000 lines. Workers upload both files per client, one matching job per
client is started (the server's `matching_concurrency` bounds how many run at
once; mutual exclusion is per client), and each job is polled to a terminal
state.

Checks (all must hold): every job `complete` with a digest; the per-client
counts of `matched`, `proposed` and `exception` line states equal the truth
file's labels; zero committed matches outside tier `t1` and the committed
count equal to the truth's `t1` count (zero false auto-matches); proposals
and exceptions equal their truth counts; no subject decided twice; the
decision log replays. Thresholds — full: at least 2,000 lines/s decided
end to end, job p99 at most 120,000 ms, server peak RSS at most 512 MiB
(each drive holds one client's pinned set in memory), zero errors; smoke
(10 clients x 1,000 lines): at least 300 lines/s, job p99 at most 60,000 ms.

```sh
MODE=full scripts/load.sh r2-match    # perf/results/r2-match-full-<stamp>.json (CI)
MODE=smoke scripts/load.sh r2-match   # target/perf/
```

Run the full size only on a quiet machine (`uptime` load average below about
6 on this 12-core laptop); otherwise run the smoke and record the full run as
owed. CI runs the smoke on every push.

The full runs live in `.github/workflows/load-full.yml`: `gh workflow run
load-full.yml -f scenario=all` (or `-f scenario=r2-match`) runs each scenario
on its own runner with its own PostgreSQL and uploads the labelled result
JSON as an artifact, pass or miss. A threshold miss fails the job and is kept
as evidence, never retried to green.
