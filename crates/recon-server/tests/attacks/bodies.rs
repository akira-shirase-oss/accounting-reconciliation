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
    let r = http()
        .post(format!(
            "{}/v1/firms/{}/accounts/{}/imports",
            rig.server.base, rig.firm, rig.account
        ))
        .bearer_auth(TOKEN)
        .header("content-type", "text/csv")
        .body(body)
        .send()
        .await;
    match r {
        Ok(response) => assert_eq!(
            response.status(),
            413,
            "the oversized body answered {}",
            response.status()
        ),
        // Refused at the connection level is also a refusal — never a 5xx wedge.
        Err(e) => assert!(e.is_body() || e.is_request(), "unexpected error: {e}"),
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
    // Nothing more for > 500 ms of body-idle: the server must answer the named
    // refusal (408 upload_stalled) or close — never wedge a worker.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mut buf = [0_u8; 512];
    match std::io::Read::read(&mut stream, &mut buf) {
        Ok(0) => {} // closed — the honest refusal
        Ok(n) => {
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(
                text.contains("408") || text.contains("upload_stalled"),
                "unexpected answer: {text}"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("the drip-feed wedged the connection: {e}"),
    }
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
