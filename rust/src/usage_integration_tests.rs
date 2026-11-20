use crate::{server::router_with_test_usage, usage::UsageStore};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    http::{Request, StatusCode, header},
    routing::post,
};
use futures_util::{StreamExt, stream};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

fn post_request(path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, "localhost:4141")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer local-key")
        .body(Body::from(
            r#"{"model":"example","messages":[],"stream":true}"#,
        ))
        .unwrap()
}
fn get_request(path: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(path)
        .header(header::HOST, "localhost:4141");
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn original_viewer_and_usage_endpoints_are_authenticated_and_record_sse() {
    let chunks = vec![
        Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n"),
        Bytes::from_static(b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":2}},\"copilot_usage\":{\"total_nano_aiu\":150}}\n\n"),
        Bytes::from_static(b"data: [DONE]\n\n"),
    ];
    let upstream = Router::new().route(
        "/chat/completions",
        post(move || {
            let chunks = chunks.clone();
            async move {
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream::iter(
                        chunks.into_iter().map(Ok::<Bytes, std::io::Error>),
                    )),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let usage = Arc::new(UsageStore::in_memory().unwrap());
    let app = router_with_test_usage(
        format!("http://{addr}"),
        Some("local-key".into()),
        usage.clone(),
    )
    .unwrap();
    let viewer = app
        .clone()
        .oneshot(get_request("/usage-viewer", None))
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::OK);
    let html = to_bytes(viewer.into_body(), 1024 * 1024).await.unwrap();
    assert!(
        std::str::from_utf8(&html)
            .unwrap()
            .contains("window.location.origin + \"/usage\"")
    );
    assert_eq!(
        app.clone()
            .oneshot(get_request("/token-usage", None))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = app
        .clone()
        .oneshot(post_request("/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let raw = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(std::str::from_utf8(&raw).unwrap().contains("data: [DONE]"));
    let response = app
        .clone()
        .oneshot(get_request(
            "/token-usage?period=lifetime",
            Some("local-key"),
        ))
        .await
        .unwrap();
    let summary: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(summary["totals"]["input_tokens"], 8);
    assert_eq!(summary["totals"]["cache_read_input_tokens"], 2);
    assert_eq!(summary["totals"]["output_tokens"], 3);
    assert_eq!(summary["totals"]["request_count"], 1);
    assert_eq!(summary["totals"]["costs"][0]["total_cost_nanos"], 2);
    let response = app
        .oneshot(get_request(
            "/token-usage/events?period=lifetime",
            Some("local-key"),
        ))
        .await
        .unwrap();
    let events: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(events["items"][0]["request_outcome"], "completed");
    assert_eq!(events["items"][0]["cost"]["source"], "copilot_aiu");
    task.abort();
}

#[tokio::test]
async fn cancelled_stream_records_observed_usage_without_waiting_for_eof() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tx.send(Bytes::from_static(
        b"data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
    ))
    .unwrap();
    let state = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let upstream = Router::new().route(
        "/chat/completions",
        post(move || {
            let state = state.clone();
            async move {
                let rx = state.lock().await.take().unwrap();
                let stream = stream::unfold(rx, |mut rx| async move {
                    rx.recv()
                        .await
                        .map(|b| (Ok::<Bytes, std::io::Error>(b), rx))
                });
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let usage = Arc::new(UsageStore::in_memory().unwrap());
    let app = router_with_test_usage(format!("http://{addr}"), None, usage.clone()).unwrap();
    let mut req = post_request("/chat/completions");
    req.headers_mut().remove(header::AUTHORIZATION);
    let response = app.oneshot(req).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().unwrap().starts_with(b"data:"));
    drop(body);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = usage.events("lifetime", 1, 20).unwrap();
            if result["total"] == 1 {
                assert_eq!(result["items"][0]["request_outcome"], "cancelled");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    task.abort();
}

#[tokio::test]
async fn json_and_failed_sse_usage_are_recorded_and_invalid_periods_default() {
    let upstream = Router::new()
        .route("/embeddings", post(|| async {
            ([(header::CONTENT_TYPE, "application/json")], r#"{"usage":{"prompt_tokens":12,"total_tokens":12}}"#)
        }))
        .route("/responses", post(|| async {
            ([(header::CONTENT_TYPE, "text/event-stream")], "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\nevent: error\ndata: {\"type\":\"error\"}\n\n")
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let usage = Arc::new(UsageStore::in_memory().unwrap());
    let app = router_with_test_usage(format!("http://{addr}"), None, usage.clone()).unwrap();
    for path in ["/v1/embeddings", "/v1/responses"] {
        let response = app.clone().oneshot(post_request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    }
    let events = usage.events("lifetime", 1, 20).unwrap();
    assert_eq!(events["total"], 2);
    let items = events["items"].as_array().unwrap();
    let failed = items
        .iter()
        .find(|row| row["endpoint"] == "responses")
        .unwrap();
    assert_eq!(failed["request_outcome"], "error");
    assert_eq!(failed["input_tokens"], 10);
    assert_eq!(failed["output_tokens"], 5);
    let embeddings = items
        .iter()
        .find(|row| row["endpoint"] == "embeddings")
        .unwrap();
    assert_eq!(embeddings["total_tokens"], 12);
    assert_eq!(embeddings["cost"], serde_json::Value::Null);
    assert_eq!(usage.summary("week").unwrap()["period"], "last_7_days");
    assert_eq!(usage.summary("month").unwrap()["period"], "last_30_days");
    assert_eq!(usage.summary("invalid").unwrap()["period"], "today");
    task.abort();
}

#[tokio::test]
async fn truncated_upstream_stream_records_error_outcome() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\nConnection: close\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\n").await.unwrap();
    });
    let usage = Arc::new(UsageStore::in_memory().unwrap());
    let app = router_with_test_usage(format!("http://{addr}"), None, usage.clone()).unwrap();
    let response = app
        .oneshot(post_request("/chat/completions"))
        .await
        .unwrap();
    assert!(to_bytes(response.into_body(), 1024 * 1024).await.is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = usage.events("lifetime", 1, 20).unwrap();
            if result["total"] == 1 {
                assert_eq!(result["items"][0]["request_outcome"], "error");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.await.unwrap();
}
