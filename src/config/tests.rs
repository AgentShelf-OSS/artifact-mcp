//! Unit tests for the configuration module.

use std::collections::BTreeSet;

use crate::model::Timestamp;

use super::{
    ARTIFACT_ID_LENGTH, AppConfig, Clock, FixedClock, IdSource, MapEnv, NanoIdSource,
    RESERVED_ARTIFACT_IDS, Secret, SeededRandom, SequentialIdSource, format_sqlite_datetime,
    is_valid_artifact_id, is_valid_webhook_id, mcp_json_limit_for, parse::parse_byte_size,
};

#[test]
fn empty_environment_matches_declared_defaults() {
    let parsed = AppConfig::from_source(&MapEnv::empty()).expect("defaults parse");
    assert_eq!(parsed, AppConfig::defaults());
    assert_eq!(parsed, AppConfig::default());
}

#[test]
fn mcp_json_limit_follows_bundle_bytes() {
    assert_eq!(mcp_json_limit_for(8 * 1024 * 1024), 50_593_792);
    let parsed =
        AppConfig::from_source(&MapEnv::empty().with("MAX_BUNDLE_BYTES", "1024")).expect("parse");
    assert_eq!(parsed.body.mcp_json, 1024 * 6 + 256 * 1024);
}

#[test]
fn byte_sizes_match_the_bytes_package() {
    assert_eq!(parse_byte_size("X", "1mb").expect("1mb"), 1_048_576);
    assert_eq!(parse_byte_size("X", "8MB").expect("8MB"), 8_388_608);
    assert_eq!(parse_byte_size("X", "64kb").expect("64kb"), 65_536);
    assert_eq!(parse_byte_size("X", "1.5kb").expect("1.5kb"), 1536);
    assert_eq!(parse_byte_size("X", "1048576").expect("bytes"), 1_048_576);
    assert!(parse_byte_size("X", "banana").is_err());
    assert!(parse_byte_size("X", "-1mb").is_err());
}

#[test]
fn generated_ids_satisfy_the_real_validators() {
    let ids = NanoIdSource::new(SeededRandom::new(7));
    for _ in 0..256 {
        let artifact = ids.artifact_id().expect("artifact id");
        assert!(is_valid_artifact_id(&artifact.0), "{artifact}");
        assert_eq!(artifact.0.len(), ARTIFACT_ID_LENGTH);
        assert!(!RESERVED_ARTIFACT_IDS.contains(&artifact.0.as_str()));
        let webhook = ids.webhook_id().expect("webhook id");
        assert!(is_valid_webhook_id(&webhook.0), "{webhook}");
    }
}

#[test]
fn sequential_ids_are_unique_and_in_alphabet() {
    let ids = SequentialIdSource::default();
    let mut seen = BTreeSet::new();
    for _ in 0..1000 {
        let id = ids.artifact_id().expect("artifact id");
        assert!(is_valid_artifact_id(&id.0), "{id}");
        assert!(seen.insert(id.0));
    }
}

#[test]
fn sqlite_datetime_rendering_is_stable() {
    assert_eq!(format_sqlite_datetime(0), "1970-01-01 00:00:00");
    assert_eq!(format_sqlite_datetime(1_767_225_600), "2026-01-01 00:00:00");
    assert_eq!(
        FixedClock::default().now_timestamp(),
        Timestamp("2026-01-01 00:00:00".to_owned())
    );
}

#[test]
fn secrets_never_render_their_value() {
    let secret = Secret::new("super-secret");
    assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    assert_eq!(format!("{secret}"), "<redacted>");
    assert_eq!(secret.expose(), "super-secret");
}
