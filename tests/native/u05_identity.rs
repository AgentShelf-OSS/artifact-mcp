//! U05 — Cloudflare Access viewer identity (`src/security/identity.rs`).
//!
//! Every token here is really signed with the fixture keys in `u05_support.rs`, and every clock
//! comparison runs against a [`FixedClock`], so the 60-second leeway boundaries are asserted at
//! the exact second rather than approximately. `u05_node_parity.rs` proves the same boundaries
//! against the real `jose` implementation Node uses.

use std::sync::Arc;

use artifact_mcp::config::{AppConfig, EnvSource, FixedClock, MapEnv};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{EmailAddress, OrgId};
use artifact_mcp::ports::{BoxFuture, ViewerIdentity};
use artifact_mcp::security::identity::{
    ACCESS_EMAIL_HEADER, ACCESS_JWT_HEADER, AccessToken, AccessViewerIdentity, NoOrgDirectory,
    OrgDirectory, TokenRejection, access_token, assert_ready, domain_of, read_access_cookie,
};
use artifact_mcp::security::jwks::{JwkDocument, StaticJwks};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::u05_support::{
    AUDIENCE, KID_CURRENT, KID_ROTATED, NOW_SECONDS, TEAM_DOMAIN, jwks, signed, tamper_payload,
    tamper_signature, token, with_signature,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("test header name"),
            HeaderValue::from_str(value).expect("test header value"),
        );
    }
    map
}

fn config(entries: &[(&str, &str)]) -> Arc<AppConfig> {
    let env = entries
        .iter()
        .fold(MapEnv::empty(), |env, (key, value)| env.with(key, value));
    Arc::new(AppConfig::from_source(&env as &dyn EnvSource).expect("test config parses"))
}

/// A `jwt`-mode configuration; `extra` adds or overrides environment entries.
fn jwt_config(extra: &[(&str, &str)]) -> Arc<AppConfig> {
    let mut entries = vec![
        ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
        ("CF_ACCESS_AUD", AUDIENCE),
    ];
    entries.extend_from_slice(extra);
    config(&entries)
}

/// An in-memory `org_domains` / `org_email_members` registry.
///
/// The email match is ASCII case-insensitive because the v21 table declares
/// `email TEXT PRIMARY KEY COLLATE NOCASE` (`lib/migrations.js:420`). Reproducing the collation
/// here is what makes the "explicit membership beats domain routing" matrix meaningful.
#[derive(Debug, Default)]
struct FakeDirectory {
    emails: Vec<(String, String)>,
    domains: Vec<(String, String)>,
}

impl FakeDirectory {
    fn with_email(mut self, email: &str, org: &str) -> Self {
        self.emails.push((email.to_owned(), org.to_owned()));
        self
    }

    fn with_domain(mut self, domain: &str, org: &str) -> Self {
        self.domains.push((domain.to_owned(), org.to_owned()));
        self
    }
}

impl OrgDirectory for FakeDirectory {
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async move {
            Ok(self
                .emails
                .iter()
                .find(|(stored, _)| stored.eq_ignore_ascii_case(&email.0))
                .map(|(_, org)| OrgId::from(org.as_str())))
        })
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async move {
            Ok(self
                .domains
                .iter()
                .find(|(stored, _)| stored == domain)
                .map(|(_, org)| OrgId::from(org.as_str())))
        })
    }
}

fn resolver_with(
    config: Arc<AppConfig>,
    kids: &[&str],
    directory: Arc<dyn OrgDirectory>,
) -> AccessViewerIdentity {
    let document = JwkDocument::from_json(&jwks(kids)).expect("fixture JWKS parses");
    AccessViewerIdentity::with_clock(
        config,
        Arc::new(StaticJwks::new(document)),
        directory,
        Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
    )
}

fn resolver(config: Arc<AppConfig>) -> AccessViewerIdentity {
    resolver_with(config, &[KID_CURRENT], Arc::new(NoOrgDirectory))
}

/// Verify a fixture token and report the outcome as `Ok(email)` / `Err(rejection)`.
async fn verify(resolver: &AccessViewerIdentity, token: &str) -> Result<String, TokenRejection> {
    resolver
        .verify(&AccessToken::new(token))
        .await
        .map(|claims| claims.email)
}

