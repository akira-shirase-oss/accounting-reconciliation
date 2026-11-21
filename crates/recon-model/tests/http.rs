//! The wire adapters against a canned loopback endpoint (failure case 9 and
//! the D-024 egress contract): the deadline, at most two retries for transient
//! statuses, the response cap, the TLS/loopback gate, no redirects — a
//! redirect's Location is never dialed — and the call budget.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a panic is the failure report"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use recon_domain::document::DocumentKind;
use recon_domain::{Currency, DocumentId, LineId};
use recon_model::{
    Budget, Candidate, Data, HttpProposer, LineFacts, ProposeError, ProposeRequest, Proposer,
};
use serde_json::{Value, json};
use uuid::Uuid;

const LINE: Uuid = Uuid::from_u128(1);
const DOC_A: Uuid = Uuid::from_u128(2);
const DOC_B: Uuid = Uuid::from_u128(3);

fn usd(minor: i64) -> recon_domain::Money {
    recon_domain::Money::new(minor, Currency::Usd).unwrap()
}

fn date(day: u8) -> recon_domain::Date {
    recon_domain::Date::new(2026, 4, day).unwrap()
}

fn request() -> ProposeRequest {
    ProposeRequest {
        prompt_version: "propose-v1".to_owned(),
        line: LineFacts {
            id: LineId::from_uuid(LINE),
            booking_date: date(2),
            amount: usd(-4215),
            narration: Data("PAYMENT INV REF-42".to_owned()),
            counterparty: Some(Data("Acme Corp".to_owned())),
        },
        candidates: vec![
            Candidate {
                id: DocumentId::from_uuid(DOC_A),
                amount: usd(4215),
                open: usd(4215),
                date: date(1),
                kind: DocumentKind::SalesInvoice,
                reference: Data("INV-1".to_owned()),
                counterparty: Some(Data("Acme Corp".to_owned())),
                excerpt: None,
            },
            Candidate {
                id: DocumentId::from_uuid(DOC_B),
                amount: usd(4215),
                open: usd(4215),
                date: date(1),
                kind: DocumentKind::SalesInvoice,
                reference: Data("INV-2".to_owned()),
                counterparty: Some(Data("Acme Corp".to_owned())),
                excerpt: None,
            },
        ],
    }
}

/// The OpenAI-shaped success body wrapping the schema's answer text.
fn chat_ok(answer: &Value) -> String {
    serde_json::to_string(&json!({
        "choices": [{"message": {"role": "assistant", "content": answer.to_string()}}]
    }))
    .unwrap()
}

/// A valid answer for `request()`: one allocation that balances exactly.
fn valid_answer() -> String {
    chat_ok(&json!({
        "kind": "proposal",
        "allocations": [{"document": DOC_A, "amount_minor": 4215, "currency": "USD"}],
        "reason": "the invoice matches the payment",
    }))
}

/// A canned endpoint: counts connections, answers each with the next
/// `(status, headers, body)` from `answers`, and — when none are left — the
/// last one. `delay` holds each answer back (the hung-provider fixture).
struct Fake {
    port: u16,
    hits: Arc<AtomicUsize>,
}

