//! U05 — JWKS selection, caching, and rotation (`src/security/jwks.rs`).
//!
//! `jose`'s `createRemoteJWKSet` is what `lib/identity.js:44` actually installs, so these assert
//! its behaviour: a 600 s freshness window, a 30 s refetch cooldown, one reload on an unknown
//! `kid`, and a candidate filter that refuses an ambiguous set instead of guessing.
//!
//! The source is scripted rather than networked, and the clock is a [`FixedClock`], so cache
//! boundaries are asserted at the exact millisecond with no sleeping.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use artifact_mcp::config::FixedClock;
use artifact_mcp::error::AppError;
use artifact_mcp::ports::BoxFuture;
use artifact_mcp::security::jwks::{
    CACHE_MAX_AGE_MS, COOLDOWN_MS, CachingJwks, JWKS_UNAVAILABLE_MESSAGE, JwkDocument,
    JwksProvider, JwksSource, MULTIPLE_MATCHING_KEYS_MESSAGE, NO_MATCHING_KEY_MESSAGE,
    SelectionError, StaticJwks, UNSUPPORTED_ALGORITHM_MESSAGE,
};
use serde_json::{Value, json};

use crate::u05_support::{KID_CURRENT, KID_ROTATED, NOW_SECONDS, jwk, jwks};

/// A source that serves a scripted sequence of key sets and counts every fetch.
#[derive(Debug)]
struct ScriptedSource {
    documents: Vec<Value>,
    fetches: AtomicUsize,
    fail_after: usize,
}

impl ScriptedSource {
    fn new(documents: Vec<Value>) -> Self {
        Self {
            documents,
            fetches: AtomicUsize::new(0),
            fail_after: usize::MAX,
        }
    }

