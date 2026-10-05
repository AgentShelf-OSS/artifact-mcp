//! PBI-073 OAuth client-credentials verification, rotation, and scope mapping.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use artifact_mcp::{
    config::{AppConfig, EnvSource, FixedClock, MapEnv, OAuthConfig},
    error::AppError,
    model::{ClientId, OrgId},
    ports::{BoxFuture, PublisherAuthenticator},
    security::{
        auth::{KeyAuthenticator, KeyHash, PublisherKeyDirectory, PublisherKeyRecord},
        jwks::{CachingJwks, JwkDocument, JwksSource, StaticJwks},
        oauth::{
            CompositePublisherAuthenticator, OAuthAccessToken, OAuthAuthenticator,
            OAuthTokenRejection, SCOPE_DELETE, SCOPE_PUBLISH, SCOPE_READ, SCOPE_REVIEW,
            SCOPE_VISIBILITY, required_scope,
        },
    },
};
use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use serde_json::{Value, json};

use crate::u05_support::{KID_CURRENT, KID_ROTATED, NOW_SECONDS, jwks, signed, tamper_signature};

const ISSUER: &str = "https://auth.example.test";
const AUDIENCE: &str = "https://artifacts.example.test/mcp";

fn oauth_config() -> OAuthConfig {
    OAuthConfig {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        jwks_url: "https://auth.example.test/jwks".to_owned(),
        ..OAuthConfig::default()
    }
}

fn claims(patch: Value) -> Value {
    let mut claims = json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "sub": "ci-publisher",
        "client_id": "ci-publisher",
        "client_name": "CI publisher",
        "org": "Acme",
        "role": "author",
        "scope": "artifacts:read artifacts:publish artifacts:review",
        "iat": NOW_SECONDS,
        "nbf": NOW_SECONDS,
        "exp": NOW_SECONDS + 600
    });
    let target = claims.as_object_mut().expect("claims object");
    for (key, value) in patch.as_object().expect("patch object") {
        if value.is_null() {
            target.remove(key);
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
    claims
}

fn token(kid: &str, patch: Value) -> String {
    signed(
        json!({ "alg": "RS256", "kid": kid, "typ": "at+jwt" }),
        claims(patch),
        kid,
    )
}

fn verifier(kids: &[&str]) -> OAuthAuthenticator {
    let document = JwkDocument::from_json(&jwks(kids)).expect("fixture JWKS");
    OAuthAuthenticator::with_clock(
        oauth_config(),
        Arc::new(StaticJwks::new(document)),
        Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
    )
}

#[tokio::test]
async fn a_valid_service_token_maps_only_trusted_identity_and_scopes() {
    let identity = verifier(&[KID_CURRENT])
        .verify(&OAuthAccessToken::new(token(KID_CURRENT, json!({}))))
        .await
        .expect("valid OAuth token");
    assert_eq!(identity.client_id, ClientId::from("ci-publisher"));
    assert_eq!(identity.org, OrgId::from("acme"));
    assert_eq!(identity.label, "CI publisher");
    assert_eq!(identity.role, "author");
    assert!(identity.is_oauth());
    assert!(identity.has_scope(SCOPE_READ));
    assert!(identity.has_scope(SCOPE_PUBLISH));
    assert!(identity.has_scope(SCOPE_REVIEW));
    assert!(!identity.has_scope(SCOPE_VISIBILITY));
    assert!(!identity.has_scope(SCOPE_DELETE));
}

#[tokio::test]
async fn issuer_audience_signature_time_and_lifetime_fail_closed() {
    let verifier = verifier(&[KID_CURRENT]);
    let cases = [
        (
            json!({ "iss": "https://attacker.example" }),
            OAuthTokenRejection::WrongIssuer,
        ),
        (
            json!({ "aud": "https://other.example/mcp" }),
            OAuthTokenRejection::WrongAudience,
        ),
        (
            json!({ "exp": NOW_SECONDS - 31 }),
            OAuthTokenRejection::Expired,
        ),
        (
            json!({ "nbf": NOW_SECONDS + 31 }),
            OAuthTokenRejection::NotYetValid,
        ),
        (
            json!({ "exp": NOW_SECONDS + 3_601 }),
            OAuthTokenRejection::LifetimeTooLong,
        ),
        (
            json!({ "exp": null }),
            OAuthTokenRejection::MissingClaim("exp"),
        ),
    ];
    for (patch, expected) in cases {
        let rejection = verifier
            .verify(&OAuthAccessToken::new(token(KID_CURRENT, patch)))
            .await
            .expect_err("token rejected");
        assert_eq!(rejection, expected);
    }
    let bad_signature = tamper_signature(&token(KID_CURRENT, json!({})));
    assert_eq!(
        verifier.verify(&OAuthAccessToken::new(bad_signature)).await,
        Err(OAuthTokenRejection::BadSignature)
    );
}

#[derive(Debug)]
struct RotatingSource {
    documents: Vec<Value>,
    fetches: AtomicUsize,
}

impl JwksSource for RotatingSource {
    fn fetch(&self) -> BoxFuture<'_, Result<JwkDocument, AppError>> {
        Box::pin(async move {
            let index = self.fetches.fetch_add(1, Ordering::SeqCst);
            let document = self
                .documents
                .get(index)
                .or_else(|| self.documents.last())
                .expect("rotation fixture");
            JwkDocument::from_json(document)
        })
    }
}

