# KNOWLEDGE — hard-won operational and security knowledge

Everything here was learned the expensive way: an independent white-hat review,
a concurrency drill, or a mutation proof that had to be argued with. Each entry
names where the fact lives in code so it can be re-verified instead of believed.

## The independent review (origin/main..e4b41dd) and what it changed

Two S1s and five S2s blocked the merge; every fix is red-first with its test.

| Finding | Essence | Fix lives in |
| --- | --- | --- |
| S1-1 | `close` read line counts and the period digest while an import flip could still commit lines into the month — the row-level closed check was an unlocked `SELECT` | `migrations/0012_period_write_lock.sql`, `periods::close` takes the account advisory lock |
| S1-2 | `PikeVM::new` built the NFA with **no size limit** — `((a{1000}){1000}){1000}` is 25 bytes of pattern and 10^9 states, checked only after the build | `rule.rs` compiles via `PikeVM::builder()` with `nfa_size_limit` + a syntax nest limit of 32; `re_evaluate` runs in `spawn_blocking` inside a lines × rules × bytes budget |
| S2-3 | `normalize` uppercased the regex SOURCE: `\d` → `\D`, `\b` → `\B` — escape classes inverted | regex values are never normalized; the NFA compiles case-insensitively instead |
| S2-4 | a preparer held `Act::Resolve`, which also gated exception acknowledgement | `Act::AcknowledgeException` (reviewer-level) gates it; preparer → `403` |
| S2-5 | the model validator checked allocation shape but not residual positivity or the rounding bound | `validate` builds through `BalancedMatch::new` so the engine's own refusals (`RoundingTooLarge`, `Unbalanced`) apply; `NonPositiveResidual` added; residual ⇒ four-eyes |
| S2-6 | D-076's "the database locks the month" covered only line inserts/status updates | `migrations/0013_period_write_scope.sql` locks the touched periods `FOR KEY SHARE` on exception rows, match status, all three allocation tables, proposals, and line/match-subject decisions |
| S2-7 | rule count was checked unlocked (racing creates could pass the cap); versions unbounded | per-client advisory lock `7264` around count+insert; `MAX_VERSIONS_PER_RULE = 100`; `list` bounded |

S3 items are tracked, not fixed: `accept_revision` skips `apply_exclusions` and has
no named closed-period pre-check; `explain::line` is unscoped by client (operator
CLI only, never HTTP); period/rule_version FKs don't tie client to account/rule;
the error.rs comment says a closed-period import "stays staging" while the code
discards it (D-076's "discarded `period_closed`" is the truth — fix the comment).

## The period write lock — how the mechanism actually works

- Advisory transaction locks (all `pg_try_advisory_xact_lock(hashtextextended(id, N))`):
  `7262` account flip (`imports.rs` `ACCOUNT_LOCK`), `7263` client documents,
  `7264` client rule create (`rules.rs` `RULES_LOCK`). `periods::close` takes
  `7262` — the SAME key the flip takes — so a close can never run inside a flip.
  Contention is a named refusal: `PeriodError::AccountBusy` → HTTP `409
  account_busy` + `Retry-After: 2`; rule-create contention → `RuleError::Conflict`.
- Statement-level transition-table triggers do the row-level work: PostgreSQL
  forbids transition tables on multi-event triggers AND on `UPDATE OF col`
  triggers, so 0012 uses one `AFTER INSERT` and one `AFTER UPDATE` (no column
  list — every line update is guarded, which S2-6 needed anyway).
- Every guard (0012 on `statement_lines`; 0013 on `line_exceptions`,
  `match_lines`, `matches`, `match_documents`, `match_residuals`, `proposals`,
  `decision_log`) upserts the touched `(firm, client, account, month)` periods,
  locks them `FOR KEY SHARE` in the same sorted order, then raises `RCP02` if
  any is closed. `FOR KEY SHARE` never conflicts with itself — writers never
  block each other; only `close`'s `FOR UPDATE` conflicts, and it locks exactly
  one row, so the scheme is deadlock-free by construction.
- Ordering consequence: a write either lands before close's `FOR UPDATE` (then
  the digest sees it) or blocks until the close commits and is refused `RCP02`.
  There is no third outcome, so the stored digest can't silently miss a decision.
