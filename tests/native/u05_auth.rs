//! U05 — publisher bearer-key authentication (`src/security/auth.rs`).
//!
//! Covers credential extraction and precedence, the hash input, the derived admin flag, the
//! invariant-1 org pinning matrix, and secret containment. The Node-parity half of the hash and
//! extraction rules lives in `u05_node_parity.rs`.

use std::sync::Arc;

use artifact_mcp::error::AppError;
use artifact_mcp::model::{ClientId, OrgId, PublisherIdentity};
use artifact_mcp::ports::{BoxFuture, PublisherAuthenticator};
use artifact_mcp::security::auth::{
    ADMIN_ORG_REQUIRED_MESSAGE, BearerKey, EmptyKeyDirectory, KeyAuthenticator, KeyHash, OrgTarget,
    PublisherKeyDirectory, PublisherKeyRecord, UNAUTHORIZED_MESSAGE, bearer_key, identity_for,
    sha256_hex,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// A directory holding exactly one active key, so "unknown" and "revoked" are indistinguishable
/// from the authenticator's side — which is the property `lib/auth.js` relies on.
///
/// Its `Debug` redacts the secret. A derived one would leak it through
/// `KeyAuthenticator`'s own derived `Debug`, which is exactly the failure the containment test
/// below is written to catch — so the fixture has to hold itself to the same rule the real
/// directory must.
struct SingleKeyDirectory {
    secret: String,
    record: PublisherKeyRecord,
}

impl std::fmt::Debug for SingleKeyDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SingleKeyDirectory")
            .field("secret", &"<redacted>")
            .field("record", &self.record)
            .finish()
    }
}

impl SingleKeyDirectory {
    fn new(secret: &str, org: &str) -> Self {
        Self {
            secret: secret.to_owned(),
            record: PublisherKeyRecord {
                client_id: ClientId::from("agent-one"),
                org: OrgId::from(org),
                label: "Agent One".to_owned(),
                role: "author".to_owned(),
            },
        }
    }
}

impl PublisherKeyDirectory for SingleKeyDirectory {
    fn find_active<'a>(
        &'a self,
        hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        Box::pin(async move {
            Ok((hash.expose() == sha256_hex(&self.secret)).then(|| self.record.clone()))
        })
    }
}

/// A directory whose lookup fails, proving a storage error is not silently downgraded to a 401.
#[derive(Debug)]
struct FailingDirectory;

impl PublisherKeyDirectory for FailingDirectory {
    fn find_active<'a>(
        &'a self,
        _hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
}

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

