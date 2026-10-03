use artifact_mcp::data::{Binding, BindingManifest, DataBroker, Event, RequestedSubscription};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::CONTENT_TYPE},
    response::Response,
    routing::any,
};
use serde_json::json;
use std::collections::BTreeMap;
use tokio::time::{Duration, timeout};
use tokio::{net::TcpListener, sync::mpsc};

#[allow(dead_code)] // Shared fixture support also serves unrelated native integration tests.
#[path = "native/u12_support.rs"]
mod support;

fn config() -> String {
    serde_json::to_string(&json!({"sources":[
        {"id":"push","org":"acme","kind":"push","operations":{"status":{"key":"status"}},"subscriptions":{"events":{"transport":"push"}}},
        {"id":"other","org":"acme","kind":"push","operations":{"status":{"key":"status"}},"subscriptions":{"events":{"transport":"push"}}}
    ]})).unwrap()
}

fn manifest(source: &str, binding: &str) -> BindingManifest {
    BindingManifest {
        bindings: BTreeMap::from([(
            binding.into(),
            Binding {
                source: source.into(),
                operations: vec!["status".into()],
                subscriptions: vec!["events".into()],
            },
        )]),
    }
}

async fn broker() -> DataBroker {
    DataBroker::from_json(&config()).unwrap()
}

#[derive(Clone)]
struct HttpSourceState {
    requests: mpsc::UnboundedSender<String>,
}

async fn http_source(
    State(state): State<HttpSourceState>,
    request: Request<Body>,
) -> Response<Body> {
    let uri = request.uri().to_string();
    let _ = state.requests.send(uri.clone());
    if uri.starts_with("/events") {
        return Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "id: upstream-1\nevent: dashboard-event\ndata: {\"ok\":true}\n\n",
            ))
            .unwrap();
    }
    if uri.starts_with("/redirect") {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "/api/items/redirected")
            .body(Body::empty())
            .unwrap();
    }
    if uri.starts_with("/too-large") {
        return Response::new(Body::from("01234567890123456789"));
    }
    if uri.starts_with("/api/items/") {
        return Response::new(Body::from(r#"{"ok":true}"#));
    }
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

async fn start_http_source() -> (
    String,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, receiver) = mpsc::unbounded_channel();
    let app = Router::new()
        .fallback(any(http_source))
        .with_state(HttpSourceState { requests });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), receiver, task)
}

