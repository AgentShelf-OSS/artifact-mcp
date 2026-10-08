//! ADR-0012 scheduled reminders and Web Push: configuration, validation, viewer routes, the MCP
//! tool, the sweeper fan-out, and the sender against a local mock push service.

use std::sync::{Arc, Mutex};

use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use artifact_mcp::{
    config::{AppConfig, FixedClock, MapEnv, Secret, SeedKeys},
    integrations::push_runtime::{
        HttpPushTransport, PushService, parse_retry_after_ms, retry_delay_ms,
    },
    mcp::protocol::OrderedJson,
    persistence::{
        db::{self, Database, DbPool},
        migrations::MigrationContext,
        push::{self, PushError, PushPayload},
    },
};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

use super::{u03_support::TempDataDir, u20_runtime::runtime};

const VAPID_KEY: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
const VAPID_PUBLIC: &str =
    "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
const ENC_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const NOW: i64 = 1_790_000_000_000;

fn enabled_env() -> MapEnv {
    MapEnv::empty()
        .with("WEB_PUSH_VAPID_PRIVATE_KEY", VAPID_KEY)
        .with("WEB_PUSH_SUBJECT", "mailto:ops@example.test")
        .with("WEB_PUSH_ENDPOINT_HOSTS", "push.test,*.push.test")
        .with("WEBHOOK_ENC_KEY", ENC_KEY)
}

fn enabled_config(data_dir: &std::path::Path) -> AppConfig {
    let mut config = AppConfig::from_source(&enabled_env()).expect("config");
    config.data_dir = data_dir.to_owned();
    config.public_base_url = "https://artifacts.example.test".to_owned();
    config
}

/// A deterministic user-agent key pair and auth secret.
struct Browser {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Browser {
    fn new(seed: u8) -> Self {
        let mut scalar = [seed; 32];
        scalar[0] = 1;
        Self {
            secret: SecretKey::from_slice(&scalar).expect("scalar"),
            auth: [seed; 16],
        }
    }
    fn p256dh(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.secret.public_key().to_encoded_point(false))
    }
    fn auth(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.auth)
    }
    /// RFC 8291 receiver: decrypt a single-record `aes128gcm` body.
    fn decrypt(&self, body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        assert_eq!(&body[16..20], &4096_u32.to_be_bytes());
        assert_eq!(body[20], 65);
        let as_public = &body[21..86];
        let ciphertext = &body[86..];
        let as_key = p256::PublicKey::from_sec1_bytes(as_public).expect("as key");
        let shared =
            p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), as_key.as_affine());
        let ua_public = self.secret.public_key().to_encoded_point(false);
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(ua_public.as_bytes());
        info.extend_from_slice(as_public);
        let mut ikm = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(&self.auth), shared.raw_secret_bytes())
            .expand(&info, &mut ikm)
            .unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0_u8; 16], [0_u8; 12]);
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();
        let mut plain = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), ciphertext)
            .expect("authentic");
        assert_eq!(plain.pop(), Some(0x02), "single record delimiter");
        plain
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn web_push_is_disabled_by_default_and_needs_the_encryption_key() {
    let defaults = AppConfig::from_source(&MapEnv::empty()).unwrap();
    assert!(!defaults.web_push_enabled());
    assert_eq!(defaults.web_push.endpoint_hosts.len(), 6);
    assert!(
        defaults
            .web_push
            .endpoint_hosts
            .contains(&"*.notify.windows.com".to_owned())
    );

    let enabled = AppConfig::from_source(&enabled_env()).unwrap();
    assert!(enabled.web_push_enabled());
    let vapid = enabled.web_push.vapid.as_ref().unwrap();
    assert_eq!(vapid.public_key, VAPID_PUBLIC);
    assert_eq!(
        enabled.web_push.endpoint_hosts,
        vec!["push.test", "*.push.test"]
    );
    assert!(
        !format!("{enabled:?}").contains(VAPID_KEY),
        "private key is redacted"
    );

    let without_enc = MapEnv::empty()
        .with("WEB_PUSH_VAPID_PRIVATE_KEY", VAPID_KEY)
        .with("WEB_PUSH_SUBJECT", "https://example.test/contact");
    let config = AppConfig::from_source(&without_enc).unwrap();
    assert!(config.web_push.vapid.is_some());
    assert!(!config.web_push_enabled());
    // A subject alone (no key) is ignored, like Node.
    assert!(AppConfig::from_source(&MapEnv::empty().with("WEB_PUSH_SUBJECT", "nope")).is_ok());
}

