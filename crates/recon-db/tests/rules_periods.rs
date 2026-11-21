//! R3 slice 3 (D-015, D-006, failure case 5): typed rules approved into immutable
//! versions, automatic exclusions applied at the flip, proposal-only re-evaluation,
//! and the period lock — close needs every line decided and every exception
//! acknowledged; reopening is owner-only with a reason and is itself a decision.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

mod common;

use std::fmt::Write as _;
use std::time::Duration;

use common::{import, import_documents, month_rows, rows, tenant};
use recon_db::matching::{self, Decided, DecidedLine, Start};
use recon_db::tenancy::Account;
use recon_db::testing::TestDb;
use recon_db::{imports, periods, review, rules};
use recon_domain::auth::Role;
use recon_domain::decision::{DecisionRecord, SubjectKind};
use recon_domain::line::{ActorKind, ExceptionKind, ExcludeReason};
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, ScoreBreakdown, Tier,
};
use recon_domain::rule::{RuleAction, RulePredicate, TextMatch, TextOp};
use recon_domain::{ClientId, Currency, DocumentId, FirmId, LineId, Money, UserId};
use serde_json::Value;
use sqlx::PgPool;
use time::Date;
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(30);
const TX_TIMEOUT: Duration = Duration::from_secs(15);
const ENGINE: &str = "match-v1";

fn usd(minor: i64) -> Money {
    Money::new(minor, Currency::Usd).unwrap()
}

fn march() -> Date {
    Date::from_calendar_date(2026, time::Month::March, 1).unwrap()
}

/// A bare `proposed` match — RCM02 requires its decision in the same transaction.
async fn insert_match(pool: &PgPool, firm: FirmId, client: ClientId) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO recon.matches (firm_id, client_id, tier, status, currency)
         VALUES ($1, $2, 't1', 'proposed', 'USD') RETURNING id",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    recon_db::decisions::insert(
        &mut tx,
        firm,
        &recon_db::decisions::NewDecision {
            client,
            subject_kind: SubjectKind::Match,
            subject_id: id,
            from_state: None,
            to_state: "proposed",
            actor_kind: ActorKind::Person,
            actor_ref: "test",
            reason_code: "test",
            evidence: b"{}",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

fn april() -> Date {
    Date::from_calendar_date(2026, time::Month::April, 1).unwrap()
}

/// `days.len()` rows of `-1.00` on the given days of `year`-`month`, each narrated
/// `tag` so a narration predicate can aim at them.
fn dated_rows(year: i32, month: u32, days: &[u32], tag: &str) -> String {
    let mut out = String::new();
    for (i, day) in days.iter().enumerate() {
        writeln!(
            out,
            "{year}-{month:02}-{day:02},,-1.00,USD,{tag} {i},REF-{year}{month:02}{day:02}-{i},Acme Ltd,"
        )
        .unwrap();
    }
    out
}

async fn owner(pool: &PgPool, firm: FirmId, tag: &str) -> UserId {
    recon_db::auth::create_user(
        pool,
        firm,
        &format!("{tag}@example.test"),
        tag,
        Role::Owner,
        None,
    )
    .await
    .unwrap()
}

/// A narration-contains-`BANK FEE` → exclude predicate pair.
fn fee_rule() -> (RulePredicate, RuleAction) {
    (
        RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Contains,
                value: "bank fee".to_owned(),
            }),
            ..RulePredicate::default()
        },
        RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        },
    )
}

/// The line ids of `account`'s statement lines in `status`, oldest row first.
async fn lines_in(pool: &PgPool, account: &Account, status: &str) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM recon.statement_lines
          WHERE firm_id = $1 AND account_id = $2 AND status = $3 ORDER BY booking_date, row_no",
    )
    .bind(account.firm.as_uuid())
    .bind(account.id.as_uuid())
    .bind(status)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The parsed evidence of a decision.
fn evidence(d: &DecisionRecord) -> Value {
    serde_json::from_slice(&d.evidence).unwrap()
}

async fn start_job(db: &TestDb, account: &Account) -> matching::JobLease {
    let job = match matching::start(&db.pool, account.firm, account.client, LEASE, ENGINE)
        .await
        .unwrap()
    {
        Start::Created(l) | Start::Resumed(l) => l,
        Start::Busy { job } => panic!("the job {job} is busy"),
    };
    matching::snapshot(&db.pool, &job).await.unwrap();
    job
}

fn t1(line: LineId, doc: DocumentId, doc_amount: i64) -> DecidedLine {
    let amount = usd(-100);
    DecidedLine {
        seq: 1,
        line,
        outcome: Decided::Matched {
            tier: Tier::T1,
            matched: BalancedMatch::new(
                vec![LineAllocation { line, amount }],
                vec![DocumentAllocation {
                    document: doc,
                    amount: usd(doc_amount),
                }],
                vec![],
            )
            .unwrap(),
            score: ScoreBreakdown::new(true, true, true, 8, true).unwrap(),
            commit: true,
        },
    }
}

// --- Immutable versions ---------------------------------------------------------------------------

