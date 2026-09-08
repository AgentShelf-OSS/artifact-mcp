//! Viewer-state persistence and HTTP contract coverage.

use artifact_mcp::{
    mcp::protocol::OrderedJson,
    model::{ArtifactId, EmailAddress},
    persistence::state::{self, StateError, StateScope},
};
use rusqlite::Connection;

fn database() -> (Connection, ArtifactId, EmailAddress) {
    let conn = Connection::open_in_memory().expect("sqlite");
    conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE artifacts (id TEXT PRIMARY KEY); CREATE TABLE artifact_state (artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE, scope TEXT NOT NULL CHECK(scope IN ('org','viewer')), viewer TEXT NOT NULL DEFAULT '', key TEXT NOT NULL, value TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL, updated_by TEXT NOT NULL, PRIMARY KEY (artifact_id,scope,viewer,key)); INSERT INTO artifacts VALUES ('state-fixture');").expect("schema");
    (
        conn,
        ArtifactId::from("state-fixture"),
        EmailAddress::from("viewer@example.test"),
    )
}

#[test]
fn state_route_contract_persists_json_and_revision_conflicts() {
    let (mut conn, id, email) = database();
    let object = OrderedJson::Object(vec![("highlight".into(), OrderedJson::Bool(true))]);
    let first = state::set(
        &mut conn,
        &id,
        "reading.note",
        &object,
        None,
        &email,
        StateScope::Org,
    )
    .expect("put");
    assert_eq!(first.revision, 1);
    let second = state::set(
        &mut conn,
        &id,
        "reading.note",
        &object,
        Some(1),
        &email,
        StateScope::Org,
    )
    .expect("put");
    assert_eq!(second.revision, 2);
    let conflict = state::set(
        &mut conn,
        &id,
        "reading.note",
        &object,
        Some(1),
        &email,
        StateScope::Org,
    )
    .expect_err("stale revision");
    assert!(matches!(conflict, StateError::Conflict(c) if c.revision == 2));
}

#[test]
fn state_route_contract_enforces_key_and_value_caps_and_cascade() {
    let (mut conn, id, email) = database();
    assert!(matches!(
        state::set(
            &mut conn,
            &id,
            "bad key",
            &OrderedJson::Null,
            None,
            &email,
            StateScope::Org
        ),
        Err(StateError::App(_))
    ));
    let oversized = OrderedJson::string("x".repeat(state::MAX_VALUE_BYTES));
    assert!(matches!(
        state::set(
            &mut conn,
            &id,
            "large",
            &oversized,
            None,
            &email,
            StateScope::Org
        ),
        Err(StateError::App(
            artifact_mcp::error::AppError::PayloadTooLarge
        ))
    ));
    state::set(
        &mut conn,
        &id,
        "survives",
        &OrderedJson::Null,
        None,
        &email,
        StateScope::Org,
    )
    .expect("put");
    conn.execute("DELETE FROM artifacts WHERE id=?", [&id.0])
        .expect("delete artifact");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM artifact_state", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn state_route_contract_enforces_key_limit_and_utf8_bytes() {
    let (mut conn, id, email) = database();
    for index in 0..state::MAX_KEYS {
        state::set(
            &mut conn,
            &id,
            &format!("k{index}"),
            &OrderedJson::Null,
            None,
            &email,
            StateScope::Org,
        )
        .expect("key within cap");
    }
    assert!(matches!(
        state::set(
            &mut conn,
            &id,
            "overflow",
            &OrderedJson::Null,
            None,
            &email,
            StateScope::Org
        ),
        Err(StateError::TooManyKeys)
    ));
    let unicode = OrderedJson::string("é".repeat(state::MAX_VALUE_BYTES / 2));
    assert!(matches!(
        state::set(
            &mut conn,
            &id,
            "unicode",
            &unicode,
            None,
            &email,
            StateScope::Org
        ),
        Err(StateError::App(
            artifact_mcp::error::AppError::PayloadTooLarge
        ))
    ));
}

#[test]
fn viewer_state_bucket_caps() {
    let (mut conn, id, alice) = database();
    let bob = EmailAddress::from("bob@example.test");
    for index in 0..state::MAX_KEYS {
        state::set(
            &mut conn,
            &id,
            &format!("a{index}"),
            &OrderedJson::Null,
            None,
            &alice,
            StateScope::Viewer,
        )
        .expect("alice bucket");
    }
    assert!(matches!(
        state::set(
            &mut conn,
            &id,
            "overflow",
            &OrderedJson::Null,
            None,
            &alice,
            StateScope::Viewer
        ),
        Err(StateError::TooManyKeys)
    ));
    state::set(
        &mut conn,
        &id,
        "first",
        &OrderedJson::Null,
        None,
        &bob,
        StateScope::Viewer,
    )
    .expect("bob gets independent bucket");
    state::set(
        &mut conn,
        &id,
        "shared",
        &OrderedJson::Null,
        None,
        &alice,
        StateScope::Org,
    )
    .expect("org gets independent bucket");
}

