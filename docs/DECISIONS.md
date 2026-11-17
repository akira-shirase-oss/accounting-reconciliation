# Decisions

Deliberate choices, newest last. Each states the choice, the reason and
what it costs. Numbers are proposed starting values; changing one needs a
measurement and a new entry. There is no engineering guide for this
project: these entries, `END_GOAL.md` and the idea brief they rewrite (kept
in the maintainer's workspace) are the contract. Questions that need the
maintainer or an outside party are listed at the end and in the README's
*Blocked on* table; everything else was closed before any code, on the
conservative side.

## Stack and shape (2026-11-16)

- **D-001 — Rust services, SvelteKit workspace, PostgreSQL.** Rust + Tokio
  + Axum + SQLx over PostgreSQL for the server and CLI; the workspace is
  SvelteKit + TypeScript (strict) + Tailwind CSS v4 + shadcn-svelte,
  started from a copy of the Colophon UI package in `packages/ui`, which
  this repository then owns (no dependency on the Colophon repository).
  This replaces the brief's Python/LangGraph backend and React frontend.
  Reason: the portfolio's one-stack rule, and the core work — bounded
  parsing, exact money, transactional matching, an append-only log — is
  where Rust and PostgreSQL are strongest. Cost: no agent-framework
  shortcuts; the workflow is plain jobs and state machines, which is also
  what makes it auditable.
- **D-002 — Deterministic matching first; a model only for the ambiguous
  remainder.** The brief's own risk note says so. The engine handles every
  line it can decide by rule; only typed exceptions the firm allows reach
  the model, and the model only proposes. Reason: results stay explainable
  and auditable, and cost does not grow with volume. Cost: lines that a
  model could have guessed stay exceptions until a person looks.
- **D-003 — No vector database; "memory" is explicit rules.** The brief's
  memory layer becomes versioned rules a person approved (D-015). Reason:
  an embedding store that silently changes future matches cannot be
  audited or explained line by line. Cost: the system learns only what a
  person writes down as a rule.
- **D-004 — Files only.** Bank statements and documents arrive as uploaded
  files. No Account Aggregator, Plaid-style or bank API connection, and no
  write-back to accounting software, until each has its own contract.
  Reason: those are regulated or privileged channels, and every one of
  them is a new failure domain. Cost: the user exports and uploads files.

## Money, data and imports

- **D-005 — Money is `i64` minor units with a fixed exponent table.**
  Supported currencies: INR, USD, EUR, GBP, AED, SGD, AUD and CAD (2
  decimal places), JPY (0) and KWD (3). A single line or document is at
  most 10^12 minor units in absolute value, so a period of up to 10^6 lines
  sums within `i64`; whole-dataset reports sum in `i128`; every addition is
  checked. Amount strings are parsed digit by digit, never through a float.
  A line and a document in different currencies are never matched: that is
  the `cross_currency` exception. Cost: no FX matching in v1.
- **D-006 — Line states and completeness.** States: `unreviewed`,
  `proposed`, `matched`, `excluded` (with a reason code), `exception`
  (typed), `superseded` (only through a revision, D-009). Every line is in
  exactly one; the current state is derived from the decision log and
  stored for reads. A period closes only when none of its lines is
  `unreviewed` or `proposed` and every exception has a reviewer's
  acknowledgement. Reason: "never silently dropped" needs a state for
  everything. Cost: acknowledging leftovers is real work at close.
- **D-007 — One normalized schema.** Statement lines and documents share
  the fields matching needs: signed minor units as the bank sees them (a
  receipt is positive, a payment negative; documents are normalized to the
  same sign), currency, dates, raw and normalized text, reference tokens,
  counterparty, source file and row. Document kinds: sales invoice,
  purchase bill, receipt, credit note and book entry (a cash-book line
  exported from the books), so bank-to-books reconciliation and
  bank-to-invoice matching use one engine. Cost: an import profile must
  declare how each source maps onto it.
- **D-008 — Import identity and deduplication.** A file's SHA-256
  identifies it; the same file again answers with the earlier import. A
  line's fingerprint is SHA-256 over (account, booking date, amount,
  currency, normalized narration, bank reference when present) plus an
  occurrence index — the number of earlier rows in the same file with the
  same tuple — so two genuine identical transactions on one day both
  survive. When a new import's date range overlaps an existing one, rows
  in the shared range are compared per fingerprint tuple: extra
  occurrences are new lines, missing ones start a revision (D-009). Where
  the file carries running balances, each row must continue the previous
  one, or the file is refused with `balance_discontinuity` at that row.
  Imports are staged and become visible in one transaction. Cost: banks
  that rewrite narrations between exports produce revisions for a person
  to confirm rather than silent merges.