/// A rule version's content never changes once written; only its status may move
/// `proposed` → `approved`, exactly once, with a decision each time. Raw SQL gets the
/// same refusal (the database, not the API, is the guarantee).
#[tokio::test]
async fn a_rule_version_is_immutable_once_written() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        1,
        user,
        true,
    )
    .await
    .unwrap();

    // Content is frozen: every column except status/approved_* refuses, even raw SQL.
    for sql in [
        "UPDATE recon.rule_versions SET predicate = '{}'::jsonb",
        "UPDATE recon.rule_versions SET action = 'assign_counterparty'",
        "UPDATE recon.rule_versions SET action_arg = 'other'",
        "UPDATE recon.rule_versions SET auto_apply = false",
        "UPDATE recon.rule_versions SET proposed_by = gen_random_uuid()",
        "DELETE FROM recon.rule_versions",
        "UPDATE recon.rule_versions SET status = 'proposed'",
        "UPDATE recon.rule_versions SET status = 'withdrawn'",
    ] {
        assert!(
            sqlx::query(sql).execute(&db.pool).await.is_err(),
            "{sql} must be refused (an immutable version)"
        );
    }
    // The rule's own row freezes name and client, and is never deleted.
    assert!(
        sqlx::query("UPDATE recon.rules SET name = 'other'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM recon.rules")
            .execute(&db.pool)
            .await
            .is_err()
    );

    // Its decisions carry who proposed and approved, and the version they act on.
    let decisions =
        recon_db::decisions::for_subject(&db.pool, account.firm, SubjectKind::Rule, rule.id)
            .await
            .unwrap();
    let states: Vec<&str> = decisions.iter().map(|d| d.to_state.as_str()).collect();
    assert_eq!(states, ["proposed", "approved"], "{decisions:?}");
    assert_eq!(evidence(&decisions[1])["version"].as_i64().unwrap(), 1);
    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// An approved auto-apply exclusion commits at the flip: the line lands `excluded`
/// with a rule decision naming the rule and the version that made it (D-015).
#[tokio::test]
async fn an_auto_apply_rule_excludes_at_the_flip() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        1,
        user,
        true,
    )
    .await
    .unwrap();

    import(
        &db.pool,
        &account,
        &dated_rows(2026, 3, &[1, 2, 3], "BANK FEE"),
    )
    .await
    .unwrap()
    .complete();
    // Days outside the first file's span (a date overlap would be a revision).
    import(&db.pool, &account, &month_rows(10, 2))
        .await
        .unwrap()
        .complete();

    let excluded = lines_in(&db.pool, &account, "excluded").await;
    assert_eq!(excluded.len(), 3);
    assert_eq!(lines_in(&db.pool, &account, "unreviewed").await.len(), 2);
    for line in excluded {
        let decisions =
            recon_db::decisions::for_subject(&db.pool, account.firm, SubjectKind::Line, line)
                .await
                .unwrap();
        let last = decisions.last().unwrap();
        assert_eq!(last.to_state, "excluded");
        assert_eq!(last.actor_kind, "rule");
        assert_eq!(last.actor_ref, format!("rule:{}@v1", rule.id));
        assert_eq!(last.reason_code, "out_of_scope");
        assert_eq!(evidence(last)["version"].as_i64().unwrap(), 1);
    }
    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// A proposed (not yet approved) version never applies; an approved version without
/// the owner's auto-apply mark proposes only — re-evaluation names it, commits nothing.
#[tokio::test]
async fn a_rule_only_auto_applies_after_approval_and_the_owners_mark() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    // Still proposed: nothing may apply it.
    import(
        &db.pool,
        &account,
        &dated_rows(2026, 3, &[1, 2], "BANK FEE"),
    )
    .await
    .unwrap()
    .complete();
    assert_eq!(lines_in(&db.pool, &account, "excluded").await.len(), 0);

    // Approved without the owner's auto-apply mark: still proposals only.
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        1,
        user,
        false,
    )
    .await
    .unwrap();
    import(
        &db.pool,
        &account,
        &dated_rows(2026, 3, &[3, 4], "BANK FEE"),
    )
    .await
    .unwrap()
    .complete();
    assert_eq!(lines_in(&db.pool, &account, "excluded").await.len(), 0);

    let diff = periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap();
    assert_eq!(diff.proposals.len(), 4, "{diff:?}");
    assert!(
        diff.proposals
            .iter()
            .all(|p| p.rule == rule.id && p.version == 1)
    );
    // Nothing committed: every line is still undecided.
    assert_eq!(lines_in(&db.pool, &account, "unreviewed").await.len(), 4);
    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

// --- Period close, lock, reopen --------------------------------------------------------------------

/// Close refuses while any line is `unreviewed` or `proposed`, or any exception lacks
/// a reviewer's acknowledgement (D-006); a closed period records the decision digest
/// and the chain head and locks its lines — even raw SQL — until an owner reopens it
/// with a reason, which is itself a decision.
#[tokio::test]
async fn a_period_locks_until_an_owner_reopens_it() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    import(&db.pool, &account, &rows(0, 3))
        .await
        .unwrap()
        .complete();
    let mut undecided = lines_in(&db.pool, &account, "unreviewed").await;
    assert_eq!(undecided.len(), 3);

    // Undecided lines refuse the close.
    let refused = periods::close(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, periods::PeriodError::UndecidedLines(3)));

    // Decide two, leave one a typed exception (unacknowledged).
    for line in undecided.drain(..2).map(LineId::from_uuid) {
        review::exclude(
            &db.pool,
            account.firm,
            account.client,
            line,
            user.as_uuid(),
            1,
            ExcludeReason::NonBusiness,
            None,
        )
        .await
        .unwrap();
    }
    let exception_line = LineId::from_uuid(undecided.pop().unwrap());
    let job = start_job(&db, &account).await;
    matching::commit_chunk(
        &db.pool,
        &job,
        &[DecidedLine {
            seq: 1,
            line: exception_line,
            outcome: Decided::Exception {
                kind: ExceptionKind::NoCandidate,
                candidates: vec![],
            },
        }],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();

    // The live exception is unacknowledged: still no close.
    let refused = periods::close(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, periods::PeriodError::Unacknowledged(1)));

    // A reviewer acknowledges it; the line stays an exception (its type is history).
    review::acknowledge(
        &db.pool,
        account.firm,
        account.client,
        exception_line,
        user.as_uuid(),
    )
    .await
    .unwrap();
    let period = periods::close(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap();
    assert_eq!(period.status, "closed");
    let digest = period.digest.expect("a closed period stores its digest");
    assert!(period.chain_seq.unwrap() > 0 && !period.chain_head.unwrap().is_empty());

    // Locked: a status change on a line in the closed month refuses — even raw SQL,
    // and so does a review decision on it.
    assert!(
        sqlx::query(
            "UPDATE recon.statement_lines SET status = 'unreviewed', row_version = row_version + 1
              WHERE status = 'excluded'"
        )
        .execute(&db.pool)
        .await
        .is_err(),
        "a closed period's lines are locked"
    );
    let excluded_line = LineId::from_uuid(lines_in(&db.pool, &account, "excluded").await[0]);
    let refused = review::exclude(
        &db.pool,
        account.firm,
        account.client,
        excluded_line,
        user.as_uuid(),
        2,
        ExcludeReason::OutOfScope,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, review::ReviewError::PeriodClosed));

    // Imports into the closed month refuse by name too.
    let Err(refused) = import(
        &db.pool,
        &account,
        &dated_rows(2026, 3, &[20, 21], "LATE FEE"),
    )
    .await
    else {
        panic!("an import into a closed period must refuse")
    };
    assert!(matches!(refused, recon_db::DbError::PeriodClosed));

    // Re-evaluation of the closed period refuses.
    let refused = periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, periods::PeriodError::Closed));

    // Reopen needs a reason; the digest of the close is untouched.
    let refused = periods::reopen(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
        "   ",
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, periods::PeriodError::ReasonRequired));
    let period = periods::reopen(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
        "the auditor asked for one more check",
    )
    .await
    .unwrap();
    assert_eq!(period.status, "reopened");
    assert_eq!(
        period.digest.as_deref(),
        Some(digest.as_slice()),
        "the stored digest of the close is history, kept"
    );

    // The reopen is a decision; the line lock is off again.
    let decisions =
        recon_db::decisions::for_subject(&db.pool, account.firm, SubjectKind::Period, period.id)
            .await
            .unwrap();
    let states: Vec<&str> = decisions.iter().map(|d| d.to_state.as_str()).collect();
    assert_eq!(states, ["closed", "reopened"], "{decisions:?}");
    assert!(
        evidence(&decisions[1])["reason"]
            .as_str()
            .unwrap()
            .contains("auditor")
    );
    import(&db.pool, &account, &dated_rows(2026, 3, &[20], "LATE FEE"))
        .await
        .unwrap()
        .complete();

    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

