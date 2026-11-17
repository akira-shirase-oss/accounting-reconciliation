# Architecture

R0 (the spine) is built: `recon-domain`, `recon-import`, `recon-db`,
`recon-server` and `recon-cli` exist with the behaviour described here for
imports; the rest of this document is the plan for later milestones.
`DECISIONS.md` D-036 to D-045 record what R0 decided while building.

## Sources of truth

1. **PostgreSQL is the only authority** — firms, users, clients, accounts,
   imports, statement lines, documents, matches and allocations, rules and
   rule versions, periods, jobs, proposals and the decision log.
2. **The decision log is append-only** — the database refuses `UPDATE`,
   `DELETE` and `TRUNCATE` on it; every record links to the previous one of
   its client by SHA-256, so an edit made around the database's guards is
   detectable (D-019). Current state (a line's status, a match) is derived
   from decisions and stored for reads; a mismatch between the two is a
   defect with a test.
3. **The blob store holds raw bytes** — every uploaded file, encrypted
   under its client's data key, stored under its firm's directory by the
   SHA-256 of its plaintext, behind a storage port. The pilot backend is
   the local filesystem, backed up with the database (D-022). Lines and
   documents reference the file and row they came from.
4. **Nothing the model says is authoritative** — a model answer becomes a
   proposal row only after validation, and a match only after a person
   approves it.

## Components

| Piece | Role |
|---|---|
| `recon-domain` | Pure types and rules: `Money` over a fixed ISO 4217 exponent table, the normalized line and document, line and match state machines, allocation balancing and residual codes, fingerprints, narration, reference and counterparty normalization, the typed rule language, the scoring function, every limit and named refusal. No I/O — CI enforces it. |
| `recon-import` | Bounded parsers from bytes to normalized rows: CSV through import profiles (delimiter, encoding, date and number formats, debit/credit columns), OFX 1.x SGML and 2.x XML with entity expansion disabled. Pure functions; fuzzed. |
| `recon-match` | The deterministic engine: a candidate index per client and currency, the tiers of D-011, the bounded combination search of D-013, scoring, exception typing. Pure and deterministic — same inputs and engine version, same output — so it is property-tested and benchmarked without a database. |
| `recon-db` | SQLx over PostgreSQL: migrations, composite-key scoping, staged imports, the decision log with its triggers and hash chain, allocation `CHECK` constraints, leased jobs, reports. |
| `recon-model` | The provider-neutral model port (a trait) and its adapters: an OpenAI-compatible HTTP adapter, a local-model adapter on loopback, and the recorded-response adapter CI uses. The only crate allowed an HTTP client, and only to configured hosts (D-016, D-024). |
| `recon-server` | Axum API: sessions and second factor, roles, uploads streamed to the blob store, import and matching jobs, review and approval, rules, periods, reports and exports; serves the built workspace on the same origin. |
| `recon-cli` | Operator and verification commands: `audit verify`, `explain line`, import dry-run against a profile, the data generator, restore checks. |
| `apps/workspace` | SvelteKit + TypeScript (strict) + Tailwind CSS v4 + shadcn-svelte, from this repository's copy of the Colophon UI package in `packages/ui`: imports, exception queue, match review with evidence, rules, period close, reports, the audit view. Client-side; holds no authority and never queries PostgreSQL. |
| `workers/pdf` (R9) | An isolated text-extraction worker for PDF statements: no network, no credentials, CPU, memory and time limits. |

## Data model

- **Firm** (tenant) → **users** with roles and client assignments; **clients**
  (the firm's client businesses) → **accounts** (bank accounts, with
  currency) and **ledgers** (where book entries come from).
- **Import**: file hash, profile and version, account or document set,
  rows read, lines created, duplicates (each naming the existing line),
  refusals, status `staging` → `complete` or `discarded`.
- **Statement line**: account, booking date, value date, amount (signed
  minor units), currency, raw narration, normalized narration, reference
  tokens (invoice-number candidates, bank references, cheque numbers),
  counterparty (raw and normalized), balance after (when the file has it),
  import and row, fingerprint and occurrence index, status.
- **Document**: kind (`sales_invoice`, `purchase_bill`, `receipt`,
  `credit_note`, `book_entry`), number (raw and normalized), issue date,
  due date, signed amount, currency, counterparty, import and row, open
  amount (derived).
- **Match**: one or more lines and one or more documents with allocated
  amounts, plus named residuals; balanced exactly; status `proposed` →
  `committed` → `suspended` or `reversed`.
- **Rule** and immutable **rule versions**: typed predicates over normalized
  fields with an action (assign counterparty, set a match tier, exclude
  with a reason); who proposed it, who approved it, from which correction.
- **Period**: client × account × month, `open` → `closed` → `reopened`.
- **Proposal**: the engine's or the model's suggestion, its candidate set,
  score breakdown or model answer, validation result and its decision.
- **Decision log**: one row per state change — firm, client, sequence,
  previous hash, record hash, subject, from and to state, actor (rule version,
  proposal and approver, or user), reason code, evidence, engine version,
  time from the database clock.

## Paths

**Import.** `POST /v1/clients/{c}/imports` streams the upload (size cap)
into the blob store encrypted → an import job parses it with the chosen
profile in bounded chunks into a staging table → whole-file checks: any
refused row refuses the file (D-010); balance continuity where balances
exist; overlap with earlier imports of the account by date range →
fingerprints are compared with existing lines → one transaction flips the
import to `complete`, inserts the new lines, records duplicates and writes
the decisions. A crash before the flip leaves a `staging` import that
matching never sees; the job resumes or discards it. A file whose hash is
already imported answers with the earlier import.

**Revision.** An overlapping import whose lines differ from the stored
ones in the shared range (a changed amount, a missing or an added line)
creates a statement revision: differences listed, existing lines never
overwritten, matched lines that changed become `source_changed` exceptions
with their match suspended (D-009).

**Match.** A matching job claims a client's open lines under a lease with a
fencing token → loads candidates (same client and currency, open amount,
date window) → `recon-match` produces, per line, a commit (auto tier
only), a proposal or a typed exception → results commit in chunks of 500
lines, each chunk one transaction (match, allocations, line status,
decisions) → the job cursor advances in the same transaction. Decisions are
keyed by (job, line, engine version), so a resumed job cannot write a
second one.

**Ambiguous → model.** Exceptions of the types the firm allows
(`multiple_candidates`, `narration_needed`) are queued for the model port
only when the firm enabled it → the request carries the line, the
candidate set and their evidence fields, delimited as data → the answer
must name candidate ids from that set (or none) with allocations → the
validator checks balance, currency, client, firm and the candidate set →
a valid answer is a proposal; an invalid one is a refusal recorded on the
line. Every call has a deadline and a per-job budget (D-016).

**Review and approve.** The exception queue orders by priority (amount,
age, type) → a reviewer opens a line: evidence, candidates, the score
breakdown, the model's answer labelled as the model's → approve, reject,
exclude with a reason, or match manually → approval checks role, four-eyes
when required, the line's and documents' row versions, and the allocation
constraints in one transaction → one decision row. A correction may
propose a rule (D-015).

**Close.** A reviewer closes a period when no line in it is `unreviewed`
or `proposed` → the period's decision digest and the chain head are
recorded on the period and printed in its report. Reopening is owner-only,
with a reason, and is itself a decision.

**Report and export.** The reconciliation report per account and period:
opening and closing balance from the statement, matched, excluded and
exception totals, the exception list, the period digest. Exports are CSV
with formula escaping; follow-up drafts are text for a person to send.

## Egress, isolation and keys

- The server's only outbound connections are to the configured model
  hosts, and only for firms with model use on (D-024). There is no URL
  import and no webhook.
- Raw files are encrypted with XChaCha20-Poly1305 under a per-client data
  key; data keys are wrapped by a master key read from a 0600 file or the
  environment at start and never logged. Purging a client destroys its
  data key; deleting a firm destroys all of its clients' keys (D-022,
  D-023).
