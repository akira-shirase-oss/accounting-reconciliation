//! Connection-level bounds (D-047), against the real binary over raw sockets: a client can
//! not hold a connection by trickling headers, by staying silent, by idling a keep-alive
//! connection or by stalling a body; connections beyond the cap are closed at once; requests
//! beyond the in-flight limit are shed with `503`. None of this needs the database: the
//! server runs with an unreachable one and these paths never touch it.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

mod common;

use std::time::{Duration, Instant};

use common::{Server, TOKEN};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

const UNREACHABLE: &str = "postgres://recon_app:x@127.0.0.1:1/none";

async fn server(extra: &[(&str, &str)]) -> Server {
    let mut env = vec![
        ("RECON_HEADER_READ_TIMEOUT_MS", "1000"),
        ("RECON_REQUEST_TIMEOUT_MS", "2000"),
    ];
    env.extend_from_slice(extra);
    let server = Server::spawn(UNREACHABLE, &env);
    let target = addr(&server);
    let deadline = Instant::now() + Duration::from_secs(60);
    // A raw probe with Connection: close, so no pooled client connection keeps holding a
    // permit while a test counts connections.
    loop {
        if let Ok(mut s) = TcpStream::connect(&target).await
            && s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await
                .is_ok()
        {
            let (response, _) = read_until_closed(&mut s, Duration::from_secs(5)).await;
            if response.starts_with("HTTP/1.1 200") {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the server never answered /healthz"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Let the probe's connection task end and return its permit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    server
}

fn addr(server: &Server) -> String {
    server.base.trim_start_matches("http://").to_owned()
}

/// Reads until the peer closes or `within` passes; returns what arrived and whether it closed.
async fn read_until_closed(stream: &mut TcpStream, within: Duration) -> (String, bool) {
    let mut seen = Vec::new();
    let deadline = Instant::now() + within;
    let mut buf = [0_u8; 4096];
    while Instant::now() < deadline {
        match tokio::time::timeout_at(deadline.into(), stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return (String::from_utf8_lossy(&seen).into_owned(), true),
            Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&seen).into_owned(), false)
}

#[tokio::test]
async fn a_trickled_header_is_cut_off_and_never_answered() {
    let server = server(&[]).await;
    let mut stream = TcpStream::connect(addr(&server)).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let mut closed = false;
    for i in 0..30 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if stream
            .write_all(format!("X-Slow-{i}: y\r\n").as_bytes())
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        let mut probe = [0_u8; 512];
        match tokio::time::timeout(Duration::from_millis(1), stream.read(&mut probe)).await {
            Ok(Ok(0) | Err(_)) => {
                closed = true;
                break;
            }
            Ok(Ok(n)) => assert!(
                !String::from_utf8_lossy(&probe[..n]).contains("200"),
                "a trickled request was answered"
            ),
            Err(_) => {}
        }
    }
    let (rest, eof) = read_until_closed(&mut stream, Duration::from_secs(3)).await;
    assert!(
        closed || eof,
        "a header trickled for {:?} was not cut off",
        started.elapsed()
    );
    assert!(
        !rest.contains("200 OK"),
        "a trickled request was answered: {rest}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "cut off only after {:?}",
        started.elapsed()
    );
    server.stop().await;
}

#[tokio::test]
async fn silent_and_idle_keep_alive_sockets_are_closed() {
    let server = server(&[]).await;
    let mut silent = TcpStream::connect(addr(&server)).await.unwrap();
    let (_, closed) = read_until_closed(&mut silent, Duration::from_secs(8)).await;
    assert!(closed, "a socket that sent nothing was kept open");

    let mut idle = TcpStream::connect(addr(&server)).await.unwrap();
    idle.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0_u8; 1024];
    let n = idle.read(&mut first).await.unwrap();
    assert!(
        String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&first[..n])
    );
    let (_, closed) = read_until_closed(&mut idle, Duration::from_secs(8)).await;
    assert!(closed, "an idle keep-alive connection was kept open");
    server.stop().await;
}

#[tokio::test]
async fn a_stalled_body_is_answered_within_the_request_deadline() {
    let server = server(&[]).await;
    let mut stream = TcpStream::connect(addr(&server)).await.unwrap();
    let head = format!(
        "POST /v1/firms HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"name\""
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let started = Instant::now();
    let (response, _) = read_until_closed(&mut stream, Duration::from_secs(8)).await;
    assert!(
        response.starts_with("HTTP/1.1 503") || response.starts_with("HTTP/1.1 408"),
        "{response:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered after {:?}",
        started.elapsed()
    );
    server.stop().await;
}

#[tokio::test]
async fn connections_beyond_the_cap_are_closed_at_once() {
    let server = server(&[
        ("RECON_MAX_CONNECTIONS", "4"),
        ("RECON_HEADER_READ_TIMEOUT_MS", "5000"),
    ])
    .await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut s = TcpStream::connect(addr(&server)).await.unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\n").await.unwrap();
        held.push(s);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // All four hold a permit: none was closed on arrival.
    for (i, s) in held.iter_mut().enumerate() {
        let mut probe = [0_u8; 1];
        // Still waiting for data (the read times out) means the connection is open.
        let open = tokio::time::timeout(Duration::from_millis(50), s.read(&mut probe))
            .await
            .is_err();
        assert!(
            open,
            "held connection {i} was closed before the cap was reached"
        );
    }
    let mut extra = TcpStream::connect(addr(&server)).await.unwrap();
    let _ = extra
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    let (response, closed) = read_until_closed(&mut extra, Duration::from_secs(4)).await;
    assert!(
        closed && !response.contains("200"),
        "the fifth connection was served: {response:?}"
    );
    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = common::http()
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "capacity returns when connections close");
    server.stop().await;
}