// --- Failure case 5: a rule change never rewrites past decisions ------------------------------------

/// The `END_GOAL` failure case: rule v1 excludes `BANK FEE` lines in a closed March and
/// an open April; v2 changes the predicate. Afterwards: March's stored digest is
/// byte-identical, April's committed match and v1 exclusions are unchanged, the closed
/// March refuses re-evaluation until the owner reopens it with a reason, and
/// re-evaluation anywhere yields proposals and zero commits.
#[tokio::test]
async fn a_rule_change_never_rewrites_past_decisions() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (v1_predicate, v1_action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &v1_predicate,
        &v1_action,
    )
    .await
    .unwrap();
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        1,
        user,
        true,
    )
    .await
    .unwrap();

    // March: only v1-matching lines — all land excluded at the flip → the month closes.
    import(
        &db.pool,
        &account,
        &dated_rows(2026, 3, &[1, 2], "BANK FEE"),
    )
    .await
    .unwrap()
    .complete();
    // April: v1-matching lines plus one line a T1 commit matches to a bill.
    import(
        &db.pool,
        &account,
        &dated_rows(2026, 4, &[1, 2], "BANK FEE"),
    )
    .await
    .unwrap()
    .complete();
    // One April line the rule does not match: it stays unreviewed for the T1 commit,
    // and a date outside March's spans keeps this file from being a revision.
    import(&db.pool, &account, &dated_rows(2026, 4, &[10], "PAYMENT"))
        .await
        .unwrap()
        .complete();
    import_documents(
        &db.pool,
        &account,
        "purchase_bill,INV-A,2026-03-01,2026-03-31,-1.00,USD,Acme Ltd,\n",
    )
    .await
    .unwrap()
    .complete();

    let march_period = periods::close(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap();
    let closed_digest = march_period.digest.clone().unwrap();

    // The April T1 commit.
    let unreviewed = lines_in(&db.pool, &account, "unreviewed").await;
    assert_eq!(unreviewed.len(), 1, "{unreviewed:?}");
    let line = LineId::from_uuid(unreviewed[0]);
    let doc = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM recon.documents WHERE firm_id = $1 AND kind = 'purchase_bill'",
    )
    .bind(account.firm.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let doc_amount: i64 =
        sqlx::query_scalar("SELECT amount_minor FROM recon.documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let job = start_job(&db, &account).await;
    matching::commit_chunk(
        &db.pool,
        &job,
        &[t1(line, DocumentId::from_uuid(doc), doc_amount)],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();
    let matched_before =
        recon_db::decisions::for_subject(&db.pool, account.firm, SubjectKind::Line, line.as_uuid())
            .await
            .unwrap();
    assert_eq!(matched_before.last().unwrap().to_state, "matched");

    // v2: a different predicate, approved — v1's decisions are history.
    let (v2_predicate, v2_action) = (
        RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Contains,
                value: "late fee".to_owned(),
            }),
            ..RulePredicate::default()
        },
        RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        },
    );
    rules::propose_version(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        user,
        &v2_predicate,
        &v2_action,
    )
    .await
    .unwrap();
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        2,
        user,
        true,
    )
    .await
    .unwrap();

    // (a) the closed period's decision digest is byte-identical.
    let recomputed = periods::digest(&db.pool, account.firm, account.client, account.id, march())
        .await
        .unwrap();
    assert_eq!(recomputed.as_slice(), closed_digest.as_slice());
    // (b) committed matches and v1 exclusions in the open period are unchanged.
    let matched_after =
        recon_db::decisions::for_subject(&db.pool, account.firm, SubjectKind::Line, line.as_uuid())
            .await
            .unwrap();
    assert_eq!(
        matched_before.len(),
        matched_after.len(),
        "no new decision on the committed line"
    );
    assert_eq!(lines_in(&db.pool, &account, "matched").await.len(), 1);
    // (c) re-evaluation is proposals and zero commits — and the closed March refuses.
    let refused = periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, periods::PeriodError::Closed));
    let diff = periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        april(),
        user,
    )
    .await
    .unwrap();
    assert_eq!(diff.proposals.len(), 0, "April has no undecided lines");
    assert_eq!(lines_in(&db.pool, &account, "matched").await.len(), 1);
    // (d) the owner reopens with a reason; only then does March re-evaluate.
    periods::reopen(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
        "re-checking under rule v2",
    )
    .await
    .unwrap();
    let diff = periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap();
    assert_eq!(diff.proposals.len(), 0, "March's lines are all decided");
    // …and even after reopening, the stored digest of the close is byte-identical.
    let recomputed = periods::digest(&db.pool, account.firm, account.client, account.id, march())
        .await
        .unwrap();
    assert_eq!(recomputed.as_slice(), closed_digest.as_slice());

    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// Versions are numbered once each; only one version is active; re-approving a decided