// ---------------------------------------------------------------------------
// Cookie parsing — lib/identity.js:96-109
// ---------------------------------------------------------------------------

fn cookie(value: &str) -> Option<String> {
    read_access_cookie(&headers(&[("cookie", value)])).map(|token| token.expose().to_owned())
}

#[test]
fn the_access_cookie_is_matched_by_exact_name_and_split_on_the_first_separator() {
    assert_eq!(cookie("CF_Authorization=token").as_deref(), Some("token"));
    assert_eq!(
        cookie("theme=dark; CF_Authorization=token; view=grid").as_deref(),
        Some("token")
    );
    // A cookie value may contain `=`; only the first separator is a separator.
    assert_eq!(
        cookie("CF_Authorization=eyJ.part=tail==").as_deref(),
        Some("eyJ.part=tail==")
    );
    assert_eq!(
        cookie("  CF_Authorization =  token  ").as_deref(),
        Some("token")
    );
    // A prefix match is not a match.
    assert_eq!(
        cookie("CF_AuthorizationX=wrong; CF_Authorization=right").as_deref(),
        Some("right")
    );
    assert_eq!(cookie("CF_AuthorizationX=wrong"), None);
    assert_eq!(cookie("cf_authorization=wrongcase"), None);
}

#[test]
fn a_malformed_or_empty_cookie_yields_nothing_without_aborting_the_scan() {
    assert_eq!(cookie(""), None);
    assert_eq!(cookie("novalue"), None);
    assert_eq!(cookie("CF_Authorization="), None);
    assert_eq!(cookie("CF_Authorization=   "), None);
    // An empty first occurrence must not mask a later real one.
    assert_eq!(
        cookie("CF_Authorization=; CF_Authorization=real").as_deref(),
        Some("real")
    );
    assert_eq!(
        cookie("novalue; CF_Authorization=real").as_deref(),
        Some("real")
    );
}

#[test]
fn repeated_cookie_headers_are_all_scanned() {
    let map = headers(&[
        ("cookie", "theme=dark"),
        ("cookie", "CF_Authorization=token"),
    ]);
    assert_eq!(
        read_access_cookie(&map)
            .map(|t| t.expose().to_owned())
            .as_deref(),
        Some("token")
    );
}

// ---------------------------------------------------------------------------
// Header vs cookie precedence — lib/identity.js:117-119
// ---------------------------------------------------------------------------

#[test]
fn the_assertion_header_is_preferred_over_the_cookie() {
    let map = headers(&[
        (ACCESS_JWT_HEADER, "from-header"),
        ("cookie", "CF_Authorization=from-cookie"),
    ]);
    assert_eq!(
        access_token(&map).map(|t| t.expose().to_owned()).as_deref(),
        Some("from-header")
    );
}

#[test]
fn an_empty_assertion_header_falls_back_to_the_cookie() {
    let map = headers(&[
        (ACCESS_JWT_HEADER, ""),
        ("cookie", "CF_Authorization=from-cookie"),
    ]);
    assert_eq!(
        access_token(&map).map(|t| t.expose().to_owned()).as_deref(),
        Some("from-cookie")
    );
    assert_eq!(access_token(&headers(&[])), None);
}

#[tokio::test]
async fn a_garbage_header_is_used_and_fails_rather_than_deferring_to_a_valid_cookie() {
    // `headerToken || readAccessCookie(req)` — a present header short-circuits the `||`, so the
    // valid cookie is never consulted and the request resolves anonymous.
    let resolver = resolver(jwt_config(&[]));
    let map = headers(&[
        (ACCESS_JWT_HEADER, "not-a-jwt"),
        (
            "cookie",
            &format!("CF_Authorization={}", token(KID_CURRENT, json!({}))),
        ),
    ]);
    assert_eq!(
        resolver.resolve(&map).await.expect("resolves"),
        Default::default()
    );
}

#[tokio::test]
async fn a_valid_cookie_alone_authenticates() {
    let resolver = resolver(jwt_config(&[]));
    let map = headers(&[(
        "cookie",
        &format!("CF_Authorization={}", token(KID_CURRENT, json!({}))),
    )]);
    let viewer = resolver.resolve(&map).await.expect("resolves");
    assert_eq!(viewer.email, Some(EmailAddress::from("member@acme.test")));
}