#[tokio::test]
async fn oauth_verification_accepts_a_rotated_key_after_the_bounded_cooldown() {
    let source = Arc::new(RotatingSource {
        documents: vec![jwks(&[KID_CURRENT]), jwks(&[KID_ROTATED])],
        fetches: AtomicUsize::new(0),
    });
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let provider = Arc::new(CachingJwks::with_clock(source.clone(), clock.clone()));
    let verifier = OAuthAuthenticator::with_clock(oauth_config(), provider, clock.clone());

    verifier
        .verify(&OAuthAccessToken::new(token(KID_CURRENT, json!({}))))
        .await
        .expect("current key");
    assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    clock.advance_millis(31_000);
    verifier
        .verify(&OAuthAccessToken::new(token(
            KID_ROTATED,
            json!({
                "iat": NOW_SECONDS + 31,
                "nbf": NOW_SECONDS + 31,
                "exp": NOW_SECONDS + 631
            }),
        )))
        .await
        .expect("rotated key");
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
}

#[derive(Debug)]
struct AnyKeyDirectory;

impl PublisherKeyDirectory for AnyKeyDirectory {
    fn find_active<'a>(
        &'a self,
        _hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        Box::pin(async {
            Ok(Some(PublisherKeyRecord {
                client_id: ClientId::from("legacy"),
                org: OrgId::from("acme"),
                label: "Legacy key".to_owned(),
                role: "author".to_owned(),
            }))
        })
    }
}

#[tokio::test]
async fn api_key_compatibility_remains_unscoped_when_oauth_is_enabled() {
    let composite = CompositePublisherAuthenticator::new(
        Some(KeyAuthenticator::new(Arc::new(AnyKeyDirectory))),
        Some(verifier(&[KID_CURRENT])),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer legacy-secret"),
    );
    let identity = composite
        .authenticate(&headers)
        .await
        .expect("legacy API key");
    assert_eq!(identity.client_id, ClientId::from("legacy"));
    assert!(!identity.is_oauth());
    assert!(identity.has_scope(SCOPE_DELETE));
}

#[test]
fn every_mcp_operation_maps_to_the_intended_least_privilege_scope() {
    assert_eq!(
        required_scope("tools/call", Some("read_artifact")),
        Some(SCOPE_READ)
    );
    assert_eq!(
        required_scope("tools/call", Some("publish_bundle")),
        Some(SCOPE_PUBLISH)
    );
    assert_eq!(
        required_scope("tools/call", Some("submit_feedback")),
        Some(SCOPE_REVIEW)
    );
    assert_eq!(
        required_scope("tools/call", Some("create_share")),
        Some(SCOPE_VISIBILITY)
    );
    assert_eq!(
        required_scope("tools/call", Some("delete_artifact")),
        Some(SCOPE_DELETE)
    );
    for name in ["list_collections", "get_collection"] {
        assert_eq!(required_scope("tools/call", Some(name)), Some(SCOPE_READ));
    }
    for name in [
        "create_collection",
        "update_collection",
        "delete_collection",
        "add_artifacts_to_collection",
        "remove_artifacts_from_collection",
    ] {
        assert_eq!(
            required_scope("tools/call", Some(name)),
            Some(SCOPE_PUBLISH)
        );
    }
    assert_eq!(required_scope("resources/read", None), Some(SCOPE_READ));
    assert_eq!(required_scope("server/discover", None), None);
}