/// version conflicts; a retired rule refuses new versions and never applies.
#[tokio::test]
async fn versions_are_numbered_and_a_retired_rule_stops_applying() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    rules::approve(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        1,
        user,
        true,
    )
    .await
    .unwrap();
    // Approving the same version again is a conflict, not a second approval.
    assert!(matches!(
        rules::approve(
            &db.pool,
            account.firm,
            account.client,
            rule.id,
            1,
            user,
            true,
        )
        .await
        .unwrap_err(),
        rules::RuleError::Conflict(_)
    ));

    // Retire: the rule stops applying; new versions are refused.
    rules::retire(&db.pool, account.firm, account.client, rule.id, user)
        .await
        .unwrap();
    assert!(matches!(
        rules::propose_version(
            &db.pool,
            account.firm,
            account.client,
            rule.id,
            user,
            &predicate,
            &action,
        )
        .await
        .unwrap_err(),
        rules::RuleError::Retired
    ));
    import(&db.pool, &account, &dated_rows(2026, 3, &[5], "BANK FEE"))
        .await
        .unwrap()
        .complete();
    assert_eq!(lines_in(&db.pool, &account, "excluded").await.len(), 0);
    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// Rules and periods are tenant data: another firm cannot see or act on them, and a
/// rule never evaluates outside its own client.
#[tokio::test]
async fn rules_and_periods_keep_their_tenant() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let other = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "bank fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    // Acting on it through another tenant is NotFound, not a leak.
    assert!(matches!(
        rules::approve(&db.pool, other.firm, other.client, rule.id, 1, user, true,)
            .await
            .unwrap_err(),
        rules::RuleError::NotFound
    ));
    assert!(matches!(
        rules::retire(&db.pool, other.firm, other.client, rule.id, user)
            .await
            .unwrap_err(),
        rules::RuleError::NotFound
    ));
    assert!(
        rules::list(&db.pool, other.firm, other.client)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        periods::close(
            &db.pool,
            other.firm,
            other.client,
            account.id,
            march(),
            user,
        )
        .await
        .unwrap_err(),
        periods::PeriodError::NotFound
    ));
    common::replay_ok(&db, account.firm).await;
    db.cleanup().await;
}

/// S1-1 (failure case 1's race): a flip in flight holds the account's advisory
/// lock; a close racing it is refused busy — never commits a digest the
/// in-flight lines are absent from. Two real connections; a [`imports::FlipHook`]
/// holds the flip inside its transaction at [`imports::FlipPoint::LinesInserted`].
#[tokio::test]
async fn a_close_during_a_flip_is_refused_busy() {
    struct Gate {
        reached: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl imports::FlipHook for Gate {
        async fn reached(&self, point: imports::FlipPoint) {
            if matches!(point, imports::FlipPoint::LinesInserted) {
                self.reached.notify_one();
                self.release.notified().await;
            }
        }
    }
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let body = format!("{}{}", common::HEADER, dated_rows(2026, 3, &[1, 2], "FEE"));
    let (staging, summary) = common::staged(&db.pool, &account, body.as_bytes())
        .await
        .unwrap();
    let gate = std::sync::Arc::new(Gate {
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let flip = {
        let pool = db.pool.clone();
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move {
            imports::flip(
                &pool,
                staging,
                &summary,
                common::OPERATOR,
                &*gate,
                common::FLIP_TIMEOUT,
            )
            .await
        })
    };
    gate.reached.notified().await;
    let refused = periods::close(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await;
    let Err(refused) = refused else {
        panic!("a close raced an in-flight flip and committed — the busy refusal is missing");
    };
    assert!(
        matches!(refused, periods::PeriodError::AccountBusy),
        "a close raced an in-flight flip and answered {refused:?} — it must refuse busy, \
         never commit a digest the new lines are absent from"
    );
    gate.release.notify_one();
    flip.await.unwrap().unwrap();
    assert_eq!(lines_in(&db.pool, &account, "unreviewed").await.len(), 2);
    db.cleanup().await;
}

/// S2-7: a rule's version history is bounded — the cap+1'th proposal refuses by
/// name and a listing never returns more than the cap.
#[tokio::test]
async fn a_rules_versions_are_capped() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    let rule = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "fees",
        &predicate,
        &action,
    )
    .await
    .unwrap();
    // Version 1 exists from create; propose up to the cap.
    for _ in 1..rules::MAX_VERSIONS_PER_RULE {
        rules::propose_version(
            &db.pool,
            account.firm,
            account.client,
            rule.id,
            user,
            &predicate,
            &action,
        )
        .await
        .unwrap();
    }
    let refused = rules::propose_version(
        &db.pool,
        account.firm,
        account.client,
        rule.id,
        user,
        &predicate,
        &action,
    )
    .await;
    assert!(
        matches!(refused, Err(rules::RuleError::VersionCap)),
        "the cap+1'th proposal answered {refused:?} — the version history must be bounded"
    );
    let listed = rules::list(&db.pool, account.firm, account.client)
        .await
        .unwrap();
    assert!(
        listed[0].versions.len()
            <= usize::try_from(rules::MAX_VERSIONS_PER_RULE).unwrap_or(usize::MAX)
    );
    db.cleanup().await;
}

/// S2-7: two creates racing the client's last rule slot land exactly once —
/// the limit check and the insert are one serialization, not a read-then-write.
#[tokio::test]
async fn concurrent_creates_cannot_pass_the_client_limit() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    for i in 0..(rules::MAX_RULES_PER_CLIENT - 1) {
        rules::create(
            &db.pool,
            account.firm,
            account.client,
            user,
            &format!("rule {i}"),
            &predicate,
            &action,
        )
        .await
        .unwrap();
    }
    let (a, b) = tokio::join!(
        rules::create(
            &db.pool,
            account.firm,
            account.client,
            user,
            "racer a",
            &predicate,
            &action,
        ),
        rules::create(
            &db.pool,
            account.firm,
            account.client,
            user,
            "racer b",
            &predicate,
            &action,
        )
    );
    assert!(
        a.is_ok() ^ b.is_ok(),
        "two racing creates answered {a:?} and {b:?} — exactly one may land",
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.rules WHERE firm_id = $1 AND client_id = $2",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, rules::MAX_RULES_PER_CLIENT);
    db.cleanup().await;
}

/// The deterministic half of the limit-race fix: while the client's rule-create
/// advisory lock is held, a create refuses by name — the count check and the
/// insert share one serialization (S2-7). `7264` is `RULES_LOCK`.
#[tokio::test]
async fn a_create_refuses_while_the_clients_rule_lock_is_held() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtextextended($1::text, 7264))",
    )
    .bind(account.client.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    let (predicate, action) = fee_rule();
    let refused = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "blocked",
        &predicate,
        &action,
    )
    .await;
    assert!(
        matches!(refused, Err(rules::RuleError::Busy)),
        "a create under the held lock answered {refused:?} — it must refuse busy"
    );
    tx.rollback().await.unwrap();
    db.cleanup().await;
}