#[test]
fn invalid_web_push_values_are_configuration_errors() {
    for key in [
        "short",
        "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw=",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "__________________________________________8",
    ] {
        let env = enabled_env().with("WEB_PUSH_VAPID_PRIVATE_KEY", key);
        assert!(AppConfig::from_source(&env).is_err(), "{key}");
    }
    for subject in [
        "",
        "mailto:",
        "http://example.test",
        "https://",
        "mailto:a b@x",
        "ops@example.test",
    ] {
        let env = enabled_env().with("WEB_PUSH_SUBJECT", subject);
        assert!(AppConfig::from_source(&env).is_err(), "{subject:?}");
    }
    for hosts in [
        "https://push.test",
        "push.test:443",
        "*.",
        "-bad.test",
        "a/b",
        "*.com",
        "com",
        "localhost",
        "127.0.0.1",
        "*.0.1",
        "push.123",
        "::1",
        "[::1]",
    ] {
        let env = enabled_env().with("WEB_PUSH_ENDPOINT_HOSTS", hosts);
        assert!(AppConfig::from_source(&env).is_err(), "{hosts}");
    }
    let env = enabled_env().with("WEB_PUSH_ENDPOINT_HOSTS", " PUSH.test , push.test ,, ");
    assert_eq!(
        AppConfig::from_source(&env)
            .unwrap()
            .web_push
            .endpoint_hosts,
        vec!["push.test"]
    );
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn endpoint_and_key_validation_follow_the_contract() {
    let hosts = vec!["push.test".to_owned(), "*.push.apple.com".to_owned()];
    assert!(push::valid_endpoint("https://push.test/abc", &hosts));
    assert!(push::valid_endpoint("https://push.test:443/abc", &hosts));
    assert!(push::valid_endpoint("https://web.push.apple.com/x", &hosts));
    for bad in [
        "http://push.test/abc",
        "https://push.test:8443/abc",
        "https://user:pw@push.test/abc",
        "https://push.apple.com/abc",
        "https://evil.test/abc",
        "not a url",
        "https://127.0.0.1/abc",
        "https://2130706433/abc",
        "https://[::1]/abc",
        "",
    ] {
        assert!(!push::valid_endpoint(bad, &hosts), "{bad}");
    }
    let long = format!("https://push.test/{}", "a".repeat(1024));
    assert!(!push::valid_endpoint(&long, &hosts));

    let browser = Browser::new(7);
    assert!(push::valid_keys(&browser.p256dh(), &browser.auth()));
    assert!(push::valid_keys(
        &browser.p256dh(),
        &format!("{}==", browser.auth())
    ));
    let mut off_curve = URL_SAFE_NO_PAD.decode(browser.p256dh()).unwrap();
    off_curve[64] ^= 1;
    assert!(!push::valid_keys(
        &URL_SAFE_NO_PAD.encode(off_curve),
        &browser.auth()
    ));
    assert!(!push::valid_keys(
        &browser.p256dh(),
        &URL_SAFE_NO_PAD.encode([0_u8; 15])
    ));
    assert!(!push::valid_keys(&browser.p256dh(), "not+base64url"));
}

#[test]
fn reminder_time_and_text_validation() {
    let n = |value: i64| OrderedJson::Number(value.into());
    let now = NOW;
    assert_eq!(
        push::resolve_fire_at(Some(&n(now + 60_000)), None, now),
        Ok(now + 60_000)
    );
    assert_eq!(
        push::resolve_fire_at(None, Some(&n(30 * 86_400)), now),
        Ok(now + 30 * 86_400_000)
    );
    for (fire_at, delay) in [
        (Some(n(now + 59_999)), None),
        (None, Some(n(30 * 86_400 + 1))),
        (Some(n(now + 120_000)), Some(n(120))),
        (None, None),
        (Some(OrderedJson::Null), None),
        (Some(OrderedJson::Null), Some(n(120))),
        (Some(n(now + 120_000)), Some(OrderedJson::Null)),
        (Some(OrderedJson::string("1")), None),
    ] {
        assert_eq!(
            push::resolve_fire_at(fire_at.as_ref(), delay.as_ref(), now),
            Err(PushError::BadTime)
        );
    }
    let s = |value: &str| OrderedJson::string(value);
    let text = push::validate_text(Some(&s("  Diaper  ")), None).unwrap();
    assert_eq!((text.title.as_str(), text.body.as_str()), ("Diaper", ""));
    assert!(push::validate_text(Some(&s(&"é".repeat(80))), Some(&s("line\nbreak"))).is_ok());
    for (title, body) in [
        (Some(s("   ")), None),
        (Some(s(&"x".repeat(81))), None),
        (Some(s("tab\there")), None),
        (Some(s("ok")), Some(s(&"x".repeat(241)))),
        (Some(s("ok")), Some(OrderedJson::Null)),
        (None, None),
    ] {
        assert_eq!(
            push::validate_text(title.as_ref(), body.as_ref()),
            Err(PushError::BadText)
        );
    }
}

#[test]
fn retry_policy_honors_backoff_and_retry_after() {
    assert_eq!(retry_delay_ms(1, None), 5_000);
    assert_eq!(retry_delay_ms(2, None), 30_000);
    assert_eq!(retry_delay_ms(3, None), 120_000);
    assert_eq!(retry_delay_ms(9, None), 600_000);
    assert_eq!(retry_delay_ms(1, Some(90_000)), 90_000);
    assert_eq!(retry_delay_ms(1, Some(3_600_000)), 600_000);
    assert_eq!(parse_retry_after_ms("120", 0), Some(120_000));
    assert_eq!(
        parse_retry_after_ms("Thu, 01 Jan 1970 00:01:00 GMT", 30_000),
        Some(30_000)
    );
    assert_eq!(parse_retry_after_ms("soon", 0), None);
}

// ---------------------------------------------------------------------------
// Sweeper
// ---------------------------------------------------------------------------

fn pool(dir: &TempDataDir) -> DbPool {
    let pool = Database::open_with(dir.path(), &MigrationContext::empty(), None).expect("db");
    let conn = db::checkout(&pool).unwrap();
    for (id, org) in [("art-acme", "acme"), ("art-beta", "beta")] {
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES (?, 'c', ?, 't')",
            params![id, org],
        )
        .unwrap();
    }
    pool
}

fn subscribe(
    conn: &mut Connection,
    id: &str,
    org: &str,
    email: &str,
    endpoint: &str,
    browser: &Browser,
) {
    push::save_subscription(
        conn,
        &format!(
            "v1:AAAAAAAAAAAAAAAA:{}:AAAAAAAAAAAAAAAAAAAAAA==",
            base64::engine::general_purpose::STANDARD.encode(endpoint)
        ),
        &push::NewSubscription {
            org,
            viewer_email: email,
            endpoint,
            p256dh: &browser.p256dh(),
            auth: &browser.auth(),
            label: "",
        },
        id.to_owned(),
        "2026-10-08 00:00:00",
    )
    .unwrap();
}

fn arm(
    conn: &mut Connection,
    artifact: &str,
    org: &str,
    scope: push::ReminderScope,
    owner: &str,
    key: &str,
    fire_at: i64,
) -> i64 {
    let text = push::ReminderText {
        title: format!("{key} title"),
        body: "body".into(),
    };
    push::set_reminder(
        conn,
        &push::ReminderWrite {
            artifact_id: artifact,
            org,
            scope,
            owner,
            key,
            fire_at,
            text: &text,
            created_by: "viewer",
        },
        "2026-10-08 00:00:00",
    )
    .unwrap()
}