impl Fake {
    /// `answers`: (status, body) canned in order; `delay` per answer; `hold`
    /// makes the fake never write at all within `hold` (the deadline fixture).
    fn start(answers: Vec<(u16, String)>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            let mut canned = answers.into_iter();
            for stream in listener.incoming() {
                counter.fetch_add(1, Ordering::Relaxed);
                let Ok(mut stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut head = String::new();
                let mut reader =
                    BufReader::new(stream.try_clone().unwrap_or_else(|_| panic!("clone")));
                let mut content_length = 0_usize;
                loop {
                    head.clear();
                    let Ok(n) = reader.read_line(&mut head) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    if head.to_ascii_lowercase().starts_with("content-length:") {
                        content_length = head[15..].trim().parse().unwrap_or(0);
                    }
                    if head == "\r\n" {
                        break;
                    }
                }
                // Drain the request body so the response is not reset away.
                let mut left = content_length;
                let mut sink = [0_u8; 8192];
                while left > 0 {
                    match reader.read(&mut sink[..left.min(8192)]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => left -= n,
                    }
                }
                std::thread::sleep(delay);
                let (status, body) = canned.next().unwrap_or_else(|| (500, String::new()));
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { port, hits }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/chat/completions", self.port)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

fn proposer(fake: &Fake, calls: usize) -> HttpProposer {
    HttpProposer::loopback(
        &fake.url(),
        "test-model",
        Duration::from_secs(3),
        Budget::calls(calls),
    )
    .unwrap()
}

#[tokio::test]
async fn a_loopback_answer_round_trips_validated() {
    let fake = Fake::start(vec![(200, valid_answer())], Duration::ZERO);
    let answer = proposer(&fake, 8)
        .propose(&request())
        .await
        .expect("a valid answer must round-trip");
    assert_eq!(fake.hits(), 1);
    assert!(matches!(answer, recon_model::Answer::Proposal { .. }));
}

#[tokio::test]
async fn a_down_provider_is_unavailable() {
    // A bound-then-dropped listener leaves a port that refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proposer = HttpProposer::loopback(
        &format!("http://127.0.0.1:{port}/v1/chat/completions"),
        "test-model",
        Duration::from_millis(500),
        Budget::calls(8),
    )
    .unwrap();
    let err = proposer.propose(&request()).await.unwrap_err();
    assert_eq!(err, ProposeError::Unavailable);
}

#[tokio::test]
async fn transient_statuses_retry_at_most_twice() {
    let fake = Fake::start(vec![(503, String::new()); 10], Duration::ZERO);
    let started = Instant::now();
    let err = proposer(&fake, 8).propose(&request()).await.unwrap_err();
    assert_eq!(err, ProposeError::Unavailable);
    assert_eq!(fake.hits(), 3, "one call plus two retries, never more");
    assert!(started.elapsed() < Duration::from_secs(3));

    // A transient then a valid answer succeeds on the retry.
    let fake = Fake::start(
        vec![(429, String::new()), (200, valid_answer())],
        Duration::ZERO,
    );
    proposer(&fake, 8)
        .propose(&request())
        .await
        .expect("the retry answered a valid answer");
    assert_eq!(fake.hits(), 2);
}

#[tokio::test]
async fn a_hung_answer_ends_at_the_deadline() {
    let fake = Fake::start(vec![(200, valid_answer())], Duration::from_secs(10));
    let proposer = HttpProposer::loopback(
        &fake.url(),
        "test-model",
        Duration::from_millis(300),
        Budget::calls(8),
    )
    .unwrap();
    let started = Instant::now();
    let err = proposer.propose(&request()).await.unwrap_err();
    assert_eq!(err, ProposeError::Unavailable);
    // Three deadline-bound attempts plus two short backoffs — not the hang.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadline did not bound the hang: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_redirect_is_never_followed() {
    // `target` counts hits: a followed redirect would land here.
    let target = Fake::start(vec![(200, valid_answer())], Duration::ZERO);
    let second = target.port;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{second}/x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    let proposer = HttpProposer::loopback(
        &format!("http://127.0.0.1:{port}/v1/chat/completions"),
        "test-model",
        Duration::from_secs(2),
        Budget::calls(8),
    )
    .unwrap();
    let err = proposer.propose(&request()).await.unwrap_err();
    assert_eq!(
        err,
        ProposeError::Unavailable,
        "a 302 is a refusal, not a hop"
    );
    assert_eq!(target.hits(), 0, "the redirect's Location was dialed");
}

#[tokio::test]
async fn an_oversized_answer_is_malformed() {
    // Valid JSON beyond the cap — a *parseable* giant, so the cap is the
    // observable protection, not the parser's own refusal.
    let body = chat_ok(&json!({"kind": "none", "reason": "x".repeat(300 * 1024)}));
    let fake = Fake::start(vec![(200, body)], Duration::ZERO);
    let err = proposer(&fake, 8).propose(&request()).await.unwrap_err();
    assert_eq!(err, ProposeError::Malformed, "past the 256 KiB cap");
}

#[tokio::test]
async fn a_spent_budget_calls_nothing() {
    let fake = Fake::start(vec![(200, valid_answer())], Duration::ZERO);
    let err = proposer(&fake, 0).propose(&request()).await.unwrap_err();
    assert_eq!(err, ProposeError::Budget);
    assert_eq!(fake.hits(), 0, "a spent budget still sent a call");
}

#[test]
fn http_to_a_non_loopback_host_is_refused_at_construction() {
    for endpoint in [
        "http://169.254.169.254/v1/chat/completions",
        "http://model.internal/v1/chat/completions",
        "ftp://127.0.0.1/x",
        "http://user:pw@127.0.0.1/x",
    ] {
        assert!(
            HttpProposer::loopback(endpoint, "m", Duration::from_secs(1), Budget::calls(1))
                .is_err(),
            "{endpoint} was accepted"
        );
    }
    // The online adapter accepts https, never http — not even loopback.
    assert!(
        HttpProposer::new(
            "http://127.0.0.1/x",
            "m",
            None,
            Duration::from_secs(1),
            Budget::calls(1)
        )
        .is_err()
    );
    assert!(
        HttpProposer::new(
            "https://model.example/v1/chat/completions",
            "m",
            Some("k"),
            Duration::from_secs(1),
            Budget::calls(1)
        )
        .is_ok()
    );
}
