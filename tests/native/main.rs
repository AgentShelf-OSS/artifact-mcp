//! Declaration-only aggregator for the `native` integration-test target.
//!
//! Cargo only auto-discovers `tests/*.rs` and `tests/<dir>/main.rs` — it does NOT recurse — so a
//! bare `tests/native/uNN_*.rs` would be silently neither compiled nor run. This file is the single
//! target that pulls in every unit's modules, and it also lets units share helpers (e.g.
//! `u03_support`) that separate top-level test binaries could not.
//!
//! OWNERSHIP (contract delta, resolved by the integrator at the M1 merge): this file is
//! **integrator-owned and append-only**. Each unit adds exactly one `mod uNN_…;` line per file it
//! owns and edits nothing else here. Concurrent legs each produce a one-line add/add conflict; the
//! integrator resolves it by taking the union. Keep the list alphabetical.

mod u02_config;
mod u03_bootstrap;
mod u03_cross_runtime;
mod u03_migrations;
mod u03_support;
mod u04_crypto;
mod u05_access_retry;
mod u05_auth;
mod u05_identity;
mod u05_jwks;
mod u05_node_parity;
mod u05_support;
mod u06_access;
mod u06_node_parity;
mod u07_node_parity;
mod u07_paths;
mod u08_failpoints;
mod u08_lifecycle;
mod u08_node_parity;
mod u08_reconciliation;
mod u08_support;
mod u09_keys;
mod u09_node_parity;
mod u09_orgs;
mod u09_support;
mod u10_engagement;
mod u10_node_parity;
mod u10_support;
mod u11_feedback;
mod u11_node_parity;
mod u11_shares;
mod u11_support;
mod u12_node_parity;
mod u12_notify;
mod u12_support;
mod u12_webhooks;
mod u13_node_parity;
mod u14_node_parity;
mod u14_response;
mod u15_assets;
mod u15_render;
mod u16_node_parity;
mod u16_preview;
mod u16_support;
mod u16_thumbnails;
mod u17_routes;
mod u18_admin_routes;
mod u19_routes;
mod u20_runtime;
mod u21_http_conformance;
mod u22_mcp_read;
mod u23_patch_artifact;
mod u23_patch_node_parity;
mod u24_org_deletion;
mod u24_security_wave2;
mod u25_api_key_capabilities;
mod u26_mcp_2026;
mod u27_oauth;
mod u28_csrf;
mod u29_preview_notifier;
mod u50_historical_fixtures;
mod u56_discord_delivery;
mod u56_envelope_fanout;
mod u56_feedback_cutover;
mod u56_lifecycle_crash_recovery;
mod u56_lifecycle_cutover;
mod u56_outbox;
mod u58_admin_audit;
mod u79_discussion_app;
mod u79_discussion_persistence;
mod u79_outbox_bridge;
mod u80_discord_inbound;
mod u81_organization_discord;

/// Fails loudly if a file in `tests/native/` is not declared above.
///
/// Cargo does not recurse into `tests/native/`, so an undeclared module is silently never compiled
/// and never run — a green suite that proves less than it appears to. This guard converts that
/// class of false-positive test result from silent to loud at zero runtime cost. The inverse — a
/// `mod` line for a deleted file — is caught by the compiler.
#[test]
fn every_test_file_is_registered() {
    let src = include_str!("main.rs");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/native");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read tests/native") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .into_string()
            .expect("utf8 filename");
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        if stem == "main" {
            continue;
        }
        if !src.contains(&format!("\nmod {stem};")) {
            missing.push(stem.to_owned());
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "unregistered test modules (never compiled, never run): {missing:?}"
    );
}