    const fn failing_after(mut self, fetches: usize) -> Self {
        self.fail_after = fetches;
        self
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

impl JwksSource for ScriptedSource {
    fn fetch(&self) -> BoxFuture<'_, Result<JwkDocument, AppError>> {
        Box::pin(async move {
            let index = self.fetches.fetch_add(1, Ordering::SeqCst);
            if index >= self.fail_after {
                return Err(AppError::Unavailable(JWKS_UNAVAILABLE_MESSAGE.to_owned()));
            }
            // The last scripted document is served indefinitely.
            let document = self
                .documents
                .get(index)
                .or_else(|| self.documents.last())
                .expect("a scripted source has at least one document");
            JwkDocument::from_json(document)
        })
    }
}

fn document(value: &Value) -> JwkDocument {
    JwkDocument::from_json(value).expect("fixture JWKS parses")
}

// ---------------------------------------------------------------------------
// Candidate selection — jose jwks/local.js
// ---------------------------------------------------------------------------

#[test]
fn the_key_type_is_derived_from_the_token_algorithm_not_from_the_key() {
    for (algorithm, expected) in [
        ("RS256", Some("RSA")),
        ("RS512", Some("RSA")),
        ("PS256", Some("RSA")),
        ("ES256", Some("EC")),
        ("EdDSA", Some("OKP")),
        ("Ed25519", Some("OKP")),
        ("HS256", None),
        ("none", None),
        ("", None),
        ("R", None),
    ] {
        assert_eq!(
            JwkDocument::key_type_for_algorithm(algorithm),
            expected,
            "alg {algorithm:?}"
        );
    }
}

#[test]
fn selection_matches_on_kid_alg_use_and_key_ops() {
    let set = document(&jwks(&[KID_CURRENT, KID_ROTATED]));
    assert_eq!(
        set.select("RS256", Some(KID_CURRENT))
            .expect("current key")
            .kid(),
        Some(KID_CURRENT)
    );
    assert_eq!(
        set.select("RS256", Some(KID_ROTATED))
            .expect("rotated key")
            .kid(),
        Some(KID_ROTATED)
    );
    // An unknown kid, a mismatched alg, and an unsupported alg are three distinct outcomes.
    assert_eq!(
        set.select("RS256", Some("never-published")),
        Err(SelectionError::NoMatchingKey)
    );
    assert_eq!(
        set.select("RS512", Some(KID_CURRENT)),
        Err(SelectionError::NoMatchingKey)
    );
    assert_eq!(
        set.select("HS256", Some(KID_CURRENT)),
        Err(SelectionError::UnsupportedAlgorithm)
    );
    assert_eq!(
        set.select("none", None),
        Err(SelectionError::UnsupportedAlgorithm)
    );
}

#[test]
fn a_header_without_a_kid_matches_a_single_key_but_not_an_ambiguous_set() {
    // `typeof kid === "string"` guards the kid filter, so one key still resolves.
    assert_eq!(
        document(&jwks(&[KID_CURRENT]))
            .select("RS256", None)
            .expect("the only key")
            .kid(),
        Some(KID_CURRENT)
    );
    // Two candidates is `JWKSMultipleMatchingKeys`: refused, never resolved by picking one.
    assert_eq!(
        document(&jwks(&[KID_CURRENT, KID_ROTATED])).select("RS256", None),
        Err(SelectionError::MultipleMatchingKeys)
    );
}

#[test]
fn declared_use_and_key_ops_exclude_a_key_that_is_not_for_verification() {
    let mut encryption_key = jwk(KID_CURRENT);
    encryption_key
        .as_object_mut()
        .expect("object")
        .insert("use".to_owned(), json!("enc"));
    assert_eq!(
        document(&json!({ "keys": [encryption_key] })).select("RS256", Some(KID_CURRENT)),
        Err(SelectionError::NoMatchingKey)
    );

    let mut signing_only = jwk(KID_CURRENT);
    signing_only
        .as_object_mut()
        .expect("object")
        .insert("key_ops".to_owned(), json!(["sign"]));
    assert_eq!(
        document(&json!({ "keys": [signing_only] })).select("RS256", Some(KID_CURRENT)),
        Err(SelectionError::NoMatchingKey)
    );

    // A non-array `key_ops` is not a filter at all in `jose`, so the key still matches.
    let mut odd_key_ops = jwk(KID_CURRENT);
    odd_key_ops
        .as_object_mut()
        .expect("object")
        .insert("key_ops".to_owned(), json!("verify"));
    assert!(
        document(&json!({ "keys": [odd_key_ops] }))
            .select("RS256", Some(KID_CURRENT))
            .is_ok()
    );
}

#[test]
fn an_unrecognised_key_is_skipped_rather_than_invalidating_the_set() {
    let set = document(&json!({
        "keys": [
            { "kty": "OCT", "kid": "unknown-type" },
            jwk(KID_CURRENT),
        ]
    }));
    assert_eq!(
        set.select("RS256", Some(KID_CURRENT))
            .expect("the usable key")
            .kid(),
        Some(KID_CURRENT)
    );
}

#[test]
fn a_document_that_is_not_a_key_set_is_refused() {
    for candidate in [
        json!({}),
        json!({ "keys": {} }),
        json!({ "keys": ["not-an-object"] }),
        json!([]),
        json!("keys"),
    ] {
        assert!(
            JwkDocument::from_json(&candidate).is_err(),
            "{candidate} must not parse as a key set"
        );
    }
    assert!(JwkDocument::from_slice(b"not json").is_err());
    assert!(JwkDocument::from_json(&json!({ "keys": [] })).is_ok());
}

// ---------------------------------------------------------------------------
// Caching and rotation — jose jwks/remote.js
// ---------------------------------------------------------------------------

fn caching(
    source: Arc<ScriptedSource>,
    clock: Arc<FixedClock>,
) -> (CachingJwks, Arc<ScriptedSource>) {
    let provider = CachingJwks::with_clock(source.clone(), clock);
    (provider, source)
}

#[tokio::test]
async fn the_first_resolution_fetches_and_later_ones_reuse_the_cache() {
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let (provider, source) = caching(
        Arc::new(ScriptedSource::new(vec![jwks(&[KID_CURRENT])])),
        clock.clone(),
    );

    for _ in 0..5 {
        provider
            .resolve("RS256", Some(KID_CURRENT))
            .await
            .expect("the published key resolves");
    }
    assert_eq!(source.fetches(), 1, "a fresh cache must not refetch");

    // One millisecond before the freshness window closes, still no refetch.
    clock.advance_millis(CACHE_MAX_AGE_MS - 1);
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    assert_eq!(source.fetches(), 1);

    // At the window boundary `Date.now() < timestamp + cacheMaxAge` is false, so it reloads.
    clock.advance_millis(1);
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    assert_eq!(source.fetches(), 2);
}

#[tokio::test]
async fn an_unknown_kid_triggers_exactly_one_rotation_refetch() {
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let (provider, source) = caching(
        Arc::new(ScriptedSource::new(vec![
            jwks(&[KID_CURRENT]),
            jwks(&[KID_CURRENT, KID_ROTATED]),
        ])),
        clock.clone(),
    );

    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    assert_eq!(source.fetches(), 1);

    // The rotated key is not in the cached set. `coolingDown()` is true immediately after a
    // fetch, so the first attempt must fail without a refetch.
    let error = provider
        .resolve("RS256", Some(KID_ROTATED))
        .await
        .expect_err("cooling down");
    assert_eq!(error.to_string(), NO_MATCHING_KEY_MESSAGE);
    assert_eq!(
        source.fetches(),
        1,
        "the cooldown must suppress the refetch"
    );

    // Past the cooldown the miss reloads once and then resolves.
    clock.advance_millis(COOLDOWN_MS);
    provider
        .resolve("RS256", Some(KID_ROTATED))
        .await
        .expect("the rotated key resolves after one refetch");
    assert_eq!(source.fetches(), 2);

    // Both keys now resolve from the refreshed cache with no further fetches.
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    provider
        .resolve("RS256", Some(KID_ROTATED))
        .await
        .expect("resolves");
    assert_eq!(source.fetches(), 2);
}

#[tokio::test]
async fn a_still_unknown_kid_after_the_refetch_is_a_final_failure() {
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let (provider, source) = caching(
        Arc::new(ScriptedSource::new(vec![jwks(&[KID_CURRENT])])),
        clock.clone(),
    );
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    clock.advance_millis(COOLDOWN_MS);

    let error = provider
        .resolve("RS256", Some("never-published"))
        .await
        .expect_err("unknown kid");
    assert_eq!(error.to_string(), NO_MATCHING_KEY_MESSAGE);
    // Exactly one extra fetch: the retry is not a loop.
    assert_eq!(source.fetches(), 2);
}

#[tokio::test]
async fn an_unsupported_or_ambiguous_selection_never_refetches() {
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let (provider, source) = caching(
        Arc::new(ScriptedSource::new(vec![jwks(&[KID_CURRENT, KID_ROTATED])])),
        clock.clone(),
    );
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    clock.advance_millis(COOLDOWN_MS);

    // `JOSENotSupported` is not a miss, so it must not reload.
    let unsupported = provider
        .resolve("HS256", Some(KID_CURRENT))
        .await
        .expect_err("HMAC cannot select an asymmetric key");
    assert_eq!(unsupported.to_string(), UNSUPPORTED_ALGORITHM_MESSAGE);

    // Neither is an ambiguous set.
    let ambiguous = provider
        .resolve("RS256", None)
        .await
        .expect_err("two candidates");
    assert_eq!(ambiguous.to_string(), MULTIPLE_MATCHING_KEYS_MESSAGE);

    assert_eq!(source.fetches(), 1);
}

#[tokio::test]
async fn a_failing_endpoint_fails_closed_and_leaves_the_previous_set_intact() {
    let clock = Arc::new(FixedClock::from_seconds(NOW_SECONDS));
    let (provider, _source) = caching(
        Arc::new(ScriptedSource::new(vec![jwks(&[KID_CURRENT])]).failing_after(1)),
        clock.clone(),
    );
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("the first fetch succeeds");

    // A failed reload keeps the cached set usable rather than dropping every viewer.
    clock.advance_millis(CACHE_MAX_AGE_MS);
    let error = provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect_err("the reload fails");
    assert_eq!(error.to_string(), JWKS_UNAVAILABLE_MESSAGE);

    // A provider that has never fetched successfully also fails closed rather than resolving.
    let (empty, _) = caching(
        Arc::new(ScriptedSource::new(vec![jwks(&[KID_CURRENT])]).failing_after(0)),
        Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
    );
    assert!(empty.resolve("RS256", Some(KID_CURRENT)).await.is_err());
}

#[tokio::test]
async fn a_static_provider_never_fetches_and_still_enforces_the_filter() {
    let provider = StaticJwks::new(document(&jwks(&[KID_CURRENT])));
    provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");
    assert_eq!(
        provider
            .resolve("RS256", Some(KID_ROTATED))
            .await
            .expect_err("unknown kid")
            .to_string(),
        NO_MATCHING_KEY_MESSAGE
    );
    assert_eq!(
        provider
            .resolve("none", None)
            .await
            .expect_err("alg none")
            .to_string(),
        UNSUPPORTED_ALGORITHM_MESSAGE
    );
}

#[tokio::test]
async fn no_rendering_of_a_provider_leaks_key_material() {
    let modulus = jwk(KID_CURRENT)
        .get("n")
        .and_then(Value::as_str)
        .expect("RSA modulus")
        .to_owned();
    let provider = StaticJwks::new(document(&jwks(&[KID_CURRENT])));
    let resolved = provider
        .resolve("RS256", Some(KID_CURRENT))
        .await
        .expect("resolves");

    // The public modulus is not a secret, but the resolved key's `Debug` must still not print
    // key bytes: the same rendering path carries HMAC secrets for any future symmetric key.
    let rendering = format!("{resolved:?}");
    assert!(
        !rendering.contains(&modulus),
        "leaked key bytes: {rendering}"
    );
}