// ---------------------------------------------------------------------------
// The 60-second leeway matrix — jose lib/jwt_claims_set.js
// ---------------------------------------------------------------------------

/// `nbf > now + tolerance` rejects, so `now + tolerance` exactly is still inside.
#[tokio::test]
async fn the_nbf_boundary_accepts_exactly_now_plus_the_leeway() {
    let resolver = resolver(jwt_config(&[]));
    for (offset, expected) in [
        (0_i64, Ok(())),
        (59, Ok(())),
        (60, Ok(())),
        (61, Err(TokenRejection::NotYetValid)),
        (600, Err(TokenRejection::NotYetValid)),
    ] {
        let claims = json!({ "nbf": NOW_SECONDS + offset, "exp": NOW_SECONDS + 100_000 });
        let outcome = verify(&resolver, &token(KID_CURRENT, claims)).await;
        match expected {
            Ok(()) => assert!(
                outcome.is_ok(),
                "nbf = now+{offset} must be accepted: {outcome:?}"
            ),
            Err(rejection) => assert_eq!(outcome, Err(rejection), "nbf = now+{offset}"),
        }
    }
}

/// `exp <= now - tolerance` rejects, so `now - tolerance` exactly is already outside.
#[tokio::test]
async fn the_exp_boundary_rejects_exactly_now_minus_the_leeway() {
    let resolver = resolver(jwt_config(&[]));
    for (offset, expected) in [
        (3600_i64, Ok(())),
        (1, Ok(())),
        (0, Ok(())),
        (-59, Ok(())),
        (-60, Err(TokenRejection::Expired)),
        (-61, Err(TokenRejection::Expired)),
    ] {
        let claims = json!({ "nbf": NOW_SECONDS - 100_000, "exp": NOW_SECONDS + offset });
        let outcome = verify(&resolver, &token(KID_CURRENT, claims)).await;
        match expected {
            Ok(()) => assert!(
                outcome.is_ok(),
                "exp = now{offset:+} must be accepted: {outcome:?}"
            ),
            Err(rejection) => assert_eq!(outcome, Err(rejection), "exp = now{offset:+}"),
        }
    }
}

#[tokio::test]
async fn the_leeway_is_configurable_and_zero_means_sixty() {
    // `Number(ACCESS_CLOCK_TOLERANCE_S) || 60` — an explicit 0 selects the default.
    for (configured, tolerance) in [("0", 60_i64), ("5", 5), ("300", 300)] {
        let resolver = resolver(jwt_config(&[("ACCESS_CLOCK_TOLERANCE_S", configured)]));
        let inside = json!({ "nbf": NOW_SECONDS + tolerance, "exp": NOW_SECONDS + 100_000 });
        let outside = json!({ "nbf": NOW_SECONDS + tolerance + 1, "exp": NOW_SECONDS + 100_000 });
        assert!(
            verify(&resolver, &token(KID_CURRENT, inside)).await.is_ok(),
            "tolerance {tolerance}: nbf at the boundary must be accepted"
        );
        assert_eq!(
            verify(&resolver, &token(KID_CURRENT, outside)).await,
            Err(TokenRejection::NotYetValid),
            "tolerance {tolerance}: one second past the boundary must be rejected"
        );
    }
}

#[tokio::test]
async fn absent_temporal_claims_are_accepted_and_non_numeric_ones_are_not() {
    let resolver = resolver(jwt_config(&[]));
    // `jose` only requires `iss` and `aud`; a token with neither `nbf` nor `exp` still verifies.
    assert!(
        verify(
            &resolver,
            &token(
                KID_CURRENT,
                json!({ "nbf": null, "exp": null, "iat": null })
            )
        )
        .await
        .is_ok()
    );
    for claim in ["iat", "nbf", "exp"] {
        let mut patch = serde_json::Map::new();
        patch.insert(claim.to_owned(), json!("not-a-number"));
        assert_eq!(
            verify(&resolver, &token(KID_CURRENT, Value::Object(patch))).await,
            Err(TokenRejection::InvalidTemporalClaim(claim)),
            "{claim} must be numeric when present"
        );
    }
}

