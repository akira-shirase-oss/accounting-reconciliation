//! Drill `hostile` (failure case 6 through the real binary, D-066): every fixture in
//! `fixtures/hostile-files/` and the generated large inputs are uploaded to a running
//! `recon-server`; each is refused with its code and row; the server's resident memory,
//! sampled every 20 ms with `ps`, grows by less than 64 MiB; nothing of a refused file is
//! persisted (no line, document or staged row); the log carries none of their content.
//! It also measures the RSS of a valid 50 MiB wide-row file (owed since D-050).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests: a panic is the failure report"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{Server, TOKEN, assert_log_clean, http, post_json};
use recon_db::testing::TestDb;
use serde_json::{Value, json};

/// The memory bound failure case 6 sets.
const RSS_BOUND: u64 = 64 * 1024 * 1024;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/hostile-files/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Resident memory of `pid` in bytes, from `ps` (KiB on Linux and macOS).
fn rss(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

/// Samples `pid`'s RSS every 20 ms into `peak` until the returned guard drops.
fn sample(pid: u32, peak: Arc<AtomicU64>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Some(r) = rss(pid) {
                peak.fetch_max(r, Ordering::Relaxed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
}

/// Uploads `chunks` as a streamed body; a broken connection after an early answer counts
/// as an answer the import row will show.
async fn send(
    url: &str,
    content_type: &str,
    chunks: Vec<Vec<u8>>,
) -> (Option<u16>, Value, Duration) {
    let stream = tokio_stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let started = Instant::now();
    let result = http()
        .post(url)
        .bearer_auth(TOKEN)
        .header("content-type", content_type)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await;
    match result {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.json().await.unwrap_or(Value::Null);
            (Some(status), body, started.elapsed())
        }
        Err(_) => (None, Value::Null, started.elapsed()),
    }
}

fn pieces(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes.chunks(16 * 1024).map(<[u8]>::to_vec).collect()
}

/// A fixture, its media type, the refusal code it must get and its first refused row.
type Case = (
    &'static str,
    &'static str,
    &'static str,
    Option<(u32, &'static str)>,
);

const HEAD: &[u8] =
    b"booking_date,value_date,amount,currency,narration,reference,counterparty,balance_after\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drill_hostile_files() {
    let db = TestDb::new().await;
    let server = Server::start(&db.app_url(), &[]).await;
    let pid = server.child.id().unwrap();
    let base = server.base.clone();
    let (_, firm) = post_json(&base, "/v1/firms", &json!({"name": "Hostile Firm"})).await;
    let firm = firm["firm"]["id"].as_str().unwrap().to_owned();
    let (_, client) = post_json(
        &base,
        &format!("/v1/firms/{firm}/clients"),
        &json!({"name": "Hostile Client"}),
    )
    .await;
    let client = client["client"]["id"].as_str().unwrap().to_owned();
    let account = |name: &'static str| {
        let (base, firm, client) = (base.clone(), firm.clone(), client.clone());
        async move {
            let (_, a) = post_json(
                &base,
                &format!("/v1/firms/{firm}/clients/{client}/accounts"),
                &json!({"name": name, "currency": "USD"}),
            )
            .await;
            a["account"]["id"].as_str().unwrap().to_owned()
        }
    };
    let hostile = account("Hostile").await;
    let csv_url = format!("{base}/v1/firms/{firm}/accounts/{hostile}/imports");
    let ofx_url = format!("{csv_url}?profile=ofx");

    // Every small fixture: refused with its code (file-level or by row).
    let cases: [Case; 19] = [
        (
            "csv-truncated-row.csv",
            "text/csv",
            "rows_refused",
            Some((3, "unterminated_quote")),
        ),
        (
            "csv-wrong-encoding.csv",
            "text/csv",
            "rows_refused",
            Some((2, "invalid_utf8")),
        ),
        (
            "csv-ambiguous-date.csv",
            "text/csv",
            "rows_refused",
            Some((2, "invalid_date")),
        ),
        (
            "csv-twenty-digit-amount.csv",
            "text/csv",
            "rows_refused",
            Some((2, "amount_out_of_range")),
        ),
        (
            "csv-formula-cell.csv",
            "text/csv",
            "rows_refused",
            Some((2, "formula_cell")),
        ),
        ("csv-utf32.csv", "text/csv", "unsupported_encoding", None),
        (
            "csv-bare-cr.csv",
            "text/csv",
            "rows_refused",
            Some((2, "bare_carriage_return")),
        ),
        (
            "csv-balance-discontinuity.csv",
            "text/csv",
            "rows_refused",
            Some((3, "balance_discontinuity")),
        ),
        (
            "csv-nul-byte.csv",
            "text/csv",
            "rows_refused",
            Some((2, "forbidden_character")),
        ),
        (
            "csv-bidi-override.csv",
            "text/csv",
            "rows_refused",
            Some((2, "forbidden_character")),
        ),
        (
            "csv-header-mismatch.csv",
            "text/csv",
            "header_mismatch",
            Some((1, "header_mismatch")),
        ),
        (
            "ofx-billion-laughs.ofx",
            "application/x-ofx",
            "declaration_refused",
            None,
        ),
        (
            "ofx-external-entity.ofx",
            "application/x-ofx",
            "declaration_refused",
            None,
        ),
        (
            "ofx-doctype.ofx",
            "application/x-ofx",
            "declaration_refused",
            None,
        ),
        (
            "ofx-comment.ofx",
            "application/x-ofx",
            "declaration_refused",
            None,
        ),
        (
            "ofx-unknown-entity.ofx",
            "application/x-ofx",
            "entity_refused",
            None,
        ),
        (
            "ofx-deep-nesting.ofx",
            "application/x-ofx",
            "malformed_ofx",
            None,
        ),
        (
            "ofx-two-statements.ofx",
            "application/x-ofx",
            "multiple_statements",
            None,
        ),
        (
            "ofx-truncated.ofx",
            "application/x-ofx",
            "malformed_ofx",
            None,
        ),
    ];
    for (name, content_type, code, first) in cases {
        let url = if content_type == "text/csv" {
            &csv_url
        } else {
            &ofx_url
        };
        let (status, body, _) = send(url, content_type, pieces(&fixture(name))).await;
        assert_eq!(status, Some(422), "{name}: {body}");
        let import = &body["error"]["import"];
        assert_eq!(import["status"], "refused", "{name}");
        assert_eq!(import["refusal_code"], code, "{name}: {body}");
        if let Some((row, row_code)) = first {
            assert_eq!(import["refusals"][0]["row"], row, "{name}: {body}");
            assert_eq!(import["refusals"][0]["code"], row_code, "{name}: {body}");
        }
    }

    // The large ones, with the server's memory sampled throughout. Where `ps`
    // cannot read the server's RSS (a sandboxed runner), the growth bound is
    // unmeasurable and only the refusal contract is asserted.
    let baseline = rss(pid);
    let peak = Arc::new(AtomicU64::new(baseline.unwrap_or(0)));
    let sampler = sample(pid, peak.clone());
    let mut timings = Vec::new();

    // A 100 MiB line: the 50 MiB bound refuses it while it streams.
    let line: Vec<Vec<u8>> = std::iter::once(HEAD.to_vec())
        .chain(std::iter::repeat_n(vec![b'a'; 64 * 1024], 1_600))
        .collect();
    let (status, _, took) = send(&csv_url, "text/csv", line).await;
    assert!(
        matches!(status, Some(413) | None),
        "a 100 MiB line answered {status:?}"
    );
    timings.push(("100 MiB line", took));

    // 10^6 columns in one record.
    let mut columns = HEAD.to_vec();
    columns.extend(b"a,".repeat(1_000_000));
    columns.push(b'\n');
    let (status, body, took) = send(&csv_url, "text/csv", pieces(&columns)).await;
    assert_eq!(status, Some(422), "{body}");
    assert_eq!(
        body["error"]["import"]["refusals"][0]["code"],
        "too_many_fields"
    );
    timings.push(("10^6 columns", took));

    // 50 MiB of rows that are all refused: counted, the first 1,000 reported.
    let bad = b"2026-03-01,2026-03-01,1e3,USD,HOSTILE ROW WITH A LONG NARRATION,REF,CP,\n";
    let per_chunk = (64 * 1024) / bad.len();
    let rows: Vec<Vec<u8>> = std::iter::once(HEAD.to_vec())
        .chain(std::iter::repeat_n(bad.repeat(per_chunk), 800))
        .collect();
    let (status, body, took) = send(&csv_url, "text/csv", rows).await;
    assert_eq!(status, Some(422), "{body}");
    assert_eq!(
        body["error"]["import"]["rows_refused"],
        u64::try_from(per_chunk * 800).unwrap()
    );
    assert_eq!(
        body["error"]["import"]["refusals"]
            .as_array()
            .unwrap()
            .len(),
        1_000
    );
    timings.push(("50 MiB of bad rows", took));

    // 50 MiB of OFX in one value, made of entity references.
    let head = b"OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nENCODING:USASCII\nCHARSET:1252\n\n<OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>USD<BANKTRANLIST><STMTTRN><NAME>".to_vec();
    let ofx: Vec<Vec<u8>> = std::iter::once(head)
        .chain(std::iter::repeat_n(b"&amp;".repeat(13_107), 790))
        .collect();
    let (status, body, took) = send(&ofx_url, "application/x-ofx", ofx).await;
    assert_eq!(status, Some(422), "{body}");
    timings.push(("50 MiB of OFX", took));

    let hostile_peak = peak.load(Ordering::Relaxed);

    // Nothing of any refused file is persisted.
    let (lines, staged, refused, other): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM recon.statement_lines),
                (SELECT count(*) FROM recon.import_staging),
                (SELECT count(*) FROM recon.imports WHERE status = 'refused'),
                (SELECT count(*) FROM recon.imports WHERE status NOT IN ('refused', 'discarded'))",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((lines, staged, other), (0, 0, 0));
    assert!(refused >= 22, "{refused} refused imports");

    // A valid 50 MiB file of wide rows (D-050): stored, in byte-bounded batches.
    let wide_account = account("Wide").await;
    let mut wide = vec![HEAD.to_vec()];
    let mut size = HEAD.len();
    let mut i = 0_u32;
    while size < 49 * 1024 * 1024 {
        let row = format!(
            "2026-04-01,,-{}.00,USD,WIDE {i} {},REF-{i},CP {i} {},\n",
            i + 1,
            "N".repeat(5_000),
            "C".repeat(2_500)
        )
        .into_bytes();
        size += row.len();
        wide.push(row);
        i += 1;
    }
    let peak_before_wide = peak.swap(rss(pid).unwrap_or(0), Ordering::Relaxed);
    let wide_baseline = rss(pid).unwrap_or(0);
    let (status, body, took) = send(
        &format!("{base}/v1/firms/{firm}/accounts/{wide_account}/imports"),
        "text/csv",
        wide,
    )
    .await;
    assert_eq!(status, Some(201), "{body}");
    assert_eq!(body["import"]["lines_created"], i);
    timings.push(("valid 50 MiB of wide rows", took));
    let wide_peak = peak.load(Ordering::Relaxed);
    sampler.abort();

    if let Some(baseline) = baseline {
        let growth = hostile_peak.max(peak_before_wide).saturating_sub(baseline);
        let wide_growth = wide_peak.saturating_sub(wide_baseline);
        eprintln!(
            "hostile drill: baseline RSS {} MiB; hostile peak growth {} MiB; wide 50 MiB file \
             growth {} MiB; times {:?}",
            baseline >> 20,
            growth >> 20,
            wide_growth >> 20,
            timings
        );
        assert!(
            growth < RSS_BOUND,
            "hostile inputs grew the server by {} MiB",
            growth >> 20
        );
        assert!(
            wide_growth < RSS_BOUND,
            "a valid 50 MiB wide file grew the server by {} MiB",
            wide_growth >> 20
        );
    } else {
        eprintln!(
            "hostile drill: RSS unmeasurable (ps unavailable); refusal contract only {timings:?}"
        );
    }

    let log = server.stop().await;
    assert_log_clean(
        &log,
        &[
            TOKEN,
            "OK ROW",
            "CAF",
            "WHICH DAY",
            "HYPERLINK",
            "HOSTILE ROW",
            "BREAKS",
            "WIDE 1 ",
            "ACME",
        ],
    );
    let firm_id = recon_domain::FirmId::from_uuid(firm.parse().unwrap());
    recon_db::replay::check(&db.pool, firm_id)
        .await
        .unwrap()
        .unwrap();
    db.cleanup().await;
}
