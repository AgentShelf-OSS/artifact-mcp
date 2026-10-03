use artifact_mcp::data::DataBroker;
use artifact_mcp::data::{Binding, BindingManifest, RequestedSubscription};
use artifact_mcp::model::{ClientId, OrgId, PublisherIdentity};
use artifact_mcp::security::audit::MutationAudit;
use axum::body::Bytes;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, header::CONTENT_TYPE},
    response::Response,
    routing::any,
};
use futures_util::stream;
use serde_json::json;
use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    net::TcpListener,
    time::{Duration, timeout},
};

#[allow(dead_code)]
#[path = "native/u12_support.rs"]
mod support;

#[test]
fn managed_definition_reuses_strict_source_schema() {
    let invalid = r#"{"sources":[{"id":"demo","org":"acme","kind":"http","base_url":"http://127.0.0.1","operations":{"status":{"path":"/status","unexpected":true}}}]}"#;
    assert!(
        matches!(DataBroker::from_json(invalid), Err(error) if error == "invalid_data_sources")
    );
}

#[test]
fn push_sources_reject_http_configuration() {
    let invalid = r#"{"sources":[{"id":"demo","org":"acme","kind":"push","base_url":"http://127.0.0.1","operations":{"status":{}},"subscriptions":{"events":{"transport":"push"}}}]}"#;
    assert!(
        matches!(DataBroker::from_json(invalid), Err(error) if error == "invalid_data_sources")
    );
}

#[tokio::test]
async fn managed_source_audit_failure_rolls_back_the_insert() {
    let dir = support::TempDir::new("connections-audit-rollback");
    let pool = support::open_pool(dir.path());
    support::seed_org(&pool, "acme").await;
    let audit = MutationAudit::publisher(&PublisherIdentity {
        client_id: ClientId("admin".into()),
        org: OrgId("acme".into()),
        label: "test".into(),
        role: "admin".into(),
        scopes: None,
    })
    .unwrap();
    let broker = DataBroker::from_json(r#"{"sources":[]}"#)
        .unwrap()
        .with_pool(pool.clone())
        .with_audit_key([7; 32]);
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute("DROP TABLE security_audit_chain_head", [])
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        Ok(())
    })
    .await
    .unwrap();
    let source = serde_json::from_value(json!({"id":"managed","org":"acme","kind":"push","operations":{"status":{"key":"status"}},"subscriptions":{"events":{"transport":"push"}}})).unwrap();
    assert!(
        broker
            .create_managed_source(source, true, audit)
            .await
            .is_err()
    );
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM data_sources", [], |row| row.get(0))
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        assert_eq!(count, 0);
        Ok(())
    })
    .await
    .unwrap();
}

#[derive(Clone)]
struct LongSseState {
    a_requests: Arc<AtomicUsize>,
    b_requests: Arc<AtomicUsize>,
}

async fn long_sse(State(state): State<LongSseState>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path();
    let counter = if path.starts_with("/a") {
        &state.a_requests
    } else {
        &state.b_requests
    };
    counter.fetch_add(1, Ordering::SeqCst);
    let body = stream::unfold(0_u8, |step| async move {
        if step == 0 {
            Some((
                Ok::<Bytes, Infallible>(Bytes::from("id: one\nevent: message\ndata: {}\n\n")),
                1,
            ))
        } else {
            std::future::pending::<Option<(Result<Bytes, Infallible>, u8)>>().await
        }
    });
    Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

#[tokio::test]
async fn source_edit_disable_reenable_restarts_only_affected_long_sse() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let state = LongSseState {
        a_requests: Arc::new(AtomicUsize::new(0)),
        b_requests: Arc::new(AtomicUsize::new(0)),
    };
    let counts = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(long_sse)).with_state(state),
        )
        .await
        .unwrap();
    });
    let dir = support::TempDir::new("connections-sse-lifecycle");
    let pool = support::open_pool(dir.path());
    support::seed_org(&pool, "acme").await;
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute("INSERT INTO artifacts (id, client_id, org, title) VALUES ('sse', 'publisher', 'acme', 'Data')", []).map_err(|_| artifact_mcp::error::AppError::Internal)?;
        Ok(())
    }).await.unwrap();
    let broker = DataBroker::from_json(r#"{"sources":[]}"#)
        .unwrap()
        .with_pool(pool)
        .with_audit_key([9; 32]);
    let audit = || {
        MutationAudit::publisher(&PublisherIdentity {
            client_id: ClientId("admin".into()),
            org: OrgId("acme".into()),
            label: "test".into(),
            role: "admin".into(),
            scopes: None,
        })
        .unwrap()
    };
    let source = |id: &str, path: &str| {
        serde_json::from_value(json!({"id":id,"org":"acme","kind":"http","base_url":format!("http://{address}"),"operations":{"status":{"path":"/status"}},"subscriptions":{"events":{"transport":"sse","path":path}}})).unwrap()
    };
    broker
        .create_managed_source(source("a", "/a"), true, audit())
        .await
        .unwrap();
    broker
        .create_managed_source(source("b", "/b"), true, audit())
        .await
        .unwrap();
    broker
        .set_bindings(
            "sse",
            BindingManifest {
                bindings: BTreeMap::from([
                    (
                        "a".into(),
                        Binding {
                            source: "a".into(),
                            operations: vec!["status".into()],
                            subscriptions: vec!["events".into()],
                        },
                    ),
                    (
                        "b".into(),
                        Binding {
                            source: "b".into(),
                            operations: vec!["status".into()],
                            subscriptions: vec!["events".into()],
                        },
                    ),
                ]),
            },
            "acme",
        )
        .await
        .unwrap();
    let mut data = broker
        .stream(
            "sse",
            vec![
                RequestedSubscription {
                    binding: "a".into(),
                    subscription: "events".into(),
                },
                RequestedSubscription {
                    binding: "b".into(),
                    subscription: "events".into(),
                },
            ],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while counts.a_requests.load(Ordering::SeqCst) < 1
            || counts.b_requests.load(Ordering::SeqCst) < 1
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let mut saw_event = false;
    timeout(Duration::from_secs(3), async {
        while !saw_event {
            if let Some(envelope) = data.receiver.recv().await {
                saw_event |= envelope.event == "message";
            }
        }
    })
    .await
    .unwrap();
    assert!(saw_event);
    assert!(
        broker
            .source_view_by_id("a")
            .await
            .unwrap()
            .health
            .last_event_at
            .is_some()
    );
    broker
        .set_managed_source("a", source("a", "/a2"), true, 1, audit())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while counts.a_requests.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        counts.b_requests.load(Ordering::SeqCst),
        1,
        "unaffected source must stay connected during edit"
    );
    broker
        .set_managed_enabled("a", false, 2, audit())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    broker
        .set_managed_enabled("a", true, 3, audit())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while counts.a_requests.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        counts.b_requests.load(Ordering::SeqCst),
        1,
        "unaffected source must not reconnect during disable/re-enable"
    );
    data.receiver.close();
    drop(data);
    server.abort();
}
