//! Logs: structured, to stderr, carrying ids, codes, counts and durations only (D-025). No
//! handler logs a narration, a counterparty, an amount, a file name, a header or a token;
//! `tests/logs.rs` and every drill scan the captured output to prove it.

use tracing_subscriber::EnvFilter;

/// Installs the subscriber: `RECON_LOG` is the filter (default `info`), `RECON_LOG_FORMAT`
/// `json` or text.
pub fn init() {
    let filter = EnvFilter::try_from_env("RECON_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true);
    if std::env::var("RECON_LOG_FORMAT").is_ok_and(|f| f == "json") {
        builder.json().init();
    } else {
        builder.init();
    }
}
