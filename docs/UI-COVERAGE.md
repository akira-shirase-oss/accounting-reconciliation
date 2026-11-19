# UI coverage audit

What every operation an accountant or operator can reach does, and where its screen is.
Re-run this audit whenever the API grows: the rule is that **no operation stays
API-only past the milestone that owns its surface** (D-068, D-070 — the workspace is
built at R3 for review/exceptions and at R5 for imports, rules, reports and audit; the
SvelteKit app is same-origin and static, holds no authority, and never touches
PostgreSQL). Until then the interface is the API's reports and the `recon` command
line, held to the same standard as the engine (D-068).

Audited against `crates/recon-server/src/lib.rs` routes and `recon --help` on
2026-11-19 (commit `63865f2`).

## Operations: API surface → UI coverage

| Operation | Route / command | Milestone | UI today | Owed |
|---|---|---|---|---|
| Create a firm | `POST /v1/firms` | R0 | none — operator action | R5 onboarding screen |
| Create a client | `POST …/clients` | R0 | none | R5 (client list) |
| Create an account | `POST …/clients/{c}/accounts` | R0 | none | R5 (client list) |
| Upload a statement | `POST …/accounts/{a}/imports` | R0/R1 | none — `recon import check` previews the file locally first | R5 (import flow: file → profile → refusal report → committed) |
| Upload documents | `POST …/clients/{c}/document-imports` | R1 | none | R5 (import flow) |
| Read an import's result | `GET …/imports/{i}` | R0/R1 | the refusal report is the interface (D-068): line, column, problem, fix | R5 (import detail + refusal list) |
| See a revision's changes | `GET …/imports/{i}/changes` | R1 | the change list over the API | R5 (diff view) |
| Accept a revision | `POST …/imports/{i}/accept` | R1 | none — a deliberate act, API only | R5 (accept/reject with the diff) |
| Reject a revision | `POST …/imports/{i}/reject` | R1 | none | R5 |
| Start a matching job | `POST …/clients/{c}/matching-jobs` | R2 | none — `client_busy` answers 409 + `Retry-After` | R3 (exception queue: job trigger + progress) |
| Read a matching job | `GET …/matching-jobs/{j}` | R2 | the job record: counters, digest, `error_code` | R3 (job progress per client) |
| Create an import profile | `POST /v1/firms/{f}/profiles` | R1 | none — `recon profile check` validates the JSON first | R5 (profile editor) |
| Read a profile | `GET …/profiles/{n}` | R1 | the profile document itself | R5 |
| Health/readiness | `GET /healthz`, `/readyz` | R0 | not accountant-facing | ops probes, never a screen |

## Operations: surfaces that arrive later (no route yet)

| Operation | Owed by | Note |
|---|---|---|
| Exception queue: list, filter, a line's candidates and evidence | R3 | the first workspace screens (D-070) |
| Review: approve, reject, exclude with a reason, manual match | R3 | four-eyes; keyboard completion is part of "works" |
| Rules: propose from corrections, approve a version, re-evaluate | R3 | immutable versions (D-015) |
| Period close / reopen; the reconciliation report and period digest | R3/R5 | owner-only reopen |
| `recon explain line` — a line's whole decision history | R3 | exists as CLI already |
| Follow-up drafts for exceptions | R5 | one client's data only (D-026) |
| Reports and CSV exports | R5 | formula escaping on export |
| Audit view (chain verify, anchors) | R5 | `recon audit verify|anchor|replay` is the interim interface |
| Model proposals labelled as the model's, with score breakdowns | R4/R5 | blocked partly on B-2 |

## Interim coverage that is NOT a screen (and is tested)

- `recon import check FILE` — reads a file exactly as the server would: what would
  import, or every refused line with its field, code and fix (D-068).
- `recon profile check PROFILE.json` — every problem in a profile, or how it reads.
- `recon audit verify FIRM`, `recon audit anchor`, `recon audit replay` — the chain,
  signed heads, and stored-state equality.
- `recon gen statement` / the labelled `gen-v3` sets — deterministic fixtures.
- Refusal reports on every upload's `GET` — naming line, column, problem and fix.
- `--help`/`recon help` on every command; `examples/` run by `tests/examples.rs`.

## Gaps and notes

- **No screen exists yet — by design.** Every covered path above is covered by an API
  response or a CLI command with regression tests; nothing accountant-facing is a
  dead end.
- **The R3 cut must include the exception queue and job progress**, or an R3 pilot
  could start jobs but not see their outcomes — the audit re-runs when R3's review
  API lands.
- **`GET …/matching-jobs/{j}` is the only job-progress surface**; a client has at most
  one running job (D-071), so the R3 screen can poll it without a list endpoint — if a
  list is wanted, add `GET …/matching-jobs` there, not ad hoc.