// ---------------------------------------------------------------------------
// Issuer, audience, signature, and algorithm
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_issuer_must_be_exactly_the_configured_team_domain() {
    let resolver = resolver(jwt_config(&[]));
    assert!(
        verify(&resolver, &token(KID_CURRENT, json!({})))
            .await
            .is_ok()
    );
    for issuer in [
        json!("https://evil.cloudflareaccess.test"),
        json!(format!("http://{TEAM_DOMAIN}")),
        json!(format!("https://{TEAM_DOMAIN}/")),
        json!(format!("https://{TEAM_DOMAIN}.evil.test")),
        json!(42),
    ] {
        assert_eq!(
            verify(&resolver, &token(KID_CURRENT, json!({ "iss": issuer }))).await,
            Err(TokenRejection::WrongIssuer),
            "issuer {issuer} must be rejected"
        );
    }
    assert_eq!(
        verify(&resolver, &token(KID_CURRENT, json!({ "iss": null }))).await,
        Err(TokenRejection::MissingClaim("iss"))
    );
}

#[tokio::test]
async fn the_audience_must_contain_the_configured_aud_tag() {
    let resolver = resolver(jwt_config(&[]));
    // `checkAudiencePresence` accepts a string or an array containing the tag.
    for audience in [
        json!(AUDIENCE),
        json!([AUDIENCE]),
        json!(["other", AUDIENCE]),
    ] {
        assert!(
            verify(&resolver, &token(KID_CURRENT, json!({ "aud": audience })))
                .await
                .is_ok(),
            "audience {audience} must be accepted"
        );
    }
    for audience in [
        json!("other"),
        json!([]),
        json!(["other"]),
        json!(1),
        json!({}),
    ] {
        assert_eq!(
            verify(&resolver, &token(KID_CURRENT, json!({ "aud": audience }))).await,
            Err(TokenRejection::WrongAudience),
            "audience {audience} must be rejected"
        );
    }
    assert_eq!(
        verify(&resolver, &token(KID_CURRENT, json!({ "aud": null }))).await,
        Err(TokenRejection::MissingClaim("aud"))
    );
}

#[tokio::test]
async fn a_tampered_signature_or_payload_never_verifies() {
    let resolver = resolver(jwt_config(&[]));
    let valid = token(KID_CURRENT, json!({}));
    assert!(verify(&resolver, &valid).await.is_ok());

    assert_eq!(
        verify(&resolver, &tamper_signature(&valid)).await,
        Err(TokenRejection::BadSignature)
    );
    // Re-writing the claims keeps the compact shape but invalidates the signature; the payload
    // must never be read as authoritative before the signature check.
    let escalated = tamper_payload(
        &valid,
        json!({
            "iss": format!("https://{TEAM_DOMAIN}"),
            "aud": AUDIENCE,
            "email": "attacker@evil.test",
            "exp": NOW_SECONDS + 3600,
        }),
    );
    assert_eq!(
        verify(&resolver, &escalated).await,
        Err(TokenRejection::BadSignature)
    );
    // A token signed by a key that is genuinely in the set, but under the other `kid`'s header,
    // must not verify either.
    let mismatched = signed(
        json!({ "alg": "RS256", "kid": KID_CURRENT, "typ": "JWT" }),
        json!({
            "iss": format!("https://{TEAM_DOMAIN}"),
            "aud": AUDIENCE,
            "email": "member@acme.test",
            "exp": NOW_SECONDS + 3600,
        }),
        KID_ROTATED,
    );
    assert_eq!(
        verify(&resolver, &mismatched).await,
        Err(TokenRejection::BadSignature)
    );
}

