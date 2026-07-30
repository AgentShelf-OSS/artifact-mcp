//! U02 native tests — environment defaults, fail-closed parsing, typed limits, and the
//! identifier alphabets/lengths the Node reference generates.
//!
//! The process environment is never mutated: every case builds a [`MapEnv`], which keeps
//! the suite deterministic under cargo's parallel test threads and avoids the `unsafe`
//! `std::env::set_var` required by edition 2024.

use std::{collections::BTreeSet, path::PathBuf};

use artifact_mcp::{config::Clock, model::Timestamp};
use artifact_mcp::{
    config::{
        ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH, AccessIdentityMode, AppConfig,
        DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS, DEFAULT_APP_BRAND, DEFAULT_APP_NAME,
        DEFAULT_CATEGORY_JSON_LIMIT, DEFAULT_DATA_DIR, DEFAULT_FEEDBACK_JSON_LIMIT,
        DEFAULT_KEY_JSON_LIMIT, DEFAULT_LISTEN_HOST, DEFAULT_MCP_JSON_FALLBACK_LIMIT, DEFAULT_PORT,
        DEFAULT_PUBLIC_BASE_URL, DEFAULT_REACTION_JSON_LIMIT, FEEDBACK_ID_ALPHABET,
        FEEDBACK_ID_LENGTH, FixedClock, IdSource, MapEnv, NanoIdSource, OsRandom, RandomSource,
        SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH, ScriptedRandom, SeededRandom, SequentialIdSource,
        WEBHOOK_ID_ALPHABET, WEBHOOK_ID_LENGTH, generate_id, is_valid_artifact_id,
        mcp_json_limit_for,
    },
    error::AppError,
};

/// A canonical 32-byte base64 key (32 `A` bytes) accepted by `parseEncryptionKey`.
const VALID_WEBHOOK_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn parse(pairs: &[(&str, &str)]) -> Result<AppConfig, AppError> {
    let env: MapEnv = pairs.iter().copied().collect();
    AppConfig::from_source(&env)
}

fn parse_ok(pairs: &[(&str, &str)]) -> AppConfig {
    parse(pairs).expect("configuration should parse")
}

// ---------------------------------------------------------------------------
// Environment defaults
// ---------------------------------------------------------------------------

#[test]
fn empty_environment_reproduces_every_node_default() {
    let config = parse_ok(&[]);

    // server.js:27, server.js:190, server.js:28, lib/db.js:8
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.port, 3480);
    assert_eq!(config.listen_host, DEFAULT_LISTEN_HOST);
    assert_eq!(config.listen_host, "0.0.0.0");
    assert_eq!(config.public_base_url, DEFAULT_PUBLIC_BASE_URL);
    assert_eq!(config.public_base_url, "http://localhost:3480");
    assert_eq!(config.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
    assert_eq!(config.data_dir, PathBuf::from("/data"));

    // lib/config.js:19-26
    assert_eq!(config.app_name, DEFAULT_APP_NAME);
    assert_eq!(config.app_name, "Artifact Index");
    assert_eq!(config.app_brand, DEFAULT_APP_BRAND);
    assert_eq!(config.app_brand, "A");
    assert_eq!(config.storage.feedback_max_body, 4000);
    assert_eq!(config.storage.max_history, 20);
    assert_eq!(config.storage.max_artifact_bytes, 2 * 1024 * 1024);
    assert_eq!(config.storage.max_bundle_bytes, 8 * 1024 * 1024);
    assert_eq!(config.storage.max_bundle_files, 100);

    // lib/identity.js:14-38
    assert!(config.access.team_domain.is_empty());
    assert!(config.access.aud.is_empty());
    assert!(!config.access.trust_headers);
    assert!(!config.access.require_jwt);
    assert!(!config.access.header_trust_allow_insecure);
    assert_eq!(
        config.access.clock_tolerance_seconds,
        DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS
    );
    assert_eq!(config.access.clock_tolerance_seconds, 60);
    assert!(config.access.domain_orgs.is_empty());
    assert!(config.access.admin_emails.is_empty());
    assert!(config.access.admin_email_domains.is_empty());

    // lib/preview.js:6-8, lib/thumbnails.js:17
    assert!(!config.preview.enabled());
    assert_eq!(config.preview.renderer_endpoint, None);
    assert_eq!(config.preview.timeout_ms, 8000);
    assert_eq!(config.preview.viewport_width, 1200);
    assert_eq!(config.preview.viewport_height, 630);
    assert_eq!(config.preview.cache_entries, 32);
    assert_eq!(config.preview.max_png_bytes, 7_500_000);

    // lib/crypto.js:9, lib/db.js:30
    assert!(config.webhook_enc_key.is_none());
    assert!(config.discord_bot_token.is_none());
    assert!(config.seed_keys.entries.is_empty());
    assert!(config.seed_keys.ignored_placeholders.is_empty());

    // `AppConfig::default()` must agree so route fixtures need no environment at all.
    assert_eq!(config, AppConfig::default());
}

