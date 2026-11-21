//! The pentest regression suite (the workspace's security practice): real attack
//! requests against a running instance, each asserting its named refusal. The
//! classes: auth bypass, cross-tenant access, header spoofing, rate-limit
//! evasion, oversized and slow bodies, replay, and SSRF — asserted as the
//! absence of any outbound-capable route.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "attack tests: a panic is the failure report"
)]

#[path = "../common/mod.rs"]
mod common;

mod auth_bypass;
mod bodies;
mod cross_tenant;
mod egress;
mod harness;
mod header_spoof;
mod rate_limit;
mod replay;
mod ssrf;
