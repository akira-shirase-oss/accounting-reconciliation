# Roadmap

Step order follows the brief's build sequence — ingest and normalize,
deterministic matching with confidence and exceptions, rules from
corrections with approval and audit, reports and drafts, a pilot with real
accountants — preceded by R0, the spine every project in this workspace
starts with (CI, pure domain, a load scenario, a failure drill), and
followed by the India and PDF milestones. The mapping: brief week 1 → R0–R1
· weeks 2–3 → R2 (deterministic) and R4 (model for fuzzy cases) · week 4
→ R3 · weeks 5–6 → R5 and R7; the evidence battery is R6.

**Status rule (D-031).** A status cell may say *Done* only when it names
the tests and the evidence path that prove the gate. Anything else is
*Planned*, *In progress* or *Blocked (B-n)*. Load evidence measured on a
busy machine is a smoke observation, never capacity, and the full run
stays listed as owed.

Every milestone ships: regression tests; the failure cases that apply to
it as automated tests in CI; a load scenario with thresholds under
`perf/` for anything new that serves traffic or processes lines; the
log-capture test over its drills; and the replay check — the decision log
replayed equals the stored current state.

| Step | Builds | Proves (the gate) | Status |
|---|---|---|---|
| **R0 — the spine** | Cargo workspace; pure `recon-domain` (`Money` over the D-005 table, decimal parsing without floats, the line state machine, fingerprints, decision records, named refusals); PostgreSQL schema for firms, users, clients, accounts, imports, lines and the decision log, with composite keys, append-only triggers and the hash chain; one fixed CSV profile imported through staging; Axum skeleton (bounded bodies, deadlines, one error envelope, readiness); the seeded data generator; `scripts/check.sh` and a CI job running it; `deploy/compose.yaml`; `cargo-deny` | CI green on the first code commit; property tests: money parsing never loses a minor unit and refuses overflow, every invalid state transition is a named refusal, a match that does not balance cannot be built; `UPDATE`/`DELETE`/`TRUNCATE` on the decision log fail in the database; a cross-firm reference fails in the schema; **drill `kill-mid-import`** (failure case 4, import half): `SIGKILL` at 20 seeded points while staging 200,000 rows — each import complete or invisible, a rerun yields identical lines; **drill `db-down`**: readiness red within 2 s, uploads answered `503`, never accepted and then lost; **load `r0-import`**: 1,000,000 generated lines imported and fingerprinted, lines/s and peak RSS recorded; the log-capture test passes | **In progress** — reopened after an independent review: fixes for the decision chain's head under `ON CONFLICT`, server connection timeouts and limits, and five medium findings are landing (DECISIONS D-046 on). What was proven before the review: CI runs 44005970678 (`059de09`) and 44006735994 (`af1dceb`), `scripts/check.sh`: property tests `parse_of_format_is_identity`, `parse_equals_the_exact_integer`, `addition_is_exact_or_refused`, `every_transition_is_allowed_by_the_table_or_refused_by_name`, `a_match_builds_if_and_only_if_it_balances`; `the_decision_log_refuses_update_delete_and_truncate_even_for_its_owner`, `a_cross_firm_reference_fails_in_the_schema`, `the_runtime_role_has_no_power_beyond_its_grants`, `a_search_path_hijack_cannot_reach_the_chain` (D-037); drills `drill_kill_mid_import` (20 seeded points, 200,000 rows) and `drill_db_down` (readiness red in 5–15 ms); `logs_carry_no_secrets_or_financial_content`. Load: `perf/results/r0-import-full-20261117T030010Z.json` — 1,000,000 lines in 17.7 s (56,469 lines/s), file p99 299 ms, server peak RSS 31 MB, every check passed, on a 4-core GitHub-hosted runner (a development observation, not capacity). Owed: the same full run on the development machine, which was at load average 22–48 and lost its Docker engine during R0 |
| **R1 — imports and the normalized schema** | Import profiles (delimiter, encoding, date formats with ambiguity detection, number formats including Indian digit grouping, debit/credit columns, parenthesized and trailing-minus negatives); OFX 1.x SGML and 2.x XML with entities disabled; document imports (invoices, bills, receipts, credit notes, book entries); overlap detection and deduplication (D-008); balance continuity; statement revisions (D-009); whole-file refusal (D-010); `cargo-fuzz` targets | **Failure case 1** (duplicate and overlapping imports, identical twins kept); **failure case 6** (every hostile fixture refused within its memory and time bounds, nothing persisted, fuzzers clean); **failure case 2, detect half** (the revision lists exactly the differences, nothing overwritten); an ambiguous date format is a question to the user, never a guess; load: the 1,000,000-line import with overlap and duplicates in it | Planned |
| **R2 — deterministic matching and the exception queue** | `recon-match`: candidate index, tiers T1–T4 (D-011), the bounded combination search (D-013), residual codes and tolerances (D-014), the scoring function and its breakdown (D-012), exception types and priority; leased matching jobs with fencing and a cursor (D-033) | **Failure case 4, matching half** (`SIGKILL` mid-job, no duplicate or half-applied decision, final digest equals an uninterrupted run); on the synthetic labelled set: zero false auto-matches, proposal recall and per-bucket precision reported; one payment for three invoices less a bank fee is a many-to-one proposal with the residual flagged, never committed automatically; worst-case search fixtures end with `search_budget_exhausted`; same seed and engine version give the same decision digest; **load `r2-match`**: 1,000,000 lines matched with the D-027 mix | Planned |
| **R3 — review, approvals, rules and periods** | Password sessions with mandatory TOTP, roles and client assignment (D-018, D-021); the review API (approve, reject, exclude with a reason, manual match); four-eyes; row versions and allocation `CHECK`s (D-032); rules proposed from corrections, approved into immutable versions, re-evaluation as proposals (D-015); period close and owner-only reopen; `recon audit verify`, `recon explain line`; the first workspace screens (exception queue, review with evidence, period close) from `packages/ui` | **Failure case 2, matched half**; **failure case 5** (closed-period digest byte-identical after a rule change, re-evaluation proposes only); **failure case 7** (32 concurrent conflicting approvals admit exactly one per line, no over-allocation even from raw SQL); the cross-firm, role, auth and tampered-audit-row security cases; load: review API smoke with 64 concurrent reviewers | Planned |
| **R4 — the model for ambiguous cases** | `recon-model`: the provider-neutral port, the recorded-response adapter, an OpenAI-compatible adapter and a loopback local-model adapter (D-016); the firm opt-in and its data-use record; per-call deadlines and per-job budgets; the output validator; proposals with the model's answer labelled as the model's | **Failure case 3** (every wrong-match fixture refused or held as a proposal, zero commits without approval); **failure case 8** (injection fixtures change nothing beyond an ordinary validated proposal); **failure case 9** (provider down, hung, over budget); the egress security case (with model use off, zero outbound connections); a real-provider pass on the synthetic set reporting latency, cost and proposal precision separately — that pass is Blocked (B-2); the fixtures are not | Planned; real-provider pass Blocked (B-2) |
| **R5 — reports, exports, drafts and the workspace** | The reconciliation report per account and period with the period digest and chain head; CSV exports with formula escaping; follow-up drafts for exceptions (D-026); workspace screens for imports, rules, reports and the audit view; Playwright tests of the core flow; keyboard completion | A whole synthetic month — import, match, review, close, report — completed in the browser and by keyboard alone; the export security case; the draft for an exception contains only that client's data (`drafts_never_mix_clients`); page budgets measured and recorded per screen | Planned |
| **R6 — evidence battery** | The labelled synthetic set at 100,000+ lines (D-028); the full runs; **drill `restore`** (failure case 10); every drill and failure case re-run on one commit; the calibration report | All ten failure cases and all security cases pass on the same commit; the full 1,000,000-line run and the 1,000,000-request review run pass their conditions; results committed with machine + commit + `uptime` | Planned |
| **R7 — pilot and ship gate** | One or two pilot firms onboarded with retention set, a signed data agreement and model use decided; an anonymized real month per firm added to the labelled set; each accountant closes one real month | The ship gate in `END_GOAL.md`: zero false auto-matches on the synthetic and real sets, every line explained, a real month closed with reviewer sign-off and the accountant's reported time recorded, the engineering evidence on the release commit | Blocked (B-1, B-3) |
| **R8 — India** (D-029) | GSTR-2B import from the JSON file the user downloads; purchase bills matched to it by GSTIN, normalized invoice number, date, taxable value and tax amounts; GSTIN format and checksum validation; invoice-number normalization across fiscal-year formats (`INV/2026-27/0012`, leading zeros, prefixes); UPI, NEFT, RTGS and IMPS reference parsing; Tally Prime export profiles for book entries | Fixture sets for each format; zero false auto-matches on an India labelled set; GSTR-2B differences reported as `missing_in_2b`, `missing_in_books` or `value_mismatch` and never corrected automatically; a malformed GSTIN is a named refusal | Planned; ordered after R7 unless the pilot firms are Indian (D-029) |
| **R9 — PDF statements** (D-034) | The isolated PDF worker: text-layer extraction, per-bank layout profiles, acceptance only when the parsed lines reconcile opening to closing balance | Hostile PDF fixtures (malformed, huge, encrypted, script-bearing) refused within bounds in the worker only; a parse that does not reconcile is refused, never imported partially; zero network from the worker | Planned |

## Load plan and ship gate

The unit is one statement line taken from import to its matched,
proposed, excluded or exception state (D-027). The generator's mix and the
pass conditions are in `END_GOAL.md` §Evidence plan; full runs are
≥1,000,000 lines and the review API run is ≥1,000,000 requests, with
results committed under `perf/results/` (machine + commit + machine load).
Model providers get functional traffic only; load runs use recorded
responses.

**Ship gate:** accountants can close a real month with the tool, no line is
auto-matched wrongly, and every line's final state can be explained — in
the testable form of `END_GOAL.md` §Ship gate. Blocked on pilot
accountants (README, *Blocked on*).

## Later extensions (explicit contracts required)

Bank-data connections (regulated channels), write-back to accounting
software, sending follow-ups by email, foreign-exchange matching, CAMT.053
and MT940 statements, a client-facing portal, OCR of scanned statements.
Each gets its own contract before work starts.