#[test]
fn production_pr_watch_examples_and_strict_config_are_accepted() {
    let sources = include_str!("../ops/data-sources.pr-watch.example.json");
    let bindings = include_str!("../ops/data-bindings.pr-watch.example.json");
    let broker = DataBroker::from_json(sources).unwrap();
    assert!(broker.sources.contains_key("pr-watch"));
    let manifest: BindingManifest = serde_json::from_str(bindings).unwrap();
    assert_eq!(manifest.bindings["reviews"].source, "pr-watch");
    assert!(DataBroker::from_json(r#"{"sources":[{"id":"x","org":"a","kind":"push","operations":{"x":{}},"subscriptions":{}},{"id":"x","org":"a","kind":"push","operations":{},"subscriptions":{}}]}"#).is_err());
    assert!(DataBroker::from_json(r#"{"sources":[{"id":"x","org":"a","kind":"http","base_url":"https://x.test/?bad=1","operations":{},"subscriptions":{}}]}"#).is_err());
    assert!(DataBroker::from_json(r#"{"sources":[{"id":"x","org":"a","kind":"push","operations":{"x":{"params":{"n":{"type":"integer","enum":[1,"bad"]}}}},"subscriptions":{}}]}"#).is_err());
}

#[tokio::test]
async fn persisted_snapshot_survives_broker_restart() {
    let dir = support::TempDir::new("live-data");
    let pool = support::open_pool(dir.path());
    support::seed_org(&pool, "acme").await;
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute("INSERT INTO artifacts (id, client_id, org, title) VALUES ('persisted', 'publisher', 'acme', 'Data')", []).map_err(|_| artifact_mcp::error::AppError::Internal)?;
        Ok(())
    }).await.unwrap();
    let first = DataBroker::from_json(&config())
        .unwrap()
        .with_pool(pool.clone());
    first
        .set_bindings("persisted", manifest("push", "reviews"), "acme")
        .await
        .unwrap();
    first
        .set_snapshot("persisted", "reviews", "status", json!({"ready":true}))
        .await
        .unwrap();
    drop(first);
    let second = DataBroker::from_json(&config()).unwrap().with_pool(pool);
    assert_eq!(
        second
            .query("persisted", "reviews", "status", json!({}))
            .await
            .unwrap(),
        json!({"ready":true})
    );
}

#[tokio::test]
async fn persisted_binding_and_event_writes_tolerate_other_database_writers() {
    let dir = support::TempDir::new("live-data-contention");
    let pool = support::open_pool(dir.path());
    support::seed_org(&pool, "acme").await;
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES ('contended', 'publisher', 'acme', 'Data')",
            [],
        )
        .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        Ok(())
    })
    .await
    .unwrap();
    let broker = DataBroker::from_json(&config())
        .unwrap()
        .with_pool(pool.clone());
    let background_pool = pool.clone();
    let background = tokio::spawn(async move {
        for index in 0..1024 {
            artifact_mcp::persistence::db::interact(&background_pool, move |conn| {
                conn.execute(
                    "UPDATE artifacts SET title=?1 WHERE id='contended'",
                    [format!("Other writer {index}")],
                )
                .map_err(|_| artifact_mcp::error::AppError::Internal)?;
                Ok(())
            })
            .await
            .unwrap();
        }
    });
    let mut errors = Vec::new();
    for index in 0..256 {
        if let Err(error) = broker
            .set_bindings("contended", manifest("push", "reviews"), "acme")
            .await
        {
            errors.push(format!("binding {index}: {error}"));
            continue;
        }
        if let Err(error) = broker
            .append_events(
                "contended",
                "reviews",
                "events",
                vec![Event {
                    id: format!("event-{index}"),
                    event: "deployment".into(),
                    data: json!({ "index": index }),
                }],
            )
            .await
        {
            errors.push(format!("event {index}: {error}"));
        }
    }
    background.await.unwrap();
    assert!(errors.is_empty(), "Concurrent writes failed: {errors:?}");
    let count = artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.query_row(
            "SELECT count(*) FROM artifact_data_events WHERE artifact_id='contended'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| artifact_mcp::error::AppError::Internal)
    })
    .await
    .unwrap();
    assert_eq!(count, 256, "Every successful batch must persist its event");
}

#[tokio::test]
async fn push_snapshots_revision_and_key_validation() {
    let broker = broker().await;
    broker
        .set_bindings("artifact", manifest("push", "reviews"), "acme")
        .await
        .unwrap();
    assert_eq!(
        broker
            .set_snapshot("artifact", "reviews", "status", json!({"state":"ok"}))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        broker
            .set_snapshot("artifact", "reviews", "status", json!({"state":"busy"}))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        broker
            .query("artifact", "reviews", "status", json!({}))
            .await
            .unwrap(),
        json!({"state":"busy"})
    );
    assert_eq!(
        broker
            .query("artifact", "reviews", "status", json!({"extra":true}))
            .await
            .unwrap_err(),
        "bad_params"
    );
    assert_eq!(
        broker
            .set_snapshot("artifact", "reviews", "unknown", json!(1))
            .await
            .unwrap_err(),
        "not_found"
    );
}