#[test]
fn viewer_state_migration_34_preserves_org_rows() {
    use artifact_mcp::persistence::migrations::{self, MigrationContext};
    let dir = super::u03_support::TempDataDir::new("viewer-state-migration");
    let path = dir.path().join("artifacts.db");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance/fixtures/historical/boundary-v33/artifacts.db"),
        &path,
    )
    .expect("copy frozen v33 fixture");
    let mut conn = Connection::open(path).expect("sqlite");
    conn.execute_batch("PRAGMA foreign_keys=ON")
        .expect("foreign keys");
    conn.execute("INSERT INTO artifact_state (artifact_id,key,value,revision,updated_at,updated_by) VALUES ('singleb33','note','{\"n\":1}',7,'yesterday','alice@example.test')", []).expect("seed org row");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("v34");
    let id = ArtifactId("singleb33".into());
    let row = state::get(&conn, &id, "note", StateScope::Org, None)
        .expect("read")
        .expect("carried row");
    assert_eq!(row.revision, 7);
    assert_eq!(row.updated_by.0, "alice@example.test");
    assert_eq!(row.updated_at.0, "yesterday");
    assert_eq!(row.value.to_json_string().expect("json"), "{\"n\":1}");
    assert!(
        state::get(
            &conn,
            &id,
            "note",
            StateScope::Viewer,
            Some("alice@example.test")
        )
        .expect("private read")
        .is_none()
    );
    assert_eq!(
        conn.query_row(
            "SELECT display_name FROM org_email_members LIMIT 1",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("member name"),
        ""
    );
}

use super::u20_runtime::runtime;
use artifact_mcp::config::{AppConfig, Secret, SeedKeys};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

