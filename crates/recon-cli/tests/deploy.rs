//! The deploy files pin the keep-alive contract (D-078): the proxy's upstream
//! idle bound sits below the server's head bound so a pooled reuse never races
//! the idle close into a reset, and the load client's pool idle is below it too.

/// The server's default head bound (RECON_HEADER_READ_TIMEOUT_MS); a proxy or
/// client idling above it reuses a connection the server may have closed.
const SERVER_HEAD_BOUND_MS: u64 = 10_000;

fn deploy_file(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../deploy/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Parses `keepalive <dur>` durations (`5s`, `500ms`, `2m`) into milliseconds.
fn keepalive_ms(text: &str) -> Vec<u64> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let value = line.strip_prefix("keepalive")?.trim();
            let (num, unit) = value.split_at(
                value
                    .find(|c: char| c.is_ascii_alphabetic())
                    .unwrap_or(value.len()),
            );
            let n: u64 = num.trim().parse().ok()?;
            Some(match unit {
                "ms" => n,
                "s" => n * 1_000,
                "m" => n * 60_000,
                _ => return None,
            })
        })
        .collect()
}

#[test]
fn the_caddy_upstream_idle_bound_is_below_the_servers() {
    let caddyfile = deploy_file("Caddyfile");
    let transport = caddyfile
        .split("transport http")
        .nth(1)
        .expect("the reverse_proxy transport block is missing");
    let bounds = keepalive_ms(transport.split('}').next().unwrap_or(transport));
    assert_eq!(
        bounds.len(),
        1,
        "the upstream transport must name one idle keepalive bound"
    );
    assert!(
        bounds[0] < SERVER_HEAD_BOUND_MS,
        "the upstream idle bound {} ms must sit below the server's {} ms head bound",
        bounds[0],
        SERVER_HEAD_BOUND_MS
    );
}

#[test]
fn the_load_client_pool_idles_below_the_servers_bound() {
    let loadgen = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../perf/loadgen/src/main.rs"
    ))
    .unwrap();
    let setting = loadgen
        .lines()
        .find(|line| line.contains("pool_idle_timeout"))
        .expect("the load client must bound its pooled idle below the server's");
    let millis = if let Some(n) = setting
        .split("from_secs(")
        .nth(1)
        .and_then(|s| s.split(')').next())
    {
        n.trim_end_matches('u').parse::<u64>().unwrap() * 1_000
    } else {
        setting
            .split("from_millis(")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .and_then(|s| s.trim_end_matches('u').parse::<u64>().ok())
            .expect("pool_idle_timeout must name a Duration")
    };
    assert!(
        millis < SERVER_HEAD_BOUND_MS,
        "the pool idle {millis} ms must sit below the server's {SERVER_HEAD_BOUND_MS} ms head bound: {setting}"
    );
}
