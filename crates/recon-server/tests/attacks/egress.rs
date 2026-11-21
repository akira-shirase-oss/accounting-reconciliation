//! The egress security case (D-024, R4): the server opens no outbound
//! connection at all — model use is off until a firm opts in, and the adapter
//! in `recon-model` is the only outbound-capable code in the workspace. A
//! recording listener on a fixed loopback port watches every test run; the
//! `egress_dial` mutant makes the accept loop dial it, proving the guard sees
//! a connection it did not ask for.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::harness::{Rig, get};

/// The mutant's dial target: a fixed loopback port it can name verbatim.
pub(crate) const EGRESS_PORT: u16 = 64977;

#[tokio::test]
async fn the_server_calls_nothing_but_the_model_endpoint() {
    let listener = TcpListener::bind(("127.0.0.1", EGRESS_PORT)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let accept = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    counter.fetch_add(1, Ordering::Relaxed);
                    drop(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let rig = Rig::start().await;
    let session = rig.enroll("owner").await;
    // Touch the surfaces an outbound call could plausibly ride: the probes the
    // proxy polls, an authenticated read, and an import (the path most likely
    // to grow a "fetch from url" regression).
    get(&rig.server.base, "/healthz", None).await;
    get(&rig.server.base, "/readyz", None).await;
    let (status, body) = get(
        &rig.server.base,
        &format!("/v1/firms/{}/clients/{}/review-queue", rig.firm, rig.client),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = crate::common::upload(
        &rig.server.base,
        &rig.firm,
        &rig.account,
        b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n\
          2026-04-02,,-1.00,USD,egress probe,REF-1,Bank,\n"
            .to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(status, 201);

    // An outbound call is asynchronous to the request that triggered it — give
    // it a beat to land, then hold the listener open for a final count.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = hits.load(Ordering::Relaxed);
    accept.join().unwrap();
    assert_eq!(
        seen.max(hits.load(Ordering::Relaxed)),
        0,
        "the server opened an outbound connection with model use off"
    );
}