#[tokio::test]
async fn requests_beyond_the_in_flight_limit_are_shed() {
    let server = server(&[
        ("RECON_MAX_IN_FLIGHT", "1"),
        ("RECON_REQUEST_TIMEOUT_MS", "3000"),
    ])
    .await;
    // One request holds the only slot by stalling its body.
    let mut stalled = TcpStream::connect(addr(&server)).await.unwrap();
    let head = format!(
        "POST /v1/firms HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    stalled.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    let r = common::http()
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body: serde_json::Value = r.json().await.unwrap_or_default();
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (503, Some("overloaded")),
        "{body}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shedding must be immediate"
    );
    drop(stalled);
    server.stop().await;
}

/// A client that sends requests and never reads the answers: once its receive window and
/// the server's send buffer are full, every write the server attempts is pending.
async fn non_reader(addr: &str) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(2048).unwrap();
    socket.connect(addr.parse().unwrap()).await.unwrap()
}

/// Non-reading clients cannot hold the connection slots: a write that makes no progress for
/// the write-stall deadline closes its connection, and capacity comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_reading_clients_cannot_hold_connection_slots() {
    let server = server(&[
        ("RECON_MAX_CONNECTIONS", "4"),
        ("RECON_WRITE_STALL_MS", "1000"),
        ("RECON_MAX_REQUESTS_PER_CONNECTION", "1000000"),
    ])
    .await;
    let target = addr(&server);
    let mut writers = Vec::new();
    for _ in 0..4 {
        let mut stream = non_reader(&target).await;
        writers.push(tokio::spawn(async move {
            let requests = b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n".repeat(20_000);
            let _ = stream.write_all(&requests).await;
            // Hold the socket open without ever reading.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    loop {
        let answered = tokio::time::timeout(
            Duration::from_secs(2),
            common::http()
                .get(format!("{}/healthz", server.base))
                .send(),
        )
        .await;
        if matches!(answered, Ok(Ok(ref r)) if r.status() == 200) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "non-reading clients still hold every slot after {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for w in writers {
        w.abort();
    }
    server.stop().await;
}

/// Pipelined requests past the per-connection cap: the limiting response says
/// `Connection: close` and nothing after it is answered on that connection.
#[tokio::test]
async fn pipelined_requests_past_the_cap_close_the_connection() {
    let server = server(&[("RECON_MAX_REQUESTS_PER_CONNECTION", "10")]).await;
    let mut stream = TcpStream::connect(addr(&server)).await.unwrap();
    stream
        .write_all(&b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n".repeat(20))
        .await
        .unwrap();
    let (responses, closed) = read_until_closed(&mut stream, Duration::from_secs(8)).await;
    let answered = responses.matches("HTTP/1.1 200").count();
    assert!(closed, "the connection stayed open after the cap");
    assert_eq!(
        answered, 10,
        "{answered} requests were answered on one connection"
    );
    let last = responses
        .rsplit("HTTP/1.1 200")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        last.contains("connection: close"),
        "the limiting response must say so: {last}"
    );
    server.stop().await;
}

/// A busy keep-alive connection is closed once it outlives its lifetime, after the request
/// in flight completes.
#[tokio::test]
async fn a_busy_connection_is_closed_after_its_lifetime() {
    let server = server(&[
        ("RECON_CONNECTION_MAX_AGE_MS", "1500"),
        ("RECON_HEADER_READ_TIMEOUT_MS", "5000"),
    ])
    .await;
    let mut stream = TcpStream::connect(addr(&server)).await.unwrap();
    let started = Instant::now();
    let mut answered = 0;
    let closed_after = loop {
        if stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .is_err()
        {
            break started.elapsed();
        }
        let mut buf = [0_u8; 2048];
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .expect("no answer on a live connection");
        match read {
            Ok(0) | Err(_) => break started.elapsed(),
            Ok(n) => {
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
                answered += 1;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "a busy connection outlived its 1.5 s lifetime ({answered} requests)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        answered >= 1,
        "the connection answered nothing before closing"
    );
    assert!(
        closed_after >= Duration::from_millis(1_400),
        "closed before its lifetime: {closed_after:?}"
    );
    server.stop().await;
}
