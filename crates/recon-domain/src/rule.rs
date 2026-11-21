//! The typed rule language (D-015): a rule version is typed data — predicates over
//! normalized fields and one action — never executable text. `equals`, `contains`,
//! `starts_with` and `token_equals` read a normalized field; a `regex` compiles only to
//! a Thompson NFA running on the linear-time Pike VM, bounded to 10 KiB of compiled
//! program — the database stores the typed data, this module compiles and evaluates it.

use regex_automata::nfa::thompson::pikevm::{Cache, PikeVM};
use regex_automata::nfa::thompson::{self};
use regex_automata::util::syntax;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::document::DocumentKind;
use crate::text::normalize;

/// The most bytes any single rule value (a predicate or an action argument) may carry.
pub const MAX_VALUE_BYTES: usize = 200;
/// The most compiled program a regex predicate may build to (D-015).
pub const REGEX_COMPILED_LIMIT: usize = 10 * 1024;
/// The most nested groups/classes a regex predicate may use — bounds parse depth.
pub const REGEX_NEST_LIMIT: u32 = 32;
/// The most narration bytes × predicates one re-evaluation may scan — the
/// PikeVM cost is linear in searched bytes, so the bound counts the real work
/// (S1-2).
pub const MAX_EVALUATION_WORK_BYTES: u64 = 128 * 1024 * 1024;

/// `true` when scanning `line_bytes` of narrations under `predicates` rules
/// exceeds the re-evaluation bound — the caller refuses by name.
pub fn evaluation_too_big(line_bytes: u64, predicates: usize) -> bool {
    line_bytes.saturating_mul(predicates as u64) > MAX_EVALUATION_WORK_BYTES
}

/// What a text clause does to a normalized field (D-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextOp {
    /// The whole field equals the value.
    Equals,
    /// The value appears anywhere in the field.
    Contains,
    /// The field starts with the value.
    StartsWith,
    /// A whitespace-separated token equals the value.
    TokenEquals,
    /// A bounded linear-time regex matches anywhere in the field.
    Regex,
}

/// One text clause: an operator and its literal value (stored normalized, like the
/// field it compares against — `normalization v1`, so uppercase and space-collapsed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextMatch {
    /// How `value` is compared.
    pub op: TextOp,
    /// Compared against the normalized field, in that field's normalized form.
    pub value: String,
}

/// A rule's conditions over a normalized line (all present clauses must hold — an
/// AND). A clause naming a document kind never matches a statement line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RulePredicate {
    /// On `narration_norm`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narration: Option<TextMatch>,
    /// `counterparty_norm` equals this exactly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    /// `amount_minor` is at least this (signed: fees are negative).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_gte: Option<i64>,
    /// `amount_minor` is at most this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_lte: Option<i64>,
    /// Only lines of this account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Uuid>,
    /// Only documents of this kind — never true of a statement line (D-015 lists it
    /// for document-side evaluation when that exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_kind: Option<String>,
}

/// What a matched rule does (D-015).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleAction {
    /// Set the line's counterparty — a proposal, never an auto-commit.
    AssignCounterparty {
        /// The normalized counterparty name.
        counterparty: String,
    },
    /// Exclude the line with this reason code — the only action an owner may mark
    /// `auto_apply` on.
    Exclude {
        /// The exclusion's `reason_code` on the decision.
        reason: String,
    },
    /// Rank a document kind first when proposing — a proposal, never an auto-commit.
    PreferDocumentKind {
        /// A `DocumentKind` code.
        document_kind: String,
    },
}

