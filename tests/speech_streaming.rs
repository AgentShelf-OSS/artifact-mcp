// Reuse the complete, authorized route harness without changing the shared native-test target.
include!("native/u19_routes.rs");

use std::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use futures_util::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

#[tokio::test]
async fn streaming_route_forwards_first_upstream_chunk_before_upstream_finishes() {
    let upstream = first_chunk_then_waiting_upstream().await;
    let token_path = std::env::temp_dir().join(format!(
        "artifact-mcp-speech-streaming-test-{}",
        std::process::id()
    ));
    std::fs::write(&token_path, "test-token\n").expect("write test token");
    // These process-local settings configure the production speech client. This test target has
    // no other speech test, so its OnceLock clients are initialized only by this test.
    unsafe {
        std::env::set_var("TTS_ENABLED", "1");
        std::env::set_var(
            "POCKET_TTS_WORKER_URL",
            format!("http://{}", upstream.address),
        );
        std::env::set_var("POCKET_TTS_WORKER_TOKEN_FILE", &token_path);
    }

    let response = build_router(deps(std::sync::Arc::new(RouteHarness::new(
        viewer("acme", false),
        Some(meta("acme")),
    ))))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/abc123def456/speech/stream")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"hello","voice":"pocket_alba"}"#))
            .expect("stream request"),
    )
    .await
    .expect("stream response");
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(100), body.next())
        .await
        .expect("the first upstream chunk reaches the route response without buffering")
        .expect("first response chunk")
        .expect("stream response is infallible");

    assert_eq!(first.as_ref(), b"pcm");
    assert!(!upstream.finished.load(Ordering::SeqCst));
    upstream
        .release
        .send(())
        .expect("upstream waits for this test");
    upstream
        .task
        .await
        .expect("controlled upstream task completes");
    std::fs::remove_file(token_path).expect("remove test token");
}

struct ControlledUpstream {
    address: SocketAddr,
    release: oneshot::Sender<()>,
    finished: std::sync::Arc<AtomicBool>,
    task: JoinHandle<()>,
}

async fn first_chunk_then_waiting_upstream() -> ControlledUpstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind controlled upstream");
    let address = listener.local_addr().expect("controlled upstream address");
    let (finish_tx, finish_rx) = oneshot::channel();
    let finished = std::sync::Arc::new(AtomicBool::new(false));
    let server_finished = std::sync::Arc::clone(&finished);
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept streaming request");
        let mut request_start = [0; 1];
        socket
            .read_exact(&mut request_start)
            .await
            .expect("read streaming request");
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.artifact.pcm\r\nTransfer-Encoding: chunked\r\n\r\n3\r\npcm\r\n",
            )
            .await
            .expect("write first upstream chunk");
        finish_rx.await.expect("test releases controlled upstream");
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("finish controlled upstream response");
        server_finished.store(true, Ordering::SeqCst);
    });
    ControlledUpstream {
        address,
        release: finish_tx,
        finished,
        task,
    }
}