#[test]
fn sweeper_fans_out_to_opted_in_recipients_by_scope() {
    use push::ReminderScope::{Org, Viewer};
    let dir = TempDataDir::new("push-sweep");
    let pool = pool(&dir);
    let mut conn = db::checkout(&pool).unwrap();
    let browser = Browser::new(3);
    // alice: two acme devices, opted in. bob: acme device, not opted in.
    // carol: beta admin-style viewer opted in to the acme artifact under org beta.
    subscribe(
        &mut conn,
        "s-alice-1",
        "acme",
        "Alice@Acme.test",
        "https://push.test/a1",
        &browser,
    );
    subscribe(
        &mut conn,
        "s-alice-2",
        "acme",
        "alice@acme.test",
        "https://push.test/a2",
        &browser,
    );
    subscribe(
        &mut conn,
        "s-bob",
        "acme",
        "bob@acme.test",
        "https://push.test/b",
        &browser,
    );
    subscribe(
        &mut conn,
        "s-carol",
        "beta",
        "carol@beta.test",
        "https://push.test/c",
        &browser,
    );
    push::opt_in(&conn, "art-acme", "acme", "alice@acme.test", "t").unwrap();
    push::opt_in(&conn, "art-acme", "beta", "carol@beta.test", "t").unwrap();

    arm(
        &mut conn,
        "art-acme",
        "acme",
        Org,
        "",
        "org-due",
        NOW - 1_000,
    );
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Viewer,
        "alice@acme.test",
        "mine",
        NOW,
    );
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Viewer,
        "bob@acme.test",
        "bob-not-opted",
        NOW,
    );
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Org,
        "",
        "stale",
        NOW - push::STALE_AFTER_MS - 1,
    );
    arm(&mut conn, "art-acme", "acme", Org, "", "future", NOW + 1);

    let report = push::sweep_due(&mut conn, NOW, "https://artifacts.example.test/").unwrap();
    assert_eq!(report.stale, 1);
    assert_eq!(report.fired, 3);
    // org: alice's two devices (carol's opt-in is under beta); viewer: alice's two devices.
    assert_eq!(report.deliveries, 4);

    let rows: Vec<(String, String, String, i64, i64)> = conn
        .prepare("SELECT subscription_id, reminder_key, payload, next_attempt_at, expires_at FROM push_deliveries ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(rows.iter().all(|row| row.0.starts_with("s-alice")));
    assert!(
        rows.iter()
            .all(|row| row.3 == NOW && row.4 == NOW + 30 * 60 * 1000)
    );
    let payload: PushPayload = serde_json::from_str(&rows[0].2).unwrap();
    assert_eq!(payload.url, "https://artifacts.example.test/art-acme");
    assert_eq!(payload.tag, format!("art-acme:{}", rows[0].1));
    assert!(rows[0].2.starts_with("{\"title\":"));
    assert!(
        !rows.iter().any(|row| row.2.contains('@')),
        "no emails in payloads"
    );

    let states: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT key, state, fired_at FROM artifact_reminders ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (key, state, fired_at) in states {
        let expected = if key == "future" { "armed" } else { "fired" };
        assert_eq!(state, expected, "{key}");
        assert_eq!(fired_at.is_some(), expected == "fired");
    }
    // A second sweep finds nothing new.
    assert_eq!(
        push::sweep_due(&mut conn, NOW, "https://x").unwrap(),
        push::SweepReport::default()
    );
}

#[test]
fn reminder_upsert_revision_and_armed_limit() {
    use push::ReminderScope::{Org, Viewer};
    let dir = TempDataDir::new("push-limit");
    let pool = pool(&dir);
    let mut conn = db::checkout(&pool).unwrap();
    assert_eq!(
        arm(&mut conn, "art-acme", "acme", Org, "", "k", NOW + 100_000),
        1
    );
    assert_eq!(
        arm(&mut conn, "art-acme", "acme", Org, "", "k", NOW + 200_000),
        2
    );
    for index in 1..15 {
        arm(
            &mut conn,
            "art-acme",
            "acme",
            Org,
            "",
            &format!("k{index}"),
            NOW + 100_000,
        );
    }
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Viewer,
        "v@acme.test",
        "mine",
        NOW + 100_000,
    );
    let text = push::ReminderText {
        title: "t".into(),
        body: String::new(),
    };
    let write = |key: &'static str| push::ReminderWrite {
        artifact_id: "art-acme",
        org: "acme",
        scope: Org,
        owner: "",
        key,
        fire_at: NOW + 100_000,
        text: &text,
        created_by: "viewer",
    };
    assert_eq!(
        push::set_reminder(&mut conn, &write("overflow"), "t"),
        Err(PushError::ReminderLimit)
    );
    assert_eq!(
        push::set_reminder(&mut conn, &write("k"), "t"),
        Ok(3),
        "existing armed key is exempt"
    );
    // A fired key that is re-armed counts as a new row.
    conn.execute(
        "UPDATE artifact_reminders SET state='fired' WHERE key='k1'",
        [],
    )
    .unwrap();
    push::set_reminder(&mut conn, &write("overflow"), "t").unwrap();
    assert_eq!(
        push::set_reminder(&mut conn, &write("k1"), "t"),
        Err(PushError::ReminderLimit)
    );
    // Cascade on artifact deletion.
    conn.execute("DELETE FROM artifacts WHERE id='art-acme'", [])
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM artifact_reminders", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------
// Sender against a local mock push service
// ---------------------------------------------------------------------------

/// Scripted responses by request path; unscripted paths answer `201`.
type Script = std::collections::HashMap<String, (u16, Option<&'static str>)>;

#[derive(Clone, Default)]
struct MockPush {
    script: Arc<Mutex<Script>>,
    received: Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>,
}

async fn mock_handler(
    State(mock): State<MockPush>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    mock.received
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), headers, body));
    let (status, retry_after) = mock
        .script
        .lock()
        .unwrap()
        .remove(uri.path())
        .unwrap_or((201, None));
    let mut response = StatusCode::from_u16(status).unwrap().into_response();
    if let Some(value) = retry_after {
        response
            .headers_mut()
            .insert("retry-after", value.parse().unwrap());
    }
    response
}

async fn start_mock() -> (MockPush, String) {
    let mock = MockPush::default();
    let app = Router::new()
        .route("/{*path}", post(mock_handler))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, format!("http://{address}"))
}

