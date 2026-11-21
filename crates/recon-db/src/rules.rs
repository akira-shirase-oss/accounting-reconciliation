//! Versioned rules (D-015): a correction proposes a rule, an owner or reviewer
//! approves it into an immutable version, and only an owner marks an exclusion for
//! automatic apply. Every proposal and approval is a decision on the rule; a version's
//! content never changes (the schema refuses, not just the API). An auto-apply
//! exclusion commits at the import flip; everything else re-evaluation proposes.

use recon_domain::decision::SubjectKind;
use recon_domain::line::ActorKind;
use recon_domain::rule::{self, CompiledPredicate, RuleAction, RulePredicate};
use recon_domain::{ClientId, FirmId, UserId};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::DbError;
use crate::decisions::{self, NewDecision};

/// The most rules one client may hold (bounded everything).
pub const MAX_RULES_PER_CLIENT: i64 = 200;
/// The most versions one rule may hold — the history is bounded (S2-7).
pub const MAX_VERSIONS_PER_RULE: i32 = 100;
/// The advisory-lock space serializing one client's rule creates, so the
/// limit check and the insert share one serialization (S2-7).
const RULES_LOCK: i64 = 7264;
/// The most rules one evaluation pass compiles.
const MAX_EVALUATED_RULES: i64 = 500;

/// A rule (its current state; its versions are the history).
#[derive(Debug)]
pub struct Rule {
    /// The rule's id (decisions about it name this).
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// The version in force, when one is approved.
    pub active_version: Option<i32>,
    /// `active` or `retired`.
    pub status: String,
    /// Its versions, numbered.
    pub versions: Vec<RuleVersionRow>,
}

/// One stored version.
#[derive(Debug)]
pub struct RuleVersionRow {
    /// The version number.
    pub version: i32,
    /// The typed predicate.
    pub predicate: RulePredicate,
    /// The typed action.
    pub action: RuleAction,
    /// The owner's automatic-exclusion mark.
    pub auto_apply: bool,
    /// `proposed`, `approved` or `withdrawn`.
    pub status: String,
    /// Who proposed it.
    pub proposed_by: Uuid,
    /// Who approved it, when approved.
    pub approved_by: Option<Uuid>,
}

/// A refused rule operation.
#[derive(Debug)]
pub enum RuleError {
    /// The rule does not exist in this tenant.
    NotFound,
    /// The predicate or action failed validation; the reason names it.
    Invalid(&'static str),
    /// The version is already decided (or the client holds the limit).
    Conflict(&'static str),
    /// The rule is retired.
    Retired,
    /// Storage.
    Db(DbError),
}

impl From<sqlx::Error> for RuleError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(DbError::from(e))
    }
}

impl From<DbError> for RuleError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

/// The caller's predicate/action normalized to canonical stored form.
fn normalized(predicate: &RulePredicate, action: &RuleAction) -> (RulePredicate, RuleAction) {
    let mut p = predicate.clone();
    let mut a = action.clone();
    rule::normalize_rule(&mut p, &mut a);
    (p, a)
}

fn invalid(e: rule::RuleError) -> RuleError {
    RuleError::Invalid(match e {
        rule::RuleError::NoCondition => "a rule needs at least one condition",
        rule::RuleError::ValueInvalid => "a rule value is empty or too long",
        rule::RuleError::AmountRangeInverted => "the amount range is inverted",
        rule::RuleError::RegexInvalid => "the regex does not compile",
        rule::RuleError::RegexTooBig => "the regex compiles past the size limit",
        rule::RuleError::ReasonInvalid => "the exclusion's reason is not a reason code",
        rule::RuleError::DocumentKindInvalid => "the document kind is unknown",
    })
}

/// The action's stored pair (`action`, `action_arg`).
fn action_columns(action: &RuleAction) -> (&'static str, &str) {
    match action {
        RuleAction::AssignCounterparty { counterparty } => ("assign_counterparty", counterparty),
        RuleAction::Exclude { reason } => ("exclude", reason),
        RuleAction::PreferDocumentKind { document_kind } => ("prefer_document_kind", document_kind),
    }
}

