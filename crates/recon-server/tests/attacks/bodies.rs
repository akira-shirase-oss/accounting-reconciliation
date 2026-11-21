//! Oversized and slow bodies: a body past the 50 MiB cap is refused with the
//! named `file_too_large` before a byte is staged, and a drip-fed body that
//! stalls past `body_idle_timeout` answers `upload_stalled` — neither wedges a
//! worker, and both leave the store untouched (checked, not assumed).

use std::io::Write as _;
use std::net::TcpStream;

use super::harness::Rig;
use crate::common::{TOKEN, http};

/// A valid CSV row, repeated — the oversized attack must be parseable, so a
/// removed cap ends in acceptance, not a parser refusal.
const ROW: &[u8] = b"2026-04-02,,-1.00,USD,BULK FEE,REF-1,Big Bank,\n";
const HEADER: &[u8] =
    b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

#[tokio::test]
async fn a_body_past_the_cap_is_refused() {
    let rig = Rig::start().await;
    // 50 MiB + 1 of VALID csv: past MAX_UPLOAD_BYTES, so the declared-length
    // check refuses before staging — and a mutant that removes the cap would
    // import ~1.4M rows, which the stored-state check also catches.
    let mut body = Vec::with_capacity(HEADER.len() + 52 * 1024 * 1024);
    body.extend_from_slice(HEADER);
    while body.len() <= 50 * 1024 * 1024 {
        body.extend_from_slice(ROW);
    }
    // Expect: 100-continue — a well-behaved client announces the body and waits
    // for the server's verdict before streaming it; without it, reqwest is
    // still mid-write when the 413 + connection-drop arrive and the named
    // refusal never surfaces (the reset is the refusal, but can't be read).
    let r = http()
        .post(format!(
            "{}/v1/firms/{}/accounts/{}/imports",
            rig.server.base, rig.firm, rig.account
        ))
        .bearer_auth(TOKEN)
        .header("content-type", "text/csv")
        .header("expect", "100-continue")
        .body(body)
        .send()
        .await;
    let body: Option<serde_json::Value> = match r {
        Ok(r) => {
            assert_eq!(
                r.status(),
                413,
                "the oversized body answered {}",
                r.status()
            );
            Some(r.json().await.unwrap())
        }
        // Under load the 413 + connection-drop can land while reqwest is still
        // writing the 50 MiB — the refusal reached the wire as a broken
        // connection; there is no body to read, and the staged-nothing check
        // below still applies. The error KIND is the check: reqwest's Display
        // is only `error sending request for url (…)` and never names the
        // reset, so a `to_string()` match is dead code. A connect or timeout
        // failure is still a wedge, not a refusal — those panic.
        Err(e) if e.is_request() || e.is_body() => None,
        Err(e) => panic!("the oversized upload failed oddly: {e}"),
    };
    if let Some(body) = body {
        // The named refusal, not just any 4xx: `file_too_large` is the contract
        // a client can act on — anything else means some other check fired first.
        assert_eq!(
            body["error"]["code"].as_str().unwrap_or(""),
            "file_too_large",
            "the refusal code: {body}"
        );
    }
    // The stored-state check: nothing was staged or imported.
    let (imports, lines): (i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete'),
           (SELECT count(*) FROM recon.statement_lines WHERE firm_id = $1)",
    )
    .bind(uuid::Uuid::parse_str(&rig.firm).unwrap())
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!((imports, lines), (0, 0), "the refused body staged data");
}

#[tokio::test]
async fn a_drip_fed_body_dies_at_the_idle_deadline() {
    // The body-idle deadline lives in the import upload's chunk loop
    // (config.body_idle_timeout → ReadFailure::Stalled → 408 upload_stalled).
    // A rig whose deadline is short enough to watch; the drip hits that route.
    let db = recon_db::testing::TestDb::new().await;
    let server =
        crate::common::Server::start(&db.app_url(), &[("RECON_BODY_IDLE_MS", "500")]).await;
    let (firm, _client, account) = crate::common::tenant(&server.base).await;
    let (host, port) = {
        let stripped = server.base.strip_prefix("http://").unwrap();
        let (h, p) = stripped.rsplit_once(':').unwrap();
        (h.to_owned(), p.parse::<u16>().unwrap())
    };
    let mut stream = TcpStream::connect((host.as_str(), port)).unwrap();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    // A request that announces 4 KiB of CSV, sends a parseable header row —
    // then goes silent. The announced body never arrives.
    let head = format!(
        "POST /v1/firms/{firm}/accounts/{account}/imports HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {TOKEN}\r\n\
         Content-Type: text/csv\r\n\
         Content-Length: 4096\r\n\r\n\
         booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    // The deadline is 500 ms of body-idle: the named refusal must be ON THE WIRE
    // inside ~1 s of the stall, or the worker was wedged. Read immediately —
    // the answer is the signal, not a sleep — with a read timeout that fails
    // the test when the deadline didn't fire. EOF/silence is a wedge here, not
    // a refusal: `upload_stalled` is the contract.
    // Bound the WHOLE wait, not a single read: short per-read timeouts inside a
    // total deadline (the 500 ms idle bound plus slack), so a refusal delivered
    // across several packets still counts and silence still fails by name.
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    let mut received = Vec::new();
    let mut buf = [0_u8; 1024];
    while std::time::Instant::now() < deadline {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&received).contains("upload_stalled") {
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("the stalled upload's socket broke: {e}"),
        }
    }
    assert!(
        !received.is_empty(),
        "the connection closed without an answer"
    );
    let text = String::from_utf8_lossy(&received);
    assert!(
        text.contains("408") && text.contains("upload_stalled"),
        "expected the named refusal, got: {text}"
    );
    // The stored-state check: the stalled upload completed nothing and staged
    // nothing a later request can see.
    let complete: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon.imports WHERE firm_id = $1 AND status = 'complete'",
    )
    .bind(uuid::Uuid::parse_str(&firm).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(complete, 0, "the stalled upload completed an import");
}
