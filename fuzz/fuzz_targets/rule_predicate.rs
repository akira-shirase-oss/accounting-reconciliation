//! A rule definition is external input (the API's JSON): any bytes either parse
//! into `RulePredicate`/`RuleAction` or are refused, and a parsed pair compiles or
//! is refused with its named error — the regex arm is the attack surface (a
//! bounded PikeVM with a compiled-size cap), never a panic, never a hang.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::rule::{CompiledPredicate, RuleAction, RulePredicate};

fuzz_target!(|data: &[u8]| {
    let Ok(mut predicate) = serde_json::from_slice::<RulePredicate>(data) else {
        return;
    };
    // The ops are the interesting axis: force regex occasionally by splicing the
    // raw bytes into a regex value when the parse came back without one.
    if predicate.narration.is_none() {
        if let Ok(text) = std::str::from_utf8(data) {
            predicate.narration = Some(recon_domain::rule::TextMatch {
                op: recon_domain::rule::TextOp::Regex,
                value: text.chars().take(1024).collect(),
            });
        }
    }
    for action in [
        RuleAction::Exclude {
            reason: "non_business".to_owned(),
        },
        RuleAction::AssignCounterparty {
            counterparty: "acme".to_owned(),
        },
        RuleAction::PreferDocumentKind {
            document_kind: "sales_invoice".to_owned(),
        },
    ] {
        let _ = CompiledPredicate::compile(&predicate, &action);
    }
});