- The test pattern for "X waits on Y's uncommitted write": do Y's write on a
  held transaction, spawn X, poll `pg_stat_activity` for `wait_event_type =
  'Lock'` — wait on the lock signal, never on time. See
  `a_close_waits_on_an_in_flight_acknowledgement` and the `FlipHook` pause in
  `a_close_during_a_flip_is_refused_busy` (`recon-db/tests/rules_periods.rs`,
  `tests/common/mod.rs::staged`).

## Regex rules — bounds and the normalization trap

- Compile path (`rule.rs`): `PikeVM::builder().syntax(case_insensitive +
  nest_limit(32)).thompson(nfa_size_limit(10 KiB))`. Build errors are
  discriminated by the public `BuildError::size_limit()` → `RegexTooBig`,
  else `RegexInvalid`; `kind()` is private — don't reach for it.
- `normalize` never touches regex source (S2-3); normalization would invert
  every escape class. Case-insensitivity is a compile flag, not a string edit.
- Evaluation: `re_evaluate` runs the rules in `tokio::task::spawn_blocking` —
  regex work is CPU-bound and must never sit on a Tokio worker — and the whole
  pass is refused `EvaluationTooLarge` past the lines × rules × bytes budget.
- Fuzz seed for the nested-quantifier bomb lives in `fuzz/seeds/rule-predicate/`.

## HTTP classification worth knowing

- `503` naming (`recon-server/src/error.rs`, D-075): `PoolTimedOut` and
  SQLSTATEs `55P03` (lock not available), `53300` (too many connections),
  `40001` (serialization), `40P01` (deadlock), **`57014` (statement timeout)**
  → `overloaded` + `Retry-After`. Only a genuinely unreachable database is
  `database_unavailable`. `57014` matters for the kill-mid-import drill: the
  killed process's session can still be finishing its flip, so the resuming
  upload hits a statement timeout while the database is perfectly up — a
  contention answer, not an outage. Callers (drills, loadgen) honour
  `Retry-After` with bounded retries; the server never queues — the prompt 503
  is the contract.
- `409 account_busy` (`Retry-After: 2`) — the account's advisory lock is held;
  retry is the designed answer.

## The mutation proof (`scripts/prove-attacks.sh` + `attacks-mutants.py`)

- 17 named mutants, one per attack; each must make exactly its attack go red,
  and the binary must COMPILE under the mutant (a broken mutant is a broken
  proof, not a pass). The tree is restored after every mutant and on exit.
- Findings the proof itself surfaced, kept because they're load-bearing:
  - axum 0.8 `Router::layer` runs **post-match**: a `map_request` that rewrites
    the URI inside the layer cannot reroute — verified live (an unconditional
    rewrite still answered 200). URI rewriting works only at the connection
    edge (`serve.rs` `service_fn`), which is where the forwarded-header mutant
    lives.
  - A "wrong-shape" attack input can produce the refusal for the wrong reason
    (a non-hex forged cookie was refused by the decoder before the session
    lookup ran). Mutants that survive almost always mean the test's probe never
    reached the protection — fix the probe, not the assertion.
  - `POST /v1/firms/{f}/users` legitimately accepts the operator token (D-041
    bootstrap) — it is not a person-only route; the operator probe uses rules.
  - Two size caps exist on upload (declared-length precheck + the streamed
    decoder bound) — a mutant must remove both.
- Every attack asserts the SPECIFIC refusal: status AND the named error code
  AND stored state re-read afterwards (a refused write leaves no row). Waiting
  on a socket/response signal with a timeout, never on sleeps or silence.

## Test and verify workflow (this machine)

- Heavy commands go through the coordinator's heavy-command wrapper (a
  slot-limiting script outside this repo): `heavy.sh <argv>` — it `exec`s
  argv, so compounds need `bash -lc '…'` as the argv, and it must run with
  the worktree as cwd.
- Cargo needs the rustup shim (Homebrew cargo ignores `rust-toolchain.toml`):
  `export PATH="$(brew --prefix rustup)/bin:$PATH"`.
- Test database: `scripts/stack.sh env` provides `RECON_TEST_ADMIN_URL`
  (postgres://recon_admin:recon-admin-dev@127.0.0.1:62432/recon_admin);
  `recon-db` tests create per-test schemas/databases and clean them up.
- `scripts/check.sh` is the CI gate: fmt, clippy `-D warnings`, workspace
  tests, `cargo deny`, architecture checks, drills. `prove-attacks.sh` is its
  own CI job; fuzz is `scripts/fuzz.sh`.
