# Changelog

Notable changes, newest first. Decisions live in `docs/DECISIONS.md`; milestone status
and evidence in `docs/ROADMAP.md`.

## Unreleased — R2: durable matching jobs

### Added

- **Matching job API** — `POST/GET /v1/firms/{f}/clients/{c}/matching-jobs` (D-071).
  `POST` returns 202 with the job; a second start while a job runs is `client_busy`
  (409 + `Retry-After`). The sweeper claims expired leases and resumes from the cursor.
- **Migration 0007** — `match_jobs` (pinned engine version, firm settings, snapshot
  digest, lease token + expiry, cursor), `match_job_lines`/`match_job_documents`
  (the pinned inputs), `matches`/`match_lines`/`match_documents`, `match_residuals`,
  `proposals`, `line_exceptions`, `documents.allocated_minor`, `decision_log.job_id`.
  Schema guards: one running job per client, one decision per (job, subject), only T1
  can commit (a trigger), a document can never be over-allocated (a `CHECK`).
- **`recon-db` matching storage** — snapshot pinning, candidate loading on the
  `(firm, client, currency, open)` index, fenced atomic `commit_chunk` (matches,
  allocations, residuals, proposals, exceptions, decisions, line states, cursor and
  lease renewal in one transaction), `renew`, `finish`, `fail`.
- **Lease heartbeat and job deadline** (D-072) — the drive renews its lease at a third
  of its length; a lost lease cancels the drive; the engine-and-commit stretch runs
  under `RECON_MATCHING_DEADLINE_MS` + `_PER_LINE_MS`, capped by `_MAX_MS`; expiry
  fails the job `deadline_exceeded`. The engine runs on `spawn_blocking`.
- **`gen-v3`** (D-073) — `recon gen` labelled matching sets: statement + documents +
  truth CSV for the D-027 mix; ambiguity is only ever the labelled kind.
- **Drills** — `drill_kill_mid_batch` (SIGKILL at seeded chunk/lifecycle points;
  replay equals stored state; resumed outcomes equal an uninterrupted reference);
  `drill_matching_deadline_exceeded` (a hung engine fails `deadline_exceeded`, the
  lease dies, the sweeper never resurrects it).
- **The labelled-set measurement** — `the_labelled_set_has_zero_false_auto_matches`:
  a 2,000-line `gen-v3` month through the real API, every line compared to its label;
  every committed match is a truth `t1`.
- **Load scenario `r2-match`** (D-073) — `MODE=full|smoke scripts/load.sh r2-match`;
  per-client line states checked against the truth, zero committed matches outside
  `t1`, no subject decided twice, replay. Full: 200 clients × 5,000 lines.

### Fixed

- A long engine compute no longer loses its lease mid-run (the heartbeat), and a hung
  engine can no longer hold a lease forever (the deadline) — found by the first
  kill-mid-batch run and the coordinator's review respectively.
- `scripts/load.sh` records the machine even where `sysctl` is unavailable.
- `recon-loadgen` can no longer hang when the server cannot be signalled (bounded
  waits; the RSS is then recorded as unknown).

## R1 — imports and the normalized schema

Done 2026-11-17 (see `docs/ROADMAP.md` for tests and evidence): import profiles with
ambiguity detection, OFX 1.x/2.x, document imports, overlap deduplication and
statement revisions, balance continuity, hostile-file fixtures and fuzz targets,
`recon import check`/`profile check`, the production path and operator guide,
`r1-import` load (1,000,000 lines, 45,485 lines/s on CI).

## R0 — the spine

Done 2026-11-17: workspace, pure `recon-domain`, the append-only decision log with its
hash chain, import staging, the Axum skeleton, `gen-v1`, CI, `r0-import` load
(1,000,000 lines, 53,199 lines/s on CI).