#[tokio::test]
async fn an_unsigned_or_algorithm_confused_token_is_refused() {
    let resolver = resolver(jwt_config(&[]));
    let claims = json!({
        "iss": format!("https://{TEAM_DOMAIN}"),
        "aud": AUDIENCE,
        "email": "attacker@evil.test",
        "exp": NOW_SECONDS + 3600,
    });

    // `alg: "none"` names no JWS key family, so it cannot even select a key.
    assert_eq!(
        verify(
            &resolver,
            &with_signature(json!({ "alg": "none", "typ": "JWT" }), claims.clone(), "")
        )
        .await,
        Err(TokenRejection::NoVerificationKey)
    );
    // An HMAC `alg` must not be able to reach an RSA key and treat its modulus as a secret.
    assert_eq!(
        verify(
            &resolver,
            &with_signature(
                json!({ "alg": "HS256", "kid": KID_CURRENT, "typ": "JWT" }),
                claims.clone(),
                "AAAA"
            )
        )
        .await,
        Err(TokenRejection::NoVerificationKey)
    );
    // A header with no `alg` at all.
    assert_eq!(
        verify(
            &resolver,
            &with_signature(json!({ "typ": "JWT" }), claims.clone(), "AAAA")
        )
        .await,
        Err(TokenRejection::MissingAlgorithm)
    );
    // An unrecognised `crit` parameter is `JOSENotSupported`.
    assert_eq!(
        verify(
            &resolver,
            &with_signature(
                json!({ "alg": "RS256", "kid": KID_CURRENT, "crit": ["exp"], "exp": 1 }),
                claims,
                "AAAA"
            )
        )
        .await,
        Err(TokenRejection::UnsupportedCriticalHeader)
    );
}

#[tokio::test]
async fn a_malformed_compact_serialization_is_refused() {
    let resolver = resolver(jwt_config(&[]));
    for candidate in [
        "",
        "not-a-jwt",
        "a.b",
        "a.b.c.d",
        "!!!.!!!.!!!",
        // Two well-formed base64url segments whose header is not a JSON object.
        "IjEi.IjEi.AAAA",
    ] {
        assert_eq!(
            verify(&resolver, candidate).await,
            Err(TokenRejection::Malformed),
            "{candidate:?} must be malformed"
        );
    }
}

// ---------------------------------------------------------------------------
// Startup modes — lib/identity.js:49-76,116-136
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jwt_mode_ignores_the_spoofable_email_header() {
    let resolver = resolver(jwt_config(&[("TRUST_ACCESS_HEADERS", "1")]));
    let map = headers(&[(ACCESS_EMAIL_HEADER, "attacker@evil.test")]);
    assert_eq!(
        resolver.resolve(&map).await.expect("resolves"),
        Default::default()
    );
}

#[tokio::test]
async fn header_trust_mode_trusts_the_email_header_and_ignores_any_cookie() {
    let config = config(&[("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", "127.0.0.1")]);
    let resolver = AccessViewerIdentity::without_keys(config);
    let map = headers(&[
        (ACCESS_EMAIL_HEADER, "  MEMBER@ACME.TEST  "),
        ("cookie", "CF_Authorization=ignored"),
    ]);
    let viewer = resolver.resolve(&map).await.expect("resolves");
    assert_eq!(viewer.email, Some(EmailAddress::from("member@acme.test")));
    assert_eq!(viewer.org, Some(OrgId::from("acme.test")));
    assert!(!viewer.is_admin);

    // No email header at all is still anonymous.
    let anonymous = AccessViewerIdentity::without_keys(config_header_trust())
        .resolve(&headers(&[("cookie", "CF_Authorization=ignored")]))
        .await
        .expect("resolves");
    assert_eq!(anonymous, Default::default());
}

fn config_header_trust() -> Arc<AppConfig> {
    config(&[("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", "127.0.0.1")])
}

#[tokio::test]
async fn disabled_mode_never_produces_an_identity() {
    let resolver = AccessViewerIdentity::without_keys(config(&[]));
    let map = headers(&[
        (ACCESS_EMAIL_HEADER, "member@acme.test"),
        ("cookie", "CF_Authorization=ignored"),
        (ACCESS_JWT_HEADER, "ignored"),
    ]);
    assert_eq!(
        resolver.resolve(&map).await.expect("resolves"),
        Default::default()
    );
}

#[test]
fn require_access_jwt_refuses_to_start_without_a_usable_jwt_configuration() {
    let error =
        assert_ready(&config(&[("REQUIRE_ACCESS_JWT", "1")])).expect_err("must refuse to start");
    assert_eq!(
        error.to_string(),
        "REQUIRE_ACCESS_JWT=1 requires both CF_ACCESS_TEAM_DOMAIN and CF_ACCESS_AUD; refusing to start"
    );
    // Half a configuration is still not a configuration.
    assert!(
        assert_ready(&config(&[
            ("REQUIRE_ACCESS_JWT", "1"),
            ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN)
        ]))
        .is_err()
    );
    assert!(assert_ready(&jwt_config(&[("REQUIRE_ACCESS_JWT", "1")])).is_ok());
}