/// S2-6 (D-076): the period lock covers exception writes, not only line writes.
/// A close racing an in-flight acknowledgement waits on the period's key-share
/// lock, then sees the acknowledged truth — the pre-guard close would read the
/// stale count and refuse the month it should have closed (or commit a digest
/// the landed decision was absent from).
#[tokio::test]
async fn a_close_waits_on_an_in_flight_acknowledgement() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    import(&db.pool, &account, &rows(0, 1))
        .await
        .unwrap()
        .complete();
    let line = LineId::from_uuid(lines_in(&db.pool, &account, "unreviewed").await[0]);
    let job = start_job(&db, &account).await;
    matching::commit_chunk(
        &db.pool,
        &job,
        &[DecidedLine {
            seq: 1,
            line,
            outcome: Decided::Exception {
                kind: ExceptionKind::NoCandidate,
                candidates: vec![],
            },
        }],
        LEASE,
        TX_TIMEOUT,
    )
    .await
    .unwrap();

    // Connection A: the acknowledgement's own write, uncommitted — the trigger's
    // FOR KEY SHARE on the line's period is what the close must wait on.
    let mut a = db.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE recon.line_exceptions
            SET acknowledged_by = $3, acknowledged_at = now()
          WHERE firm_id = $1 AND line_id = $2 AND acknowledged_at IS NULL",
    )
    .bind(account.firm.as_uuid())
    .bind(line.as_uuid())
    .bind(user.as_uuid())
    .execute(&mut *a)
    .await
    .unwrap();

    // Connection B: the close — it must block on the in-flight stamp, not read
    // the pre-ack count and refuse (or commit past it).
    let close = tokio::spawn({
        let pool = db.pool.clone();
        async move {
            periods::close(
                &pool,
                account.firm,
                account.client,
                account.id,
                march(),
                user,
            )
            .await
        }
    });
    // Wait on the definite signal: B is blocked on the period row's lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND datname = current_database()",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the close finished ({:?}) or never blocked — it did not wait on the \
             in-flight acknowledgement's period lock",
            close.await.unwrap().err()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    a.commit().await.unwrap();
    close.await.unwrap().unwrap();
    db.cleanup().await;
}

/// N1: the flip's automatic exclusions run under the same evaluation bound as
/// re-evaluation — a file whose lines × rules exceed it is refused by name,
/// never run unbounded on the flip's worker.
#[tokio::test]
async fn an_import_over_the_evaluation_budget_is_refused_by_name() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    let (predicate, action) = fee_rule();
    for i in 0..20 {
        let rule = rules::create(
            &db.pool,
            account.firm,
            account.client,
            user,
            &format!("rule {i}"),
            &predicate,
            &action,
        )
        .await
        .unwrap();
        rules::approve(
            &db.pool,
            account.firm,
            account.client,
            rule.id,
            1,
            user,
            true,
        )
        .await
        .unwrap();
    }
    // 2,000 lines of ~4 KiB narrations ≈ 8 MiB searched × 20 rules > 128 MiB.
    let narration = "x".repeat(4 * 1024);
    let mut csv = String::new();
    for i in 0..2_000 {
        use std::fmt::Write as _;
        let _ = writeln!(
            csv,
            "2026-03-01,,-1.00,USD,{narration},REF-20260301-{i},Acme Ltd,"
        );
    }
    match import(&db.pool, &account, &csv).await {
        Err(recon_db::DbError::EvaluationTooLarge) => {}
        Err(err) => panic!("the over-budget import was refused as {err:?}"),
        Ok(outcome) => panic!("the over-budget import was accepted: {outcome:?}"),
    }
}