#[test]
fn oauth_configuration_is_optional_complete_and_fail_closed() {
    let defaults =
        AppConfig::from_source(&MapEnv::empty() as &dyn EnvSource).expect("default configuration");
    assert!(!defaults.oauth.enabled());
    assert!(defaults.oauth.api_keys_enabled);

    let complete = AppConfig::from_source(
        &MapEnv::empty()
            .with("MCP_OAUTH_ISSUER", ISSUER)
            .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
            .with("MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks") as &dyn EnvSource,
    )
    .expect("complete OAuth configuration");
    assert!(complete.oauth.enabled());

    let partial =
        AppConfig::from_source(&MapEnv::empty().with("MCP_OAUTH_ISSUER", ISSUER) as &dyn EnvSource)
            .expect_err("partial OAuth configuration rejected");
    assert!(partial.to_string().contains("requires MCP_OAUTH_ISSUER"));

    let disabled_without_oauth = AppConfig::from_source(
        &MapEnv::empty().with("MCP_API_KEYS_ENABLED", "0") as &dyn EnvSource,
    )
    .expect("parsing and startup validation are separate");
    assert!(disabled_without_oauth.validate_startup().is_err());
}

#[test]
fn oauth_urls_require_https_except_explicit_canonical_loopback_development_mode() {
    let base = MapEnv::empty()
        .with("MCP_OAUTH_ISSUER", ISSUER)
        .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
        .with("MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks");
    let insecure = base
        .clone()
        .with("MCP_OAUTH_ISSUER", "http://auth.example.test");
    let error = AppConfig::from_source(&insecure).expect_err("public HTTP issuer rejected");
    assert!(error.to_string().contains("MCP_OAUTH_ISSUER"));
    assert!(!error.to_string().contains("auth.example.test"));

    for host in [
        "localhost",
        "127.0.0.1",
        "127.0.0.2",
        "127.1",
        "0x7f000001",
        "[::1]",
    ] {
        let env = base
            .clone()
            .with("MCP_OAUTH_ISSUER", &format!("http://{host}/issuer"))
            .with("MCP_OAUTH_JWKS_URL", &format!("http://{host}/jwks"));
        assert!(
            AppConfig::from_source(&env).is_err(),
            "opt-in required for {host}"
        );
    }

    let loopback = base
        .clone()
        .with("MCP_OAUTH_ISSUER", "http://127.1:3480")
        .with("MCP_OAUTH_JWKS_URL", "http://[::1]:3481/jwks")
        .with("MCP_OAUTH_ALLOW_LOOPBACK_HTTP", "1");
    let config = AppConfig::from_source(&loopback).expect("explicit loopback HTTP accepted");
    assert!(config.oauth.allow_loopback_http);

    for host in [
        "localhost",
        "127.0.0.1",
        "127.0.0.2",
        "127.1",
        "0x7f000001",
        "[::1]",
    ] {
        let env = MapEnv::empty()
            .with("MCP_OAUTH_ISSUER", &format!("http://{host}/issuer"))
            .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
            .with("MCP_OAUTH_JWKS_URL", &format!("http://{host}/jwks"))
            .with("MCP_OAUTH_ALLOW_LOOPBACK_HTTP", "1");
        assert!(AppConfig::from_source(&env).is_ok(), "accepted {host}");
    }

    for host in [
        "http://192.168.1.10",
        "http://127.0.0.1.evil.example",
        "http://localhost.evil.example",
        "http://[::ffff:127.0.0.1]",
    ] {
        let env = loopback.clone().with("MCP_OAUTH_ISSUER", host);
        assert!(AppConfig::from_source(&env).is_err(), "rejected {host}");
    }

    let malformed_switch = loopback
        .clone()
        .with("MCP_OAUTH_ALLOW_LOOPBACK_HTTP", " 1 ");
    let error = AppConfig::from_source(&malformed_switch).expect_err("whitespace switch rejected");
    assert!(error.to_string().contains("MCP_OAUTH_ALLOW_LOOPBACK_HTTP"));
}

#[test]
fn oauth_url_errors_do_not_echo_credentials_or_fragments() {
    let env = MapEnv::empty()
        .with(
            "MCP_OAUTH_ISSUER",
            "https://client:super-secret@auth.example.test",
        )
        .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
        .with(
            "MCP_OAUTH_JWKS_URL",
            "https://auth.example.test/jwks#fragment",
        );
    let error = AppConfig::from_source(&env).expect_err("credentials are rejected");
    let rendered = error.to_string();
    assert!(rendered.contains("MCP_OAUTH_ISSUER"));
    assert!(!rendered.contains("super-secret"));
    assert!(!rendered.contains("client:"));
    assert!(!rendered.contains("#fragment"));

    let empty_userinfo = MapEnv::empty()
        .with("MCP_OAUTH_ISSUER", "https://@auth.example.test")
        .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
        .with("MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks");
    assert!(AppConfig::from_source(&empty_userinfo).is_err());
}

