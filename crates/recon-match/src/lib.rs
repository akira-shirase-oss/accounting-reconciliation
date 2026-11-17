//! The deterministic matching engine (ARCHITECTURE: `recon-match`; D-011 to D-014, D-070).
//!
//! [`run`] takes a client's open statement lines and open documents and decides, for every
//! line, one outcome: a balanced match with its tier and score, or a typed exception with
//! the candidates that made it one. Only a T1 match may commit without a person, and only
//! when the firm allows it; T2, T3 and T4 are proposals; a match with a residual always
//! needs four-eyes.
//!
//! Deterministic and order-independent: every line is decided on its own against the
//! documents' open amounts, in a fixed candidate order (date distance, then id), and
//! contention is resolved afterwards over the whole set — a document two lines would take
//! makes both lines `multiple_candidates`, never "first come, first served". The same
//! inputs in any order give the same outcomes and the same [`Run::digest`]. Nothing is
//! matched that is not certain: an ambiguity is an exception naming its reason.
//!
//! Bounded: at most [`Settings::max_candidates`] candidates per line enter the combination
//! search, combinations of at most [`Settings::max_combination`] documents, and at most
//! [`Settings::node_budget`] search nodes per line; an exhausted budget is
//! `search_budget_exhausted`, never a partial guess. No I/O, no clock, no randomness.
#![deny(clippy::arithmetic_side_effects)]

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use recon_domain::document::DocumentKind;
use recon_domain::line::ExceptionKind;
use recon_domain::matching::{
    BalancedMatch, DocumentAllocation, LineAllocation, Residual, ResidualCode, ScoreBreakdown, Tier,
};
use recon_domain::text::normalize;
use recon_domain::{Currency, Date, DocumentId, LineId, Money};

/// The engine's version, recorded on every decision it causes (D-035).
pub const MATCH_ENGINE_VERSION: &str = "match-v1";

/// Most candidates an exception lists.
pub const MAX_LISTED_CANDIDATES: usize = 10;

/// An open statement line, as the engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInput {
    /// The line.
    pub id: LineId,
    /// The booking date.
    pub booking_date: Date,
    /// The signed amount.
    pub amount: Money,
    /// The narration after `normalization v1`.
    pub narration_norm: String,
    /// The bank's reference, as written.
    pub reference: Option<String>,
    /// The counterparty after `normalization v1`.
    pub counterparty_norm: Option<String>,
}

/// An open document, as the engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentInput {
    /// The document.
    pub id: DocumentId,
    /// The kind.
    pub kind: DocumentKind,
    /// The number after `normalization v1`.
    pub number_norm: Option<String>,
    /// The issue date.
    pub issue_date: Date,
    /// The due date.
    pub due_date: Option<Date>,
    /// What is still open, signed as the bank would see it.
    pub open_amount: Money,
    /// The counterparty after `normalization v1`.
    pub counterparty_norm: Option<String>,
}

/// How far a residual may go, per code, in minor units (D-014: zero until the firm sets it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tolerances {
    /// A bank fee deducted from a payment.
    pub bank_fee: u64,
    /// Tax withheld at source.
    pub tax_withheld: u64,
    /// Rounding, at most one minor unit per document in any case.
    pub rounding: u64,
}

/// The engine's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// T1 may commit without a person (the firm may turn it off, D-011).
    pub auto_commit_t1: bool,
    /// Residual tolerances per currency.
    pub tolerances: BTreeMap<Currency, Tolerances>,
    /// Most candidates per line in the combination search (D-013: 40).
    pub max_candidates: usize,
    /// Most documents in one combination (D-013: 5).
    pub max_combination: usize,
    /// Most search nodes per line (D-013: 100,000).
    pub node_budget: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_commit_t1: true,
            tolerances: BTreeMap::new(),
            max_candidates: 40,
            max_combination: 5,
            node_budget: 100_000,
        }
    }
}

/// What the engine decided for one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A balanced match.
    Matched {
        /// The tier that found it.
        tier: Tier,
        /// The match.
        matched: BalancedMatch,
        /// The score and its breakdown.
        score: ScoreBreakdown,
        /// True only for T1 when the firm allows auto-commit and no residual is present.
        commit: bool,
    },
    /// A typed exception.
    Exception {
        /// Why.
        kind: ExceptionKind,
        /// The candidates behind it, nearest first (at most [`MAX_LISTED_CANDIDATES`]).
        candidates: Vec<DocumentId>,
    },
}