/// The stored pair back into the typed action.
fn action_from(kind: &str, arg: &str) -> RuleAction {
    match kind {
        "exclude" => RuleAction::Exclude {
            reason: arg.to_owned(),
        },
        "prefer_document_kind" => RuleAction::PreferDocumentKind {
            document_kind: arg.to_owned(),
        },
        _ => RuleAction::AssignCounterparty {
            counterparty: arg.to_owned(),
        },
    }
}

fn evidence(v: &impl Serialize) -> Result<Vec<u8>, DbError> {
    serde_json::to_vec(v).map_err(|e| DbError::Inconsistent(format!("evidence: {e}")))
}

#[expect(
    clippy::too_many_arguments,
    reason = "a decision row's own columns; bundling them hides the audit's shape"
)]
async fn decide(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    rule: Uuid,
    from: Option<&str>,
    to: &str,
    user: UserId,
    reason: &str,
    evidence: &[u8],
) -> Result<(), DbError> {
    decisions::insert(
        conn,
        firm,
        &NewDecision {
            client,
            subject_kind: SubjectKind::Rule,
            subject_id: rule,
            from_state: from,
            to_state: to,
            actor_kind: ActorKind::Person,
            actor_ref: &user.as_uuid().to_string(),
            reason_code: reason,
            evidence,
        },
    )
    .await?;
    Ok(())
}

