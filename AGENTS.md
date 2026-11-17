# Accounting Reconciliation — agent instructions

A reconciliation workspace for small accounting firms: bank statements and
documents normalized into one schema, deterministic matching first, a
model only as a proposer for ambiguous cases, approval gates, explicit
versioned rules and an append-only audit trail. Rust + Axum + Tokio + SQLx
over PostgreSQL; a SvelteKit workspace. There is no engineering guide:
`docs/END_GOAL.md` and `docs/DECISIONS.md` are the contract. Status: R0
(the spine) built, in progress again for review fixes — see `docs/ROADMAP.md`; the
ship gate is blocked on pilot accountants (README, *Blocked on*). The local
stack, ports and database roles are D-036 and D-037.

## Read first

1. `docs/END_GOAL.md` — what done means, scope in and out, the invariants,
   the ten required failure cases and the security cases (each with its
   test and pass condition), the ship gate.
2. `docs/ARCHITECTURE.md` — sources of truth, components, data model,
   paths, egress and keys, consistency, layout.
3. `docs/DECISIONS.md` — D-001 to D-045 and the open questions; do not
   silently resolve an open question — record a new D-NNN when one is
   decided.
4. `docs/ROADMAP.md` — R0–R9; each milestone's gate is its proof, and the
   gate passes before the next milestone starts.

## Non-negotiables

1. **Completeness.** Every imported row becomes one line, a reported
   duplicate or a reported refusal; every line is in exactly one state; a
   period closes only when nothing in it is unreviewed or proposed.
2. **Only rules and people commit.** The T1 tier is the only automatic
   commit (D-011); model and fuzzy-tier output are proposals; a model never
   commits, edits or deletes anything.
3. **Exact money.** `i64` minor units with the D-005 exponent table,
   checked arithmetic, no floats anywhere in parsing or matching; a match
   balances exactly or is refused; residuals are named and reviewed
   (D-014).
4. **Idempotent, atomic imports.** Staged and flipped visible in one
   transaction; deduplicated by file hash and line fingerprint (D-008); a
   changed statement is a revision (D-009); one malformed row refuses the
   file (D-010).
5. **Append-only audit trail.** Every state change writes a decision with
   actor, reason and evidence; the database refuses `UPDATE`, `DELETE` and
   `TRUNCATE` on the log; the chain per client verifies (D-019).
6. **Rules are explicit and versioned.** Approved by a person, immutable
   per version, recorded on every decision; a rule change never rewrites a
   past decision; closed periods are locked (D-015).
7. **Content is data.** Narrations, counterparty names and document text
   are never instructions; model answers are validated against the
   candidate set they were given (D-016).
8. **Isolation and protection.** Composite firm keys (D-020), a second
   factor for every user (D-021), per-client encryption of raw files
   (D-022), retention set per client (D-023), no outbound call except the
   configured model endpoint (D-024), no secrets or financial content in
   logs (D-025).
9. **Bounded everything; evidence over claims.** Every upload, parser,
   queue, search, model call and job has a limit and a deadline; load
   results carry machine + commit + machine load; misses are kept, never
   rerun until green; laptop numbers are development observations; no
   third party is load-tested.
10. **Pure core.** `recon-domain`, `recon-import` and `recon-match` carry no
    I/O — enforced by the architecture check.

## Rust rules

- Use the `rust-mastery` skill, and prefix every cargo command with
  `export PATH="$(brew --prefix rustup)/bin:$PATH"` — Homebrew's cargo comes
  first on `PATH` and ignores the pinned toolchain.
- Typed errors (`thiserror`) in libraries; `anyhow` only in binaries. No
  `unwrap`/`expect` on external input — every byte of an uploaded file and
  of a model answer is external input. Every I/O await has a deadline.
- Money is checked integer arithmetic with an explicit currency exponent —
  never floats, never unchecked `+`/`-` on amounts.

## Frontend rules

- SvelteKit + TypeScript (strict) + Tailwind CSS v4 + shadcn-svelte, from
  this repository's own copy of the Colophon UI package in `packages/ui` —
  never a dependency on the Colophon repository.
- The workspace is a static client-side app served by the Rust server on
  the same origin; it never queries PostgreSQL and holds no authority.
- A model's answer is always labelled as the model's; scores show their
  breakdown; keyboard completion of review and close is part of "works".

## Testing rule (workspace standard)

Every feature ships regression tests + a fixture path (`fixtures/`: the
seeded generator, labelled sets, hostile files, injection and
wrong-match fixtures, recorded model responses) + a load scenario, in the
same change. Every fixed bug gets a test that fails without the fix. The
ten failure cases in `docs/END_GOAL.md` are contract, not options. After
every drill, the decision log is replayed and must equal the stored
current state.

## Verification gate (CI from the first R0 commit)

`scripts/check.sh` runs what CI runs: `cargo fmt --all --check`;
`cargo clippy --workspace --all-targets --locked -- -D warnings`;
`cargo test --workspace --locked`; `cargo deny check`; the architecture
checks (no I/O in `recon-domain`, `recon-import` or `recon-match`; an HTTP
client only in `recon-model`; the workspace app imports no database
client); `svelte-check` and a workspace build once the app exists; the
drills of every finished milestone. Full load runs are ≥1,000,000 lines
per the workspace standard; results are committed under `perf/results/`
with machine + commit.

## Working rules

- Small slices: each states the user outcome, the invariant, the failure
  case, the permission and the acceptance test.
- Report exactly what ran and what passed; honest `unknown` beats a
  reassuring claim.
- Repository: `github.com/akira-shirase-oss/accounting-reconciliation`.
  Work lands on `main` in small green steps: `scripts/check.sh` passes,
  then commit and push. No long-lived branches, no force-push, no history
  rewrites.
- A roadmap status cell says Done only when it names its tests and
  evidence path (D-031).
- Never commit real client data, even anonymized (D-028).