#[test]
fn header_trust_refuses_a_non_loopback_bind_unless_explicitly_acknowledged() {
    let error = assert_ready(&config(&[("TRUST_ACCESS_HEADERS", "1")]))
        .expect_err("0.0.0.0 must refuse to start");
    assert!(
        error.to_string().starts_with(
            "TRUST_ACCESS_HEADERS=1 trusts a spoofable identity header and is unsafe on a \
             non-loopback bind (0.0.0.0)."
        ),
        "{error}"
    );
    for host in ["127.0.0.1", "::1", "localhost"] {
        assert!(
            assert_ready(&config(&[
                ("TRUST_ACCESS_HEADERS", "1"),
                ("LISTEN_HOST", host)
            ]))
            .is_ok(),
            "{host} is a loopback bind"
        );
    }
    assert!(
        assert_ready(&config(&[
            ("TRUST_ACCESS_HEADERS", "1"),
            ("HEADER_TRUST_ALLOW_INSECURE", "1")
        ]))
        .is_ok()
    );
    // `jwt` mode outranks `TRUST_ACCESS_HEADERS`, so the unsafe-bind check does not apply.
    assert!(assert_ready(&jwt_config(&[("TRUST_ACCESS_HEADERS", "1")])).is_ok());
    // Nothing configured is safe on any bind.
    assert!(assert_ready(&config(&[])).is_ok());
}

// ---------------------------------------------------------------------------
// Email → org and admin resolution — lib/identity.js:83-91
// ---------------------------------------------------------------------------

#[test]
fn the_domain_is_taken_after_the_last_at_sign() {
    assert_eq!(domain_of("member@acme.test"), "acme.test");
    assert_eq!(domain_of("odd@name@ACME.Test"), "acme.test");
    assert_eq!(domain_of("no-at-sign"), "");
    assert_eq!(domain_of("trailing@"), "");
    assert_eq!(domain_of(""), "");
}

async fn org_of(resolver: &AccessViewerIdentity, email: &str) -> (String, bool) {
    let (org, is_admin) = resolver.org_for_email(email).await.expect("resolves");
    (org.0, is_admin)
}

#[tokio::test]
async fn admin_configuration_outranks_every_org_mapping() {
    let directory = FakeDirectory::default()
        .with_email("boss@acme.test", "acme")
        .with_domain("acme.test", "acme");
    let resolver = resolver_with(
        jwt_config(&[
            ("ADMIN_EMAILS", " Boss@Acme.Test , other@x.test "),
            ("ADMIN_EMAIL_DOMAINS", "ops.test"),
            ("ORG_EMAIL_DOMAINS", "acme.test:from-env"),
        ]),
        &[KID_CURRENT],
        Arc::new(directory),
    );

    // An admin by explicit address, even though both the registry and the env map claim them.
    assert_eq!(
        org_of(&resolver, "boss@acme.test").await,
        ("admin".to_owned(), true)
    );
    // An admin by domain.
    assert_eq!(
        org_of(&resolver, "anyone@ops.test").await,
        ("admin".to_owned(), true)
    );
    // Everyone else follows the mapping chain.
    assert_eq!(
        org_of(&resolver, "member@acme.test").await,
        ("acme".to_owned(), false)
    );
}

#[tokio::test]
async fn explicit_membership_beats_domain_routing_which_beats_the_environment_map() {
    let directory = FakeDirectory::default()
        .with_email("contractor@shared.test", "acme")
        .with_domain("registered.test", "from-registry");
    let resolver = resolver_with(
        jwt_config(&[(
            "ORG_EMAIL_DOMAINS",
            "registered.test:from-env,configured.test:from-env,dup.test:first,dup.test:last",
        )]),
        &[KID_CURRENT],
        Arc::new(directory),
    );

    // 1. explicit email membership
    assert_eq!(
        org_of(&resolver, "contractor@shared.test").await,
        ("acme".to_owned(), false)
    );
    // The v21 table collates NOCASE, so a differently-cased stored address still matches.
    assert_eq!(
        org_of(&resolver, "CONTRACTOR@shared.test").await,
        ("acme".to_owned(), false)
    );
    // 2. registered domain — the registry wins over the environment for the same domain
    assert_eq!(
        org_of(&resolver, "user@registered.test").await,
        ("from-registry".to_owned(), false)
    );
    // 3. environment domain map
    assert_eq!(
        org_of(&resolver, "user@configured.test").await,
        ("from-env".to_owned(), false)
    );
    // `new Map(entries)` keeps the last duplicate.
    assert_eq!(
        org_of(&resolver, "user@dup.test").await,
        ("last".to_owned(), false)
    );
    // 4. the bare domain
    assert_eq!(
        org_of(&resolver, "user@unmapped.test").await,
        ("unmapped.test".to_owned(), false)
    );
    // An address with no domain at all resolves to the empty org, exactly as Node does.
    assert_eq!(org_of(&resolver, "nodomain").await, (String::new(), false));
}