#[tokio::test]
async fn sender_encrypts_signs_and_applies_every_outcome() {
    let dir = TempDataDir::new("push-send");
    let pool = pool(&dir);
    let config = enabled_config(dir.path());
    let clock = Arc::new(FixedClock::from_millis(NOW));
    let service = PushService::with_clock(&config, pool.clone(), clock.clone())
        .unwrap()
        .expect("enabled");
    let (mock, origin) = start_mock().await;
    let transport = HttpPushTransport::with_origin_override(&origin).unwrap();
    let browser = Browser::new(9);

    // Five subscriptions through the real service (encrypted endpoints).
    let mut ids = Vec::new();
    for (index, email) in [
        "a@acme.test",
        "b@acme.test",
        "c@acme.test",
        "d@acme.test",
        "e@acme.test",
    ]
    .iter()
    .enumerate()
    {
        let id = service
            .save_subscription(
                "acme".into(),
                (*email).into(),
                format!("https://push.test/sub/{index}"),
                browser.p256dh(),
                browser.auth(),
                String::new(),
            )
            .await
            .unwrap();
        service
            .set_opt_in("art-acme".into(), "acme".into(), (*email).into(), true)
            .await
            .unwrap();
        ids.push(id);
    }
    {
        let conn = db::checkout(&pool).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT endpoint_ciphertext FROM push_subscriptions LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored.starts_with("v1:") && !stored.contains("push.test"),
            "endpoint encrypted at rest"
        );
    }
    let n = |value: i64| OrderedJson::Number(value.into());
    let title = OrderedJson::string("Diaper change due");
    service
        .set_reminder(
            "art-acme".into(),
            "acme".into(),
            push::ReminderScope::Org,
            String::new(),
            "diaper".into(),
            artifact_mcp::integrations::push_runtime::ReminderInput {
                fire_at: None,
                delay_seconds: Some(&n(60)),
                title: Some(&title),
                body: None,
            },
            "viewer".into(),
        )
        .await
        .unwrap();
    clock.advance_millis(60_000);
    assert_eq!(service.sweep_once().await.unwrap().deliveries, 5);

    // Outcomes by device: accepted, gone, rate limited (Retry-After), server error, forbidden.
    *mock.script.lock().unwrap() = [
        ("/sub/0", (201, None)),
        ("/sub/1", (410, None)),
        ("/sub/2", (429, Some("90"))),
        ("/sub/3", (503, None)),
        ("/sub/4", (403, None)),
    ]
    .into_iter()
    .map(|(path, outcome)| (path.to_owned(), outcome))
    .collect();
    assert_eq!(service.send_due(&transport).await.unwrap(), 5);
    let received = mock.received.lock().unwrap().clone();
    assert_eq!(received.len(), 5);
    let (_, headers, body) = received
        .iter()
        .find(|(path, _, _)| path == "/sub/0")
        .expect("device 0 was sent");
    assert_eq!(headers["ttl"], "1800");
    assert_eq!(headers["urgency"], "high");
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(
        headers["topic"].to_str().unwrap(),
        artifact_mcp::integrations::web_push::topic("art-acme:diaper")
    );
    let authorization = headers["authorization"].to_str().unwrap();
    assert!(
        authorization.starts_with("vapid t=")
            && authorization.ends_with(&format!(", k={VAPID_PUBLIC}"))
    );
    let jwt = authorization
        .trim_start_matches("vapid t=")
        .split(',')
        .next()
        .unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jwt.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["aud"], "https://push.test");
    assert_eq!(claims["sub"], "mailto:ops@example.test");
    assert_eq!(claims["exp"], (NOW + 60_000) / 1000 + 12 * 3600);
    let payload: Value = serde_json::from_slice(&browser.decrypt(body)).unwrap();
    assert_eq!(
        payload,
        json!({"title":"Diaper change due","body":"","url":"https://artifacts.example.test/art-acme","tag":"art-acme:diaper"})
    );
    // Same audience reuses the cached JWT.
    assert_eq!(
        received[1].1["authorization"],
        received[0].1["authorization"]
    );

    {
        let conn = db::checkout(&pool).unwrap();
        let state = |sub: &str| -> Option<(String, i64, i64, Option<i64>)> {
            conn.query_row(
                "SELECT state, attempts, next_attempt_at, last_status FROM push_deliveries WHERE subscription_id = ?",
                [sub],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
        };
        let now = NOW + 60_000;
        assert_eq!(
            state(&ids[0]),
            Some((
                "accepted".into(),
                1,
                now + push::DELIVERY_LEASE_MS,
                Some(201)
            ))
        );
        assert_eq!(
            state(&ids[1]),
            None,
            "410 removes the subscription and cascades"
        );
        assert_eq!(
            state(&ids[2]),
            Some(("pending".into(), 1, now + 90_000, Some(429)))
        );
        assert_eq!(
            state(&ids[3]),
            Some(("pending".into(), 1, now + 5_000, Some(503)))
        );
        assert_eq!(
            state(&ids[4]),
            Some(("dead".into(), 1, now + push::DELIVERY_LEASE_MS, Some(403)))
        );
        let subs: i64 = conn
            .query_row("SELECT COUNT(*) FROM push_subscriptions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(subs, 4);
        let (success, failures): (Option<String>, i64) = conn
            .query_row(
                "SELECT last_success_at, failure_count FROM push_subscriptions WHERE id = ?",
                [&ids[0]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(success.is_some());
        assert_eq!(failures, 0);
        let failures: i64 = conn
            .query_row(
                "SELECT failure_count FROM push_subscriptions WHERE id = ?",
                [&ids[3]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failures, 1);
    }

    let counts = service.telemetry().snapshot();
    assert_eq!(
        (
            counts.deliveries_accepted,
            counts.subscriptions_removed,
            counts.deliveries_retried,
            counts.deliveries_dead
        ),
        (1, 1, 2, 1)
    );
    let metrics = service.telemetry().render_prometheus();
    assert!(metrics.contains("artifact_mcp_push_deliveries_total{outcome=\"accepted\"} 1"));
    assert!(!metrics.contains("push.test") && !metrics.contains('@'));

    // Nothing is due before its backoff; the 503 retry runs after 5s and is then accepted.
    assert_eq!(service.send_due(&transport).await.unwrap(), 0);
    clock.advance_millis(5_000);
    assert_eq!(service.send_due(&transport).await.unwrap(), 1);
    // Past expiry, the remaining retry is marked dead without a send.
    clock.advance_millis(31 * 60 * 1000);
    assert_eq!(service.send_due(&transport).await.unwrap(), 0);
    let conn = db::checkout(&pool).unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM push_deliveries WHERE state='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(mock.received.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn sender_rechecks_the_allowlist_and_treats_network_errors_as_retryable() {
    let dir = TempDataDir::new("push-allowlist");
    let pool = pool(&dir);
    let config = enabled_config(dir.path());
    let clock = Arc::new(FixedClock::from_millis(NOW));
    let service = PushService::with_clock(&config, pool.clone(), clock.clone())
        .unwrap()
        .unwrap();
    let browser = Browser::new(5);
    for (index, email) in ["a@acme.test", "b@acme.test"].iter().enumerate() {
        service
            .save_subscription(
                "acme".into(),
                (*email).into(),
                format!("https://push.test/{index}"),
                browser.p256dh(),
                browser.auth(),
                String::new(),
            )
            .await
            .unwrap();
        service
            .set_opt_in("art-acme".into(), "acme".into(), (*email).into(), true)
            .await
            .unwrap();
    }
    {
        let mut conn = db::checkout(&pool).unwrap();
        arm(
            &mut conn,
            "art-acme",
            "acme",
            push::ReminderScope::Org,
            "",
            "k",
            NOW,
        );
    }
    service.sweep_once().await.unwrap();
    // The operator narrows the allowlist after the save: the sender refuses the old host.
    let mut narrowed = config.clone();
    narrowed.web_push.endpoint_hosts = vec!["other.test".into()];
    let narrowed_service = PushService::with_clock(&narrowed, pool.clone(), clock.clone())
        .unwrap()
        .unwrap();
    // Unreachable origin: a refused connection is a network error.
    let transport = HttpPushTransport::with_origin_override("http://127.0.0.1:9").unwrap();
    {
        let conn = db::checkout(&pool).unwrap();
        conn.execute("UPDATE push_deliveries SET next_attempt_at = next_attempt_at + 1 WHERE id = (SELECT MAX(id) FROM push_deliveries)", []).unwrap();
    }
    assert_eq!(narrowed_service.send_due(&transport).await.unwrap(), 1);
    assert_eq!(narrowed_service.telemetry().snapshot().deliveries_dead, 1);
    clock.advance_millis(1);
    assert_eq!(service.send_due(&transport).await.unwrap(), 1);
    assert_eq!(service.telemetry().snapshot().deliveries_retried, 1);
    let conn = db::checkout(&pool).unwrap();
    let states: Vec<(String, Option<i64>)> = conn
        .prepare("SELECT state, last_status FROM push_deliveries ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        states,
        vec![("dead".into(), None), ("pending".into(), None)]
    );
}

// ---------------------------------------------------------------------------
// Routes and MCP through the real runtime
// ---------------------------------------------------------------------------

struct Observer;
impl runtime::StartupObserver for Observer {
    fn stage(&self, _: runtime::StartupStage) {}
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    email: Option<&str>,
    body: Option<Value>,
    extra: &[(&str, &str)],
) -> (u16, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-artifact-mutation", "1");
    if !extra.iter().any(|(name, _)| *name == "sec-fetch-site") {
        builder = builder.header("sec-fetch-site", "same-origin");
    }
    if let Some(email) = email {
        builder = builder.header("cf-access-authenticated-user-email", email);
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn runtime_config(dir: &TempDataDir, enabled: bool) -> AppConfig {
    let mut config = if enabled {
        enabled_config(dir.path())
    } else {
        AppConfig {
            data_dir: dir.path().to_owned(),
            ..AppConfig::defaults()
        }
    };
    config.listen_host = "127.0.0.1".to_owned();
    config.audit_ledger_hmac_key = Some(Secret::new(ENC_KEY));
    config.access.trust_headers = true;
    config
        .access
        .domain_orgs
        .insert("acme.test".into(), "acme".into());
    config
        .access
        .domain_orgs
        .insert("beta.test".into(), "beta".into());
    config.seed_keys = SeedKeys::parse(
        "publisher:acme:push-test-publisher-secret,other:beta:push-test-beta-secret",
    );
    config.ingress.state_per_window = 500;
    config.ingress.verified_viewers_per_window = 1000;
    config
}

async fn publish(app: &Router, secret: &str) -> String {
    let (status, _, published) = call(app, "POST", "/mcp", None, Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish_artifact","arguments":{"html":"<h1>Log</h1>","title":"Log"}}})), &[("authorization", &format!("Bearer {secret}"))]).await;
    assert_eq!(status, 200);
    published["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn tool(app: &Router, secret: &str, arguments: Value) -> Value {
    let (status, _, body) = call(app, "POST", "/mcp", None, Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"set_artifact_reminder","arguments":arguments}})), &[("authorization", &format!("Bearer {secret}"))]).await;
    assert_eq!(status, 200);
    body["result"].clone()
}

#[tokio::test]
async fn push_routes_follow_the_viewer_contract() {
    let dir = TempDataDir::new("push-routes");
    let config = runtime_config(&dir, true);
    let browser = Browser::new(11);
    runtime::run_with_bind(config, Arc::new(Observer), move |_, _, app| async move {
        let id = publish(&app, "push-test-publisher-secret").await;
        let alice = Some("alice@acme.test");
        let mallory = Some("mallory@beta.test");

        // Static files: no viewer needed.
        let (status, headers, _) = call(&app, "GET", "/sw.js", None, None, &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/javascript; charset=utf-8");
        assert_eq!(headers["service-worker-allowed"], "/");
        assert!(headers["cache-control"].to_str().unwrap().starts_with("no-cache"));
        let (status, headers, manifest) = call(&app, "GET", "/manifest.webmanifest", None, None, &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "application/manifest+json");
        assert!(manifest.is_object());
        let (status, headers, _) = call(&app, "GET", "/icons/badge-72.png", None, None, &[]).await;
        assert_eq!((status, headers["content-type"].to_str().unwrap()), (200, "image/png"));
        assert_eq!(call(&app, "GET", "/icons/nope.png", None, None, &[]).await.0, 404);

        // Config requires a viewer.
        assert_eq!(call(&app, "GET", "/push/config", None, None, &[]).await.0, 404);
        let (status, headers, config) = call(&app, "GET", "/push/config", alice, None, &[]).await;
        assert_eq!((status, config), (200, json!({"enabled": true, "vapid_public_key": VAPID_PUBLIC})));
        assert!(headers["cache-control"].to_str().unwrap().starts_with("no-store"));

        // Subscriptions.
        let sub = |endpoint: &str| json!({"endpoint": endpoint, "keys": {"p256dh": browser.p256dh(), "auth": browser.auth()}, "label": "iPhone", "expirationTime": null});
        let (status, _, saved) = call(&app, "PUT", "/push/subscriptions", alice, Some(sub("https://push.test/a")), &[]).await;
        assert_eq!(status, 200);
        let first_id = saved["id"].as_str().unwrap().to_owned();
        assert_eq!(first_id.len(), 36);
        let (_, _, again) = call(&app, "PUT", "/push/subscriptions", alice, Some(sub("https://push.test/a")), &[]).await;
        assert_eq!(again["id"], first_id, "upsert by endpoint");
        assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(sub("http://push.test/a")), &[]).await.2, json!({"error":"bad_endpoint"}));
        assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/x","keys":{"p256dh":"AA","auth":browser.auth()}})), &[]).await.2, json!({"error":"bad_keys"}));
        assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/x","keys":{"p256dh":browser.p256dh(),"auth":browser.auth(),"x":1}})), &[]).await.2, json!({"error":"bad_keys"}));
        assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/x","extra":1})), &[]).await.2, json!({"error":"bad_body"}));
        assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/x","label":"x".repeat(61)})), &[]).await.2, json!({"error":"bad_body"}));
        let (status, _, body) = call(&app, "PUT", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/x","label":"y".repeat(9000)})), &[]).await;
        assert_eq!((status, body), (413, json!({"error":"too_large"})));
        // Cap: an 11th device evicts the oldest.
        for index in 0..10 {
            assert_eq!(call(&app, "PUT", "/push/subscriptions", alice, Some(sub(&format!("https://push.test/cap{index}"))), &[]).await.0, 200);
        }
        // Mallory cannot delete Alice's endpoint (still 204).
        assert_eq!(call(&app, "DELETE", "/push/subscriptions", mallory, Some(json!({"endpoint":"https://push.test/cap9"})), &[]).await.0, 204);
        assert_eq!(call(&app, "DELETE", "/push/subscriptions", alice, Some(json!({"endpoint":"https://push.test/cap9","x":1})), &[]).await.2, json!({"error":"bad_body"}));

        // Opt-in and status; concealed for another org.
        assert_eq!(call(&app, "GET", &format!("/{id}/push"), mallory, None, &[]).await.2, json!({"error":"Not found"}));
        assert_eq!(call(&app, "GET", &format!("/{id}/push"), alice, None, &[]).await.2, json!({"enabled":true,"opted_in":false,"devices":10}));
        assert_eq!(call(&app, "PUT", &format!("/{id}/push/optin"), alice, Some(json!({})), &[]).await.2, json!({"opted_in":true}));
        assert_eq!(call(&app, "GET", &format!("/{id}/push"), alice, None, &[]).await.2["opted_in"], true);
        // Mutation guard: a cross-site write with a session is refused.
        assert_eq!(call(&app, "PUT", &format!("/{id}/push/optin"), alice, None, &[("sec-fetch-site", "cross-site"), ("cookie", "CF_Authorization=x")]).await.0, 403);

        // Reminders.
        let base = format!("/{id}/reminders");
        let (status, _, saved) = call(&app, "PUT", &format!("{base}/diaper"), alice, Some(json!({"delay_seconds": 7200, "title": " Diaper ", "body": "Change"})), &[]).await;
        assert_eq!(status, 200);
        assert_eq!((saved["key"].clone(), saved["scope"].clone(), saved["revision"].clone()), (json!("diaper"), json!("org"), json!(1)));
        assert_eq!(call(&app, "PUT", &format!("{base}/diaper"), alice, Some(json!({"delay_seconds": 3600, "title": "Diaper"})), &[]).await.2["revision"], 2);
        assert_eq!(call(&app, "PUT", &format!("{base}/mine?scope=viewer"), alice, Some(json!({"delay_seconds": 120, "title": "Mine"})), &[]).await.2["scope"], "viewer");
        assert_eq!(call(&app, "PUT", &format!("{base}/bad%20key"), alice, Some(json!({"delay_seconds": 120, "title": "x"})), &[]).await.2, json!({"error":"bad_key"}));
        assert_eq!(call(&app, "PUT", &format!("{base}/k?scope=team"), alice, Some(json!({"delay_seconds": 120, "title": "x"})), &[]).await.2, json!({"error":"bad_scope"}));
        assert_eq!(call(&app, "PUT", &format!("{base}/k"), alice, Some(json!({"delay_seconds": 10, "title": "x"})), &[]).await.2, json!({"error":"bad_time"}));
        assert_eq!(call(&app, "PUT", &format!("{base}/k"), alice, Some(json!({"delay_seconds": 120, "title": ""})), &[]).await.2, json!({"error":"bad_text"}));
        assert_eq!(call(&app, "PUT", &format!("{base}/k"), alice, Some(json!({"delay_seconds": 120, "title": "x", "scope": "org"})), &[]).await.2, json!({"error":"bad_body"}));
        assert_eq!(call(&app, "PUT", &format!("{base}/k"), mallory, Some(json!({"delay_seconds": 120, "title": "x"})), &[]).await.2, json!({"error":"Not found"}));
        let (_, _, listed) = call(&app, "GET", &base, alice, None, &[]).await;
        assert_eq!(listed["reminders"].as_array().unwrap().len(), 1);
        assert_eq!(listed["reminders"][0]["title"], "Diaper");
        assert_eq!(listed["reminders"][0]["body"], "");
        let (_, _, private) = call(&app, "GET", &format!("{base}?scope=viewer"), Some("bob@acme.test"), None, &[]).await;
        assert_eq!(private, json!({"reminders": []}), "viewer scope lists only the caller's rows");
        for index in 0..14 {
            assert_eq!(call(&app, "PUT", &format!("{base}/r{index}"), alice, Some(json!({"delay_seconds": 120, "title": "x"})), &[]).await.0, 200);
        }
        let (status, _, body) = call(&app, "PUT", &format!("{base}/over"), alice, Some(json!({"delay_seconds": 120, "title": "x"})), &[]).await;
        assert_eq!((status, body), (409, json!({"error":"reminder_limit"})));
        assert_eq!(call(&app, "DELETE", &format!("{base}/r0"), alice, None, &[]).await.0, 204);
        assert_eq!(call(&app, "DELETE", &format!("{base}/r0"), alice, None, &[]).await.0, 204);

        // Null time fields are bad_time on routes.
        for body in [json!({"fire_at": null, "delay_seconds": 120, "title": "x"}), json!({"fire_at": null, "title": "x"})] {
            assert_eq!(call(&app, "PUT", &format!("{base}/r1"), alice, Some(body), &[]).await.2, json!({"error":"bad_time"}));
        }
        // Over MCP the input schema rejects null before the tool runs.
        let (_, _, rpc) = call(&app, "POST", "/mcp", None, Some(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"set_artifact_reminder","arguments":{"id": id, "key": "k", "fire_at": null, "title": "x"}}})), &[("authorization", "Bearer push-test-publisher-secret")]).await;
        assert_eq!(rpc["error"]["code"], -32602);

        // MCP tool.
        let result = tool(&app, "push-test-publisher-secret", json!({"id": id, "key": "feed", "delay_seconds": 600, "title": "Feed"})).await;
        assert_eq!(result["structuredContent"]["scope"], "org");
        assert_eq!(result["structuredContent"]["revision"], 1);
        let result = tool(&app, "push-test-publisher-secret", json!({"id": id, "key": "feed", "clear": true})).await;
        assert_eq!(result["structuredContent"], json!({"id": id, "key": "feed", "cleared": true}));
        let result = tool(&app, "push-test-publisher-secret", json!({"id": id, "key": "bad key", "clear": true})).await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().starts_with("Invalid reminder key"));
        let result = tool(&app, "push-test-publisher-secret", json!({"id": id, "key": "late", "delay_seconds": 5, "title": "x"})).await;
        assert!(result["content"][0]["text"].as_str().unwrap().starts_with("Invalid reminder time"));
        let result = tool(&app, "push-test-beta-secret", json!({"id": id, "key": "k", "delay_seconds": 600, "title": "x"})).await;
        assert_eq!(result["content"][0]["text"], format!("Unknown artifact: {id}"));
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disabled_feature_conceals_every_route_but_config() {
    let dir = TempDataDir::new("push-disabled");
    let mut config = runtime_config(&dir, false);
    // The disabled answer comes before the state budget, so a budget of 1 never yields a 429.
    config.ingress.state_per_window = 1;
    runtime::run_with_bind(config, Arc::new(Observer), move |_, _, app| async move {
        let id = publish(&app, "push-test-publisher-secret").await;
        let alice = Some("alice@acme.test");
        assert_eq!(
            call(&app, "GET", "/push/config", alice, None, &[]).await.2,
            json!({"enabled": false, "vapid_public_key": null})
        );
        assert_eq!(call(&app, "GET", "/sw.js", None, None, &[]).await.0, 200);
        let disabled = json!({"error":"push_disabled"});
        for (method, path) in [
            ("PUT", "/push/subscriptions".to_owned()),
            ("DELETE", "/push/subscriptions".to_owned()),
            ("GET", format!("/{id}/push")),
            ("PUT", format!("/{id}/push/optin")),
            ("DELETE", format!("/{id}/push/optin")),
            ("GET", format!("/{id}/reminders")),
            ("PUT", format!("/{id}/reminders/k")),
            ("DELETE", format!("/{id}/reminders/k")),
        ] {
            let (status, _, body) = call(&app, method, &path, alice, Some(json!({})), &[]).await;
            assert_eq!((status, body), (404, disabled.clone()), "{method} {path}");
        }
        // Concealment wins over the disabled answer for another org.
        assert_eq!(
            call(
                &app,
                "GET",
                &format!("/{id}/push"),
                Some("m@beta.test"),
                None,
                &[]
            )
            .await
            .2,
            json!({"error":"Not found"})
        );
        let result = tool(
            &app,
            "push-test-publisher-secret",
            json!({"id": id, "key": "k", "delay_seconds": 600, "title": "x"}),
        )
        .await;
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "Web Push reminders are not configured on this server."
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn sweeper_follows_the_artifacts_current_org_and_includes_admins() {
    use push::ReminderScope::{Org, Viewer};
    let dir = TempDataDir::new("push-move");
    let pool = pool(&dir);
    let mut conn = db::checkout(&pool).unwrap();
    let browser = Browser::new(4);
    subscribe(
        &mut conn,
        "s-old",
        "acme",
        "old@acme.test",
        "https://push.test/old",
        &browser,
    );
    subscribe(
        &mut conn,
        "s-new",
        "beta",
        "new@beta.test",
        "https://push.test/new",
        &browser,
    );
    subscribe(
        &mut conn,
        "s-admin",
        "admin",
        "root@admin.test",
        "https://push.test/admin",
        &browser,
    );
    for email in ["old@acme.test", "new@beta.test", "root@admin.test"] {
        push::opt_in(&conn, "art-acme", "acme", email, "t").unwrap();
    }
    arm(&mut conn, "art-acme", "acme", Org, "", "org", NOW);
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Viewer,
        "old@acme.test",
        "old-own",
        NOW,
    );
    arm(
        &mut conn,
        "art-acme",
        "acme",
        Viewer,
        "new@beta.test",
        "new-own",
        NOW,
    );
    // The artifact moves to beta after the reminders and opt-ins were recorded under acme.
    conn.execute(
        "UPDATE artifacts SET org = 'beta' WHERE id = 'art-acme'",
        [],
    )
    .unwrap();
    let report = push::sweep_due(&mut conn, NOW, "https://x").unwrap();
    assert_eq!(report.fired, 3);
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT reminder_key, subscription_id FROM push_deliveries ORDER BY reminder_key, subscription_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("new-own".to_owned(), "s-new".to_owned()),
            ("org".to_owned(), "s-admin".to_owned()),
            ("org".to_owned(), "s-new".to_owned()),
        ],
        "old-org viewer gets nothing; new-org and admin viewers get the org reminder"
    );
    assert_eq!(
        push::device_count(&conn, "beta", "root@admin.test").unwrap(),
        1
    );
    assert_eq!(
        push::device_count(&conn, "beta", "old@acme.test").unwrap(),
        0
    );
}

#[test]
fn org_refresh_and_handover_drop_only_pending_deliveries() {
    let dir = TempDataDir::new("push-handover");
    let pool = pool(&dir);
    let mut conn = db::checkout(&pool).unwrap();
    let browser = Browser::new(6);
    subscribe(
        &mut conn,
        "s1",
        "acme",
        "a@acme.test",
        "https://push.test/1",
        &browser,
    );
    subscribe(
        &mut conn,
        "s2",
        "acme",
        "a@acme.test",
        "https://push.test/2",
        &browser,
    );
    push::opt_in(&conn, "art-acme", "acme", "a@acme.test", "t").unwrap();
    arm(
        &mut conn,
        "art-acme",
        "acme",
        push::ReminderScope::Org,
        "",
        "k",
        NOW,
    );
    assert_eq!(
        push::sweep_due(&mut conn, NOW, "https://x")
            .unwrap()
            .deliveries,
        2
    );
    conn.execute(
        "UPDATE push_deliveries SET state = 'accepted' WHERE subscription_id = 's2'",
        [],
    )
    .unwrap();
    // Handover of s1 to another viewer drops its pending delivery.
    subscribe(
        &mut conn,
        "unused",
        "acme",
        "b@acme.test",
        "https://push.test/1",
        &browser,
    );
    let count =
        |conn: &Connection, sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM push_deliveries WHERE subscription_id = 's1'"
        ),
        0
    );
    // Org refresh keeps finished rows and moves the subscription.
    assert_eq!(
        push::refresh_subscription_org(&mut conn, "A@acme.test", "beta").unwrap(),
        1
    );
    assert_eq!(
        push::refresh_subscription_org(&mut conn, "a@acme.test", "beta").unwrap(),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM push_deliveries WHERE subscription_id = 's2'"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM push_subscriptions WHERE id = 's2' AND org = 'beta'"
        ),
        1
    );
}