#[test]
fn empty_and_whitespace_values_fall_back_exactly_like_node() {
    // Node: `Number("")` is 0 (not > 0) and `"" || fallback` selects the fallback, so an
    // exported-but-blank variable is indistinguishable from an unset one.
    let config = parse_ok(&[
        ("PORT", "   "),
        ("APP_NAME", ""),
        ("APP_BRAND", "  "),
        ("MAX_HISTORY", ""),
        ("MAX_BUNDLE_BYTES", "  "),
        ("MCP_JSON_LIMIT", ""),
        ("PUBLIC_BASE_URL", ""),
        ("DATA_DIR", ""),
        ("LISTEN_HOST", "  "),
        ("ACCESS_CLOCK_TOLERANCE_S", ""),
        ("PREVIEW_VIEWPORT", "   "),
        ("WEBHOOK_ENC_KEY", "  "),
        ("ARTIFACT_API_KEYS", "   "),
    ]);
    assert_eq!(config, AppConfig::default());
}

#[test]
fn typed_overrides_are_parsed_as_numbers_and_paths() {
    let config = parse_ok(&[
        ("PORT", "8080"),
        ("LISTEN_HOST", " 127.0.0.1 "),
        ("PUBLIC_BASE_URL", "https://artifacts.example.com"),
        ("DATA_DIR", "/srv/artifacts"),
        ("APP_NAME", "  Cairn  "),
        ("APP_BRAND", " C "),
        ("FEEDBACK_MAX_BODY", "512"),
        ("MAX_HISTORY", "3"),
        ("MAX_ARTIFACT_BYTES", "1024"),
        ("MAX_BUNDLE_BYTES", "4096"),
        ("MAX_BUNDLE_FILES", "7"),
    ]);

    assert_eq!(config.port, 8080);
    assert_eq!(config.listen_host, "127.0.0.1");
    assert_eq!(config.public_base_url, "https://artifacts.example.com");
    assert_eq!(config.data_dir, PathBuf::from("/srv/artifacts"));
    assert_eq!(config.app_name, "Cairn");
    assert_eq!(config.app_brand, "C");
    assert_eq!(config.storage.feedback_max_body, 512);
    assert_eq!(config.storage.max_history, 3);
    assert_eq!(config.storage.max_artifact_bytes, 1024);
    assert_eq!(config.storage.max_bundle_bytes, 4096);
    assert_eq!(config.storage.max_bundle_files, 7);
}

#[test]
fn ingress_controls_parse_as_positive_bounded_origin_limits() {
    let config = parse_ok(&[
        ("INGRESS_MAX_HEADERS", "48"),
        ("INGRESS_MAX_HEADER_BYTES", "16384"),
        ("INGRESS_MAX_CONNECTIONS", "12"),
        ("INGRESS_MCP_PER_WINDOW", "45"),
        ("INGRESS_UPLOADS_PER_WINDOW", "6"),
        ("INGRESS_FEEDBACK_PER_WINDOW", "7"),
        ("INGRESS_ADMIN_PER_WINDOW", "8"),
        ("TRUSTED_PROXY_CIDRS", " 127.0.0.1/32, 2001:db8::/32 "),
    ]);
    assert_eq!(config.ingress.max_headers, 48);
    assert_eq!(config.ingress.max_header_bytes, 16_384);
    assert_eq!(config.ingress.max_connections, 12);
    assert_eq!(config.ingress.mcp_per_window, 45);
    assert_eq!(config.ingress.uploads_per_window, 6);
    assert_eq!(config.ingress.feedback_per_window, 7);
    assert_eq!(config.ingress.admin_per_window, 8);
    assert_eq!(config.ingress.trusted_proxy_cidrs.len(), 2);

    assert!(parse(&[("INGRESS_MAX_HEADER_BYTES", "8191")]).is_err());
    assert!(parse(&[("TRUSTED_PROXY_CIDRS", "not-a-cidr")]).is_err());
}

#[test]
fn derived_paths_follow_the_data_directory() {
    let config = parse_ok(&[("DATA_DIR", "/srv/artifacts")]);
    // lib/db.js:9-10, lib/thumbnails.js:70
    assert_eq!(
        config.artifact_dir(),
        PathBuf::from("/srv/artifacts/artifacts")
    );
    assert_eq!(
        config.database_path(),
        PathBuf::from("/srv/artifacts/artifacts.db")
    );
    assert_eq!(
        config.preview_dir(),
        PathBuf::from("/srv/artifacts/previews")
    );
}

#[test]
fn artifact_urls_use_the_public_base_without_doubling_slashes() {
    let id = artifact_mcp::model::ArtifactId::from("abc123def456");
    assert_eq!(
        parse_ok(&[]).artifact_url(&id),
        "http://localhost:3480/abc123def456"
    );
    assert_eq!(
        parse_ok(&[("PUBLIC_BASE_URL", "https://example.com/")]).artifact_url(&id),
        "https://example.com/abc123def456"
    );
}

