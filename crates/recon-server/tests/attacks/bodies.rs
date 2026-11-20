//! Oversized and slow bodies: a body past the cap is refused with its named
//! status, and a drip-fed body that stalls past the idle deadline gets its
//! connection closed — neither wedges a worker.

use std::io::Write as _;
use std::net::TcpStream;

use super::harness::Rig;
use crate::common::{TOKEN, http};

#[tokio::test]
async fn a_body_past_the_cap_is_refused() {
    let rig = Rig::start().await;
    // The import upload's cap is the tightest on the API; 8 MiB is past it.
    let body = vec![b'x'; 8 * 1024 * 1024];
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
        // 413 at the connection layer or 422 as the upload's named refusal —
        // both are a bounded refusal, never a wedge.
        Ok(response) => assert!(
            response.status() == 413 || response.status() == 422,
            "the oversized body answered {}",
            response.status()
        ),
        Err(e) => assert!(e.is_body() || e.is_request(), "unexpected error: {e}"),
    }
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
}