fn presented(pairs: &[(&str, &str)]) -> Option<String> {
    bearer_key(&headers(pairs)).map(|key| key.expose().to_owned())
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

#[test]
fn sha256_hex_is_lowercase_hex_of_the_utf8_bytes() {
    // Published SHA-256 vectors; `lib/auth.js` hashes the same UTF-8 bytes.
    assert_eq!(
        sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // Non-ASCII must hash as UTF-8, not as any Latin-1 rendering.
    assert_eq!(sha256_hex("ünïcødé-🎉"), sha256_hex("ünïcødé-🎉"));
    assert_ne!(sha256_hex("secret"), sha256_hex("Secret"));
    assert_eq!(KeyHash::of("abc").expose(), sha256_hex("abc"));
    assert_eq!(BearerKey::new("abc").hash().expose(), sha256_hex("abc"));
}

// ---------------------------------------------------------------------------
// Credential extraction — lib/auth.js:12-18
// ---------------------------------------------------------------------------

#[test]
fn bearer_header_is_matched_case_insensitively_and_trimmed() {
    assert_eq!(
        presented(&[("authorization", "Bearer s3cret")]).as_deref(),
        Some("s3cret")
    );
    assert_eq!(
        presented(&[("authorization", "bearer s3cret")]).as_deref(),
        Some("s3cret")
    );
    assert_eq!(
        presented(&[("authorization", "BEARER s3cret")]).as_deref(),
        Some("s3cret")
    );
    assert_eq!(
        presented(&[("authorization", "  Bearer \t  s3cret \t ")]).as_deref(),
        Some("s3cret")
    );
    // `(.+?)` is lazy but `\s*$` is anchored, so interior spaces survive.
    assert_eq!(
        presented(&[("authorization", "Bearer a b c")]).as_deref(),
        Some("a b c")
    );
}

#[test]
fn a_matching_bearer_header_shadows_x_api_key_even_when_it_yields_nothing() {
    // `bearer()` returns from the `if (m)` branch, so the fallback is unreachable behind a
    // syntactically valid but empty Bearer value. [lib/auth.js:15]
    assert_eq!(
        presented(&[("authorization", "Bearer    "), ("x-api-key", "fallback")]),
        None
    );
    assert_eq!(
        presented(&[("authorization", "Bearer real"), ("x-api-key", "fallback")]).as_deref(),
        Some("real")
    );
}

#[test]
fn a_non_matching_authorization_falls_through_to_x_api_key() {
    assert_eq!(
        presented(&[
            ("authorization", "Basic dXNlcjpwdw=="),
            ("x-api-key", "fallback")
        ])
        .as_deref(),
        Some("fallback")
    );
    // `\s+` requires whitespace after the scheme, so `Bearerx` is not a Bearer header.
    assert_eq!(
        presented(&[("authorization", "Bearerx nope"), ("x-api-key", "fallback")]).as_deref(),
        Some("fallback")
    );
    assert_eq!(
        presented(&[("authorization", "Bearer"), ("x-api-key", "fallback")]).as_deref(),
        Some("fallback")
    );
}

#[test]
fn absent_blank_and_whitespace_only_credentials_are_no_credential() {
    assert_eq!(presented(&[]), None);
    assert_eq!(presented(&[("x-api-key", "")]), None);
    assert_eq!(presented(&[("x-api-key", "   \t ")]), None);
    assert_eq!(presented(&[("authorization", "")]), None);
    assert_eq!(
        presented(&[("x-api-key", "  s3cret  ")]).as_deref(),
        Some("s3cret")
    );
}

#[test]
fn a_non_utf8_header_is_treated_as_absent() {
    let mut map = HeaderMap::new();
    map.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_bytes(&[0xff, 0xfe]).expect("opaque bytes are a legal header value"),
    );
    assert_eq!(bearer_key(&map).map(|key| key.expose().to_owned()), None);
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_known_active_key_authenticates() {
    let authenticator = KeyAuthenticator::new(Arc::new(SingleKeyDirectory::new("s3cret", "acme")));
    let identity = authenticator
        .authenticate(&headers(&[("authorization", "Bearer s3cret")]))
        .await
        .expect("known key authenticates");
    assert_eq!(identity.client_id, ClientId::from("agent-one"));
    assert_eq!(identity.org, OrgId::from("acme"));
    assert_eq!(identity.label, "Agent One");
    assert!(!identity.is_admin());
}

#[tokio::test]
async fn missing_unknown_and_revoked_keys_share_one_401() {
    let authenticator = KeyAuthenticator::new(Arc::new(SingleKeyDirectory::new("s3cret", "acme")));
    // A revoked row is filtered by the directory, so it arrives here as "unknown".
    for request in [
        headers(&[]),
        headers(&[("authorization", "Bearer wrong")]),
        headers(&[("x-api-key", "wrong")]),
        headers(&[("authorization", "Bearer  ")]),
    ] {
        let error = authenticator
            .authenticate(&request)
            .await
            .expect_err("rejected");
        assert!(
            matches!(&error, AppError::Unauthorized(message) if message == UNAUTHORIZED_MESSAGE),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn an_empty_directory_never_authenticates() {
    let authenticator = KeyAuthenticator::new(Arc::new(EmptyKeyDirectory));
    let error = authenticator
        .authenticate(&headers(&[("authorization", "Bearer anything")]))
        .await
        .expect_err("rejected");
    assert!(matches!(error, AppError::Unauthorized(_)));
}

#[tokio::test]
async fn a_lookup_failure_is_not_downgraded_to_unauthorized() {
    let authenticator = KeyAuthenticator::new(Arc::new(FailingDirectory));
    let error = authenticator
        .authenticate(&headers(&[("authorization", "Bearer anything")]))
        .await
        .expect_err("rejected");
    assert!(matches!(error, AppError::Internal), "{error:?}");
}

// ---------------------------------------------------------------------------
// Admin derivation — lib/access.js:35
// ---------------------------------------------------------------------------

fn identity(org: &str) -> PublisherIdentity {
    identity_for(PublisherKeyRecord {
        client_id: ClientId::from("agent"),
        org: OrgId::from(org),
        label: String::new(),
        role: "author".to_owned(),
    })
}

#[test]
fn admin_is_exactly_the_literal_org_admin() {
    assert!(identity("admin").is_admin());
    for org in [
        "Admin",
        "ADMIN",
        "admins",
        "admin ",
        " admin",
        "administrator",
        "acme",
        "",
    ] {
        assert!(!identity(org).is_admin(), "org {org:?} must not be admin");
    }
}

// ---------------------------------------------------------------------------
// Invariant 1 — org pinning [lib/mcp.js:334-337,436-437]
// ---------------------------------------------------------------------------

#[test]
fn a_non_admin_key_can_never_be_redirected_to_another_org() {
    let acme = identity("acme");
    for requested in [
        None,
        Some(""),
        Some("   "),
        Some("other"),
        Some("admin"),
        Some(" other "),
    ] {
        assert_eq!(
            OrgTarget::pinned(&acme, requested).org(),
            &OrgId::from("acme"),
            "requested {requested:?} must not override a pinned org"
        );
        assert_eq!(
            OrgTarget::explicit(&acme, requested)
                .expect("a non-admin key never needs an explicit org")
                .org(),
            &OrgId::from("acme"),
        );
    }
}

#[test]
fn an_admin_key_targets_a_named_org_and_otherwise_falls_back() {
    let admin = identity("admin");
    assert_eq!(
        OrgTarget::pinned(&admin, Some("acme")).org(),
        &OrgId::from("acme")
    );
    // `args.org.trim()` is what is stored, not the raw argument.
    assert_eq!(
        OrgTarget::pinned(&admin, Some("  acme  ")).org(),
        &OrgId::from("acme")
    );
    // A blank org is falsy in Node, so the key's own org wins — which is the literal `admin`.
    assert_eq!(OrgTarget::pinned(&admin, None).org(), &OrgId::from("admin"));
    assert_eq!(
        OrgTarget::pinned(&admin, Some("   ")).org(),
        &OrgId::from("admin")
    );
}

#[test]
fn the_category_shape_requires_an_admin_key_to_name_an_org() {
    let admin = identity("admin");
    assert_eq!(
        OrgTarget::explicit(&admin, Some(" acme "))
            .expect("named")
            .into_org(),
        OrgId::from("acme")
    );
    for requested in [None, Some(""), Some("  ")] {
        let error = OrgTarget::explicit(&admin, requested).expect_err("admin must name an org");
        assert!(
            matches!(&error, AppError::Validation(message) if message == ADMIN_ORG_REQUIRED_MESSAGE),
            "{error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Secret containment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_rendering_of_a_key_or_its_hash_can_leak_it() {
    const SECRET: &str = "sup3r-s3cret-publisher-key";
    let key = BearerKey::new(SECRET);
    let hash = key.hash();
    let digest = sha256_hex(SECRET);

    let renderings = [
        format!("{key:?}"),
        format!("{key}"),
        format!("{hash:?}"),
        format!("{hash}"),
        format!(
            "{:?}",
            KeyAuthenticator::new(Arc::new(SingleKeyDirectory::new(SECRET, "acme")))
        ),
    ];
    for rendering in &renderings {
        assert!(!rendering.contains(SECRET), "leaked the key: {rendering}");
        assert!(!rendering.contains(&digest), "leaked the hash: {rendering}");
    }

    // The 401 an attacker actually receives must be a constant.
    let authenticator = KeyAuthenticator::new(Arc::new(EmptyKeyDirectory));
    let error = authenticator
        .authenticate(&headers(&[("authorization", &format!("Bearer {SECRET}"))]))
        .await
        .expect_err("rejected");
    let rendered = format!("{error}|{error:?}");
    assert!(!rendered.contains(SECRET), "leaked the key: {rendered}");
    assert!(!rendered.contains(&digest), "leaked the hash: {rendered}");
    assert_eq!(error.to_string(), UNAUTHORIZED_MESSAGE);
}
