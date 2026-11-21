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
use recon_domain::line::{ExceptionKind, ExcludeReason};
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, ScoreBreakdown, Tier,
};
use recon_domain::rule::{RuleAction, RulePredicate, TextMatch, TextOp};
use recon_domain::{Currency, DocumentId, FirmId, LineId, Money, UserId};
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
            imports::flip(&pool, staging, &summary, common::OPERATOR, &*gate, common::FLIP_TIMEOUT)
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