/// Creates a rule with version 1 `proposed` and its decision. The stored predicate
/// and action are normalized to the form the lines carry (canonical typed data).
pub async fn create(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    user: UserId,
    name: &str,
    predicate: &RulePredicate,
    action: &RuleAction,
) -> Result<Rule, RuleError> {
    let (predicate, action) = normalized(predicate, action);
    CompiledPredicate::compile(&predicate, &action).map_err(invalid)?;
    if name.is_empty() || name.len() > 200 {
        return Err(RuleError::Invalid("a rule name is 1-200 bytes"));
    }
    let mut tx = pool.begin().await?;
    if !crate::imports::try_lock(&mut tx, client.as_uuid(), RULES_LOCK).await? {
        return Err(RuleError::Conflict("the client's rule set is busy"));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.rules WHERE firm_id = $1 AND client_id = $2",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_RULES_PER_CLIENT {
        return Err(RuleError::Conflict("the client's rule limit is reached"));
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO recon.rules (firm_id, id, client_id, name) VALUES ($1, $2, $3, $4)")
        .bind(firm.as_uuid())
        .bind(id)
        .bind(client.as_uuid())
        .bind(name)
        .execute(&mut *tx)
        .await?;
    insert_version(&mut tx, firm, client, id, 1, user, &predicate, &action).await?;
    let body = evidence(&serde_json::json!({
        "name": name, "version": 1, "predicate": predicate, "action": action,
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        id,
        None,
        "proposed",
        user,
        "rule_proposed",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(Rule {
        id,
        name: name.to_owned(),
        active_version: None,
        status: "active".to_owned(),
        versions: vec![RuleVersionRow {
            version: 1,
            predicate,
            action,
            auto_apply: false,
            status: "proposed".to_owned(),
            proposed_by: user.as_uuid(),
            approved_by: None,
        }],
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "a version row's own columns; bundling them hides the rule's shape"
)]
async fn insert_version(
    tx: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    rule: Uuid,
    version: i32,
    user: UserId,
    predicate: &RulePredicate,
    action: &RuleAction,
) -> Result<(), RuleError> {
    let (kind, arg) = action_columns(action);
    sqlx::query(
        "INSERT INTO recon.rule_versions
             (firm_id, rule_id, version, client_id, predicate, action, action_arg, proposed_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(firm.as_uuid())
    .bind(rule)
    .bind(version)
    .bind(client.as_uuid())
    .bind(serde_json::to_value(predicate).map_err(|e| DbError::Inconsistent(e.to_string()))?)
    .bind(kind)
    .bind(arg)
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Proposes the rule's next version (`version = max + 1`) and its decision; a retired
/// rule takes no new versions.
pub async fn propose_version(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    rule: Uuid,
    user: UserId,
    predicate: &RulePredicate,
    action: &RuleAction,
) -> Result<i32, RuleError> {
    let (predicate, action) = normalized(predicate, action);
    CompiledPredicate::compile(&predicate, &action).map_err(invalid)?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, (SELECT max(version) FROM recon.rule_versions v
          WHERE v.firm_id = r.firm_id AND v.rule_id = r.id) AS max_version
           FROM recon.rules r WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(rule)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RuleError::NotFound)?;
    if row.try_get::<String, _>("status")? == "retired" {
        return Err(RuleError::Retired);
    }
    let version = row
        .try_get::<Option<i32>, _>("max_version")?
        .unwrap_or(0)
        .saturating_add(1);
    if version > MAX_VERSIONS_PER_RULE {
        return Err(RuleError::Conflict("the rule's version history is bounded"));
    }
    insert_version(
        &mut tx, firm, client, rule, version, user, &predicate, &action,
    )
    .await?;
    let body = evidence(&serde_json::json!({
        "version": version, "predicate": predicate, "action": action,
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        rule,
        None,
        "proposed",
        user,
        "rule_proposed",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(version)
}

/// Approves a proposed version: it becomes the rule's active version, the approving
/// owner's `auto_apply` mark (exclusions only) is recorded, and the decision names
/// who approved, the version and what it supersedes.
pub async fn approve(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    rule: Uuid,
    version: i32,
    user: UserId,
    auto_apply: bool,
) -> Result<(), RuleError> {
    let mut tx = pool.begin().await?;
    let rule_row = sqlx::query(
        "SELECT status, active_version FROM recon.rules
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(rule)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RuleError::NotFound)?;
    if rule_row.try_get::<String, _>("status")? == "retired" {
        return Err(RuleError::Retired);
    }
    let supersedes: Option<i32> = rule_row.try_get("active_version")?;
    let version_row = sqlx::query(
        "SELECT status, action FROM recon.rule_versions
          WHERE firm_id = $1 AND rule_id = $2 AND version = $3 FOR UPDATE",
    )
    .bind(firm.as_uuid())
    .bind(rule)
    .bind(version)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RuleError::NotFound)?;
    if version_row.try_get::<String, _>("status")? != "proposed" {
        return Err(RuleError::Conflict("the version was already decided"));
    }
    if auto_apply && version_row.try_get::<String, _>("action")? != "exclude" {
        return Err(RuleError::Invalid(
            "only an exclusion carries the auto-apply mark",
        ));
    }
    sqlx::query(
        "UPDATE recon.rule_versions
            SET status = 'approved', approved_by = $4, approved_at = now(), auto_apply = $5
          WHERE firm_id = $1 AND rule_id = $2 AND version = $3 AND status = 'proposed'",
    )
    .bind(firm.as_uuid())
    .bind(rule)
    .bind(version)
    .bind(user.as_uuid())
    .bind(auto_apply)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE recon.rules SET active_version = $3, row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(rule)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    let body = evidence(&serde_json::json!({
        "version": version, "auto_apply": auto_apply, "supersedes": supersedes,
    }))?;
    decide(
        &mut tx,
        firm,
        client,
        rule,
        Some("proposed"),
        "approved",
        user,
        "rule_approved",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Retires the rule: it stops applying everywhere and takes no new versions.
pub async fn retire(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    rule: Uuid,
    user: UserId,
) -> Result<(), RuleError> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE recon.rules SET status = 'retired', row_version = row_version + 1
          WHERE firm_id = $1 AND client_id = $2 AND id = $3 AND status = 'active'",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(rule)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM recon.rules
             WHERE firm_id = $1 AND client_id = $2 AND id = $3)",
        )
        .bind(firm.as_uuid())
        .bind(client.as_uuid())
        .bind(rule)
        .fetch_one(&mut *tx)
        .await?;
        return Err(if exists {
            RuleError::Conflict("the rule is already retired")
        } else {
            RuleError::NotFound
        });
    }
    let body = evidence(&serde_json::json!({}))?;
    decide(
        &mut tx,
        firm,
        client,
        rule,
        Some("active"),
        "retired",
        user,
        "rule_retired",
        &body,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The client's rules with their versions (current state — history is the log).
pub async fn list(pool: &PgPool, firm: FirmId, client: ClientId) -> Result<Vec<Rule>, DbError> {
    let rule_rows = sqlx::query(
        "SELECT id, name, active_version, status FROM recon.rules
          WHERE firm_id = $1 AND client_id = $2 ORDER BY id",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .fetch_all(pool)
    .await?;
    let mut rules = Vec::with_capacity(rule_rows.len());
    for r in &rule_rows {
        let mut versions = sqlx::query(
            "SELECT version, predicate, action, action_arg, auto_apply, status,
                    proposed_by, approved_by
               FROM recon.rule_versions
              WHERE firm_id = $1 AND rule_id = $2 ORDER BY version DESC LIMIT $3",
        )
        .bind(firm.as_uuid())
        .bind(r.try_get::<Uuid, _>("id")?)
        .bind(i64::from(MAX_VERSIONS_PER_RULE))
        .fetch_all(pool)
        .await?;
        versions.reverse();
        rules.push(Rule {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            active_version: r.try_get("active_version")?,
            status: r.try_get("status")?,
            versions: versions
                .iter()
                .map(|v| {
                    Ok(RuleVersionRow {
                        version: v.try_get("version")?,
                        predicate: serde_json::from_value(v.try_get("predicate")?)
                            .map_err(|e| DbError::Inconsistent(format!("predicate json: {e}")))?,
                        action: action_from(v.try_get("action")?, v.try_get("action_arg")?),
                        auto_apply: v.try_get("auto_apply")?,
                        status: v.try_get("status")?,
                        proposed_by: v.try_get("proposed_by")?,
                        approved_by: v.try_get("approved_by")?,
                    })
                })
                .collect::<Result<_, DbError>>()?,
        });
    }
    Ok(rules)
}

/// The client's in-force rule versions (the rule `active` and the version `approved`).
/// `auto_only` limits it to the owner's auto-apply exclusions (the flip's set).
pub(crate) async fn active_rules(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    auto_only: bool,
) -> Result<Vec<(Uuid, i32, RulePredicate, RuleAction)>, DbError> {
    let rows = sqlx::query(
        "SELECT v.rule_id, v.version, v.predicate, v.action, v.action_arg
           FROM recon.rule_versions v
           JOIN recon.rules r ON r.firm_id = v.firm_id AND r.id = v.rule_id
          WHERE v.firm_id = $1 AND r.client_id = $2 AND r.status = 'active'
            AND v.status = 'approved' AND v.version = r.active_version
            AND (v.auto_apply OR NOT $3)
          ORDER BY v.rule_id
          LIMIT $4",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(auto_only)
    .bind(MAX_EVALUATED_RULES)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("rule_id")?,
                r.try_get("version")?,
                serde_json::from_value(r.try_get("predicate")?)
                    .map_err(|e| DbError::Inconsistent(format!("predicate json: {e}")))?,
                action_from(r.try_get("action")?, r.try_get("action_arg")?),
            ))
        })
        .collect()
}

/// The flip's automatic exclusions (D-015): every line this import published is
/// evaluated against the client's auto-apply rules; a matched line lands `excluded`
/// with a rule decision naming the version that made it. Runs inside the flip's
/// transaction so the lines and their decisions commit together.
pub async fn apply_exclusions(
    conn: &mut PgConnection,
    firm: FirmId,
    client: ClientId,
    import: Uuid,
) -> Result<u32, DbError> {
    let rules = active_rules(conn, firm, client, true).await?;
    if rules.is_empty() {
        return Ok(0);
    }
    let mut compiled = Vec::with_capacity(rules.len());
    for (rule_id, version, predicate, action) in &rules {
        // A stored predicate was validated at write; a mismatch is a defect.
        compiled.push((
            *rule_id,
            *version,
            CompiledPredicate::compile(predicate, action)
                .map_err(|e| DbError::Inconsistent(format!("stored rule: {e}")))?,
            action.clone(),
        ));
    }
    let lines = sqlx::query(
        "SELECT id, narration_norm, counterparty_norm, amount_minor, account_id
           FROM recon.statement_lines
          WHERE firm_id = $1 AND import_id = $2 AND status = 'unreviewed'
          ORDER BY row_no",
    )
    .bind(firm.as_uuid())
    .bind(import)
    .fetch_all(&mut *conn)
    .await?;
    // The work bound (N1): the same lines × rules × bytes budget re-evaluation
    // carries, checked before the search runs on a blocking worker — the flip's
    // transaction holds the account lock, so an unbounded search would stall
    // the account and a Tokio worker alike.
    let mut owned: Vec<(Uuid, String, Option<String>, i64, Uuid)> = Vec::with_capacity(lines.len());
    let mut searched_bytes: u64 = 0;
    for line in &lines {
        let narration: String = line.try_get("narration_norm")?;
        searched_bytes = searched_bytes.saturating_add(narration.len() as u64);
        owned.push((
            line.try_get("id")?,
            narration,
            line.try_get("counterparty_norm")?,
            line.try_get("amount_minor")?,
            line.try_get("account_id")?,
        ));
    }
    if rule::evaluation_too_big(searched_bytes, compiled.len()) {
        return Err(DbError::EvaluationTooLarge);
    }
    let matched = tokio::task::spawn_blocking(move || {
        let mut compiled = compiled;
        let mut matched: Vec<(Uuid, Uuid, i32, String)> = Vec::new();
        for (id, narration, counterparty, amount_minor, account) in &owned {
            let facts = rule::LineFacts {
                narration,
                counterparty: counterparty.as_deref(),
                amount_minor: *amount_minor,
                account: *account,
            };
            for entry in &mut compiled {
                if entry.2.matches(&facts) {
                    if let RuleAction::Exclude { reason } = &entry.3 {
                        matched.push((*id, entry.0, entry.1, reason.clone()));
                    }
                    break; // first matching rule wins
                }
            }
        }
        matched
    })
    .await
    .map_err(|e| DbError::Inconsistent(format!("exclusion worker: {e}")))?;
    if matched.is_empty() {
        return Ok(0);
    }
    // One bulk state move and one bulk decision insert (N1): the same
    // commit-time state as per-line writes, two round trips, in line order.
    let line_ids: Vec<Uuid> = matched.iter().map(|m| m.0).collect();
    sqlx::query(
        "UPDATE recon.statement_lines SET status = 'excluded', row_version = row_version + 1
          WHERE firm_id = $1 AND id = ANY($2) AND status = 'unreviewed'",
    )
    .bind(firm.as_uuid())
    .bind(&line_ids)
    .execute(&mut *conn)
    .await?;
    let mut ids = Vec::with_capacity(matched.len());
    let mut subjects = Vec::with_capacity(matched.len());
    let mut refs = Vec::with_capacity(matched.len());
    let mut reasons = Vec::with_capacity(matched.len());
    let mut evidences = Vec::with_capacity(matched.len());
    for (line_id, rule_id, version, reason) in &matched {
        ids.push(Uuid::now_v7());
        subjects.push(*line_id);
        refs.push(format!("rule:{rule_id}@v{version}"));
        reasons.push(reason.clone());
        evidences.push(evidence(&serde_json::json!({
            "rule": rule_id, "version": version,
        }))?);
    }
    sqlx::query(
        "INSERT INTO recon.decision_log
             (firm_id, client_id, id, subject_kind, subject_id, from_state, to_state,
              actor_kind, actor_ref, reason_code, evidence, engine_version)
         SELECT $1, $2, d.id, 'line', d.subject_id, 'unreviewed', 'excluded',
                'rule', d.actor_ref, d.reason_code, d.evidence, $3
           FROM unnest($4::uuid[], $5::uuid[], $6::text[], $7::text[], $8::bytea[])
                AS d(id, subject_id, actor_ref, reason_code, evidence)",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(decisions::engine_version())
    .bind(&ids)
    .bind(&subjects)
    .bind(&refs)
    .bind(&reasons)
    .bind(&evidences)
    .execute(&mut *conn)
    .await?;
    Ok(u32::try_from(matched.len()).unwrap_or(u32::MAX))
}