struct Observer;
impl runtime::StartupObserver for Observer {
    fn stage(&self, _: runtime::StartupStage) {}
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    email: Option<&str>,
    value: Option<Value>,
    extra: &[(&str, &str)],
) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-artifact-mutation", "1");
    if !extra
        .iter()
        .any(|(key, _)| *key == "sec-fetch-site" || *key == "origin")
    {
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
                .body(value.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn viewer_state_scope_isolation_and_admin_ownership() {
    let dir = super::u03_support::TempDataDir::new("viewer-state-http");
    let mut config = AppConfig {
        data_dir: dir.path().to_owned(),
        listen_host: "127.0.0.1".to_owned(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        ..AppConfig::defaults()
    };
    config.access.trust_headers = true;
    config
        .access
        .domain_orgs
        .insert("acme.test".into(), "acme".into());
    config
        .access
        .domain_orgs
        .insert("beta.test".into(), "beta".into());
    config.access.admin_emails.insert("admin@beta.test".into());
    config.seed_keys = SeedKeys::parse("publisher:acme:state-test-publisher-secret");
    config.ingress.state_per_window = 300;
    config.ingress.verified_viewers_per_window = 500;
    let db_path = dir.path().join("artifacts.db");
    runtime::run_with_bind(config, Arc::new(Observer), move |_, _, app| async move {
        let (status, published) = call(&app, "POST", "/mcp", None, Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish_artifact","arguments":{"html":"<h1>State</h1>","title":"State"}}})), &[("authorization", "Bearer state-test-publisher-secret")]).await;
        assert_eq!(status, 200); let id = published["result"]["structuredContent"]["id"].as_str().expect("published artifact");
        let base = format!("/{id}/state"); let key = format!("{base}/note");
        let member_path = "/settings/orgs/acme/emails";
        let added = call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"alice@acme.test","display_name":" Alice Example "})), &[]).await;
        assert_eq!(added.0, 200);
        assert_eq!(added.1["display_name"], "Alice Example");
        let updated = call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"alice@acme.test","display_name":"Reader"})), &[]).await;
        assert_eq!(updated.0, 200);
        assert_eq!(updated.1["display_name"], "Reader");
        let shell = app.clone().oneshot(Request::builder().uri(format!("/{id}")).header("cf-access-authenticated-user-email", "alice@acme.test").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(shell.status().as_u16(), 200);
        let shell = String::from_utf8(to_bytes(shell.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        let attribute = |name: &str| shell.split(&format!("{name}=\"")).nth(1).unwrap().split('"').next().unwrap().to_owned();
        assert!(attribute("data-viewer-id").contains("95378937b370bd59"));
        assert!(attribute("data-viewer-name").contains("Reader"));
        assert!(!attribute("data-viewer-name").contains('@'));
        for invalid in ["\tReader", "Reader\u{7f}", "Reader\u{85}", &"a".repeat(41)] {
            assert_eq!(call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"alice@acme.test","display_name":invalid})), &[]).await.0, 400);
        }

        assert_eq!(call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"alice@acme.test"})), &[]).await.0, 400);
        assert_eq!(call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"bob@acme.test","display_name":7})), &[]).await.0, 400);
        assert_eq!(call(&app, "POST", member_path, Some("admin@beta.test"), Some(json!({"email":"bob@acme.test","display_name":"bad\nname"})), &[]).await.0, 400);
        let viewer = Some("alice@acme.test");
        for query in ["scope=team", "scope=", "scope=VIEWER", "scope=org&scope=viewer", "scope%5B%5D=viewer"] {
            for (method, suffix) in [("GET", ""), ("GET", "/note"), ("PUT", "/note"), ("DELETE", "/note")] {
                let value = (method == "PUT").then(|| json!({"value":"invalid scope"}));
                assert_eq!(call(&app, method, &format!("{base}{suffix}?{query}"), viewer, value, &[]).await, (400, json!({"error":"bad_scope"})));
            }
        }
        assert_eq!(call(&app, "GET", &format!("{base}?scope=viewer"), None, None, &[]).await.0, 404);

        assert_eq!(call(&app,"GET",&base,viewer,None,&[]).await, (200,json!({"keys":[]})));
        let viewer_key = format!("{base}/private");
        assert_eq!(call(&app,"PUT",&format!("{viewer_key}?scope=viewer"),Some("alice@acme.test"),Some(json!({"value":"alice"})),&[]).await.0,200);
        assert_eq!(call(&app,"GET",&format!("{viewer_key}?scope=viewer"),Some("bob@acme.test"),None,&[]).await.0,404);
        assert_eq!(call(&app,"GET",&format!("{viewer_key}?scope=viewer"),Some("admin@beta.test"),None,&[]).await.0,404);
        assert_eq!(call(&app,"GET",&format!("{viewer_key}?scope=viewer"),Some("alice@acme.test"),None,&[]).await.1["value"],"alice");
        assert_eq!(call(&app,"DELETE",&format!("{viewer_key}?scope=viewer"),Some("bob@acme.test"),None,&[]).await.0,204);
        let private = call(&app,"GET",&format!("{viewer_key}?scope=viewer&viewer=bob%40acme.test"),Some("ALICE@ACME.TEST"),None,&[]).await;
        assert_eq!(private.1["value"], "alice");
        assert!(private.1.get("updated_by").is_none());
        assert_eq!(call(&app,"DELETE",&format!("{viewer_key}?scope=viewer"),Some("admin@beta.test"),None,&[]).await.0,204);
        assert_eq!(call(&app,"GET",&format!("{viewer_key}?scope=viewer"),viewer,None,&[]).await.1["value"], "alice");

        let first = call(&app,"PUT",&key,viewer,Some(json!({"value":{"n":1},"if_revision":0})),&[]).await;
        assert_eq!(first.0,200); assert_eq!(first.1["revision"],1); assert!(first.1.get("ok").is_none());
        let read = call(&app,"GET",&key,Some("bob@acme.test"),None,&[]).await;
        assert_eq!(read.0,200);assert_eq!(read.1["value"],json!({"n":1}));assert_eq!(read.1["updated_by"],"alice@acme.test");
        for (name, arguments) in [
            ("update_artifact", json!({"id":id,"html":"<h1>Updated state artifact</h1>","expected_revision":1})),
            ("restore_artifact", json!({"id":id,"revision":1})),
        ] {
            let changed = call(&app,"POST","/mcp",None,Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}})),&[("authorization","Bearer state-test-publisher-secret")]).await;
            assert_eq!(changed.0,200);assert!(changed.1.get("error").is_none());assert!(changed.1["result"]["structuredContent"]["revision"].as_u64().unwrap() > 1);
            let state = call(&app,"GET",&key,viewer,None,&[]).await;
            assert_eq!(state.1["value"],json!({"n":1}));assert_eq!(state.1["revision"],1);
        }
        assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":false,"if_revision":1})),&[]).await.1["revision"],2);
        assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":"stale","if_revision":1})),&[]).await,(409,json!({"error":"conflict","value":false,"revision":2})));
        for email in [None,Some("viewer@beta.test")] {
            for (method,suffix) in [("GET",""),("GET","/note"),("PUT","/note"),("DELETE","/note")] {
                let value=(method=="PUT").then(||json!({"value":"denied"}));
                let foreign=call(&app,method,&format!("{base}{suffix}"),email,value.clone(),&[]).await;
                let absent=call(&app,method,&format!("/zzzzzzzzzzzz/state{suffix}"),email,value,&[]).await;
                assert_eq!(foreign,(404,json!({"error":"Not found"})));assert_eq!(foreign,absent);
            }
        }
        assert_eq!(call(&app,"PUT",&key,Some("admin@beta.test"),Some(json!({"value":null,"if_revision":2.0})),&[]).await.0,200);
        assert_eq!(call(&app,"GET",&key,Some("admin@beta.test"),None,&[]).await.1["updated_by"],"admin@beta.test");
        for headers in [vec![("sec-fetch-site","cross-site")],vec![("sec-fetch-site","none"),("origin","null")],vec![("origin","null")]] {
            assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":"denied"})),&headers).await.0,403);
        }
        for body in [json!(null),json!([]),json!({}),json!({"value":1,"extra":2})] {
            assert_eq!(call(&app,"PUT",&key,viewer,Some(body),&[]).await,(400,json!({"error":"bad_body"})));
        }
        for revision in [json!(null),json!(-1),json!(1.5),json!("1"),json!(9007199254740992_u64)] {
            assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":1,"if_revision":revision})),&[]).await,(400,json!({"error":"bad_revision"})));
        }
        assert_eq!(call(&app,"PUT",&format!("{base}/bad%20key"),viewer,Some(json!({"value":1})),&[]).await,(400,json!({"error":"bad_key"})));
        assert_eq!(call(&app,"PUT",&format!("{base}/ends%0A"),viewer,Some(json!({"value":1})),&[]).await,(400,json!({"error":"bad_key"})));
        assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":"x".repeat(262142)})),&[]).await.0,200);
        for value in ["x".repeat(262143),"é".repeat(131072)] { assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":value})),&[]).await,(413,json!({"error":"too_large"}))); }
        for index in 1..64 {assert_eq!(call(&app,"PUT",&format!("{base}/k{index}"),viewer,Some(json!({"value":index})),&[]).await.0,200);}
        assert_eq!(call(&app,"PUT",&format!("{base}/overflow"),viewer,Some(json!({"value":1})),&[]).await,(409,json!({"error":"too_many_keys"})));
        assert_eq!(call(&app,"PUT",&key,viewer,Some(json!({"value":"update at cap"})),&[]).await.0,200);
        assert_eq!(call(&app,"DELETE",&key,viewer,None,&[]).await.0,204);
        assert_eq!(call(&app,"DELETE",&key,viewer,None,&[]).await.0,204);
        assert_eq!(call(&app,"PUT",&format!("{base}/replacement"),viewer,Some(json!({"value":1})),&[]).await.0,200);
        assert_eq!(call(&app,"DELETE",&format!("{base}/replacement"),viewer,None,&[]).await.0,204);
        assert_eq!(call(&app,"PUT",&format!("{base}/Mixed.Case_Key-1"),viewer,Some(json!({"value":1})),&[]).await.0,200);
        assert_eq!(call(&app,"DELETE",&format!("{base}/Mixed.Case_Key-1"),viewer,None,&[]).await.0,204);
        {
        let conn = Connection::open(db_path).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM security_audit_events WHERE operation='state.delete'",[],|row|row.get(0)).unwrap();
        assert_eq!(count,6);
        let target: String = conn.query_row("SELECT target_id FROM security_audit_events WHERE operation='state.delete' ORDER BY sequence DESC LIMIT 1",[],|row|row.get(0)).unwrap();
        assert_eq!(target, id);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM security_audit_events WHERE operation='state.put'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
        conn.execute_batch("CREATE TRIGGER fail_state_audit BEFORE INSERT ON security_audit_events WHEN NEW.operation='state.delete' BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;").unwrap();
        }
        assert_eq!(call(&app,"PUT",&format!("{base}/replacement"),viewer,Some(json!({"value":1})),&[]).await.0,200);
        assert_eq!(call(&app,"DELETE",&format!("{base}/replacement"),viewer,None,&[]).await.0,500);
        assert_eq!(call(&app,"GET",&format!("{base}/replacement"),viewer,None,&[]).await.1["value"],1);
        Ok(())
    }).await.unwrap();
}