/// One run's outcomes and their digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// One outcome per line, by line id.
    pub outcomes: BTreeMap<LineId, Outcome>,
    /// SHA-256 over every outcome in line order: the same inputs give the same digest.
    pub digest: [u8; 32],
}

/// Days since 1970-01-01 (proleptic Gregorian), for date distances.
pub fn day_number(d: Date) -> i64 {
    // Howard Hinnant's days_from_civil, in i64 (years 1990-2100: no overflow).
    let (y, m, day) = (
        i64::from(d.year()),
        i64::from(d.month()),
        i64::from(d.day()),
    );
    let y = if m <= 2 { y.saturating_sub(1) } else { y };
    let era = y.div_euclid(400);
    let yoe = y.saturating_sub(era.saturating_mul(400));
    let mp = if m > 2 {
        m.saturating_sub(3)
    } else {
        m.saturating_add(9)
    };
    let doy = (153_i64.saturating_mul(mp).saturating_add(2))
        .div_euclid(5)
        .saturating_add(day)
        .saturating_sub(1);
    let doe = yoe
        .saturating_mul(365)
        .saturating_add(yoe.div_euclid(4))
        .saturating_sub(yoe.div_euclid(100))
        .saturating_add(doy);
    era.saturating_mul(146_097)
        .saturating_add(doe)
        .saturating_sub(719_468)
}

/// The reference tokens of a line: its reference, and every word of its narration (and
/// each part of a word split at `/`), trimmed of punctuation, upper-cased, at least three
/// characters with a digit — invoice-number candidates (D-029's generic tokens).
pub fn reference_tokens(reference: Option<&str>, narration_norm: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut take = |word: &str| {
        let t = word.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | '(' | ')' | '#'));
        if t.chars().count() >= 3 && t.chars().any(|c| c.is_ascii_digit()) {
            out.insert(normalize(t));
        }
    };
    if let Some(r) = reference {
        take(r.trim());
    }
    for word in narration_norm.split(' ') {
        take(word);
        if word.contains('/') {
            word.split('/').for_each(&mut take);
        }
    }
    out
}

/// True when a document of this kind is a candidate for a line on `booking`: the window
/// (D-011) is issue date − 7 days to due date (or issue date) + 90; book entries ± 5 days.
fn in_window(line: &LineInput, doc: &DocumentInput) -> bool {
    let b = day_number(line.booking_date);
    let issue = day_number(doc.issue_date);
    if doc.kind == DocumentKind::BookEntry {
        return b >= issue.saturating_sub(5) && b <= issue.saturating_add(5);
    }
    let end = day_number(doc.due_date.unwrap_or(doc.issue_date));
    b >= issue.saturating_sub(7) && b <= end.saturating_add(90)
}

/// A candidate with what ranks it.
#[derive(Debug, Clone, Copy)]
struct Candidate<'d> {
    doc: &'d DocumentInput,
    distance: i64,
}

/// The candidates of `line`: same currency and direction, open, in the window, nearest by
/// date then by id.
fn candidates<'d>(line: &LineInput, docs: &'d [DocumentInput]) -> Vec<Candidate<'d>> {
    let mut out: Vec<Candidate<'d>> = docs
        .iter()
        .filter(|d| {
            d.open_amount.currency() == line.amount.currency()
                && !d.open_amount.is_zero()
                && (d.open_amount.minor() > 0) == (line.amount.minor() > 0)
                && in_window(line, d)
        })
        .map(|d| Candidate {
            doc: d,
            distance: day_number(line.booking_date)
                .saturating_sub(day_number(d.issue_date))
                .abs(),
        })
        .collect();
    out.sort_by(|a, b| a.distance.cmp(&b.distance).then(a.doc.id.cmp(&b.doc.id)));
    out
}

fn counterparty_matches(line: &LineInput, doc: &DocumentInput) -> bool {
    match (&line.counterparty_norm, &doc.counterparty_norm) {
        (Some(l), Some(d)) => l == d || line.narration_norm.contains(d.as_str()),
        (None, Some(d)) => line.narration_norm.contains(d.as_str()),
        _ => false,
    }
}

/// 10 points at the same day, one less per 9 days, 0 from 90 days.
fn proximity(distance: i64) -> u8 {
    u8::try_from(10_i64.saturating_sub(distance.div_euclid(9)).clamp(0, 10)).unwrap_or(0)
}