// ---------------------------------------------------------------------------
// Fail-closed parsing
// ---------------------------------------------------------------------------

#[test]
fn present_but_invalid_values_fail_closed_with_validation_errors() {
    let cases: [(&str, &str); 14] = [
        ("PORT", "not-a-port"),
        ("PORT", "0"),
        ("PORT", "70000"),
        ("PORT", "-1"),
        ("MAX_ARTIFACT_BYTES", "0"),
        ("MAX_ARTIFACT_BYTES", "-5"),
        ("MAX_BUNDLE_FILES", "1.5"),
        ("MAX_HISTORY", "lots"),
        ("MCP_JSON_LIMIT", "banana"),
        ("MCP_JSON_LIMIT", "0mb"),
        ("PREVIEW_VIEWPORT", "1200"),
        ("PREVIEW_RENDERER_URL", "ftp://renderer.internal"),
        ("PUBLIC_BASE_URL", "not-a-url"),
        ("ACCESS_CLOCK_TOLERANCE_S", "soon"),
    ];

    for (key, value) in cases {
        let error = parse(&[(key, value)]).expect_err(&format!(
            "{key}={value} must be rejected, not silently defaulted"
        ));
        assert!(
            matches!(error, AppError::Validation(_)),
            "{key}={value} produced {error:?}, expected AppError::Validation"
        );
        assert!(
            error.to_string().contains(key),
            "{key}={value} error message must name the variable: {error}"
        );
    }
}