#[test]
fn loopback_http_switch_is_ignored_when_oauth_is_disabled() {
    let env = MapEnv::empty().with("MCP_OAUTH_ALLOW_LOOPBACK_HTTP", "not-a-flag");
    let config = AppConfig::from_source(&env).expect("API-key-only defaults remain compatible");
    assert!(!config.oauth.enabled());
    assert!(!config.oauth.allow_loopback_http);
}

#[test]
fn oauth_url_policy_matches_node_for_both_settings() {
    use crate::u05_support::{node_reference_available, run_node};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if !node_reference_available(root, &["lib/oauth.js"]) {
        return;
    }
    let mut cases = Vec::new();
    for setting in ["MCP_OAUTH_ISSUER", "MCP_OAUTH_JWKS_URL"] {
        for (url, loopback_http) in [
            ("https://auth.example.test/issuer", false),
            ("http://localhost/issuer", true),
            ("http://LOCALHOST/issuer", true),
            ("http://127.0.0.2/issuer", true),
            ("http://127.1/issuer", true),
            ("http://0x7f000001/issuer", true),
            ("http://[::1]/issuer", true),
            ("http://[0:0:0:0:0:0:0:1]/issuer", true),
            ("http://localhost./issuer", false),
            ("http://localhost.example.test/issuer", false),
            ("http://127.0.0.1.example.test/issuer", false),
            ("http://192.168.1.10/issuer", false),
            ("http://auth.example.test/issuer", false),
            ("http://[::ffff:127.0.0.1]/issuer", false),
            (
                "https://user:synthetic-password@auth.example.test/issuer",
                false,
            ),
            ("https://@auth.example.test/issuer", false),
            ("https://auth.example.test/issuer#", false),
            ("https://auth.example.test/issuer#fragment", false),
            ("ftp://auth.example.test/issuer", false),
            ("invalid?synthetic-query-secret", false),
        ] {
            for opt_in in [None, Some("0"), Some("1")] {
                let expected =
                    url.starts_with("https://") && !url.contains('@') && !url.contains('#')
                        || loopback_http && opt_in == Some("1");
                cases.push(
                    json!({"setting": setting, "url": url, "optIn": opt_in, "expected": expected}),
                );
            }
        }
    }
    for (opt_in, expected) in [("", true), (" ", true), ("yes", false), (" 1 ", false)] {
        cases.push(json!({"setting": "MCP_OAUTH_ISSUER", "url": ISSUER, "optIn": opt_in, "expected": expected}));
    }
    let rust: Vec<bool> = cases
        .iter()
        .map(|case| {
            let mut env = MapEnv::empty()
                .with("MCP_OAUTH_ISSUER", ISSUER)
                .with("MCP_OAUTH_AUDIENCE", AUDIENCE)
                .with("MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks")
                .with(
                    case["setting"].as_str().expect("setting"),
                    case["url"].as_str().expect("url"),
                );
            if let Some(opt_in) = case["optIn"].as_str() {
                env = env.with("MCP_OAUTH_ALLOW_LOOPBACK_HTTP", opt_in);
            }
            let result = AppConfig::from_source(&env);
            if let Err(error) = &result {
                let message = error.to_string();
                assert!(message.contains("MCP_OAUTH_"));
                assert!(!message.contains("synthetic-password"));
                assert!(!message.contains("synthetic-query-secret"));
            }
            assert_eq!(
                result.is_ok(),
                case["expected"].as_bool().expect("expected"),
                "{case}"
            );
            result.is_ok()
        })
        .collect();
    let node = run_node(
        root,
        r#"
        const { oauthConfigFromEnv } = await import('./lib/oauth.js');
        const cases = JSON.parse(process.argv[1]);
        console.log(JSON.stringify(cases.map(row => {
            const env = {
                MCP_OAUTH_ISSUER: 'https://auth.example.test',
                MCP_OAUTH_AUDIENCE: 'https://artifacts.example.test/mcp',
                MCP_OAUTH_JWKS_URL: 'https://auth.example.test/jwks',
                [row.setting]: row.url
            };
            if (row.optIn !== null) env.MCP_OAUTH_ALLOW_LOOPBACK_HTTP = row.optIn;
            try { oauthConfigFromEnv(env); return true; } catch { return false; }
        })));
    "#,
        &json!(cases),
        &[],
    );
    assert_eq!(json!(rust), node);
}