- **D-009 — A changed statement is a revision, never an overwrite.** When
  an overlapping import disagrees with stored lines (a changed amount, a
  line missing or added), the import is held as a revision listing every
  difference. Accepting it marks replaced lines `superseded` and adds the
  new ones; a replaced line that was matched becomes a `source_changed`
  exception with its match suspended; nothing is deleted, and every step
  is a decision. Cost: a reviewer must accept each revision.
- **D-010 — Bounded parsing; one bad row refuses the file; ambiguity is a
  question.** Limits: upload 50 MiB, 2,000,000 rows, 64 KiB per line, 256
  fields per row, 8 KiB per field, 60 s per parse. Encodings: UTF-8 (BOM
  allowed), UTF-16 with a BOM, and Windows-1252 only when the profile says
  so. OFX XML is parsed with DTDs and entity expansion disabled. Any
  refused row refuses the whole file, and the report lists up to 1,000 row
  errors with their codes. Dates such as `01/02/2026` are refused as
  `ambiguous_date_format` unless the profile declares the order. Reason: a
  statement imported with holes breaks completeness and running balances.
  Cost: the user fixes and re-uploads instead of accepting a partial file.

## Matching

- **D-011 — Tiers, and the only automatic commit.** Candidates are open
  documents of the same client and currency within the date window (default
  issue date − 7 days to due date + 90 days; book entries ± 5 days).
  **T1** — the amount equals the document's open amount, a reference token
  in the line equals the document's normalized number or recorded payment
  reference, and exactly one candidate qualifies: T1 may auto-commit, which
  a firm can turn off. **T2** — exact amount, a unique candidate by amount
  in the window, and a counterparty match (normalized equality or an
  approved rule): a proposal. **T3** — an exact combination (D-013): a
  proposal. **T4** — a balance that closes only with a residual (D-014): a
  proposal needing four-eyes. Anything else is a typed exception:
  `no_candidate`, `multiple_candidates`, `amount_mismatch`,
  `cross_currency`, `search_budget_exhausted`, `source_changed`,
  `duplicate_suspect`, `narration_needed`. Only T1 can ever commit without
  a person in v1. Cost: T2 matches that are almost always right still need
  a click; the labelled set measures how many.
- **D-012 — Confidence is a deterministic, versioned score.** A score from
  0 to 100 from fixed weights (`scoring v1`: exact amount 40, exact
  reference 30, counterparty 15, date proximity up to 10, uniqueness 5),
  shown with its breakdown on every proposal. A model's stated confidence
  is shown separately and labelled as the model's claim; the two are never
  combined. Calibration — the share of proposals per score bucket that the
  ground truth confirms — is reported per engine version. Cost: the score
  is a ranking aid, not a probability, and is never described as one.
- **D-013 — Combinations are bounded and must be unique.** Many-to-one and
  one-to-many search runs over at most 40 candidates of the same
  counterparty (nearest by date), combinations of at most 5 items, and a
  budget of 100,000 search nodes per line, in a fixed order. If more than
  one exact combination exists the line is `multiple_candidates` — the
  engine never picks one. An exhausted budget is
  `search_budget_exhausted`, never a partial guess. Cost: large batch
  payments with many small invoices fall to a person.
- **D-014 — Residuals are named and always reviewed.** Residual codes:
  `bank_fee`, `tax_withheld` and `rounding` (at most 1 minor unit per
  document). Each code has a firm-set tolerance, zero until the firm sets
  it. A match with any residual is a proposal that needs four-eyes; a
  short payment is a partial allocation that leaves the document open, not
  a residual. Cost: fee-bearing payments are never automatic.