#[test]
fn claims_lease_rows_and_finished_rows_are_pruned() {
    let dir = TempDataDir::new("push-lease");
    let pool = pool(&dir);
    let mut conn = db::checkout(&pool).unwrap();
    let browser = Browser::new(8);
    subscribe(
        &mut conn,
        "s1",
        "acme",
        "a@acme.test",
        "https://push.test/1",
        &browser,
    );
    push::opt_in(&conn, "art-acme", "acme", "a@acme.test", "t").unwrap();
    arm(
        &mut conn,
        "art-acme",
        "acme",
        push::ReminderScope::Org,
        "",
        "k",
        NOW,
    );
    push::sweep_due(&mut conn, NOW, "https://x").unwrap();
    let first = push::claim_due(&mut conn, NOW, 10, push::DELIVERY_LEASE_MS).unwrap();
    assert_eq!(first.len(), 1);
    assert!(
        push::claim_due(&mut conn, NOW + 1, 10, push::DELIVERY_LEASE_MS)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        push::claim_due(
            &mut conn,
            NOW + push::DELIVERY_LEASE_MS,
            10,
            push::DELIVERY_LEASE_MS
        )
        .unwrap()
        .len(),
        1,
        "lease expiry makes the row due again"
    );
    conn.execute(
        "UPDATE push_deliveries SET state = 'accepted', updated_at = '2000-01-01 00:00:00'",
        [],
    )
    .unwrap();
    assert_eq!(push::prune_finished(&conn, NOW).unwrap(), 1);
}