#[tokio::test]
async fn event_batches_are_atomic_and_ids_are_idempotent() {
    let broker = broker().await;
    broker
        .set_bindings("artifact", manifest("push", "reviews"), "acme")
        .await
        .unwrap();
    let invalid = vec![
        Event {
            id: "ok".into(),
            event: "message".into(),
            data: json!(1),
        },
        Event {
            id: "bad\n".into(),
            event: "message".into(),
            data: json!(2),
        },
    ];
    assert_eq!(
        broker
            .append_events("artifact", "reviews", "events", invalid)
            .await
            .unwrap_err(),
        "bad_params"
    );
    let valid = vec![Event {
        id: "one".into(),
        event: "dashboard-event".into(),
        data: json!({"n":1}),
    }];
    assert_eq!(
        broker
            .append_events("artifact", "reviews", "events", valid.clone())
            .await
            .unwrap(),
        (1, 0)
    );
    assert_eq!(
        broker
            .append_events("artifact", "reviews", "events", valid)
            .await
            .unwrap(),
        (0, 1)
    );
}

#[tokio::test]
async fn replacing_bindings_clears_changed_data_and_retains_unchanged() {
    let broker = broker().await;
    broker
        .set_bindings("artifact", manifest("push", "reviews"), "acme")
        .await
        .unwrap();
    broker
        .set_snapshot("artifact", "reviews", "status", json!("kept"))
        .await
        .unwrap();
    broker
        .set_bindings("artifact", manifest("other", "reviews"), "acme")
        .await
        .unwrap();
    assert_eq!(
        broker
            .query("artifact", "reviews", "status", json!({}))
            .await
            .unwrap_err(),
        "not_found"
    );
    broker
        .set_bindings("artifact", manifest("other", "reviews"), "acme")
        .await
        .unwrap();
    assert_eq!(
        broker.try_bindings("artifact").await.unwrap().bindings["reviews"].source,
        "other"
    );
}

