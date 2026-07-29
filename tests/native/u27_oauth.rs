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
