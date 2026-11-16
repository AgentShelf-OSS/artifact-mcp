use super::*;
use artifact_mcp::{
    config::{AppConfig, NanoIdSource},
    model::{
        EmailAddress,
        state::{StateKey, StateValue},
    },
    persistence::{db::DbPool, viewer_state_service::SqliteViewerState},
    ports::BoxFuture,
    security::audit::MutationAudit,
};
use axum::{body::Body, http::Request};
use std::sync::atomic::{AtomicBool, Ordering};
use tower::ServiceExt;

const TOKEN: &[u8] = b"synthetic-test-credential-01234567890123456789";

fn fixture(org: &str) -> (AppState, DbPool) {
    let manager = r2d2_sqlite::SqliteConnectionManager::memory();
    let pool = r2d2::Pool::builder().max_size(1).build(manager).unwrap();
    {
        let conn = pool.get().unwrap();
        conn.execute_batch("CREATE TABLE artifacts(id TEXT PRIMARY KEY, client_id TEXT DEFAULT 'fixture', org TEXT, title TEXT DEFAULT 'Fixture', description TEXT DEFAULT '', bytes INTEGER DEFAULT 0, created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '', uploader_label TEXT DEFAULT '', owner_email TEXT, is_bundle INTEGER DEFAULT 0, entry TEXT DEFAULT 'index.html', revision INTEGER DEFAULT 1, category TEXT DEFAULT '', hidden INTEGER DEFAULT 0, body_sha256 TEXT DEFAULT '', artifact_key TEXT);
            CREATE TABLE artifact_durability_intents(artifact_id TEXT);
            CREATE TABLE artifact_state(artifact_id TEXT NOT NULL REFERENCES artifacts(id),scope TEXT NOT NULL,viewer TEXT NOT NULL DEFAULT '',key TEXT NOT NULL,value TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 1,updated_at TEXT NOT NULL,updated_by TEXT NOT NULL,PRIMARY KEY(artifact_id,scope,viewer,key));").unwrap();
        conn.execute(
            "INSERT INTO artifacts(id,org) VALUES(?1,?2)",
            [ARTIFACT_ID, org],
        )
        .unwrap();
    }
    let store = ArtifactStore::new(
        pool.clone(),
        std::env::temp_dir(),
        AppConfig::default().storage,
        Arc::new(NanoIdSource::default()),
        [7; 32],
    );
    let service = SqliteViewerState::new(pool.clone(), None);
    (
        AppState::new(Arc::new(store), Arc::new(service), TOKEN).unwrap(),
        pool,
    )
}

fn event(command: Command, id: &str) -> Event {
    let now = OffsetDateTime::now_utc();
    validate(
        &LogRequest {
            command,
            request_id: id.into(),
            occurred_at: now.format(&Rfc3339).unwrap(),
        },
        now,
    )
    .unwrap()
}

async fn value(app: &AppState, event: &Event) -> StateValue {
    app.state
        .get(
            app.grant().await.unwrap(),
            event.shard.clone(),
            StateAudience::Org,
        )
        .await
        .unwrap()
        .unwrap()
}

async fn seed(app: &AppState, event: &Event, document: Value) {
    app.state
        .put(
            app.grant().await.unwrap(),
            event.shard.clone(),
            serde_json::from_value(document).unwrap(),
            Some(0),
            WRITER.into(),
            StateAudience::Org,
        )
        .await
        .unwrap();
}

#[test]
fn all_commands_have_the_expected_amounts_and_flags() {
    for (command, expected) in [
        (
            Command::Bottle2oz,
            json!({"k":"bottle","ml":59.147,"src":"formula"}),
        ),
        (
            Command::Bottle4oz,
            json!({"k":"bottle","ml":118.294,"src":"formula"}),
        ),
        (
            Command::WetDiaper,
            json!({"k":"nappy","wet":true,"dirty":false,"col":""}),
        ),
        (
            Command::DirtyDiaper,
            json!({"k":"nappy","wet":false,"dirty":true,"col":""}),
        ),
    ] {
        let e = event(command, "abcdefghijklmnop");
        for (key, want) in expected.as_object().unwrap() {
            assert_eq!(&e.entry[key], want);
        }
    }
}

#[test]
fn local_month_and_timestamp_stay_together_at_month_boundary() {
    let occurred = "2026-10-31T23:59:59-06:00";
    let now = OffsetDateTime::parse(occurred, &Rfc3339).unwrap();
    let e = validate(
        &LogRequest {
            command: Command::Bottle2oz,
            request_id: "abcdefghijklmnop".into(),
            occurred_at: occurred.into(),
        },
        now,
    )
    .unwrap();
    assert_eq!(e.shard, "baby.log.202610");
    assert_eq!(
        e.entry["t"].as_i64().unwrap(),
        i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap()
    );
}

#[test]
fn rejects_unknown_commands_fields_and_untrusted_times() {
    assert!(serde_json::from_value::<LogRequest>(json!({"command":"bottle_3oz","request_id":"abcdefghijklmnop","occurred_at":"2026-10-31T23:59:59Z"})).is_err());
    assert!(serde_json::from_value::<LogRequest>(json!({"command":"bottle_2oz","ounces":4,"request_id":"abcdefghijklmnop","occurred_at":"2026-10-31T23:59:59Z"})).is_err());
    let now = OffsetDateTime::now_utc();
    for id in ["short", "abcdefghijklmnop/other"] {
        assert!(
            validate(
                &LogRequest {
                    command: Command::WetDiaper,
                    request_id: id.into(),
                    occurred_at: now.format(&Rfc3339).unwrap()
                },
                now
            )
            .is_err()
        );
    }
    assert!(
        validate(
            &LogRequest {
                command: Command::WetDiaper,
                request_id: "abcdefghijklmnop".into(),
                occurred_at: (now - time::Duration::minutes(6)).format(&Rfc3339).unwrap()
            },
            now
        )
        .is_err()
    );
}

#[tokio::test]
async fn saves_and_retries_once_without_creating_a_second_entry() {
    let (app, _) = fixture(ORG);
    let e = event(Command::Bottle2oz, "abcdefghijklmnop");
    let grant = app.grant().await.unwrap();
    assert!(!append(app.state.as_ref(), grant.clone(), &e).await.unwrap());
    assert!(append(app.state.as_ref(), grant.clone(), &e).await.unwrap());
    let saved = value(&app, &e).await;
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.value.into_value()["items"], json!([e.entry]));
    let mut altered = e;
    altered.entry["ml"] = json!(118.294);
    assert!(matches!(
        append(app.state.as_ref(), grant, &altered).await,
        Err(LogError::Conflict)
    ));
    assert_eq!(value(&app, &altered).await.revision, 1);
}