#[test]
fn invalid_configuration_never_panics_and_maps_to_http_400() {
    let error = parse(&[("PORT", "0")]).expect_err("PORT=0 rejected");
    assert_eq!(error.http_status(), axum::http::StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn body_limits_match_every_express_json_call() {
    let config = parse_ok(&[]);

    // lib/app.js:261,302,322,341,362,373,380,392,409 — "64kb"
    assert_eq!(config.body.key_json, DEFAULT_KEY_JSON_LIMIT);
    assert_eq!(config.body.key_json, 65_536);
    // lib/app.js:545 — "8kb"
    assert_eq!(config.body.reaction_json, DEFAULT_REACTION_JSON_LIMIT);
    assert_eq!(config.body.reaction_json, 8_192);
    // lib/app.js:563 — "16kb"
    assert_eq!(config.body.feedback_json, DEFAULT_FEEDBACK_JSON_LIMIT);
    assert_eq!(config.body.feedback_json, 16_384);
    // lib/app.js:634,643,669,679,708 — "8kb"
    assert_eq!(config.body.category_json, DEFAULT_CATEGORY_JSON_LIMIT);
    assert_eq!(config.body.category_json, 8_192);
    // lib/app.js:152 — the "8mb" fallback express uses when no limit is supplied
    assert_eq!(DEFAULT_MCP_JSON_FALLBACK_LIMIT, 8 * 1024 * 1024);
}

#[test]
fn mcp_json_limit_defaults_to_the_bundle_derived_envelope() {
    // lib/config.js:13-17,27 and server.js:186
    assert_eq!(mcp_json_limit_for(8 * 1024 * 1024), 50_593_792);
    assert_eq!(parse_ok(&[]).body.mcp_json, 50_593_792);
    assert_eq!(
        parse_ok(&[("MAX_BUNDLE_BYTES", "4194304")]).body.mcp_json,
        4_194_304 * 6 + 256 * 1024
    );
}

#[test]
fn mcp_json_limit_accepts_the_byte_size_strings_express_understands() {
    // conformance/cases/mcp.oversized-413.json pins MCP_JSON_LIMIT="1mb".
    assert_eq!(
        parse_ok(&[("MCP_JSON_LIMIT", "1mb")]).body.mcp_json,
        1_048_576
    );
    assert_eq!(
        parse_ok(&[("MCP_JSON_LIMIT", "1MB")]).body.mcp_json,
        1_048_576
    );
    assert_eq!(
        parse_ok(&[("MCP_JSON_LIMIT", "64kb")]).body.mcp_json,
        65_536
    );
    assert_eq!(
        parse_ok(&[("MCP_JSON_LIMIT", "1.5kb")]).body.mcp_json,
        1_536
    );
    assert_eq!(parse_ok(&[("MCP_JSON_LIMIT", "512b")]).body.mcp_json, 512);
    assert_eq!(
        parse_ok(&[("MCP_JSON_LIMIT", "1048576")]).body.mcp_json,
        1_048_576
    );
    // An explicit MCP_JSON_LIMIT wins over the bundle-derived default.
    assert_eq!(
        parse_ok(&[("MAX_BUNDLE_BYTES", "4194304"), ("MCP_JSON_LIMIT", "1mb")])
            .body
            .mcp_json,
        1_048_576
    );
}

#[test]
fn preview_configuration_normalizes_the_renderer_endpoint() {
    // lib/preview.js:25-36 appends `render` to the configured base.
    let config = parse_ok(&[
        ("PREVIEW_RENDERER_URL", "http://preview-renderer:3000"),
        ("PREVIEW_RENDER_TIMEOUT_MS", "2500"),
        ("PREVIEW_VIEWPORT", "800x600"),
        ("PREVIEW_MAX_PNG_BYTES", "1000"),
    ]);
    assert!(config.preview.enabled());
    assert_eq!(
        config.preview.renderer_endpoint.as_deref(),
        Some("http://preview-renderer:3000/render")
    );
    assert_eq!(config.preview.timeout_ms, 2500);
    assert_eq!(config.preview.viewport_width, 800);
    assert_eq!(config.preview.viewport_height, 600);
    assert_eq!(config.preview.max_png_bytes, 1000);

    // A trailing slash must not produce a doubled path segment.
    assert_eq!(
        parse_ok(&[("PREVIEW_RENDERER_URL", "https://render.example.com/api/")])
            .preview
            .renderer_endpoint
            .as_deref(),
        Some("https://render.example.com/api/render")
    );
}

// ---------------------------------------------------------------------------
// Secrets and seeded keys
// ---------------------------------------------------------------------------

#[test]
fn webhook_encryption_key_requires_canonical_32_byte_base64() {
    // lib/crypto.js:9-18
    let config = parse_ok(&[("WEBHOOK_ENC_KEY", VALID_WEBHOOK_KEY)]);
    let key = config.webhook_enc_key.as_ref().expect("key parsed");
    assert_eq!(key.expose(), VALID_WEBHOOK_KEY);

    for bad in [
        "not-base64!!",
        "QUJD",                                          // 3 bytes, not 32
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",   // unpadded, non-canonical
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", // 33 bytes
    ] {
        let error =
            parse(&[("WEBHOOK_ENC_KEY", bad)]).expect_err("non-canonical key must be rejected");
        assert_eq!(
            error,
            AppError::Validation("WEBHOOK_ENC_KEY must be a 32-byte base64 value.".to_owned()),
            "unexpected message for {bad}"
        );
    }
}

#[test]
fn discord_bot_token_is_optional_bounded_and_redacted() {
    let config = parse_ok(&[("DISCORD_BOT_TOKEN", "bot.token_value-123")]);
    assert_eq!(
        config
            .discord_bot_token
            .as_ref()
            .expect("bot token parsed")
            .expose(),
        "bot.token_value-123"
    );
    for bad in ["\"", "contains space", "x\\y"] {
        assert!(matches!(
            parse(&[("DISCORD_BOT_TOKEN", bad)]),
            Err(AppError::Validation(_))
        ));
    }
    assert!(matches!(
        parse(&[("DISCORD_BOT_TOKEN", &"x".repeat(513))]),
        Err(AppError::Validation(_))
    ));
}

#[test]
fn discord_inbound_gateway_is_an_explicit_strict_kill_switch() {
    assert!(!parse_ok(&[]).discord_inbound_enabled);
    assert!(!parse_ok(&[("DISCORD_INBOUND_ENABLED", "0")]).discord_inbound_enabled);
    assert!(parse_ok(&[("DISCORD_INBOUND_ENABLED", "1")]).discord_inbound_enabled);
    for bad in ["true", "yes", "2"] {
        assert!(matches!(
            parse(&[("DISCORD_INBOUND_ENABLED", bad)]),
            Err(AppError::Validation(_))
        ));
    }
}

#[test]
fn secrets_are_redacted_in_debug_output() {
    let config = parse_ok(&[
        ("WEBHOOK_ENC_KEY", VALID_WEBHOOK_KEY),
        ("DISCORD_BOT_TOKEN", "navi.local.secret"),
        ("ARTIFACT_API_KEYS", "agent:acme:hunter2-super-secret"),
    ]);
    let rendered = format!("{config:?}");
    assert!(!rendered.contains(VALID_WEBHOOK_KEY), "{rendered}");
    assert!(!rendered.contains("navi.local.secret"), "{rendered}");
    assert!(!rendered.contains("hunter2-super-secret"), "{rendered}");
    assert!(rendered.contains("Secret(<redacted>)"), "{rendered}");
}

#[test]
fn artifact_api_keys_parse_exactly_like_seed_keys_from_env() {
    // lib/db.js:30-58
    let config = parse_ok(&[(
        "ARTIFACT_API_KEYS",
        " agent-a:acme:s3cret , agent-b:legacy-secret , agent-c:beta:with:colons ,\
         broken , :acme:no-client , agent-d:acme: , agent-e:acme:CHANGE_ME ,\
         agent-f:acme:REPLACE_WITH_LONG_RANDOM_SECRET",
    )]);

    let entries = &config.seed_keys.entries;
    assert_eq!(entries.len(), 3, "{entries:?}");

    assert_eq!(entries[0].client_id.0, "agent-a");
    assert_eq!(entries[0].org.0, "acme");
    assert_eq!(entries[0].secret.expose(), "s3cret");

    // A two-part entry maps to org `default`. [lib/db.js:46]
    assert_eq!(entries[1].client_id.0, "agent-b");
    assert_eq!(entries[1].org.0, "default");
    assert_eq!(entries[1].secret.expose(), "legacy-secret");

    // Secrets containing colons are rejoined. [lib/db.js:44]
    assert_eq!(entries[2].client_id.0, "agent-c");
    assert_eq!(entries[2].org.0, "beta");
    assert_eq!(entries[2].secret.expose(), "with:colons");

    // Documented placeholders are recorded but never seeded. [lib/db.js:38,51-54]
    let ignored: Vec<&str> = config
        .seed_keys
        .ignored_placeholders
        .iter()
        .map(|id| id.0.as_str())
        .collect();
    assert_eq!(ignored, ["agent-e", "agent-f"]);
}

// ---------------------------------------------------------------------------
// Cloudflare Access
// ---------------------------------------------------------------------------

#[test]
fn access_identity_mode_matches_the_node_matrix() {
    // lib/identity.js:50-55
    assert_eq!(
        parse_ok(&[]).access.identity_mode(),
        AccessIdentityMode::Disabled
    );
    assert_eq!(
        parse_ok(&[("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", "127.0.0.1")])
            .access
            .identity_mode(),
        AccessIdentityMode::HeaderTrust
    );
    assert_eq!(
        parse_ok(&[
            ("CF_ACCESS_TEAM_DOMAIN", "team.cloudflareaccess.com"),
            ("CF_ACCESS_AUD", "aud-tag"),
        ])
        .access
        .identity_mode(),
        AccessIdentityMode::Jwt
    );
    // Only the exact string "1" enables header trust. [lib/identity.js:16]
    assert!(
        !parse_ok(&[("TRUST_ACCESS_HEADERS", "true")])
            .access
            .trust_headers
    );
    assert!(
        !parse_ok(&[("TRUST_ACCESS_HEADERS", "yes")])
            .access
            .trust_headers
    );
    // A half-configured Access application never reaches JWT mode. [lib/identity.js:50]
    assert_eq!(
        parse_ok(&[("CF_ACCESS_TEAM_DOMAIN", "team.cloudflareaccess.com")])
            .access
            .identity_mode(),
        AccessIdentityMode::Disabled
    );
    assert_eq!(AccessIdentityMode::Jwt.as_str(), "jwt");
    assert_eq!(AccessIdentityMode::HeaderTrust.as_str(), "header-trust");
    assert_eq!(AccessIdentityMode::Disabled.as_str(), "disabled");
}

#[test]
fn jwks_url_is_derived_from_the_team_domain() {
    // lib/identity.js:43
    assert_eq!(parse_ok(&[]).access.jwks_url(), None);
    assert_eq!(
        parse_ok(&[("CF_ACCESS_TEAM_DOMAIN", "team.cloudflareaccess.com")])
            .access
            .jwks_url()
            .as_deref(),
        Some("https://team.cloudflareaccess.com/cdn-cgi/access/certs")
    );
}

#[test]
fn startup_validation_reproduces_assert_ready() {
    // lib/identity.js:57-61
    let error = parse_ok(&[("REQUIRE_ACCESS_JWT", "1")])
        .validate_startup()
        .expect_err("REQUIRE_ACCESS_JWT without CF_ACCESS_* must refuse to start");
    assert_eq!(
        error,
        AppError::Validation(
            "REQUIRE_ACCESS_JWT=1 requires both CF_ACCESS_TEAM_DOMAIN and CF_ACCESS_AUD; refusing to start"
                .to_owned()
        )
    );

    // lib/identity.js:66-76 — header trust on a non-loopback bind is refused.
    let error = parse_ok(&[("TRUST_ACCESS_HEADERS", "1")])
        .validate_startup()
        .expect_err("header trust on 0.0.0.0 must refuse to start");
    let message = error.to_string();
    assert!(message.starts_with("TRUST_ACCESS_HEADERS=1 trusts a spoofable identity header"));
    assert!(message.contains("(0.0.0.0)"));
    assert!(message.contains("HEADER_TRUST_ALLOW_INSECURE=1"));

    // Loopback binds and the explicit opt-out are both accepted.
    for host in ["127.0.0.1", "::1", "localhost"] {
        parse_ok(&[("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", host)])
            .validate_startup()
            .unwrap_or_else(|error| panic!("{host} should be allowed: {error}"));
    }
    parse_ok(&[
        ("TRUST_ACCESS_HEADERS", "1"),
        ("HEADER_TRUST_ALLOW_INSECURE", "1"),
    ])
    .validate_startup()
    .expect("explicit opt-out is allowed");

    // Fully configured JWT mode and the default disabled mode both start.
    parse_ok(&[]).validate_startup().expect("defaults start");
    parse_ok(&[
        ("REQUIRE_ACCESS_JWT", "1"),
        ("CF_ACCESS_TEAM_DOMAIN", "team.cloudflareaccess.com"),
        ("CF_ACCESS_AUD", "aud-tag"),
    ])
    .validate_startup()
    .expect("configured JWT mode starts");
}

#[test]
fn org_and_admin_lists_are_normalized_like_node() {
    // lib/identity.js:25-39
    let config = parse_ok(&[
        (
            "ORG_EMAIL_DOMAINS",
            " Example.COM : acme , team.example.org:teamb , broken , :orphan , dup.com:first , dup.com:second ",
        ),
        ("ADMIN_EMAILS", " Root@Example.com , , ops@example.com "),
        ("ADMIN_EMAIL_DOMAINS", " Admin.Example.COM ,, "),
    ]);

    assert_eq!(
        config
            .access
            .domain_orgs
            .get("example.com")
            .map(String::as_str),
        Some("acme")
    );
    assert_eq!(
        config
            .access
            .domain_orgs
            .get("team.example.org")
            .map(String::as_str),
        Some("teamb")
    );
    // A later duplicate wins, matching `new Map(entries)`.
    assert_eq!(
        config.access.domain_orgs.get("dup.com").map(String::as_str),
        Some("second")
    );
    assert_eq!(
        config.access.domain_orgs.len(),
        3,
        "{:?}",
        config.access.domain_orgs
    );

    let admins: BTreeSet<&str> = config
        .access
        .admin_emails
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        admins,
        BTreeSet::from(["root@example.com", "ops@example.com"])
    );
    let domains: BTreeSet<&str> = config
        .access
        .admin_email_domains
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(domains, BTreeSet::from(["admin.example.com"]));
}

#[test]
fn access_clock_tolerance_treats_zero_as_the_default() {
    // `Number(x) || 60` — lib/identity.js:22
    assert_eq!(
        parse_ok(&[("ACCESS_CLOCK_TOLERANCE_S", "0")])
            .access
            .clock_tolerance_seconds,
        60
    );
    assert_eq!(
        parse_ok(&[("ACCESS_CLOCK_TOLERANCE_S", "120")])
            .access
            .clock_tolerance_seconds,
        120
    );
}

// ---------------------------------------------------------------------------
// Identifier shapes
// ---------------------------------------------------------------------------

/// Every generated id must be exactly `length` characters drawn from `alphabet`.
fn assert_shape(value: &str, alphabet: &str, length: usize, label: &str) {
    assert_eq!(value.chars().count(), length, "{label}: {value}");
    assert_eq!(value.len(), length, "{label} must stay ASCII: {value}");
    for symbol in value.chars() {
        assert!(
            alphabet.contains(symbol),
            "{label}: character {symbol:?} is outside the frozen alphabet ({value})"
        );
    }
}

#[test]
fn frozen_alphabets_match_the_node_nanoid_declarations() {
    // lib/store.js:30 — note the omitted `l` and `o`.
    assert_eq!(ARTIFACT_ID_ALPHABET, "0123456789abcdefghijkmnpqrstuvwxyz");
    assert_eq!(ARTIFACT_ID_ALPHABET.len(), 34);
    assert!(!ARTIFACT_ID_ALPHABET.contains('l'));
    assert!(!ARTIFACT_ID_ALPHABET.contains('o'));
    assert_eq!(ARTIFACT_ID_LENGTH, 12);

    // lib/shares.js:6 — nanoid's URL-safe alphabet.
    assert_eq!(
        SHARE_TOKEN_ALPHABET,
        "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_-"
    );
    assert_eq!(SHARE_TOKEN_ALPHABET.len(), 64);
    assert_eq!(SHARE_TOKEN_LENGTH, 24);

    // lib/feedback.js:10 — full lowercase alphanumerics.
    assert_eq!(FEEDBACK_ID_ALPHABET, "0123456789abcdefghijklmnopqrstuvwxyz");
    assert_eq!(FEEDBACK_ID_ALPHABET.len(), 36);
    assert_eq!(FEEDBACK_ID_LENGTH, 16);

    // lib/webhooks.js:11 — shares the artifact alphabet.
    assert_eq!(WEBHOOK_ID_ALPHABET, ARTIFACT_ID_ALPHABET);
    assert_eq!(WEBHOOK_ID_LENGTH, 12);
}

#[test]
fn frozen_alphabets_match_the_conformance_case_manifests() {
    // conformance/** is owned by U00; reading its declarations here keeps the Rust
    // generator honest against the same oracle the runner enforces.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conformance/cases");
    let declarations = [
        (
            "raw.single-html.delivery.json",
            ARTIFACT_ID_ALPHABET,
            ARTIFACT_ID_LENGTH,
        ),
        (
            "raw.bundle-delivery.json",
            ARTIFACT_ID_ALPHABET,
            ARTIFACT_ID_LENGTH,
        ),
        (
            "publisher.tenant-lock.json",
            ARTIFACT_ID_ALPHABET,
            ARTIFACT_ID_LENGTH,
        ),
        (
            "share.public-delivery.json",
            SHARE_TOKEN_ALPHABET,
            SHARE_TOKEN_LENGTH,
        ),
    ];

    let mut checked = 0_usize;
    for (file, alphabet, length) in declarations {
        let raw = std::fs::read_to_string(root.join(file))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        let case: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {file}: {error}"));
        let mut found = false;
        for assertion in find_symbol_assertions(&case) {
            let declared_alphabet = assertion
                .get("alphabet")
                .and_then(serde_json::Value::as_str)
                .expect("manifest declares an alphabet");
            let declared_length = assertion
                .get("length")
                .and_then(serde_json::Value::as_u64)
                .expect("manifest declares a length");
            assert_eq!(declared_alphabet, alphabet, "{file}");
            assert_eq!(
                usize::try_from(declared_length).expect("length fits"),
                length,
                "{file}"
            );
            found = true;
            checked += 1;
        }
        assert!(found, "{file} declares no symbol alphabet assertion");
    }
    assert!(checked >= 4, "expected at least four manifest declarations");
}

/// Collect every object that declares both `alphabet` and `length`, at any depth.
fn find_symbol_assertions(
    value: &serde_json::Value,
) -> Vec<&serde_json::Map<String, serde_json::Value>> {
    let mut found = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("alphabet") && map.contains_key("length") {
                found.push(map);
            }
            for nested in map.values() {
                found.extend(find_symbol_assertions(nested));
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                found.extend(find_symbol_assertions(nested));
            }
        }
        _ => {}
    }
    found
}

