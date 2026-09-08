//! Viewer-state persistence and HTTP contract coverage.

use artifact_mcp::{
    mcp::protocol::OrderedJson,
    model::{ArtifactId, EmailAddress},
    persistence::state::{self, StateError},
};
use rusqlite::Connection;

fn database() -> (Connection, ArtifactId, EmailAddress) {
    let conn = Connection::open_in_memory().expect("sqlite");
    conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE artifacts (id TEXT PRIMARY KEY); CREATE TABLE artifact_state (artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE, key TEXT NOT NULL, value TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, updated_at TEXT NOT NULL, updated_by TEXT NOT NULL, PRIMARY KEY (artifact_id,key)); INSERT INTO artifacts VALUES ('state-fixture');").expect("schema");
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
    let first = state::set(&mut conn, &id, "reading.note", &object, None, &email).expect("put");
    assert_eq!(first.revision, 1);
    let second = state::set(&mut conn, &id, "reading.note", &object, Some(1), &email).expect("put");
    assert_eq!(second.revision, 2);
    let conflict = state::set(&mut conn, &id, "reading.note", &object, Some(1), &email)
        .expect_err("stale revision");
    assert!(matches!(conflict, StateError::Conflict(c) if c.revision == 2));
}

#[test]
fn state_route_contract_enforces_key_and_value_caps_and_cascade() {
    let (mut conn, id, email) = database();
    assert!(matches!(
        state::set(&mut conn, &id, "bad key", &OrderedJson::Null, None, &email),
        Err(StateError::App(_))
    ));
    let oversized = OrderedJson::string("x".repeat(state::MAX_VALUE_BYTES));
    assert!(matches!(
        state::set(&mut conn, &id, "large", &oversized, None, &email),
        Err(StateError::App(
            artifact_mcp::error::AppError::PayloadTooLarge
        ))
    ));
    state::set(&mut conn, &id, "survives", &OrderedJson::Null, None, &email).expect("put");
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
        )
        .expect("key within cap");
    }
    assert!(matches!(
        state::set(&mut conn, &id, "overflow", &OrderedJson::Null, None, &email),
        Err(StateError::TooManyKeys)
    ));
    let unicode = OrderedJson::string("é".repeat(state::MAX_VALUE_BYTES / 2));
    assert!(matches!(
        state::set(&mut conn, &id, "unicode", &unicode, None, &email),
        Err(StateError::App(
            artifact_mcp::error::AppError::PayloadTooLarge
        ))
    ));
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
async fn viewer_state_http_real_runtime_auth_conflicts_caps_and_audit() {
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
        let viewer = Some("alice@acme.test");
        assert_eq!(call(&app,"GET",&base,viewer,None,&[]).await, (200,json!({"keys":[]})));
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
        assert_eq!(count,4);
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