- PostgreSQL runs on an encrypted volume; that is the host's job and is
  checked at deploy, not claimed by the application.
- The PDF worker (R9) runs in a rootless container: read-only root, no
  network, no credentials, CPU, memory and wall-time limits.

## Ports (all loopback, 62400–62499 only; D-036)

| Port | Process |
|---|---|
| 62432 | PostgreSQL (`deploy/compose.yaml`, compose project `accounting-reconciliation`) |
| 62480 | `recon-server` API (and the workspace, from R3) |
| 62490 | `recon-server` during load runs (`scripts/load.sh`) |
| 62400–62479 | test servers and the `db-down` drill's proxy |

## Database roles (D-037, D-049)

The cluster administrator creates roles and databases (`recon db setup`) and
runs migrations acting as the owner (`recon db migrate`: `SET ROLE
recon_owner`). `recon_owner` owns the database, schema `recon` and every
object, and cannot log in. The server logs in as `recon_app` (member of
`recon_runtime`): `SELECT` and `INSERT`, column-level `UPDATE` on imports,
`DELETE` on the staging table, nothing else — no superuser, ownership,
`TRUNCATE`, DDL or `TEMP`. Every function pins `search_path = pg_catalog,
pg_temp`; the server checks its role at startup and refuses to serve
otherwise. Chain heads are signed and anchored outside the database (`recon
audit anchor`), because whoever acts as the owner can rewrite a chain
consistently inside it.

## Consistency story

Every state change is one PostgreSQL transaction that writes the new
state, the allocations and the decision rows together; the decision log
can be replayed to the current state, and a test does so after every
drill. Imports become visible atomically. Jobs are leased with fencing
tokens and advance their cursor in the same transaction as their results,
so a crash resumes instead of repeating. Concurrent approvals are
serialized by row versions and allocation `CHECK` constraints, so the loser
gets a conflict rather than an over-allocation. The model is outside every
transaction: its answer is stored first, validated second, and applied
only by a person.

## Layout

```
crates/      recon-domain, recon-import, recon-db, recon-server, recon-cli
             (R0); recon-match (R2), recon-model (R4)
apps/        workspace          (SvelteKit, static, same-origin served)
packages/    ui                 (this repository's copy of the Colophon UI)
workers/     pdf                (R9)
docs/        END_GOAL, ARCHITECTURE, ROADMAP, DECISIONS
fixtures/    generator seeds, labelled sets, hostile-files, injection,
             model-wrong-match, recorded model responses, import profiles
perf/        loadgen (the load driver), results (machine + commit on every run)
scripts/     check.sh (what CI runs), stack.sh, load.sh, drills
```

## Failure cases

See `END_GOAL.md` — the ten required cases and the security cases are
contract, each with its test and pass condition.
