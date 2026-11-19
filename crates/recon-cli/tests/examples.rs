//! The examples stay runnable (D-068): every command `examples/README.md` shows runs here
//! against the real `recon` binary, with the exit status and the lines an accountant reads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use std::process::Command;

fn recon(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_recon"))
        .args(args)
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env_clear()
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

const SAVINGS: &str = "examples/profiles/indian-bank-savings.json";

#[test]
fn help_lists_every_command_and_explains_each_one() {
    for args in [&["--help"][..], &["help"], &[]] {
        let (code, text) = recon(args);
        assert_eq!(code, 0, "{args:?}");
        for command in [
            "import check",
            "profile check",
            "db setup",
            "db drop",
            "audit verify",
            "audit anchor",
        ] {
            assert!(text.contains(command), "{args:?} lacks {command}");
        }
    }
    for topic in [
        "import check",
        "profile check",
        "db setup",
        "db migrate",
        "audit verify",
        "audit anchor",
        "audit replay",
        "gen statement",
    ] {
        let words: Vec<&str> = topic.split(' ').collect();
        let (code, text) = recon(&[&["help"][..], &words].concat());
        assert_eq!(code, 0, "help {topic}");
        assert!(
            text.starts_with(&format!("recon {topic}")),
            "help {topic}: {text}"
        );
        let (code, flag) = recon(&[&words[..], &["--help"]].concat());
        assert_eq!((code, flag), (0, text), "{topic} --help");
    }
    let (code, text) = recon(&["import", "chek"]);
    assert_eq!(code, 1);
    assert!(text.contains("unknown command: import chek"), "{text}");
    assert!(text.contains("recon help COMMAND"));
}

#[test]
fn a_profile_check_shows_how_it_reads_or_every_problem() {
    let (code, text) = recon(&["profile", "check", SAVINGS]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Dates:   DD/MM/YYYY"), "{text}");
    assert!(text.contains("Amounts: like 12,34,567.89"), "{text}");
    let dir = std::env::temp_dir().join(format!("recon-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let broken = dir.join("broken.json");
    let mut value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/profiles/indian-bank-savings.json"
        ))
        .unwrap(),
    )
    .unwrap();
    value["definition"]["delimiter"] = serde_json::json!("x");
    value["definition"]["columns"]["reference"] = serde_json::json!("Ref");
    std::fs::write(&broken, value.to_string()).unwrap();
    let (code, text) = recon(&["profile", "check", broken.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("cannot be used (2 problems)"), "{text}");
    assert!(
        text.contains("columns.reference: names no header column"),
        "{text}"
    );
}

#[test]
fn the_example_statement_would_import_with_its_money_exactly() {
    let (code, text) = recon(&[
        "import",
        "check",
        "examples/statements/indian-bank-march-2026.csv",
        "--profile",
        SAVINGS,
        "--currency",
        "INR",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("would import 12 rows"), "{text}");
    assert!(text.contains("Dates:   2026-03-01 to 2026-03-30"), "{text}");
    assert!(
        text.contains("money in 228384.50 INR, money out 120661.25 INR"),
        "{text}"
    );
    let (code, json) = recon(&[
        "import",
        "check",
        "examples/statements/indian-bank-march-2026.csv",
        "--profile",
        SAVINGS,
        "--currency",
        "INR",
        "--json",
    ]);
    assert_eq!(code, 0);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["money"][0]["money_out"], "120661.25 INR");
}

#[test]
fn the_example_with_errors_says_line_column_problem_and_fix() {
    let (code, text) = recon(&[
        "import",
        "check",
        "examples/statements/indian-bank-march-2026-with-errors.csv",
        "--profile",
        SAVINGS,
        "--currency",
        "INR",
    ]);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("3 of 12 rows have a problem"), "{text}");
    for expected in [
        "line 4, column \"Date\" [invalid_date]",
        "The profile reads dates as DD/MM/YYYY.",
        "line 7, column \"Withdrawal Amt.\" [invalid_amount]",
        "Remove currency symbols",
        "line 10, column \"Withdrawal Amt. / Deposit Amt.\" [debit_and_credit]",
        "How to fix:",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}

#[test]
fn a_draft_profile_asks_the_date_question_with_its_answer() {
    let (code, text) = recon(&[
        "import",
        "check",
        "examples/statements/indian-bank-march-2026.csv",
        "--profile",
        "examples/profiles/indian-bank-draft.json",
        "--currency",
        "INR",
    ]);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("[ambiguous_date_format]"), "{text}");
    assert!(text.contains("\"order\": \"dmy\""), "{text}");
}

#[test]
fn documents_and_ofx_examples_check_too() {
    let (code, text) = recon(&[
        "import",
        "check",
        "examples/documents/sales-register-march-2026.csv",
        "--profile",
        "examples/profiles/sales-register.json",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("would import 4 rows"), "{text}");
    assert!(text.contains("money in 228200.00 INR"), "{text}");
    let (code, text) = recon(&[
        "import",
        "check",
        "fixtures/ofx/sgml-1x.ofx",
        "--profile",
        "ofx",
        "--currency",
        "USD",
    ]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = recon(&[
        "import",
        "check",
        "fixtures/hostile-files/ofx-billion-laughs.ofx",
        "--profile",
        "ofx",
        "--currency",
        "USD",
    ]);
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("the whole file [declaration_refused]"),
        "{text}"
    );
    // A statement needs the account's currency, and says so.
    let (code, text) = recon(&[
        "import",
        "check",
        "examples/statements/indian-bank-march-2026.csv",
        "--profile",
        SAVINGS,
    ]);
    assert_eq!(code, 1);
    assert!(text.contains("--currency is required"), "{text}");
}

#[test]
fn db_drop_refuses_anything_but_a_scratch_name() {
    // The guard runs before the database is touched, so the refusal needs no server.
    for name in ["recon", "postgres", "recon_production"] {
        let (code, text) = recon(&["db", "drop", "--database", name]);
        assert_eq!(code, 1, "{name}");
        assert!(
            text.contains("refusing to drop") && text.contains("scratch"),
            "{name}: {text}"
        );
    }
}