#[tokio::test]
async fn a_verified_token_produces_the_resolved_viewer() {
    let resolver = resolver_with(
        jwt_config(&[("ADMIN_EMAIL_DOMAINS", "ops.test")]),
        &[KID_CURRENT],
        Arc::new(FakeDirectory::default().with_domain("acme.test", "acme")),
    );

    let member = resolver
        .resolve(&headers(&[(
            ACCESS_JWT_HEADER,
            &token(KID_CURRENT, json!({ "email": "  Member@ACME.test " })),
        )]))
        .await
        .expect("resolves");
    assert_eq!(member.email, Some(EmailAddress::from("member@acme.test")));
    assert_eq!(member.org, Some(OrgId::from("acme")));
    assert!(!member.is_admin);

    let admin = resolver
        .resolve(&headers(&[(
            ACCESS_JWT_HEADER,
            &token(KID_CURRENT, json!({ "email": "sre@ops.test" })),
        )]))
        .await
        .expect("resolves");
    assert_eq!(admin.org, Some(OrgId::from("admin")));
    assert!(admin.is_admin);

    // A verified token whose `email` claim is missing or unusable is still no identity.
    for email in [json!(null), json!(""), json!("   "), json!([]), json!({})] {
        let map = headers(&[(
            ACCESS_JWT_HEADER,
            &token(KID_CURRENT, json!({ "email": email })),
        )]);
        assert_eq!(
            resolver.resolve(&map).await.expect("resolves"),
            Default::default(),
            "email {email} must not produce an identity"
        );
    }
}

#[tokio::test]
async fn every_rejection_reaches_the_route_layer_as_an_anonymous_viewer() {
    let resolver = resolver(jwt_config(&[]));
    let expired = token(
        KID_CURRENT,
        json!({ "exp": NOW_SECONDS - 61, "nbf": NOW_SECONDS - 100 }),
    );
    for candidate in [
        String::new(),
        "not-a-jwt".to_owned(),
        tamper_signature(&token(KID_CURRENT, json!({}))),
        expired,
        token(KID_ROTATED, json!({})),
        token(KID_CURRENT, json!({ "aud": "other" })),
    ] {
        let map = headers(&[(ACCESS_JWT_HEADER, &candidate)]);
        assert_eq!(
            resolver
                .resolve(&map)
                .await
                .expect("resolution never errors"),
            Default::default(),
            "{candidate:?} must resolve anonymous"
        );
    }
}

// ---------------------------------------------------------------------------
// Secret containment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_rendering_of_a_token_or_resolver_can_leak_the_assertion() {
    let assertion = token(KID_CURRENT, json!({}));
    let wrapped = AccessToken::new(&assertion);
    let resolver = resolver(jwt_config(&[]));
    let signature = assertion
        .rsplit_once('.')
        .expect("compact token")
        .1
        .to_owned();

    for rendering in [
        format!("{wrapped:?}"),
        format!("{wrapped}"),
        format!("{resolver:?}"),
        format!("{:?}", resolver.verify(&AccessToken::new("bogus")).await),
        format!(
            "{:?}",
            resolver
                .resolve(&headers(&[(ACCESS_JWT_HEADER, &assertion)]))
                .await
        ),
    ] {
        assert!(
            !rendering.contains(&assertion),
            "leaked the token: {rendering}"
        );
        assert!(
            !rendering.contains(&signature),
            "leaked the signature: {rendering}"
        );
    }
}