fn listed(cands: &[Candidate<'_>]) -> Vec<DocumentId> {
    cands
        .iter()
        .take(MAX_LISTED_CANDIDATES)
        .map(|c| c.doc.id)
        .collect()
}

fn exception(kind: ExceptionKind, cands: &[Candidate<'_>]) -> Outcome {
    Outcome::Exception {
        kind,
        candidates: listed(cands),
    }
}

/// The result of searching combinations: none, exactly one, more than one, or out of budget.
enum Found {
    None,
    One(Vec<usize>, i64),
    Many,
    Exhausted,
}

/// Searches combinations of `sizes` documents (indexes into `amounts`, ascending magnitudes
/// with their ids as the tie-break) whose total `t` satisfies `target <= t <= target +
/// slack`, in a fixed order, within `budget` nodes.
fn search(amounts: &[u64], target: u64, slack: u64, sizes: (usize, usize), budget: u32) -> Found {
    struct State<'a> {
        amounts: &'a [u64],
        target: u64,
        limit: u64,
        sizes: (usize, usize),
        nodes: u32,
        budget: u32,
        found: Vec<(Vec<usize>, u64)>,
        exhausted: bool,
    }
    fn walk(s: &mut State<'_>, start: usize, picked: &mut Vec<usize>, sum: u64) {
        for i in start..s.amounts.len() {
            if s.exhausted || s.found.len() > 1 {
                return;
            }
            s.nodes = s.nodes.saturating_add(1);
            if s.nodes > s.budget {
                s.exhausted = true;
                return;
            }
            let a = s.amounts.get(i).copied().unwrap_or(u64::MAX);
            let total = sum.saturating_add(a);
            if total > s.limit {
                // Ascending amounts: every later one is at least as large.
                return;
            }
            picked.push(i);
            if total >= s.target && picked.len() >= s.sizes.0 {
                s.found.push((picked.clone(), total));
            }
            if picked.len() < s.sizes.1 {
                walk(s, i.saturating_add(1), picked, total);
            }
            picked.pop();
        }
    }
    let mut s = State {
        amounts,
        target,
        limit: target.saturating_add(slack),
        sizes,
        nodes: 0,
        budget,
        found: Vec::new(),
        exhausted: false,
    };
    walk(&mut s, 0, &mut Vec::new(), 0);
    if s.exhausted {
        return Found::Exhausted;
    }
    match s.found.len() {
        0 => Found::None,
        1 => s.found.pop().map_or(Found::None, |(picked, total)| {
            Found::One(picked, i64::try_from(total).unwrap_or(i64::MAX))
        }),
        _ => Found::Many,
    }
}

fn single(line: &LineInput, doc: &DocumentInput) -> Option<BalancedMatch> {
    BalancedMatch::new(
        vec![LineAllocation {
            line: line.id,
            amount: line.amount,
        }],
        vec![DocumentAllocation {
            document: doc.id,
            amount: doc.open_amount,
        }],
        vec![],
    )
    .ok()
}

/// Decides one line against the documents' open amounts.
fn decide(line: &LineInput, docs: &[DocumentInput], settings: &Settings) -> Outcome {
    let cands = candidates(line, docs);
    let tokens = reference_tokens(line.reference.as_deref(), &line.narration_norm);
    if cands.is_empty() {
        // Only documents in another currency name this line: no FX matching in v1 (D-005).
        let foreign: Vec<DocumentId> = docs
            .iter()
            .filter(|d| {
                d.open_amount.currency() != line.amount.currency()
                    && in_window(line, d)
                    && (d.number_norm.as_ref().is_some_and(|n| tokens.contains(n))
                        || counterparty_matches(line, d))
            })
            .map(|d| d.id)
            .take(MAX_LISTED_CANDIDATES)
            .collect();
        let kind = if foreign.is_empty() {
            ExceptionKind::NoCandidate
        } else {
            ExceptionKind::CrossCurrency
        };
        return Outcome::Exception {
            kind,
            candidates: foreign,
        };
    }
    let by_reference: Vec<Candidate<'_>> = cands
        .iter()
        .copied()
        .filter(|c| {
            c.doc
                .number_norm
                .as_ref()
                .is_some_and(|n| tokens.contains(n))
        })
        .collect();
    let exact: Vec<Candidate<'_>> = cands
        .iter()
        .copied()
        .filter(|c| c.doc.open_amount == line.amount)
        .collect();
    let t1: Vec<Candidate<'_>> = by_reference
        .iter()
        .copied()
        .filter(|c| c.doc.open_amount == line.amount)
        .collect();
    // T1: exact amount and reference, exactly one candidate.
    match t1.as_slice() {
        [only] => {
            if let Some(matched) = single(line, only.doc) {
                let score = ScoreBreakdown::new(
                    true,
                    true,
                    counterparty_matches(line, only.doc),
                    proximity(only.distance),
                    true,
                );
                if let Some(score) = score {
                    return Outcome::Matched {
                        tier: Tier::T1,
                        matched,
                        score,
                        commit: settings.auto_commit_t1,
                    };
                }
            }
        }
        [] => {}
        many => return exception(ExceptionKind::MultipleCandidates, many),
    }
    // A reference names a document whose open amount differs: a person decides.
    if !by_reference.is_empty() && exact.is_empty() {
        return exception(ExceptionKind::AmountMismatch, &by_reference);
    }
    // T2: exact amount, unique in the window, and the counterparty agrees.
    match exact.as_slice() {
        [only] if counterparty_matches(line, only.doc) => {
            if let (Some(matched), Some(score)) = (
                single(line, only.doc),
                ScoreBreakdown::new(true, false, true, proximity(only.distance), true),
            ) {
                return Outcome::Matched {
                    tier: Tier::T2,
                    matched,
                    score,
                    commit: false,
                };
            }
        }
        [_] => return exception(ExceptionKind::NarrationNeeded, &exact),
        [] => {}
        many => return exception(ExceptionKind::MultipleCandidates, many),
    }
    combinations(line, &cands, settings)
}

/// T3 (an exact combination) and T4 (a combination with a named residual) over the
/// nearest candidates of the line's counterparty (D-013, D-014).
fn combinations(line: &LineInput, cands: &[Candidate<'_>], settings: &Settings) -> Outcome {
    let mut pool: Vec<Candidate<'_>> = cands
        .iter()
        .copied()
        .filter(|c| counterparty_matches(line, c.doc))
        .take(settings.max_candidates)
        .collect();
    if pool.is_empty() {
        return exception(ExceptionKind::NoCandidate, cands);
    }
    pool.sort_by(|a, b| {
        a.doc
            .open_amount
            .minor()
            .unsigned_abs()
            .cmp(&b.doc.open_amount.minor().unsigned_abs())
            .then(a.doc.id.cmp(&b.doc.id))
    });
    let amounts: Vec<u64> = pool
        .iter()
        .map(|c| c.doc.open_amount.minor().unsigned_abs())
        .collect();
    let target = line.amount.minor().unsigned_abs();
    let max = settings.max_combination.max(1);
    match search(&amounts, target, 0, (2, max), settings.node_budget) {
        Found::One(picked, _) => return build(line, &pool, &picked, Tier::T3, None),
        Found::Many => return exception(ExceptionKind::MultipleCandidates, &pool),
        Found::Exhausted => return exception(ExceptionKind::SearchBudgetExhausted, &pool),
        Found::None => {}
    }
    let tol = settings
        .tolerances
        .get(&line.amount.currency())
        .copied()
        .unwrap_or_default();
    let slack = tol.bank_fee.max(tol.tax_withheld).max(tol.rounding);
    if slack > 0 {
        match search(&amounts, target, slack, (1, max), settings.node_budget) {
            Found::One(picked, total) => {
                let diff = u64::try_from(total)
                    .unwrap_or(u64::MAX)
                    .saturating_sub(target);
                if diff == 0 {
                    return build(line, &pool, &picked, Tier::T3, None);
                }
                let docs_n = u64::try_from(picked.len()).unwrap_or(u64::MAX);
                let code = if diff <= tol.rounding.min(docs_n) {
                    ResidualCode::Rounding
                } else if diff <= tol.bank_fee {
                    ResidualCode::BankFee
                } else if diff <= tol.tax_withheld {
                    ResidualCode::TaxWithheld
                } else {
                    return exception(ExceptionKind::AmountMismatch, &pool);
                };
                return build(line, &pool, &picked, Tier::T4, Some((code, diff)));
            }
            Found::Many => return exception(ExceptionKind::MultipleCandidates, &pool),
            Found::Exhausted => return exception(ExceptionKind::SearchBudgetExhausted, &pool),
            Found::None => {}
        }
    }
    exception(ExceptionKind::AmountMismatch, &pool)
}

fn build(
    line: &LineInput,
    pool: &[Candidate<'_>],
    picked: &[usize],
    tier: Tier,
    residual: Option<(ResidualCode, u64)>,
) -> Outcome {
    let chosen: Vec<Candidate<'_>> = picked
        .iter()
        .filter_map(|i| pool.get(*i).copied())
        .collect();
    let documents: Vec<DocumentAllocation> = chosen
        .iter()
        .map(|c| DocumentAllocation {
            document: c.doc.id,
            amount: c.doc.open_amount,
        })
        .collect();
    // The line receives (or pays) less than the documents by the residual: the residual
    // carries the line's sign reversed, so lines = documents + residual exactly.
    let residuals: Vec<Residual> = residual
        .and_then(|(code, diff)| {
            let signed = i64::try_from(diff).ok()?;
            let signed = if line.amount.minor() > 0 {
                signed.checked_neg()?
            } else {
                signed
            };
            Some(Residual {
                code,
                amount: Money::new(signed, line.amount.currency()).ok()?,
            })
        })
        .into_iter()
        .collect();
    let lines = vec![LineAllocation {
        line: line.id,
        amount: line.amount,
    }];
    let nearest = chosen.iter().map(|c| c.distance).min().unwrap_or(i64::MAX);
    let score = ScoreBreakdown::new(residuals.is_empty(), false, true, proximity(nearest), true);
    match (BalancedMatch::new(lines, documents, residuals), score) {
        (Ok(matched), Some(score)) => Outcome::Matched {
            tier,
            matched,
            score,
            commit: false,
        },
        // Unreachable for a combination the search accepted; refused, never forced.
        _ => exception(ExceptionKind::AmountMismatch, pool),
    }
}

/// Decides every line (see the crate docs). `lines` and `documents` may come in any order.
pub fn run(lines: &[LineInput], documents: &[DocumentInput], settings: &Settings) -> Run {
    let mut decided: BTreeMap<LineId, Outcome> = lines
        .iter()
        .map(|l| (l.id, decide(l, documents, settings)))
        .collect();
    // Contention: a document more than one line would take makes each of them an
    // exception, whatever order they came in.
    let mut claims: BTreeMap<DocumentId, BTreeSet<LineId>> = BTreeMap::new();
    for (line, outcome) in &decided {
        if let Outcome::Matched { matched, .. } = outcome {
            for d in matched.documents() {
                claims.entry(d.document).or_default().insert(*line);
            }
        }
    }
    let contested: BTreeMap<LineId, BTreeSet<DocumentId>> = claims
        .iter()
        .filter(|(_, ls)| ls.len() > 1)
        .fold(BTreeMap::new(), |mut acc, (doc, ls)| {
            for l in ls {
                acc.entry(*l).or_insert_with(BTreeSet::new).insert(*doc);
            }
            acc
        });
    for (line, docs) in contested {
        decided.insert(
            line,
            Outcome::Exception {
                kind: ExceptionKind::MultipleCandidates,
                candidates: docs.into_iter().take(MAX_LISTED_CANDIDATES).collect(),
            },
        );
    }
    let digest = digest(&decided);
    Run {
        outcomes: decided,
        digest,
    }
}

/// SHA-256 over the outcomes' canonical bytes, in line order.
fn digest(outcomes: &BTreeMap<LineId, Outcome>) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(MATCH_ENGINE_VERSION.as_bytes());
    for (line, outcome) in outcomes {
        h.update(line.as_bytes());
        match outcome {
            Outcome::Matched {
                tier,
                matched,
                score,
                commit,
            } => {
                h.update([1, *tier as u8, u8::from(*commit), score.total()]);
                for d in matched.documents() {
                    h.update(d.document.as_bytes());
                    h.update(d.amount.minor().to_be_bytes());
                }
                for r in matched.residuals() {
                    h.update(r.code.code().as_bytes());
                    h.update(r.amount.minor().to_be_bytes());
                }
            }
            Outcome::Exception { kind, candidates } => {
                h.update([2]);
                h.update(kind.code().as_bytes());
                for c in candidates {
                    h.update(c.as_bytes());
                }
            }
        }
        h.update([0xFF]);
    }
    h.finalize().into()
}

#[cfg(test)]
mod tests;