#[tokio::test]
async fn tombstones_and_corrupt_shards_are_never_overwritten() {
    for document in [
        json!({"v":1,"items":[],"tomb":["voice_abcdefghijklmnop"]}),
        json!({"v":2,"items":[],"tomb":[]}),
        json!({"v":1,"items":"broken","tomb":[]}),
    ] {
        let (app, _) = fixture(ORG);
        let e = event(Command::WetDiaper, "abcdefghijklmnop");
        seed(&app, &e, document.clone()).await;
        assert!(
            append(app.state.as_ref(), app.grant().await.unwrap(), &e)
                .await
                .is_err()
        );
        let saved = value(&app, &e).await;
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.value.into_value(), document);
    }
}

// Insert a real competing mobile edit immediately before the first voice CAS.
struct RacingState {
    inner: Arc<dyn ViewerStateService>,
    first: AtomicBool,
}
impl ViewerStateService for RacingState {
    fn list(
        &self,
        a: AuthorizedArtifact,
        s: StateAudience,
    ) -> BoxFuture<'_, Result<Vec<StateKey>, artifact_mcp::error::AppError>> {
        self.inner.list(a, s)
    }
    fn get(
        &self,
        a: AuthorizedArtifact,
        k: String,
        s: StateAudience,
    ) -> BoxFuture<'_, Result<Option<StateValue>, artifact_mcp::error::AppError>> {
        self.inner.get(a, k, s)
    }
    fn delete(
        &self,
        a: AuthorizedArtifact,
        k: String,
        s: StateAudience,
        m: MutationAudit,
    ) -> BoxFuture<'_, Result<(), artifact_mcp::error::AppError>> {
        self.inner.delete(a, k, s, m)
    }
    fn put(
        &self,
        a: AuthorizedArtifact,
        k: String,
        v: OrderedJson,
        r: Option<u64>,
        w: EmailAddress,
        s: StateAudience,
    ) -> BoxFuture<'_, Result<StateValue, StateError>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                let current = self
                    .inner
                    .get(a.clone(), k.clone(), s.clone())
                    .await?
                    .unwrap();
                let mut d = current.value.into_value();
                d["items"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":"mobile-entry","t":100,"k":"nappy","wet":true}));
                d["tomb"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("deleted-mobile-entry"));
                d["mobile_extra"] = json!("keep");
                self.inner
                    .put(
                        a.clone(),
                        k.clone(),
                        serde_json::from_value(d).unwrap(),
                        Some(current.revision),
                        "mobile@example.test".into(),
                        s.clone(),
                    )
                    .await?;
            }
            self.inner.put(a, k, v, r, w, s).await
        })
    }
}

#[tokio::test]
async fn conflict_retry_preserves_the_new_mobile_entry_tombstone_and_extra_fields() {
    let (app, _) = fixture(ORG);
    let e = event(Command::Bottle4oz, "abcdefghijklmnop");
    seed(
        &app,
        &e,
        json!({"v":1,"items":[{"id":"earlier-entry","t":10}],"tomb":[],"earlier_extra":"keep"}),
    )
    .await;
    let racing = RacingState {
        inner: app.state.clone(),
        first: AtomicBool::new(true),
    };
    assert!(
        !append(&racing, app.grant().await.unwrap(), &e)
            .await
            .unwrap()
    );
    let saved = value(&app, &e).await;
    assert_eq!(saved.revision, 3);
    let d = saved.value.into_value();
    assert_eq!(d["items"].as_array().unwrap().len(), 3);
    assert_eq!(d["items"][1]["id"], "mobile-entry");
    assert_eq!(d["tomb"], json!(["deleted-mobile-entry"]));
    assert_eq!(d["earlier_extra"], "keep");
    assert_eq!(d["mobile_extra"], "keep");
    assert_eq!(d["items"][2], e.entry);
}

#[tokio::test]
async fn http_auth_limits_and_foreign_organization_fail_closed() {
    let (app, _) = fixture(ORG);
    let unauth = Request::builder()
        .method("POST")
        .uri("/log")
        .body(Body::from("not json"))
        .unwrap();
    assert_eq!(
        router(app.clone()).oneshot(unauth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let authorized = |body: String| {
        Request::builder()
            .method("POST")
            .uri("/log")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", std::str::from_utf8(TOKEN).unwrap()),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    assert_eq!(
        router(app.clone())
            .oneshot(authorized("x".repeat(1025)))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let body=json!({"command":"bottle_2oz","request_id":"abcdefghijklmnop","occurred_at":OffsetDateTime::now_utc().format(&Rfc3339).unwrap()}).to_string();
    let response = router(app).oneshot(authorized(body.clone())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(result["ok"], true);
    let (foreign, _) = fixture("other-organization");
    assert_eq!(
        router(foreign)
            .oneshot(authorized(body))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}