- **D-015 — Rules from corrections are explicit, versioned and approved.**
  A rule is typed data: predicates over normalized narration (equals,
  contains, starts with, token equals), counterparty, amount range,
  account and document kind; actions assign a counterparty, exclude with a
  reason code, or prefer a document kind. Regular expressions are allowed
  only through Rust's linear-time `regex` engine with a 10 KiB compiled
  size limit. A correction may propose a rule; an owner or reviewer
  approves it; every version is immutable and every decision records the
  version that made it. A rule's exclusions apply automatically only if
  the approving owner marked it so; otherwise they are proposals. A rule
  change applies to lines not yet decided; re-evaluating an open period
  produces a diff of proposals; a closed period is refused until an owner
  reopens it with a reason. Cost: teaching the system takes an explicit
  approval every time.
- **D-016 — The model port.** One trait, `Proposer`, with three adapters: a
  recorded-response adapter (CI, drills and load runs), an
  OpenAI-compatible HTTP adapter, and a loopback local-model adapter. A
  request carries the line, the candidate set the engine computed and
  their evidence fields as structured data, with file text in delimited
  data fields and never in the instructions; account numbers are never
  sent. The answer schema is: chosen candidate ids with allocations, or
  none; an optional residual code and amount; a reason of at most 500
  characters. The validator refuses an answer that names anything outside
  the candidate set, another client or firm, a different currency, or
  allocations that do not balance. Deadlines: 20 s per call, at most two
  automatic retries for transient codes. Budgets: calls, tokens and money
  per job, and a monthly cap per firm. Prompts are versioned data recorded
  on each proposal. Model use is off per firm until an owner turns it on,
  which records the provider and its data-use terms. Cost: a second-rate
  model cannot be blamed for a wrong match, because it never makes one.
- **D-017 — Proposals are never silent, and firm data trains nothing.**
  Every model or fuzzy-tier output is a proposal row; approval or rejection
  is a decision. Rejections feed the evaluation report and may propose a
  rule; they are never hidden memory, and no firm's data is used to train
  or fine-tune any model. Cost: no "it gets smarter on its own".

## Gates, audit and access

