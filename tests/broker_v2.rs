//! ADR-0017: POST operations with typed JSON bodies and streaming (NDJSON) operations.
use artifact_mcp::data::{Binding, BindingManifest, DataBroker};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, header::CONTENT_TYPE},
    response::Response,
    routing::any,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, convert::Infallible};
use tokio::{
    net::TcpListener,
    time::{Duration, Instant, sleep, timeout},
};

fn ndjson(lines: Vec<(u64, &'static str)>) -> Response<Body> {
    let stream = futures_util::stream::unfold(lines.into_iter(), |mut rest| async move {
        let (delay, line) = rest.next()?;
        sleep(Duration::from_millis(delay)).await;
        Some((Ok::<_, Infallible>(Bytes::from(line)), rest))
    });
    Response::builder()
        .header(CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn upstream(request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let kind = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    if uri.starts_with("/echo") {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        return Response::new(Body::from(
            json!({"method":method,"uri":uri,"content_type":kind,"body":parsed}).to_string(),
        ));
    }
    match uri.as_str() {
        // First line now, second after a pause: proves lines are relayed as they arrive.
        "/stream" => ndjson(vec![
            (0, "{\"type\":\"status\",\"n\":1}\n"),
            (600, "{\"type\":\"final\",\"n\":2}\n"),
        ]),
        // A line split across chunks, a blank line and CRLF endings.
        "/split" => ndjson(vec![(0, "{\"a\":"), (10, "1}\r\n\n"), (10, "{\"b\":2}")]),
        "/bad" => ndjson(vec![(0, "{\"ok\":true}\n"), (10, "not json\n")]),
        "/big" => ndjson(vec![
            (0, "{\"pad\":\"0123456789\"}\n"),
            (10, "{\"pad\":\"0123456789\"}\n"),
        ]),
        "/plain" => Response::new(Body::from("{\"ok\":true}")),
        _ => Response::builder().status(404).body(Body::empty()).unwrap(),
    }
}

async fn start() -> String {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(upstream)))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

fn source(base: &str, operations: Value) -> String {
    json!({"sources":[{"id":"chat","org":"acme","kind":"http","base_url":base,"operations":operations}]})
        .to_string()
}

async fn bound(base: &str, operations: Value) -> DataBroker {
    let names: Vec<String> = operations.as_object().unwrap().keys().cloned().collect();
    let broker = DataBroker::from_json(&source(base, operations)).unwrap();
    broker
        .set_bindings(
            "artifact",
            BindingManifest {
                bindings: BTreeMap::from([(
                    "concierge".into(),
                    Binding {
                        source: "chat".into(),
                        operations: names,
                        subscriptions: vec![],
                    },
                )]),
            },
            "acme",
        )
        .await
        .unwrap();
    broker
}

fn long_params(count: usize, max_length: usize) -> Value {
    let mut params = serde_json::Map::new();
    for i in 0..count {
        params.insert(
            format!("p{i}"),
            json!({"type":"string","max_length":max_length}),
        );
    }
    Value::Object(params)
}

#[test]
fn post_and_stream_fields_are_validated() {
    let ok = |ops: Value| DataBroker::from_json(&source("http://127.0.0.1:9", ops)).is_ok();
    assert!(ok(
        json!({"chat":{"path":"/chat","method":"POST","params":long_params(32, 16384)}})
    ));
    assert!(!ok(
        json!({"chat":{"path":"/chat","method":"POST","params":long_params(33, 256)}})
    ));
    assert!(!ok(
        json!({"chat":{"path":"/chat","method":"POST","params":long_params(1, 16385)}})
    ));
    assert!(!ok(
        json!({"chat":{"path":"/chat","params":long_params(1, 257)}})
    ));
    assert!(!ok(
        json!({"chat":{"path":"/chat","params":long_params(17, 10)}})
    ));
    assert!(!ok(json!({"chat":{"path":"/chat","method":"PUT"}})));
    assert!(ok(
        json!({"chat":{"path":"/chat","method":"POST","stream":true,"timeout_ms":120000}})
    ));
    assert!(!ok(
        json!({"chat":{"path":"/chat","stream":true,"timeout_ms":120001}})
    ));
    assert!(!ok(json!({"chat":{"path":"/chat","timeout_ms":30001}})));
    let push = |op: Value| {
        DataBroker::from_json(
            &json!({"sources":[{"id":"p","org":"acme","kind":"push","operations":{"s":op}}]})
                .to_string(),
        )
        .is_ok()
    };
    assert!(push(json!({"key":"s"})));
    assert!(!push(json!({"key":"s","method":"POST"})));
    assert!(!push(json!({"key":"s","stream":true})));
}

#[tokio::test]
async fn post_sends_typed_json_body_with_defaults_and_path_params() {
    let base = start().await;
    let broker = bound(
        &base,
        json!({"echo":{"path":"/echo/{room}","method":"POST","params":{
            "room":{"type":"string","required":true},
            "msg":{"type":"string","max_length":4000,"required":true},
            "n":{"type":"integer","minimum":1,"maximum":5},
            "flag":{"type":"boolean","default":true}
        }}}),
    )
    .await;
    let long = "x".repeat(3000);
    let echoed = broker
        .query(
            "artifact",
            "concierge",
            "echo",
            json!({"room":"a b","msg":long,"n":3}),
        )
        .await
        .unwrap();
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["uri"], "/echo/a%20b");
    assert!(
        echoed["content_type"]
            .as_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(echoed["body"], json!({"msg":long,"n":3,"flag":true}));
    assert_eq!(
        broker
            .query(
                "artifact",
                "concierge",
                "echo",
                json!({"room":"r","msg":"x","n":9})
            )
            .await
            .unwrap_err(),
        "bad_params"
    );
}

#[tokio::test]
async fn streaming_relays_lines_as_they_arrive() {
    let base = start().await;
    let broker = bound(
        &base,
        json!({"chat":{"path":"/stream","method":"POST","stream":true,"timeout_ms":10000}}),
    )
    .await;
    let started = Instant::now();
    let mut lines = broker
        .query_stream("artifact", "concierge", "chat", json!({}))
        .await
        .unwrap();
    let first = timeout(Duration::from_secs(2), lines.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, json!({"type":"status","n":1}));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "first line was buffered"
    );
    let second = lines.recv().await.unwrap().unwrap();
    assert_eq!(second, json!({"type":"final","n":2}));
    assert!(lines.recv().await.is_none());
}

#[tokio::test]
async fn stream_framing_errors_and_compatibility() {
    let base = start().await;
    let broker = bound(
        &base,
        json!({
            "split":{"path":"/split","stream":true},
            "bad":{"path":"/bad","stream":true},
            "big":{"path":"/big","stream":true,"max_bytes":30},
            "plain":{"path":"/plain","stream":true},
            "normal":{"path":"/echo"}
        }),
    )
    .await;
    let mut split = broker
        .query_stream("artifact", "concierge", "split", json!({}))
        .await
        .unwrap();
    assert_eq!(split.recv().await.unwrap().unwrap(), json!({"a":1}));
    assert_eq!(split.recv().await.unwrap().unwrap(), json!({"b":2}));
    assert!(split.recv().await.is_none());

    let mut bad = broker
        .query_stream("artifact", "concierge", "bad", json!({}))
        .await
        .unwrap();
    assert_eq!(bad.recv().await.unwrap().unwrap(), json!({"ok":true}));
    assert_eq!(bad.recv().await.unwrap().unwrap_err(), "data_unavailable");

    let mut big = broker
        .query_stream("artifact", "concierge", "big", json!({}))
        .await
        .unwrap();
    assert!(big.recv().await.unwrap().is_ok());
    assert_eq!(big.recv().await.unwrap().unwrap_err(), "too_large");

    assert_eq!(
        broker
            .query_stream("artifact", "concierge", "plain", json!({}))
            .await
            .unwrap_err(),
        "data_unavailable"
    );
    assert_eq!(
        broker
            .query_stream("artifact", "concierge", "normal", json!({}))
            .await
            .unwrap_err(),
        "bad_params"
    );
    // Older clients calling /data/query on a streaming operation receive its last line.
    assert_eq!(
        broker
            .query("artifact", "concierge", "split", json!({}))
            .await
            .unwrap(),
        json!({"b":2})
    );
}

#[tokio::test]
async fn discovery_reports_method_and_stream() {
    let broker = DataBroker::from_json(&source(
        "http://127.0.0.1:9",
        json!({"chat":{"path":"/chat","method":"POST","stream":true},"get":{"path":"/get"}}),
    ))
    .unwrap();
    let sources = broker.public_sources(Some("acme")).await;
    let ops = &sources[0].operations;
    assert_eq!(ops["chat"]["method"], "POST");
    assert_eq!(ops["chat"]["stream"], true);
    assert_eq!(ops["get"]["method"], "GET");
    assert_eq!(ops["get"]["stream"], false);
}