#[test]
fn generated_artifact_ids_satisfy_the_real_validators() {
    let ids = NanoIdSource::new(SeededRandom::new(0x5EED));
    for _ in 0..2000 {
        let id = ids.artifact_id().expect("artifact id");
        assert_shape(
            &id.0,
            ARTIFACT_ID_ALPHABET,
            ARTIFACT_ID_LENGTH,
            "artifact id",
        );
        // lib/thumbnails.js:19 — /^[0-9a-z]{6,24}$/ gates every thumbnail path.
        assert!(
            is_valid_artifact_id(&id.0),
            "thumbnail validator rejected {id}"
        );
    }
}

#[test]
fn generated_share_tokens_feedback_ids_and_webhook_ids_match_their_alphabets() {
    let ids = NanoIdSource::new(SeededRandom::new(11));
    for _ in 0..1000 {
        assert_shape(
            &ids.share_token().expect("share token").0,
            SHARE_TOKEN_ALPHABET,
            SHARE_TOKEN_LENGTH,
            "share token",
        );
        assert_shape(
            &ids.feedback_id().expect("feedback id").0,
            FEEDBACK_ID_ALPHABET,
            FEEDBACK_ID_LENGTH,
            "feedback id",
        );
        let webhook = ids.webhook_id().expect("webhook id");
        assert_shape(
            &webhook.0,
            WEBHOOK_ID_ALPHABET,
            WEBHOOK_ID_LENGTH,
            "webhook id",
        );
        assert!(is_valid_artifact_id(&webhook.0));
    }
}

