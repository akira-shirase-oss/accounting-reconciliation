# Accounting Reconciliation

> **Status — in development.** R0, the spine, is done, including an
> independent review's fixes: CI, the pure domain, the import path through
> staging with its database guards, the `kill-mid-import` and `db-down`
> drills, and a 1,000,000-line `r0-import` load run (`perf/results/`). R1, the
> imports, is built and its gate met except the full 1,000,000-line `r1-import`
> run, which is owed: declared CSV profiles, OFX, document imports, overlap
> detection, statement revisions held for a person, the hostile-file drill and
> six fuzz targets (`docs/ROADMAP.md` names the tests, the evidence and what is
> owed). Nothing here matches documents yet (R2). The ship gate needs one or
> two real accountants, who have not been found yet (see *Blocked on*).

Bank statements and invoices in; every bank line out as matched, excluded
or an exception — each with a decision you can explain. A reconciliation
workspace for small accounting firms and freelance accountants:
deterministic rules match the bulk (exact amount, a date window, a
reference, counterparty rules); a model is consulted only for the
ambiguous remainder, behind a provider-neutral port, and only proposes;
people approve. Rules learned from corrections are explicit, versioned
rules someone approved, not opaque memory. Every decision lands in an
append-only, hash-chained audit trail.

- Imports: bank statements as CSV and OFX (PDF later), invoices, bills,
  receipts, credit notes and cash-book entries as CSV, all normalized into
  one schema with exact integer money.
- Duplicate and overlapping imports add nothing twice; a changed statement
  becomes a revision, never an overwrite; a malformed file is refused
  whole with row numbers.
- Confidence scores with their feature breakdown, a typed exception queue,
  approval gates with four-eyes, period close and owner-only reopening.
- Firm tenancy enforced in the schema, a second factor for every user,
  per-client encryption of raw files, retention terms set per client.
- Later: India (GSTR-2B, invoice numbers, UPI references, Tally) and PDF
  statements.

The tool reads records and reports. It never moves money, never sends
email, never writes back to accounting software and never connects to a
bank.

## Blocked on

These need the maintainer or an outside party; everything else is decided
in `docs/DECISIONS.md`.

| # | Needed | From | Blocks |
|---|---|---|---|
| B-1 | One or two practising accountants or small firms willing to pilot: an anonymized real month each, and one real month closed on the tool | the maintainer, then the accountants | the real-data half of the labelled set and the R7 ship gate; which books system the first book-entry profile targets; whether R8 (India) moves ahead of R7 |
| B-2 | A model provider (or a local model) for real runs, its data-use terms (retention, no training) and a spending cap | the maintainer | R4's real-provider pass only; every test and load run uses recorded responses and model use stays off until then |
| B-3 | Pilot hosting region, a data-processing agreement with each pilot firm, and each client's retention period | the maintainer with each firm (counsel if a firm asks) | loading any real client data (R7) |

## Documents

- `docs/END_GOAL.md` — what done means; scope in and out; the invariants;
  the ten required failure cases and the security cases, each with its test
  and pass condition; the ship gate in testable form; the evidence plan
- `docs/ARCHITECTURE.md` — sources of truth, components, data model, paths,
  egress and keys, ports, consistency, layout
- `docs/ROADMAP.md` — R0 spine → R1 imports → R2 deterministic matching →
  R3 review, rules and periods → R4 the model for ambiguous cases → R5
  reports and workspace → R6 evidence battery → R7 pilot and ship gate →
  R8 India → R9 PDF
- `docs/DECISIONS.md` — D-001 to D-067 and the questions still open

## Running it (R0, R1)

```sh
scripts/stack.sh up        # PostgreSQL 18.7 on 127.0.0.1:62432 (compose project accounting-reconciliation)
scripts/stack.sh setup     # roles recon_owner / recon_runtime / recon_app, database recon, migrations
eval "$(scripts/stack.sh env)"
RECON_OPERATOR_TOKEN=$(openssl rand -hex 24) cargo run -p recon-server   # 127.0.0.1:62480
scripts/check.sh           # what CI runs: fmt, clippy, architecture, tests, drills, cargo-deny
FUZZ_TOOLCHAIN=nightly FUZZ_SECONDS=20 scripts/fuzz.sh   # CI's fuzz job (60 s per target)
```

Imports (every `/v1` route takes `Authorization: Bearer $RECON_OPERATOR_TOKEN`):

| Route | What it does |
|---|---|
| `POST /v1/firms/{f}/profiles` | store the next version of a CSV profile (`statement_csv` or `documents_csv`, D-062) |
| `GET /v1/firms/{f}/profiles/{name}[?version=]` | a profile: the firm's, or `r0-generic-csv`, `ofx`, `r1-generic-documents` |
| `POST /v1/firms/{f}/accounts/{a}/imports[?profile=&version=]` | a statement: `text/csv`, or `application/x-ofx` with `profile=ofx`; `201` complete, `200` the same file again, `202` held as a revision, `422` refused with rows (or `ambiguous_date_format` with its question) |
| `POST /v1/firms/{f}/clients/{c}/document-imports[?profile=]` | invoices, bills, receipts, credit notes, book entries (`text/csv`) |
| `GET /v1/firms/{f}/imports/{i}[/changes]` | an import; a revision's differences |
| `POST /v1/firms/{f}/imports/{i}/accept`, `…/reject` | decide a held revision (D-065) |

Prefix cargo with `export PATH="$(brew --prefix rustup)/bin:$PATH"` on a
machine where Homebrew's cargo shadows rustup (the toolchain is pinned to
1.99.1).

## Stack

Rust (Tokio, Axum, SQLx) over PostgreSQL; a SvelteKit + TypeScript (strict)
+ Tailwind CSS v4 + shadcn-svelte workspace, started from a copy of the
Colophon UI package. No vector database, no agent framework.

## What counts as done

The ship gate in `docs/END_GOAL.md`: zero false auto-matches on the
labelled dataset, every line's final state explained, one or two real
accountants each closing a real month with the tool, and every failure
case passing on the release commit with the 1,000,000-line load evidence
committed under `perf/results/` (machine + commit on every result).

## License

MIT — see `LICENSE`.