#[tokio::test]
async fn concurrent_senders_never_send_the_same_delivery_twice() {
    let dir = TempDataDir::new("push-concurrent");
    let pool = pool(&dir);
    let config = enabled_config(dir.path());
    let clock = Arc::new(FixedClock::from_millis(NOW));
    let one = PushService::with_clock(&config, pool.clone(), clock.clone())
        .unwrap()
        .unwrap();
    let two = PushService::with_clock(&config, pool.clone(), clock.clone())
        .unwrap()
        .unwrap();
    let browser = Browser::new(12);
    for index in 0..20 {
        let email = format!("v{index}@acme.test");
        one.save_subscription(
            "acme".into(),
            email.clone(),
            format!("https://push.test/c{index}"),
            browser.p256dh(),
            browser.auth(),
            String::new(),
        )
        .await
        .unwrap();
        one.set_opt_in("art-acme".into(), "acme".into(), email, true)
            .await
            .unwrap();
    }
    {
        let mut conn = db::checkout(&pool).unwrap();
        arm(
            &mut conn,
            "art-acme",
            "acme",
            push::ReminderScope::Org,
            "",
            "k",
            NOW,
        );
    }
    assert_eq!(one.sweep_once().await.unwrap().deliveries, 20);
    let (mock, origin) = start_mock().await;
    let transport = HttpPushTransport::with_origin_override(&origin).unwrap();
    let (a, b) = tokio::join!(one.send_due(&transport), two.send_due(&transport));
    assert_eq!(a.unwrap() + b.unwrap(), 20);
    let mut paths: Vec<String> = mock
        .received
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.0.clone())
        .collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 20);
    assert_eq!(mock.received.lock().unwrap().len(), 20);
}