#[test]
fn generation_covers_the_whole_alphabet_without_bias() {
    // A masked rejection sampler must be able to emit every symbol, including the ones
    // past the mask boundary; a truncating implementation would silently drop them.
    let ids = NanoIdSource::new(OsRandom);
    let mut seen: BTreeSet<char> = BTreeSet::new();
    for _ in 0..4000 {
        seen.extend(ids.artifact_id().expect("artifact id").0.chars());
    }
    assert_eq!(seen.len(), ARTIFACT_ID_ALPHABET.chars().count(), "{seen:?}");

    let mut token_symbols: BTreeSet<char> = BTreeSet::new();
    for _ in 0..4000 {
        token_symbols.extend(ids.share_token().expect("share token").0.chars());
    }
    assert_eq!(token_symbols.len(), SHARE_TOKEN_ALPHABET.chars().count());
}

#[test]
fn generated_ids_are_unique_across_a_large_sample() {
    let ids = NanoIdSource::new(OsRandom);
    let mut seen = BTreeSet::new();
    for _ in 0..5000 {
        assert!(seen.insert(ids.artifact_id().expect("artifact id").0));
    }
    assert!(
        !seen.contains("mcp"),
        "reserved ids are unreachable at length 12"
    );
}

#[test]
fn a_seeded_random_source_reproduces_the_same_identifiers() {
    let first: Vec<String> = (0..8)
        .map(|_| {
            NanoIdSource::new(SeededRandom::new(42))
                .artifact_id()
                .expect("artifact id")
                .0
        })
        .collect();
    assert!(
        first.windows(2).all(|pair| pair[0] == pair[1]),
        "a fresh seeded source must replay the same first id: {first:?}"
    );

    let stream = NanoIdSource::new(SeededRandom::new(42));
    let a = stream.artifact_id().expect("artifact id");
    let b = stream.artifact_id().expect("artifact id");
    assert_ne!(a, b, "consecutive draws from one stream must differ");
}