#[tokio::test]
async fn push_stream_delivers_two_topics_and_replays_gap() {
    let broker = broker().await;
    let mut bindings = manifest("push", "reviews");
    bindings.bindings.insert(
        "audit".into(),
        Binding {
            source: "push".into(),
            operations: vec!["status".into()],
            subscriptions: vec!["events".into()],
        },
    );
    broker
        .set_bindings("artifact", bindings, "acme")
        .await
        .unwrap();
    broker
        .append_events(
            "artifact",
            "reviews",
            "events",
            vec![Event {
                id: "r1".into(),
                event: "dashboard-event".into(),
                data: json!(1),
            }],
        )
        .await
        .unwrap();
    broker
        .append_events(
            "artifact",
            "audit",
            "events",
            vec![Event {
                id: "a1".into(),
                event: "dashboard-event".into(),
                data: json!(2),
            }],
        )
        .await
        .unwrap();
    let mut stream = broker
        .stream(
            "artifact",
            vec![
                RequestedSubscription {
                    binding: "reviews".into(),
                    subscription: "events".into(),
                },
                RequestedSubscription {
                    binding: "audit".into(),
                    subscription: "events".into(),
                },
            ],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut saw = Vec::new();
    while saw.len() < 2 {
        let item = timeout(Duration::from_secs(2), stream.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if item.event == "dashboard-event" {
            saw.push((item.binding, item.id));
        }
    }
    assert!(saw.contains(&("reviews".into(), "r1".into())));
    assert!(saw.contains(&("audit".into(), "a1".into())));
    drop(stream);
    let mut cursor = BTreeMap::new();
    cursor.insert("reviews:events".into(), "missing".into());
    let mut replay = broker
        .stream(
            "artifact",
            vec![RequestedSubscription {
                binding: "reviews".into(),
                subscription: "events".into(),
            }],
            cursor,
        )
        .await
        .unwrap();
    let mut resync = false;
    let mut replayed = false;
    while !resync || !replayed {
        let item = timeout(Duration::from_secs(2), replay.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        resync |= item.event == "data:resync";
        replayed |= item.event == "dashboard-event" && item.id == "r1";
    }
    assert!(resync && replayed);
}

#[tokio::test]
async fn http_queries_bound_responses_and_encode_paths_and_queries() {
    let (base_url, mut requests, server) = start_http_source().await;
    let raw = serde_json::to_string(&json!({"sources":[{
        "id":"http","org":"acme","kind":"http","base_url":base_url,
        "operations":{
            "item":{"path":"/api/items/{item}","params":{"item":{"type":"string","required":true},"q":{"type":"string"}}},
            "redirect":{"path":"/redirect"},
            "large":{"path":"/too-large","max_bytes":8}
        },"subscriptions":{}
    }]})).unwrap();
    let broker = DataBroker::from_json(&raw).unwrap();
    broker
        .set_bindings(
            "artifact",
            BindingManifest {
                bindings: BTreeMap::from([(
                    "reviews".into(),
                    Binding {
                        source: "http".into(),
                        operations: vec!["item".into(), "redirect".into(), "large".into()],
                        subscriptions: vec![],
                    },
                )]),
            },
            "acme",
        )
        .await
        .unwrap();
    assert_eq!(
        broker
            .query(
                "artifact",
                "reviews",
                "item",
                json!({"item":"run-1","q":"a&b"})
            )
            .await
            .unwrap(),
        json!({"ok":true})
    );
    let request = timeout(Duration::from_secs(2), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request.starts_with("/api/items/run-1?"));
    assert!(request.contains("q=a%26b"));
    assert_eq!(
        broker
            .query("artifact", "reviews", "item", json!({"item":"a/b"}))
            .await
            .unwrap_err(),
        "bad_params"
    );
    assert_eq!(
        broker
            .query(
                "artifact",
                "reviews",
                "item",
                json!({"item":"run-1","extra":1})
            )
            .await
            .unwrap_err(),
        "bad_params"
    );
    assert_eq!(
        broker
            .query("artifact", "reviews", "redirect", json!({}))
            .await
            .unwrap_err(),
        "data_unavailable"
    );
    assert_eq!(
        broker
            .query("artifact", "reviews", "large", json!({}))
            .await
            .unwrap_err(),
        "too_large"
    );
    drop(server);
}

#[tokio::test]
async fn sse_named_events_report_independent_source_failure_and_drop_cleanup() {
    let (base_url, mut requests, server) = start_http_source().await;
    let raw = serde_json::to_string(&json!({"sources":[
        {"id":"good","org":"acme","kind":"http","base_url":base_url,"operations":{},"subscriptions":{"events":{"transport":"sse","path":"/events","events":["dashboard-event"]}}},
        {"id":"bad","org":"acme","kind":"http","base_url":base_url,"operations":{},"subscriptions":{"events":{"transport":"sse","path":"/missing","events":["dashboard-event"]}}}
    ]})).unwrap();
    let broker = DataBroker::from_json(&raw).unwrap();
    broker
        .set_bindings(
            "artifact",
            BindingManifest {
                bindings: BTreeMap::from([
                    (
                        "good".into(),
                        Binding {
                            source: "good".into(),
                            operations: vec![],
                            subscriptions: vec!["events".into()],
                        },
                    ),
                    (
                        "bad".into(),
                        Binding {
                            source: "bad".into(),
                            operations: vec![],
                            subscriptions: vec!["events".into()],
                        },
                    ),
                ]),
            },
            "acme",
        )
        .await
        .unwrap();
    let mut stream = broker
        .stream(
            "artifact",
            vec![
                RequestedSubscription {
                    binding: "good".into(),
                    subscription: "events".into(),
                },
                RequestedSubscription {
                    binding: "bad".into(),
                    subscription: "events".into(),
                },
            ],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let mut good_connected = false;
    let mut good_event = false;
    let mut bad_unavailable = false;
    while !(good_connected && good_event && bad_unavailable) {
        let item = timeout(Duration::from_secs(4), stream.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if item.binding == "good"
            && item.event == "data:status"
            && item.data["state"] == "connected"
        {
            good_connected = true;
        }
        if item.binding == "good" && item.event == "dashboard-event" && item.id == "upstream-1" {
            good_event = true;
        }
        if item.binding == "bad"
            && item.event == "data:status"
            && item.data["state"] == "unavailable"
        {
            bad_unavailable = true;
        }
    }
    let mut saw_good_request = false;
    let mut saw_bad_request = false;
    while let Ok(Some(uri)) = timeout(Duration::from_millis(100), requests.recv()).await {
        saw_good_request |= uri == "/events";
        saw_bad_request |= uri == "/missing";
        if saw_good_request && saw_bad_request {
            break;
        }
    }
    assert!(saw_good_request && saw_bad_request);
    drop(stream);
    drop(server);
}

#[tokio::test]
async fn persisted_events_and_mapped_snapshot_keys_are_tenant_scoped() {
    let dir = support::TempDir::new("live-data-org");
    let pool = support::open_pool(dir.path());
    support::seed_org(&pool, "acme").await;
    support::seed_org(&pool, "beta").await;
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute("INSERT INTO artifacts(id,client_id,org,title) VALUES('mapped','publisher','acme','Data')",[]).map_err(|_|artifact_mcp::error::AppError::Internal)?;
        Ok(())
    }).await.unwrap();
    let raw=json!({"sources":[
        {"id":"a","org":"acme","kind":"push","operations":{"latest":{"key":"summary"}},"subscriptions":{"events":{"transport":"push"}}},
        {"id":"b","org":"beta","kind":"push","operations":{"latest":{"key":"summary"}},"subscriptions":{"events":{"transport":"push"}}}
    ]}).to_string();
    let bindings = |source: &str| BindingManifest {
        bindings: BTreeMap::from([(
            "reviews".into(),
            Binding {
                source: source.into(),
                operations: vec!["latest".into()],
                subscriptions: vec!["events".into()],
            },
        )]),
    };
    let first = DataBroker::from_json(&raw).unwrap().with_pool(pool.clone());
    first
        .set_bindings("mapped", bindings("a"), "acme")
        .await
        .unwrap();
    first
        .set_snapshot("mapped", "reviews", "summary", json!({"version":1}))
        .await
        .unwrap();
    first
        .append_events(
            "mapped",
            "reviews",
            "events",
            vec![Event {
                id: "one".into(),
                event: "deployment".into(),
                data: json!({"version":1}),
            }],
        )
        .await
        .unwrap();
    first
        .set_bindings("mapped", bindings("a"), "acme")
        .await
        .unwrap();
    drop(first);
    let second = DataBroker::from_json(&raw).unwrap().with_pool(pool.clone());
    assert_eq!(
        second
            .query("mapped", "reviews", "latest", json!({}))
            .await
            .unwrap(),
        json!({"version":1})
    );
    assert_eq!(
        second
            .append_events(
                "mapped",
                "reviews",
                "events",
                vec![Event {
                    id: "one".into(),
                    event: "deployment".into(),
                    data: json!(2)
                }]
            )
            .await
            .unwrap(),
        (0, 1)
    );
    let mut stream = second
        .stream(
            "mapped",
            vec![RequestedSubscription {
                binding: "reviews".into(),
                subscription: "events".into(),
            }],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    loop {
        let event = timeout(Duration::from_secs(2), stream.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if event.event == "deployment" {
            assert_eq!(event.id, "one");
            assert_eq!(event.data, json!({"version":1}));
            break;
        }
    }
    drop(stream);
    artifact_mcp::persistence::db::interact(&pool, |conn| {
        conn.execute("UPDATE artifacts SET org='beta' WHERE id='mapped'", [])
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        second
            .query("mapped", "reviews", "latest", json!({}))
            .await
            .unwrap_err(),
        "not_found"
    );
    second
        .set_bindings("mapped", bindings("b"), "beta")
        .await
        .unwrap();
    assert_eq!(
        second
            .query("mapped", "reviews", "latest", json!({}))
            .await
            .unwrap_err(),
        "not_found"
    );
    let remaining=artifact_mcp::persistence::db::interact(&pool,|conn|{
        conn.query_row("SELECT (SELECT count(*) FROM artifact_data_snapshots)+(SELECT count(*) FROM artifact_data_events)",[],|row|row.get::<_,i64>(0)).map_err(|_|artifact_mcp::error::AppError::Internal)
    }).await.unwrap();
    assert_eq!(
        remaining, 0,
        "Rebinding after a tenant move discards prior tenant data"
    );
}