/// A refused rule definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// A rule needs at least one condition.
    #[error("rule_needs_a_condition")]
    NoCondition,
    /// A value is empty or over the limit.
    #[error("rule_value_invalid")]
    ValueInvalid,
    /// `amount_gte` above `amount_lte`.
    #[error("rule_amount_range_inverted")]
    AmountRangeInverted,
    /// The regex does not compile.
    #[error("rule_regex_invalid")]
    RegexInvalid,
    /// The regex compiles past the limit.
    #[error("rule_regex_too_big")]
    RegexTooBig,
    /// The exclusion's reason is not a `reason_code`.
    #[error("rule_reason_invalid")]
    ReasonInvalid,
    /// The document kind is not a known code.
    #[error("rule_document_kind_invalid")]
    DocumentKindInvalid,
}

/// `true` when `s` is a decision `reason_code` (`^[a-z][a-z0-9_]{0,63}$`).
fn is_reason_code(s: &str) -> bool {
    let mut chars = s.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    s.len() <= 64 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn valid_value(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_VALUE_BYTES
}

/// Checks a predicate's shape without compiling (regex validity needs a compile).
pub fn check(predicate: &RulePredicate, action: &RuleAction) -> Result<(), RuleError> {
    let conditions = [
        predicate.narration.is_some(),
        predicate.counterparty.is_some(),
        predicate.amount_gte.is_some() | predicate.amount_lte.is_some(),
        predicate.account.is_some(),
        predicate.document_kind.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if conditions == 0 {
        return Err(RuleError::NoCondition);
    }
    if let Some(t) = &predicate.narration
        && !valid_value(&t.value)
    {
        return Err(RuleError::ValueInvalid);
    }
    if let Some(c) = &predicate.counterparty
        && !valid_value(c)
    {
        return Err(RuleError::ValueInvalid);
    }
    if let (Some(gte), Some(lte)) = (predicate.amount_gte, predicate.amount_lte)
        && gte > lte
    {
        return Err(RuleError::AmountRangeInverted);
    }
    if let Some(kind) = &predicate.document_kind
        && DocumentKind::from_code(kind).is_none()
    {
        return Err(RuleError::DocumentKindInvalid);
    }
    match action {
        RuleAction::AssignCounterparty { counterparty } => {
            if !valid_value(counterparty) {
                return Err(RuleError::ValueInvalid);
            }
        }
        RuleAction::Exclude { reason } => {
            if !is_reason_code(reason) {
                return Err(RuleError::ReasonInvalid);
            }
        }
        RuleAction::PreferDocumentKind {
            document_kind: kind,
        } => {
            if DocumentKind::from_code(kind).is_none() {
                return Err(RuleError::DocumentKindInvalid);
            }
        }
    }
    Ok(())
}

/// Normalizes the values a predicate and action compare against, in place — the
/// write path calls this before storing so a rule's data is canonical, and
/// [`CompiledPredicate::compile`] applies it again (idempotent) so evaluation is
/// right whatever the caller handed it.
pub fn normalize_rule(predicate: &mut RulePredicate, action: &mut RuleAction) {
    if let Some(m) = &mut predicate.narration {
        // A regex's source is its semantics: normalizing would invert `\d` into
        // `\D` (S2-3). `compile` matches it case-insensitively instead.
        if m.op != TextOp::Regex {
            m.value = normalize(&m.value);
        }
    }
    if let Some(c) = &mut predicate.counterparty {
        *c = normalize(c);
    }
    if let RuleAction::AssignCounterparty { counterparty } = action {
        *counterparty = normalize(counterparty);
    }
}

/// The normalized facts a predicate reads off a statement line.
#[derive(Debug, Clone, Copy)]
pub struct LineFacts<'a> {
    /// `narration_norm`.
    pub narration: &'a str,
    /// `counterparty_norm`, when the row carries one.
    pub counterparty: Option<&'a str>,
    /// `amount_minor`, signed.
    pub amount_minor: i64,
    /// `account_id`.
    pub account: Uuid,
}

/// A predicate checked, normalized and compiled once for a batch of lines (the
/// regex's Pike VM and its search cache live here).
pub struct CompiledPredicate {
    inner: RulePredicate,
    regex: Option<PikeVM>,
    cache: Option<Cache>,
}

impl CompiledPredicate {
    /// Normalizes the predicate's values, validates the shape and compiles the regex,
    /// refusing any of the three by name.
    pub fn compile(predicate: &RulePredicate, action: &RuleAction) -> Result<Self, RuleError> {
        let mut normalized = predicate.clone();
        let mut action = action.clone();
        normalize_rule(&mut normalized, &mut action);
        check(&normalized, &action)?;
        let (regex, cache) = match &normalized.narration {
            Some(m) if m.op == TextOp::Regex => {
                // The NFA cap must bound the build itself, not be tested after it
                // (S1-2): a nested quantifier expands past the cap inside a few
                // source bytes. The regex source is never normalized, so the VM is
                // case-insensitive — `\d` stays `\d` (S2-3).
                let vm = PikeVM::builder()
                    .syntax(
                        syntax::Config::new()
                            .case_insensitive(true)
                            .nest_limit(REGEX_NEST_LIMIT),
                    )
                    .thompson(
                        thompson::Config::new().nfa_size_limit(Some(REGEX_COMPILED_LIMIT)),
                    )
                    .build(&m.value)
                    .map_err(|e| {
                        if e.size_limit().is_some() {
                            RuleError::RegexTooBig
                        } else {
                            RuleError::RegexInvalid
                        }
                    })?;
                if vm.get_nfa().memory_usage() > REGEX_COMPILED_LIMIT {
                    return Err(RuleError::RegexTooBig);
                }
                let cache = vm.create_cache();
                (Some(vm), Some(cache))
            }
            _ => (None, None),
        };
        Ok(Self {
            inner: normalized,
            regex,
            cache,
        })
    }

    /// `true` when every clause of the predicate holds for the line.
    pub fn matches(&mut self, line: &LineFacts<'_>) -> bool {
        let p = &self.inner;
        if let Some(text) = &p.narration {
            let hit = match text.op {
                TextOp::Equals => line.narration == text.value,
                TextOp::Contains => line.narration.contains(&text.value),
                TextOp::StartsWith => line.narration.starts_with(&text.value),
                TextOp::TokenEquals => line.narration.split_whitespace().any(|t| t == text.value),
                TextOp::Regex => match (&self.regex, &mut self.cache) {
                    (Some(vm), Some(cache)) => vm.is_match(cache, line.narration),
                    _ => false,
                },
            };
            if !hit {
                return false;
            }
        }
        if let Some(want) = &p.counterparty
            && line.counterparty != Some(want.as_str())
        {
            return false;
        }
        if p.amount_gte.is_some_and(|gte| line.amount_minor < gte)
            || p.amount_lte.is_some_and(|lte| line.amount_minor > lte)
        {
            return false;
        }
        if p.account.is_some_and(|account| account != line.account) {
            return false;
        }
        // A document-kind clause never holds for a statement line.
        p.document_kind.is_none()
    }
}

/// `true` when the action may carry an owner's `auto_apply` mark (D-015: exclusions
/// only — assigning a counterparty or preferring a kind is always a proposal).
pub const fn auto_apply_allowed(action: &RuleAction) -> bool {
    matches!(action, RuleAction::Exclude { .. })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;

    fn facts<'a>(narration: &'a str, counterparty: Option<&'a str>, amount: i64) -> LineFacts<'a> {
        LineFacts {
            narration,
            counterparty,
            amount_minor: amount,
            account: Uuid::nil(),
        }
    }

    #[test]
    fn the_text_ops_read_the_normalized_narration() {
        // `narration` is `narration_norm` — already normalized; the predicate's values
        // are normalized by `compile`, whatever the caller stored.
        let line = facts("BANK FEE MONTHLY", Some("BIG BANK"), -100);
        for (op, value, want) in [
            (TextOp::Equals, "bank fee monthly", true),
            (TextOp::Equals, "bank fee", false),
            (TextOp::Contains, "fee mon", true),
            (TextOp::StartsWith, "bank fee", true),
            (TextOp::StartsWith, "fee", false),
            (TextOp::TokenEquals, "fee", true),
            (TextOp::TokenEquals, "fe", false),
            (TextOp::Regex, "^bank.fee", true),
            (TextOp::Regex, "^fee", false),
        ] {
            let p = RulePredicate {
                narration: Some(TextMatch {
                    op,
                    value: value.to_owned(),
                }),
                ..RulePredicate::default()
            };
            let action = RuleAction::Exclude {
                reason: "out_of_scope".to_owned(),
            };
            let mut compiled = CompiledPredicate::compile(&p, &action).unwrap();
            assert_eq!(compiled.matches(&line), want, "{op:?} {value:?}");
        }
    }

    #[test]
    fn every_clause_must_hold() {
        let line = facts("ACME PAYMENT", Some("ACME LTD"), -500);
        let p = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Contains,
                value: "acme".to_owned(),
            }),
            counterparty: Some("acme ltd".to_owned()),
            amount_gte: Some(-1000),
            amount_lte: Some(0),
            account: Some(Uuid::nil()),
            document_kind: None,
        };
        let action = RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        };
        let mut compiled = CompiledPredicate::compile(&p, &action).unwrap();
        assert!(compiled.matches(&line));
        let mut wrong_account = p.clone();
        wrong_account.account = Some(Uuid::from_u128(0xdead_beef));
        let mut compiled = CompiledPredicate::compile(&wrong_account, &action).unwrap();
        assert!(!compiled.matches(&line));
        let mut doc_clause = p.clone();
        doc_clause.document_kind = Some("receipt".to_owned());
        let mut compiled = CompiledPredicate::compile(&doc_clause, &action).unwrap();
        assert!(!compiled.matches(&line), "a line has no document kind");
    }

    #[test]
    fn validation_names_each_refusal() {
        let action = RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        };
        assert_eq!(
            check(&RulePredicate::default(), &action),
            Err(RuleError::NoCondition)
        );
        let bad_range = RulePredicate {
            amount_gte: Some(5),
            amount_lte: Some(0),
            ..RulePredicate::default()
        };
        assert_eq!(
            check(&bad_range, &action),
            Err(RuleError::AmountRangeInverted)
        );
        let long = RulePredicate {
            counterparty: Some("x".repeat(MAX_VALUE_BYTES + 1)),
            ..RulePredicate::default()
        };
        assert_eq!(check(&long, &action), Err(RuleError::ValueInvalid));
        let narration = |value: &str| RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Contains,
                value: value.to_owned(),
            }),
            ..RulePredicate::default()
        };
        assert_eq!(check(&narration(""), &action), Err(RuleError::ValueInvalid));
        assert_eq!(
            check(
                &narration("x"),
                &RuleAction::Exclude {
                    reason: "NotACode".to_owned()
                }
            ),
            Err(RuleError::ReasonInvalid)
        );
        assert_eq!(
            check(
                &narration("x"),
                &RuleAction::PreferDocumentKind {
                    document_kind: "invoice".to_owned()
                }
            ),
            Err(RuleError::DocumentKindInvalid)
        );
    }

    #[test]
    fn a_regex_must_compile_inside_the_limit() {
        let action = RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        };
        let invalid = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Regex,
                value: "(unclosed".to_owned(),
            }),
            ..RulePredicate::default()
        };
        assert!(matches!(
            CompiledPredicate::compile(&invalid, &action),
            Err(RuleError::RegexInvalid)
        ));
        // A pattern whose compiled program passes the cap refuses by name.
        let huge = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Regex,
                value: "[0-9a-f]{5000}".to_owned(),
            }),
            ..RulePredicate::default()
        };
        let Err(RuleError::RegexTooBig) = CompiledPredicate::compile(&huge, &action) else {
            panic!("a too-big regex must refuse by name");
        };
    }

    #[test]
    fn a_regex_nfa_is_bounded_while_it_builds() {
        // S1-2: the cap must bound the NFA as it compiles, not after — this
        // pattern expands to ~10^9 states inside 25 bytes, under the 200-byte cap.
        let action = RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        };
        let bomb = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Regex,
                value: "((a{1000}){1000}){1000}".to_owned(),
            }),
            ..RulePredicate::default()
        };
        let Err(RuleError::RegexTooBig) = CompiledPredicate::compile(&bomb, &action) else {
            panic!("a regex that expands past the NFA cap must refuse by name");
        };
    }

    #[test]
    fn a_regex_keeps_its_escapes_and_stays_case_insensitive() {
        // S2-3: uppercasing a regex inverts \d \s \w \b into their opposites.
        // The source is stored verbatim; the compiled VM is case-insensitive.
        let mut predicate = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Regex,
                value: "^atm\\s\\d+".to_owned(),
            }),
            ..RulePredicate::default()
        };
        let mut action = RuleAction::Exclude {
            reason: "out_of_scope".to_owned(),
        };
        normalize_rule(&mut predicate, &mut action);
        assert_eq!(
            predicate.narration.as_ref().unwrap().value,
            "^atm\\s\\d+",
            "normalizing must not rewrite a regex's escapes"
        );
        let mut compiled = CompiledPredicate::compile(&predicate, &action).unwrap();
        assert!(compiled.matches(&facts("ATM 1234 FEE", None, -100)));
        assert!(compiled.matches(&facts("atm 9", None, -100)));
        assert!(!compiled.matches(&facts("ATM FEE", None, -100)));
        // \b survives: a word boundary does not match inside a word.
        let boundary = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::Regex,
                value: "\\bfee\\b".to_owned(),
            }),
            ..RulePredicate::default()
        };
        let mut compiled = CompiledPredicate::compile(&boundary, &action).unwrap();
        assert!(compiled.matches(&facts("BANK FEE", None, -100)));
        assert!(!compiled.matches(&facts("FEES", None, -100)));
    }

    #[test]
    fn the_evaluation_work_bound_counts_lines_times_rules_times_bytes() {
        // S1-2: the bound is on searched bytes — a small month under a few rules
        // passes, the same month under hundreds refuses.
        assert!(!evaluation_too_big(10_000 * 200, 20));
        assert!(evaluation_too_big(10_000 * 8 * 1024, 2));
        assert!(evaluation_too_big(u64::MAX, usize::MAX));
    }

    #[test]
    fn only_exclusions_take_the_auto_apply_mark() {
        assert!(auto_apply_allowed(&RuleAction::Exclude {
            reason: "x".to_owned()
        }));
        assert!(!auto_apply_allowed(&RuleAction::AssignCounterparty {
            counterparty: "x".to_owned()
        }));
        assert!(!auto_apply_allowed(&RuleAction::PreferDocumentKind {
            document_kind: "receipt".to_owned()
        }));
    }

    #[test]
    fn predicate_and_action_round_trip_through_json() {
        let p = RulePredicate {
            narration: Some(TextMatch {
                op: TextOp::TokenEquals,
                value: "fee".to_owned(),
            }),
            amount_gte: Some(-100),
            ..RulePredicate::default()
        };
        let action = RuleAction::PreferDocumentKind {
            document_kind: "purchase_bill".to_owned(),
        };
        let p_json = serde_json::to_value(&p).unwrap();
        let a_json = serde_json::to_value(&action).unwrap();
        assert_eq!(a_json["kind"], "prefer_document_kind");
        let p2: RulePredicate = serde_json::from_value(p_json).unwrap();
        let a2: RuleAction = serde_json::from_value(a_json).unwrap();
        assert_eq!(p2, p);
        assert_eq!(a2, action);
    }
}