- **D-018 — Roles and four-eyes.** Roles: owner (firm settings, users,
  model use, reopening periods), reviewer (approvals, second approvals,
  closing periods, rules), preparer (imports, manual matches as proposals,
  approving T2 and T3 proposals below the firm's threshold), viewer
  (read). Four-eyes — a second person, distinct from the first — is
  required for residual matches, manual matches, rule approvals, and
  matches above a per-currency threshold the firm sets. Users see only the
  clients assigned to them. Cost: small firms of one person cannot use
  four-eyes and must record that the setting is off.
- **D-019 — An append-only, hash-chained decision log per client.** Triggers
  refuse `UPDATE`, `DELETE` and `TRUNCATE`; each record stores the
  previous record's hash and its own SHA-256 over canonical bytes; one
  chain per client keeps clients' work parallel. A period close records
  the chain head, and the period report prints it. `recon audit verify`
  walks the chain and names the first broken link. Limit, stated plainly:
  a database superuser, or anyone acting as the schema's owner (which can
  disable, drop or replace the log's own triggers), can rewrite a chain's
  tail consistently, head included; nothing inside the database can catch
  that. Heads signed and anchored outside the database (D-049) and the
  printed period heads are what expose it. Evidence fields holding financial
  content are stored encrypted under the client's data key, and the hashes
  cover the ciphertext, so purging a client (D-023) keeps the chain
  verifiable. Cost: one chain lock per client on every decision write,
  measured in the load run.
- **D-020 — Firm tenancy by composite keys.** A firm is the tenant. Every
  row carries `firm_id`; primary keys are `(firm_id, id)` and every foreign
  key includes `firm_id`, so a cross-firm reference cannot exist in the
  schema. Repositories take a typed firm scope plus the user's client
  assignments. Row-level security may be added later as defence in depth.
- **D-021 — Sign-in.** Passwords hashed with argon2id; TOTP (RFC 6238)
  enrolment is mandatory at first sign-in for every user, with ten hashed
  recovery codes; no magic links and no single sign-on in v1. Sessions:
  host-only, `Secure`, `HttpOnly`, `SameSite=Strict` cookies, rotated at
  sign-in, 30 min idle and 12 h absolute; a CSRF token on every mutating
  request; sign-in limited per account and per address with growing
  back-off. Reads are recorded in an access log (user × client × day);
  every mutation is a decision. Cost: a second factor for every user of a
  small firm.
- **D-022 — Encryption at rest.** Raw files are encrypted with
  XChaCha20-Poly1305 under a per-client data key, stored under the firm's
  directory by the plaintext's SHA-256. Data keys are wrapped by a master
  key read at start from a 0600 file or the environment; key ids and
  versions are recorded; rotating the master key rewraps data keys.
  PostgreSQL runs on an encrypted volume, which the deployment checks
  rather than the application claims. Backups are encrypted to an offline
  key and kept 35 days. Cost: losing the master key loses the raw files;
  its backup is an operator duty in the runbook.
- **D-023 — Retention terms.** At onboarding the firm sets, per client, how
  long data is kept after a period closes (1 to 10 years); onboarding does
  not complete without it, because statutory periods differ by
  jurisdiction and law and are the firm's call, not this tool's. When a
  period passes its retention, a purge job — the only database role that
  may delete lines and documents — removes them and destroys the client's
  data key once nothing it protects remains; the decision log keeps its
  skeleton and its now-unreadable evidence. Purged data leaves backups
  within their 35-day rotation. A firm that leaves gets an export and its
  keys are destroyed within 30 days. The deletion itself is logged as
  metadata. Cost: there is no undelete.
- **D-024 — Egress: the model endpoint or nothing.** The only outbound
  connections are to model hosts on an operator allowlist, only for firms
  with model use on; TLS for anything but the loopback local model;
  resolved addresses in private ranges refused unless the host is that
  configured loopback model; no redirects followed; responses capped at
  256 KiB. There is no URL import and no webhook. An architecture check
  keeps HTTP clients out of every crate but `recon-model`.
- **D-025 — No secrets or financial content in logs, proven by a test.**
  Logs carry ids, codes, counts and durations. A log-capture test runs over
  every drill and fails on session tokens, TOTP secrets, recovery codes,
  provider keys, and the fixtures' narrations, counterparty names, account
  numbers, amounts and file names. Cost: debugging a line means opening it
  in the workspace, not reading a log.
- **D-026 — Follow-ups are drafts.** For an exception, the workspace
  produces a follow-up text from a versioned template (for example, a
  missing invoice for a payment) containing only that client's data. The
  system sends nothing: a person copies the draft, and the copy is
  recorded in the access log. Cost: no reminder automation in v1.

## Evidence, operations and later milestones

- **D-027 — The load unit is a statement line matched.** One unit is one
  line taken from import to its matched, proposed, excluded or exception
  state. The full run is 1,000,000 lines against 1,100,000 documents over
  200 clients, with the mix in `END_GOAL.md` §Evidence plan, using the
  recorded model adapter. The review API gets its own 1,000,000-request
  run. No third party is load-tested.
- **D-028 — The labelled dataset.** A seeded generator (versioned) writes
  statements, documents and the ground truth, with traps: equal-amount
  invoices, near-duplicate narrations, fees, partial payments, refunds and
  reversals, cross-currency pairs, and revisions. Real months from pilot
  firms are anonymized and used only inside the pilot deployment; they are
  never committed to this public repository — only aggregate metrics are.
  Cost: outside reviewers can reproduce the synthetic results, not the
  real-month ones.
- **D-029 — The India milestone and its order.** R8 adds GSTR-2B matching
  from the JSON file the user downloads from the GST portal (no portal or
  GST Suvidha Provider API access), GSTIN validation, invoice-number
  normalization across fiscal-year formats, UPI, NEFT, RTGS and IMPS
  reference parsing, and Tally Prime export profiles for book entries.
  Before R8, Indian files work through generic CSV profiles and generic
  reference tokens (long digit runs). R8 follows the ship gate unless the
  pilot firms are Indian, in which case its reference parsing and Tally
  profiles move ahead of R7 and GSTR-2B stays after. Write-back to Tally
  still needs its own contract.
- **D-030 — Deployment: one self-hosted instance per pilot.** No public
  sign-up and no multi-firm hosted service in v1. Each pilot runs on a
  single instance the maintainer operates, in a region agreed with the
  firm (an Indian region for Indian firms), with the 35-day encrypted
  backup of D-022 and the restore drill. Memory and disk are sized from the
  load run, not guessed.
- **D-031 — "Done" cites evidence.** A roadmap status cell says Done only
  when it names the tests and the evidence path; busy-machine load results
  are smoke observations and the full run stays owed.
- **D-032 — Concurrency by row versions and allocation constraints.** Lines
  and documents carry a row version; approvals lock the documents they
  allocate (`SELECT … FOR UPDATE`) and update stored allocated sums under
  `CHECK (allocated ≤ |amount|)`, so even a code bug or raw SQL cannot
  over-allocate. The loser of a race gets `409 conflict` naming the
  winning decision. Cost: approvals on one hot document serialize.
- **D-033 — Durable jobs.** Import and matching jobs are rows claimed under
  a 30 s renewable lease with a fencing token; results commit in chunks of
  500 lines, and the job cursor advances in the same transaction; a
  decision is unique on (job, line, engine version), so a resumed job
  cannot write a second one. A stale worker's chunk is refused by its
  fencing token. Cost: chunk size trades commit overhead against redo after
  a crash; the load run measures it.
- **D-034 — PDF statements later, and only if they reconcile.** R9 adds
  an isolated worker that extracts the text layer with per-bank layout
  profiles. A parsed statement is accepted only if its lines carry the
  opening balance to the closing balance exactly; otherwise it is refused
  whole. No OCR. Cost: many PDFs will be refused, which is the honest
  outcome.
- **D-035 — Versioned engine data.** `MATCH_ENGINE_VERSION`, the scoring
  weights, prompt versions and import-profile versions are recorded on
  every decision and proposal; a change to any of them is measured on the
  labelled set before release.

## Built in R0 (2026-11-16)

- **D-036 — Local stack and ports.** The project's own compose project
  `accounting-reconciliation` runs PostgreSQL 18.7 (pinned by digest) on
  `127.0.0.1:62432`, with TLS on (the image's self-signed certificate) so the
  rustls path that Neon-style `sslmode=require` URLs use is tested, and bind
  parameters kept out of PostgreSQL's own log. Every port of this project is
  in 62400–62499: the server on 62480, load runs on 62490, test servers and
  the drill proxy on 62400–62479. This replaces the 54xxx ports first planned
  in `ARCHITECTURE.md`, which other projects' ranges crowd. CI starts the same
  compose file rather than a service container. Cost: none.
- **D-037 — Three database roles; the server is never the owner.** The
  cluster administrator only creates roles and databases (`recon db setup`);
  `recon_owner` owns the database, schema and every object and only migrates
  (`recon db migrate`); the server logs in as `recon_app`, a member of
  `recon_runtime`, which holds only `SELECT`/`INSERT` (plus column-level
  `UPDATE` on imports and `DELETE` on the staging table) — no superuser,
  `BYPASSRLS`, ownership, `TRUNCATE`, DDL or `TEMP` (`PUBLIC`'s default
  `CONNECT` and `TEMP` are revoked on the database). Every function in schema
  `recon` pins `search_path = pg_catalog, pg_temp` and qualifies what it
  touches; the chain and flip triggers that need more than the server has
  are `SECURITY DEFINER` and executable by no one. At startup the server
  checks its own role, every function's pinned path and the schema version,
  and exits with `EX_CONFIG` (78) on any problem. Tests: `recon-db`
  `tests/schema.rs` (`the_runtime_role_has_no_power_beyond_its_grants`,
  `every_function_pins_search_path_and_the_check_can_fail`,
  `a_search_path_hijack_cannot_reach_the_chain`) and `recon-server`
  `tests/startup.rs`. Cost: setup is two commands, and a migration must grant
  what each new table needs.
- **D-038 — An import's lines are created by one decision.** Creating a
  line (none → `unreviewed`) is recorded by the import's completion
  decision, whose evidence carries the counts and a SHA-256 over every row's
  fingerprint in row order; per-line decisions begin with the first later
  transition (R2). Reason: a decision per imported line would double the
  rows written and serialize a million chain-lock updates per client for no
  information the import's digest does not already pin. The replay check
  recomputes each complete import's digest and counts from its lines and
  duplicates, and derives every line's state from its decisions
  (`unreviewed` until its first). Refused and discarded imports get their
  own decisions. Cost: explaining a line starts from its import's decision,
  not a decision of its own.
- **D-039 — Staging is unlogged and the flip verifies it; file identity is
  per account.** `recon.import_staging` is `UNLOGGED` (no WAL for rows that
  are about to be copied): a database crash may empty it, so the flip
  refuses (`import_interrupted`) unless every staged row is present, and a
  lost row can never become a partial import. Flips into one account are
  serialized by a transaction advisory lock, so duplicate detection never
  races. A file's identity is (firm, account, SHA-256); the same file again
  answers with the earlier import. In R0 a row whose fingerprint an earlier
  line of the account already has is recorded as a duplicate naming that
  line and not inserted; overlap and revision detection (D-009) remain R1.
  Cost: a crash during staging means uploading the file again.
- **D-040 — The R0 profile and the identity definitions, v1.**
  `r0-generic-csv` v1: UTF-8, the header exactly `booking_date, value_date,
  amount, currency, narration, reference, counterparty, balance_after`, ISO
  dates, plain decimals with at most the currency's places, the account's
  currency on every row, zero amounts refused, control, byte-order-mark and
  bidirectional-override characters refused in every field.
  `normalization v1` collapses runs of space and no-break spaces, trims and
  upper-cases ASCII only, so no toolchain's Unicode tables can change a
  fingerprint. `fingerprint v1` is SHA-256 over a length-prefixed tuple, then
  over the tuple hash and the occurrence index (`recon-domain::fingerprint`,
  pinned by an independent computation, and equal to the SQL flip's by test).
  Cost: R1 profiles must translate other layouts into these fields, and any
  change to a definition is a new version.
- **D-041 — An operator token until sessions exist.** R0 has no users
  signing in; `/v1` requires `Authorization: Bearer` with
  `RECON_OPERATOR_TOKEN` (at least 32 characters, compared in constant time,
  never logged), and decisions record actor `operator`. The server binds
  loopback by default. Sessions, TOTP and roles replace it in R3 (D-021).
  Cost: R0 is not deployable to anyone but the operator.
- **D-042 — The database computes the chain; Rust verifies it.** A
  `SECURITY DEFINER` trigger takes the client's chain-head row lock, assigns
  `seq`, links `prev_hash`, stamps `decided_at` from the database clock and
  hashes the canonical bytes (v1, documented in `recon-domain::decision`);
  whatever the caller sent for those columns is ignored. `recon audit verify`
  recomputes every hash in Rust in constant memory and also checks that the
  stored head is the chain's last record, so a removed tail is caught.
  Evidence is `bytea` (canonical JSON now, ciphertext once D-019's
  per-client encryption lands), so the hash never depends on JSON
  formatting. Cost: the encoding is defined twice, held equal by a test.
- **D-043 — The flip's plan does not depend on statistics.** Measured on the
  development machine: an anti-join against the flip's own `inserted` CTE
  was quadratic (3.5 s for 2,000 rows, over 60 s for 20,000), and on a fresh
  database the planner, seeing empty never-analyzed tables, chose nested
  loops that walked every index entry the statement itself inserted (over
  120 s for 30,000 rows). The runtime role cannot `ANALYZE` (it owns
  nothing), so the flip's transaction runs with `enable_nestloop = off`: its
  joins are hash or merge joins, one pass over the account's lines (0.46 s
  for 30,000 rows on a fresh database); the `ON CONFLICT` and foreign-key
  probes use their indexes regardless. The replay check uses joins, not
  per-line probes, for the same reason. Test:
  `a_large_flip_is_linear_not_quadratic`. Cost: a flip into an account with
  many lines scans them once (linear in the account's size).
- **D-044 — Leases outlive the work that renews them.** A staged batch runs
  under a lock on its import (the sweeper skips locked rows) and extends the
  lease from the clock *after* the insert; between batches the server renews
  every third of the lease, waking from the body wait to do so. Found by the
  kill drill on a loaded machine: a 5,000-row batch took 13.6 s, longer than
  the lease, and a lease computed from the statement's start was already
  over when it committed. Test: `a_slow_upload_keeps_its_lease_until_the_
  parse_deadline` (fails without the fix). Defaults: lease 30 s, body idle
  15 s, parse deadline 60 s; the drills use 5 s.
- **D-045 — The load driver is Rust.** `perf/loadgen` streams generated
  statements as files, verifies the database afterwards (exact line count,
  unique fingerprints, nothing staged, the replay check) and reads the
  server's peak RSS from `/usr/bin/time`. A request-rate tool like k6 fits
  the review API's 1,000,000-request run (R3), not a line-import run whose
  unit is a statement line. It is a workspace member but not a production
  crate: the HTTP-client architecture rule exempts it. Cost: one more binary
  to build for a load run.

## Fixed after the R0 review (2026-11-16)

- **D-046 — A chain head always names a real record.** The chain trigger
  must run BEFORE INSERT (it assigns `seq`, `prev_hash`, `decided_at` and
  `record_hash`, and a multi-row insert needs each row to see the previous
  row's head), but PostgreSQL fires it before an `ON CONFLICT` check: `INSERT
  … ON CONFLICT DO NOTHING` with an existing decision id inserted nothing yet
  moved the head to a record that did not exist, a permanent break the
  runtime role could cause. Migration 2 makes the trigger refuse an id the
  firm already recorded (`RCA03`, checked after the head lock), adds a
  deferred constraint trigger that refuses any commit whose head names no
  decision with that `seq` and hash (`RCA04`: the cross-client race the
  first check cannot see, or a head moved by hand), and lets a head only
  move forward. Idempotent decision writes (D-033, R2) must therefore check
  for the decision first, never rely on `ON CONFLICT`. Tests:
  `on_conflict_do_nothing_cannot_move_the_chain_head`,
  `a_racing_duplicate_id_cannot_commit_a_phantom_head`,
  `a_head_that_names_no_record_cannot_commit`,
  `concurrent_decisions_keep_every_chain_whole`. Cost: one index probe per
  decision at insert and one at commit.

- **D-047 — Connections are bounded before a request exists.** `axum::serve`
  builds hyper without a timer, so hyper's header-read timeout never runs: a
  client could hold a connection forever by trickling headers or saying
  nothing, and nothing capped how many it held. The server now runs its own
  accept loop on hyper's HTTP/1 builder with a timer and a header-read
  timeout (`RECON_HEADER_READ_TIMEOUT_MS`, default 10 s), which also bounds
  the idle time between requests on a keep-alive connection; a connection
  semaphore (`RECON_MAX_CONNECTIONS`, default 512) closes a new socket at
  once when full; a global in-flight limit (`RECON_MAX_IN_FLIGHT`, default
  256) sheds excess requests with `503 overloaded` instead of queueing them;
  shutdown stops accepting and drains up to its deadline. The request
  deadline still bounds a stalled body. Tests: `tests/connections.rs`
  (a trickled header, silent and idle keep-alive sockets, a stalled body,
  the connection cap, shedding). Cost: one more piece of serving code to own.

- **D-048 — `audit verify` reads one snapshot.** The walk and the head read
  ran as separate statements, so a decision committed between them looked
  like a removed tail: 27 false `HeadMismatch` alarms in 60 verifications of
  an intact chain under a live writer. Verification now runs in one
  `REPEATABLE READ, READ ONLY` transaction and checks the chain as of its
  snapshot. Test: `verifying_a_live_chain_never_raises_a_false_alarm`. Cost:
  a long verification holds back vacuum on the log for its duration.

- **D-049 — The owner cannot log in; heads are anchored outside.** The
  owner of the log owns its triggers: acting as it, anyone can disable them,
  delete a chain's tail and move the head back to a real record, and the
  chain then verifies inside the database. So `recon_owner` is `NOLOGIN`;
  migrations run as the administrator with `SET ROLE recon_owner` (`recon db
  migrate`), and the server's startup check refuses a cluster whose owner can
  log in. `recon audit anchor FIRM --out FILE` appends every client's head —
  sequence, hash and the database clock — signed with HMAC-SHA256 under a key
  from `RECON_ANCHOR_KEY_FILE` (at least 32 bytes, refused if group or others
  can read it); `recon audit verify FIRM --anchors FILE` checks every
  signature and requires the record at every anchored sequence to exist with
  the anchored hash (`AnchorMismatch` otherwise). The anchor file belongs where
  the server and the database owner cannot write: another host or write-once
  storage. Tests: `the_owner_cannot_log_in`,
  `a_chain_rewritten_by_its_owner_is_caught`,
  `anchors_verify_and_catch_tampering_and_an_owner_rewrite` (the binary), the
  anchor signature tests in `recon-domain`. Cost: an operator duty — anchor
  after each period close and keep the key and the file off the database
  host.

- **D-050 — Staging is bounded by bytes, per batch and in total.** Batches
  were capped at 5,000 rows only, so 5,000 wide rows (up to 8 KiB per field,
  held raw and normalized) could sit in memory per upload, times the import
  concurrency. A batch now flushes at 5,000 rows or 4 MiB of row text
  (`NewLine::heap_bytes`), whichever comes first, and every upload reserves
  its buffered bytes from one staging budget (`RECON_STAGING_BUDGET_MIB`,
  default 64, at least 8) before reading on: with the budget spent, an upload
  stops reading its body until another stores a batch, bounded by the parse
  deadline. Tests: `wide_rows_are_staged_in_byte_bounded_batches` (fails on
  the row-only cap), `the_staging_budget_is_shared_and_returned`. Owed: an
  RSS measurement of a 50 MiB wide-row file in the load scenario (R1's
  failure case 6). Cost: more, smaller batches for wide files.

- **D-051 — Readiness is cached, not queried per request.** The
  unauthenticated `/readyz` ran a database round trip per request with no
  cap, so a flood of probes competed with uploads for the pool. A background
  probe now reaches the database every 500 ms (800 ms deadline) and records
  when it last succeeded; `/readyz` answers from that and is red once the
  last success is older than 1.5 s, so it still goes red within the 2 s the
  `db-down` drill requires and green within one interval of recovery. Test:
  `readiness_is_served_from_a_cache_not_a_query_per_probe` (16 s for 20
  probes before, immediate now); `drill_db_down`. Cost: readiness lags the
  database by up to 1.5 s.

- **D-052 — Deserialized money is constructed money.** `Money` derived
  `Deserialize`, which bypassed `Money::new`'s ±10^12 bound: `i64::MIN` minor
  units deserialized, and negating it lost a unit. `Money` now deserializes
  through `try_from` into `Money::new`, negation is `checked_neg`, and
  `ScoreBreakdown` deserializes only with `scoring-v1`'s weights. Allocations
  and residuals (R3's manual matches, R4's model answers) inherit the check.
  Tests: `deserializing_is_range_checked_like_construction`,
  `deserialized_allocations_and_scores_are_checked`.

- **D-053 — A busy account refuses a flip at once.** A flip into an account
  another flip holds waited on the advisory lock until the session's 5 s
  lock timeout and surfaced as `503 database_unavailable`, leaving its
  staging import to the sweeper. The flip now takes the lock with
  `pg_try_advisory_xact_lock`: when the account is held, the import is
  discarded (reason `account_busy`, with its decision) and the client gets
  `409 account_busy` with `Retry-After`. The flip's lease check uses
  `clock_timestamp()`, like staging and renewal. Test:
  `a_flip_into_a_busy_account_is_refused_at_once` (5.02 s before, immediate
  now).

## Open questions (blocked on the maintainer or an outside party)

Each is in the README's *Blocked on* table. None blocks R0–R6.

- **Pilot accountants (B-1).** One or two practising accountants or small
  firms willing to pilot: an anonymized real month each for the labelled
  set, and one real month closed on the tool. Who they are also decides
  which books system the first book-entry profile targets (Tally,
  QuickBooks, Xero) and whether R8 moves ahead of R7 (D-029).
- **Model provider and data-use terms (B-2).** Which provider (or local
  model) the real-provider pass uses, its retention and no-training terms,
  and a spending cap. Until then model use stays off and every test runs on
  recorded responses.
- **Hosting, data agreement and retention per pilot firm (B-3).** The
  region, a data-processing agreement under the law that applies (India's
  DPDP Act 2023 for Indian firms; GDPR if a firm has EU clients — counsel
  if a firm asks), and each client's retention period (D-023). No real
  client data is loaded before this exists.