#[tokio::test]
async fn a_re_evaluation_does_not_stall_writes_to_the_month() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    // Non-literal regex rules make the evaluation phase seconds long while
    // staying under the searched-bytes budget: ~1 MiB of narrations × 4 rules.
    for i in 0..4 {
        let rule = rules::create(
            &db.pool,
            account.firm,
            account.client,
            user,
            &format!("rule {i}"),
            &RulePredicate {
                narration: Some(TextMatch {
                    op: TextOp::Regex,
                    value: format!("nomatch{i}|[a-z]+q?zz{i}"),
                }),
                ..RulePredicate::default()
            },
            &RuleAction::Exclude {
                reason: "probe".to_string(),
            },
        )
        .await
        .unwrap();
        rules::approve(
            &db.pool,
            account.firm,
            account.client,
            rule.id,
            1,
            user,
            true,
        )
        .await
        .unwrap();
    }
    let narration = "x".repeat(4 * 1024);
    let mut csv = String::new();
    for i in 0..250 {
        use std::fmt::Write as _;
        let _ = writeln!(
            csv,
            "2026-03-01,,-1.00,USD,{narration},REF-20260301-{i},Acme Ltd,"
        );
    }
    import(&db.pool, &account, &csv).await.unwrap().complete();

    // The evaluation's own cost, measured uncontended.
    let start = std::time::Instant::now();
    periods::re_evaluate(
        &db.pool,
        account.firm,
        account.client,
        account.id,
        march(),
        user,
    )
    .await
    .unwrap();
    let evaluation = start.elapsed();
    assert!(
        evaluation > Duration::from_millis(400),
        "the evaluation must be observable: {evaluation:?}"
    );

    // An ordinary writer holds the period row for 1.2 s while a second
    // re-evaluation runs. If the row lock spans the evaluation (the old shape)
    // the call serializes hold + eval ≈ E + 1.2 s; if the lock is taken only to
    // write the decision, the eval overlaps the hold ≈ E.
    let mut hold = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT id FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4
          FOR UPDATE",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(account.id.as_uuid())
    .bind(march())
    .fetch_one(&mut *hold)
    .await
    .unwrap();
    let start = std::time::Instant::now();
    let running = tokio::spawn({
        let pool = db.pool.clone();
        async move {
            periods::re_evaluate(
                &pool,
                account.firm,
                account.client,
                account.id,
                march(),
                user,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    hold.commit().await.unwrap();
    running.await.unwrap().unwrap();
    let elapsed = start.elapsed();
    // New shape: the eval overlaps the hold → elapsed ≈ max(E, hold). Old
    // shape: the lock spans the eval → elapsed ≈ E + hold. The bound
    // E + hold/2 separates them for any E > hold/2 and tolerates the jitter
    // a loaded test host adds to both runs.
    assert!(
        elapsed < evaluation + Duration::from_millis(600),
        "the re-evaluation stalled behind a writer for the whole evaluation: \
         {elapsed:?} vs solo {evaluation:?}"
    );
}

#[tokio::test]
async fn a_period_write_waits_while_the_months_lock_is_held() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import(&db.pool, &account, &dated_rows(2026, 3, &[1], "FEE"))
        .await
        .unwrap()
        .complete();
    let line = lines_in(&db.pool, &account, "unreviewed").await[0];

    // Baseline: resolving the line's period on the unlocked row is
    // milliseconds. Without the FOR KEY SHARE the held call below takes the
    // same time — it would check the period status without waiting on it.
    let start = std::time::Instant::now();
    sqlx::query("SELECT recon.periods_lock_lines($1, $2::uuid[])")
        .bind(account.firm.as_uuid())
        .bind(vec![line])
        .execute(&db.pool)
        .await
        .unwrap();
    let baseline = start.elapsed();
    assert!(baseline < Duration::from_millis(500), "{baseline:?}");

    let mut hold = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT id FROM recon.periods
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4
          FOR UPDATE",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(account.id.as_uuid())
    .bind(march())
    .fetch_one(&mut *hold)
    .await
    .unwrap();
    let start = std::time::Instant::now();
    let write = tokio::spawn({
        let pool = db.pool.clone();
        async move {
            sqlx::query("SELECT recon.periods_lock_lines($1, $2::uuid[])")
                .bind(account.firm.as_uuid())
                .bind(vec![line])
                .execute(&pool)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(700)).await;
    hold.commit().await.unwrap();
    write.await.unwrap().unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(700),
        "the period write did not wait on the month's lock ({elapsed:?} vs baseline {baseline:?})"
    );
}

/// One guarded write: refused while the month is closed, accepted once its own
/// trigger is dropped — the drop is the proof the guard is the refusing layer.
macro_rules! guard {
    ($admin:expr, $write:expr, $trigger:literal, $table:literal) => {{
        let err = $write.execute($admin).await.unwrap_err();
        assert!(
            format!("{err:?}").to_lowercase().contains("closed"),
            "the write was not refused by the closed-period guard: {err:?}"
        );
        sqlx::query(concat!("DROP TRIGGER ", $trigger, " ON ", $table))
            .execute($admin)
            .await
            .unwrap();
    }};
}

#[tokio::test]
async fn the_closed_period_guards_refuse_writes() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    import(&db.pool, &account, &dated_rows(2026, 3, &[1, 2], "FEE"))
        .await
        .unwrap()
        .complete();
    let lines = lines_in(&db.pool, &account, "unreviewed").await;
    let match_line = lines[0];
    let alloc_line = lines[1];

    // Rows the guards resolve through, built while the month is open.
    let admin = db.admin_pool().await;
    let match_id = insert_match(&db.pool, account.firm, account.client).await;
    sqlx::query(
        "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
         VALUES ($1, $2, $3, $4, -100)",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(match_id)
    .bind(match_line)
    .execute(&db.pool)
    .await
    .unwrap();
    import_documents(
        &db.pool,
        &account,
        "receipt,R-1,2026-03-05,,1.00,USD,Acme Ltd,\n",
    )
    .await
    .unwrap()
    .complete();
    let doc: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.documents WHERE firm_id = $1 AND client_id = $2 LIMIT 1",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let match2 = insert_match(&db.pool, account.firm, account.client).await;

    // The writes must stay domain-valid to reach the period guard (an allocation
    // needs an unreviewed line), so the month is marked closed directly — a real
    // close would refuse on the still-unreviewed line.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE recon.periods
            SET status = 'closed', closed_by = $5, closed_at = now(),
                row_version = row_version + 1, chain_seq = 0,
                digest = decode(repeat('00', 32), 'hex'),
                chain_head = decode(repeat('00', 32), 'hex')
          WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(account.id.as_uuid())
    .bind(march())
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    let period_id: Uuid =
        sqlx::query_scalar("SELECT id FROM recon.periods WHERE firm_id = $1 AND client_id = $2 AND account_id = $3 AND month = $4")
            .bind(account.firm.as_uuid())
            .bind(account.client.as_uuid())
            .bind(account.id.as_uuid())
            .bind(march())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    recon_db::decisions::insert(
        &mut tx,
        account.firm,
        &recon_db::decisions::NewDecision {
            client: account.client,
            subject_kind: SubjectKind::Period,
            subject_id: period_id,
            from_state: Some("open"),
            to_state: "closed",
            actor_kind: ActorKind::Person,
            actor_ref: "test",
            reason_code: "test",
            evidence: b"{}",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // decision_log insert: a line-subject decision is a write on the period too.
    {
        let mut tx = db.pool.begin().await.unwrap();
        let err = recon_db::decisions::insert(
            &mut tx,
            account.firm,
            &recon_db::decisions::NewDecision {
                client: account.client,
                subject_kind: SubjectKind::Line,
                subject_id: match_line,
                from_state: Some("matched"),
                to_state: "excluded",
                actor_kind: ActorKind::Person,
                actor_ref: "test",
                reason_code: "test",
                evidence: b"{}",
            },
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:?}").to_lowercase().contains("closed"),
            "the line decision was not refused by the closed-period guard: {err:?}"
        );
        tx.rollback().await.unwrap();
        sqlx::query("DROP TRIGGER decision_log_period_guard_insert ON recon.decision_log")
            .execute(&admin)
            .await
            .unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        recon_db::decisions::insert(
            &mut tx,
            account.firm,
            &recon_db::decisions::NewDecision {
                client: account.client,
                subject_kind: SubjectKind::Line,
                subject_id: match_line,
                from_state: Some("matched"),
                to_state: "excluded",
                actor_kind: ActorKind::Person,
                actor_ref: "test",
                reason_code: "test",
                evidence: b"{}",
            },
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }

    // match_lines insert: a new allocation into the closed month.
    guard!(
        &admin,
        sqlx::query(
            "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
             VALUES ($1, $2, $3, $4, -100)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(match2)
        .bind(alloc_line),
        "match_lines_period_guard_insert",
        "recon.match_lines"
    );
    // matches update: a status change resolves through its allocations — and
    // takes its own decision (RCM01/RCM02) to stay domain-valid.
    {
        let mut tx = admin.begin().await.unwrap();
        let err = reject_match(&mut tx, account.firm, account.client, match_id)
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").to_lowercase().contains("closed"),
            "the match update was not refused by the closed-period guard: {err:?}"
        );
        tx.rollback().await.unwrap();
        sqlx::query("DROP TRIGGER matches_period_guard_update ON recon.matches")
            .execute(&admin)
            .await
            .unwrap();
        let mut tx = admin.begin().await.unwrap();
        reject_match(&mut tx, account.firm, account.client, match_id)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }
    // match_documents insert.
    guard!(
        &admin,
        sqlx::query(
            "INSERT INTO recon.match_documents (firm_id, client_id, match_id, document_id, amount_minor)
             VALUES ($1, $2, $3, $4, 100)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(match_id)
        .bind(doc),
        "match_documents_period_guard_insert",
        "recon.match_documents"
    );
    // match_residuals insert.
    guard!(
        &admin,
        sqlx::query(
            "INSERT INTO recon.match_residuals (firm_id, client_id, match_id, code, amount_minor)
             VALUES ($1, $2, $3, 'rounding', 1)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(match_id),
        "match_residuals_period_guard_insert",
        "recon.match_residuals"
    );
    // proposals insert.
    guard!(
        &admin,
        sqlx::query(
            "INSERT INTO recon.proposals
                 (firm_id, client_id, line_id, match_id, actor, engine_version, candidates)
             VALUES ($1, $2, $3, $4, 'engine', 'test', $5)",
        )
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .bind(alloc_line)
        .bind(match2)
        .bind(vec![doc]),
        "proposals_period_guard_insert",
        "recon.proposals"
    );
    // Every write the guard refused now lands — proving the trigger was the wall.
    sqlx::query(
        "INSERT INTO recon.match_lines (firm_id, client_id, match_id, line_id, amount_minor)
         VALUES ($1, $2, $3, $4, -100)",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(match2)
    .bind(alloc_line)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recon.match_documents (firm_id, client_id, match_id, document_id, amount_minor)
         VALUES ($1, $2, $3, $4, 100)",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(match_id)
    .bind(doc)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recon.match_residuals (firm_id, client_id, match_id, code, amount_minor)
         VALUES ($1, $2, $3, 'rounding', 1)",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(match_id)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recon.proposals
             (firm_id, client_id, line_id, match_id, actor, engine_version, candidates)
         VALUES ($1, $2, $3, $4, 'engine', 'test', $5)",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .bind(alloc_line)
    .bind(match2)
    .bind(vec![doc])
    .execute(&admin)
    .await
    .unwrap();
}

/// A domain-valid status change on a match — the update plus its decision.
async fn reject_match(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    firm: FirmId,
    client: ClientId,
    id: Uuid,
) -> Result<(), recon_db::DbError> {
    sqlx::query(
        "UPDATE recon.matches
            SET status = 'rejected', row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(id)
    .execute(&mut **tx)
    .await?;
    recon_db::decisions::insert(
        tx,
        firm,
        &recon_db::decisions::NewDecision {
            client,
            subject_kind: SubjectKind::Match,
            subject_id: id,
            from_state: Some("proposed"),
            to_state: "rejected",
            actor_kind: ActorKind::Person,
            actor_ref: "test",
            reason_code: "test",
            evidence: b"{}",
        },
    )
    .await?;
    Ok(())
}

/// N5 — the decision-chain lock precedes the period lock on every path that
/// writes decisions, so a writer holding `chain_heads` and reaching for the
/// period cannot deadlock with `re_evaluate` (which used to take them in the
/// opposite order → 40P01 → 503).
#[tokio::test]
async fn the_chain_lock_orders_before_the_period_so_no_deadlock() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    import(&db.pool, &account, &dated_rows(2026, 3, &[1], "FEE"))
        .await
        .unwrap()
        .complete();
    let line = lines_in(&db.pool, &account, "unreviewed").await[0];
    let user = owner(&db.pool, account.firm, "owner").await;

    // Writer order (the chain trigger's): chain_heads FOR UPDATE, then the
    // period via periods_lock_lines' FOR KEY SHARE.
    let mut w = db.pool.begin().await.unwrap();
    sqlx::query("SELECT recon.chain_heads_lock($1, $2)")
        .bind(account.firm.as_uuid())
        .bind(account.client.as_uuid())
        .execute(&mut *w)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let eval = tokio::spawn({
        let pool = db.pool.clone();
        async move {
            periods::re_evaluate(
                &pool,
                account.firm,
                account.client,
                account.id,
                march(),
                user,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Now reach for the period the way a writer does. With the old order the
    // re_evaluate holds the period FOR UPDATE and waits on chain_heads while
    // this waits on the period — the deadlock detector kills one of them.
    let reached = sqlx::query("SELECT recon.periods_lock_lines($1, $2::uuid[])")
        .bind(account.firm.as_uuid())
        .bind(vec![line])
        .execute(&mut *w)
        .await;
    assert!(reached.is_ok(), "the writer deadlocked: {reached:?}");
    w.commit().await.unwrap();
    eval.await.unwrap().expect("re_evaluate deadlocked");
}

/// N7 — retired rules hold no capacity: a client at the active cap is refused
/// `rule_limit`, but retiring a rule frees the slot.
#[tokio::test]
async fn a_retired_rule_frees_its_cap_slot() {
    let db = TestDb::new().await;
    let account = tenant(&db.pool).await;
    let user = owner(&db.pool, account.firm, "owner").await;
    for i in 0..recon_db::rules::MAX_RULES_PER_CLIENT {
        sqlx::query("INSERT INTO recon.rules (firm_id, id, client_id, name, status) VALUES ($1, $2, $3, $4, 'active')")
            .bind(account.firm.as_uuid())
            .bind(Uuid::now_v7())
            .bind(account.client.as_uuid())
            .bind(format!("r{i}"))
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let (predicate, action) = fee_rule();
    let refused = rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "cap",
        &predicate,
        &action,
    )
    .await;
    assert!(
        matches!(refused, Err(rules::RuleError::Limit)),
        "{refused:?}"
    );
    let r0: Uuid = sqlx::query_scalar(
        "SELECT id FROM recon.rules WHERE firm_id=$1 AND client_id=$2 AND name='r0'",
    )
    .bind(account.firm.as_uuid())
    .bind(account.client.as_uuid())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    rules::retire(&db.pool, account.firm, account.client, r0, user)
        .await
        .unwrap();
    rules::create(
        &db.pool,
        account.firm,
        account.client,
        user,
        "cap",
        &predicate,
        &action,
    )
    .await
    .expect("a retired rule must free its cap slot");
}

/// N6 — the trigger hot path measured: an all-excluded import runs two
/// statement-level trigger passes per line-write statement; the flip's total
/// is compared with a no-rule import of the same size, plus a direct
/// `periods_lock_lines` call over the import's ids. Run with --ignored
/// --nocapture; numbers are recorded under perf/results/.
#[tokio::test]
#[ignore = "measurement; run by hand, recorded under perf/results"]
async fn n6_measure_the_exclusion_trigger_cost() {
    let db = TestDb::new().await;
    let days: Vec<u32> = (1..=28).collect();
    // Same file twice: once with an auto-apply exclude rule, once without.
    let mut body = String::new();
    for round in 0..40 {
        for d in &days {
            use std::fmt::Write as _;
            let _ = writeln!(
                body,
                "2026-03-{d:02},,-1.{:02},USD,BANK FEE CHARGE {d},{d}{round}X,BANK,",
                round % 100
            );
        }
    }
    let ruled = tenant(&db.pool).await;
    let user = owner(&db.pool, ruled.firm, "owner").await;
    let (p, a) = fee_rule();
    let rule = rules::create(&db.pool, ruled.firm, ruled.client, user, "fee", &p, &a)
        .await
        .unwrap();
    rules::approve(&db.pool, ruled.firm, ruled.client, rule.id, 1, user, true)
        .await
        .unwrap();
    let start = std::time::Instant::now();
    let imported = import(&db.pool, &ruled, &body).await.unwrap().complete();
    let with_rule = start.elapsed();
    let ids = lines_in(&db.pool, &ruled, "excluded").await;
    assert_eq!(ids.len(), 1120, "{imported:?}");

    let plain = tenant(&db.pool).await;
    let start = std::time::Instant::now();
    import(&db.pool, &plain, &body).await.unwrap().complete();
    let without_rule = start.elapsed();

    let start = std::time::Instant::now();
    sqlx::query("SELECT recon.periods_lock_lines($1, $2::uuid[])")
        .bind(ruled.firm.as_uuid())
        .bind(ids.clone())
        .execute(&db.pool)
        .await
        .unwrap();
    let lock_scan = start.elapsed();
    eprintln!(
        "N6 measurement: rows=1120 flip_with_auto_exclude={with_rule:?} \
         flip_no_rule={without_rule:?} periods_lock_lines_1120_ids={lock_scan:?}"
    );
}