#[test]
fn a_scripted_random_source_pins_an_exact_identifier() {
    // Byte 0 masks to index 0 in every alphabet, so an all-zero script is fully pinned.
    let ids = NanoIdSource::new(ScriptedRandom::new(vec![0_u8]));
    assert_eq!(ids.artifact_id().expect("artifact id").0, "000000000000");
    assert_eq!(
        ids.share_token().expect("share token").0,
        "000000000000000000000000"
    );
    assert_eq!(
        ids.feedback_id().expect("feedback id").0,
        "0000000000000000"
    );
}

#[test]
fn rejection_sampling_skips_out_of_range_draws() {
    // The artifact alphabet has 34 symbols and a 63-wide mask, so bytes masking to 34..=63
    // must be rejected rather than folded back onto a symbol.
    let random = ScriptedRandom::new(vec![63, 40, 34, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    let generated = generate_id(&random, ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH)
        .expect("generated with rejection");
    assert_shape(
        &generated,
        ARTIFACT_ID_ALPHABET,
        ARTIFACT_ID_LENGTH,
        "rejection-sampled id",
    );
    assert!(generated.starts_with("0123"), "{generated}");
}

#[test]
fn sequential_ids_are_deterministic_unique_and_valid() {
    let ids = SequentialIdSource::default();
    assert_eq!(ids.artifact_id().expect("id").0, "000000000000");
    assert_eq!(ids.artifact_id().expect("id").0, "000000000001");

    let ids = SequentialIdSource::starting_at(1000);
    let mut seen = BTreeSet::new();
    for _ in 0..2000 {
        let id = ids.artifact_id().expect("id");
        assert_shape(
            &id.0,
            ARTIFACT_ID_ALPHABET,
            ARTIFACT_ID_LENGTH,
            "sequential id",
        );
        assert!(is_valid_artifact_id(&id.0));
        assert!(seen.insert(id.0));
    }

    let ids = SequentialIdSource::default();
    assert_shape(
        &ids.share_token().expect("token").0,
        SHARE_TOKEN_ALPHABET,
        SHARE_TOKEN_LENGTH,
        "sequential share token",
    );
    assert_shape(
        &ids.feedback_id().expect("id").0,
        FEEDBACK_ID_ALPHABET,
        FEEDBACK_ID_LENGTH,
        "sequential feedback id",
    );
    assert_shape(
        &ids.webhook_id().expect("id").0,
        WEBHOOK_ID_ALPHABET,
        WEBHOOK_ID_LENGTH,
        "sequential webhook id",
    );
}

// ---------------------------------------------------------------------------
// Deterministic clock and randomness
// ---------------------------------------------------------------------------

#[test]
fn the_fixed_clock_is_stable_and_advanceable() {
    let clock = FixedClock::default();
    assert_eq!(clock.now_unix_seconds(), 1_767_225_600);
    assert_eq!(clock.now_unix_millis(), 1_767_225_600_000);
    assert_eq!(
        clock.now_timestamp(),
        Timestamp("2026-01-01 00:00:00".to_owned())
    );
    // Repeat reads never drift.
    assert_eq!(clock.now_timestamp(), clock.now_timestamp());

    clock.advance_millis(90_000);
    assert_eq!(
        clock.now_timestamp(),
        Timestamp("2026-01-01 00:01:30".to_owned())
    );

    let pinned = FixedClock::from_seconds(0);
    assert_eq!(
        pinned.now_timestamp(),
        Timestamp("1970-01-01 00:00:00".to_owned())
    );
}

#[test]
fn deterministic_random_sources_replay_identical_streams() {
    let mut first = [0_u8; 32];
    let mut second = [0_u8; 32];
    SeededRandom::new(9).fill_bytes(&mut first).expect("fill");
    SeededRandom::new(9).fill_bytes(&mut second).expect("fill");
    assert_eq!(first, second);

    let mut different = [0_u8; 32];
    SeededRandom::new(10)
        .fill_bytes(&mut different)
        .expect("fill");
    assert_ne!(first, different);

    let scripted = ScriptedRandom::new(vec![1, 2, 3]);
    let mut cycled = [0_u8; 7];
    scripted.fill_bytes(&mut cycled).expect("fill");
    assert_eq!(cycled, [1, 2, 3, 1, 2, 3, 1]);
}
