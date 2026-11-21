//! End-to-end proxy tests using an in-process router and an actual loopback upstream.
use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::any,
};
use futures_util::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc};
use tower::ServiceExt;

use crate::server::router_with_test_credential;

struct Captured {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

struct MockState {
    captured: mpsc::UnboundedSender<Captured>,
    chunks: Mutex<Option<mpsc::UnboundedReceiver<Bytes>>>,
    status: StatusCode,
}

async fn mock_handler(State(state): State<Arc<MockState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 17 * 1024 * 1024).await.unwrap();
    state
        .captured
        .send(Captured {
            path: parts.uri.path().to_owned(),
            headers: parts.headers,
            body,
        })
        .unwrap();
    let rx = state.chunks.lock().await.take().unwrap();
    let chunks = stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (Ok::<Bytes, std::io::Error>(item), rx))
    });
    Response::builder()
        .status(state.status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::RETRY_AFTER, "7")
        .header(header::SET_COOKIE, "server=secret")
        .body(Body::from_stream(chunks))
        .unwrap()
}

struct Mock {
    base: String,
    requests: mpsc::UnboundedReceiver<Captured>,
    chunks: mpsc::UnboundedSender<Bytes>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn mock(status: StatusCode) -> Mock {
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (chunks, chunk_rx) = mpsc::unbounded_channel();
    let state = Arc::new(MockState {
        captured: request_tx,
        chunks: Mutex::new(Some(chunk_rx)),
        status,
    });
    let app = Router::new().fallback(any(mock_handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock {
        base,
        requests,
        chunks,
        server,
    }
}
fn request(path: &str, body: Body) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, "localhost:4141")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}
fn app(base: &str, key: Option<&str>) -> Router {
    router_with_test_credential(
        base.to_owned(),
        "fake-copilot-token".into(),
        key.map(str::to_owned),
    )
    .unwrap()
}

#[tokio::test]
async fn models_endpoint_uses_copilot_model_access_and_preserves_catalog() {
    let mut upstream = mock(StatusCode::OK).await;
    upstream
        .chunks
        .send(Bytes::from_static(
            br#"{"data":[{"id":"fake-model","capabilities":{"type":"chat"}}]}"#,
        ))
        .unwrap();
    let request = Request::builder()
        .uri("/v1/models")
        .header(header::HOST, "localhost:4141")
        .header(header::AUTHORIZATION, "Bearer local-secret")
        .body(Body::empty())
        .unwrap();
    let response = app(&upstream.base, Some("local-secret"))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let captured = upstream.requests.recv().await.unwrap();
    assert_eq!(captured.path, "/models");
    assert_eq!(
        captured.headers[header::AUTHORIZATION],
        "Bearer fake-copilot-token"
    );
    assert_eq!(captured.headers["x-interaction-type"], "model-access");
    let mut body = response.into_body().into_data_stream();
    let data = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(json["data"][0]["id"], "fake-model");
}

#[tokio::test]
async fn forwards_allowed_headers_body_and_status_but_not_client_secrets() {
    let mut upstream = mock(StatusCode::TOO_MANY_REQUESTS).await;
    upstream
        .chunks
        .send(Bytes::from_static(b"upstream error"))
        .unwrap();
    let mut req = request(
        "/v1/messages",
        Body::from(r#"{"model":"claude","messages":[{"role":"user","content":"hello"}]}"#),
    );
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "Bearer local-secret".parse().unwrap(),
    );
    req.headers_mut()
        .insert("x-api-key", "other-local-secret".parse().unwrap());
    req.headers_mut()
        .insert(header::COOKIE, "session=private".parse().unwrap());
    req.headers_mut()
        .insert("anthropic-version", "2023-06-01".parse().unwrap());
    req.headers_mut().insert(
        "anthropic-beta",
        "extended-cache-ttl-2025-04-11,unknown-beta"
            .parse()
            .unwrap(),
    );
    let response = app(&upstream.base, Some("local-secret"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "7");
    assert!(response.headers().get(header::SET_COOKIE).is_none());
    let captured = tokio::time::timeout(Duration::from_secs(2), upstream.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(captured.path, "/v1/messages");
    assert_eq!(
        captured.headers[header::AUTHORIZATION],
        "Bearer fake-copilot-token"
    );
    assert!(captured.headers.get("x-api-key").is_none());
    assert!(captured.headers.get(header::COOKIE).is_none());
    assert_eq!(captured.headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        captured.headers["anthropic-beta"],
        "extended-cache-ttl-2025-04-11"
    );
    assert_eq!(captured.headers["x-initiator"], "user");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap()["model"],
        "claude"
    );
    let chunk = tokio::time::timeout(
        Duration::from_secs(2),
        response.into_body().into_data_stream().next(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(chunk, Bytes::from_static(b"upstream error"));
}

#[tokio::test]
async fn streams_first_chunk_before_upstream_finishes() {
    let mut upstream = mock(StatusCode::OK).await;
    upstream
        .chunks
        .send(Bytes::from_static(b"data: first\n\n"))
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        app(&upstream.base, None).oneshot(request(
            "/v1/chat/completions",
            Body::from(r#"{"model":"m","stream":true,"messages":[]}"#),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let captured = upstream.requests.recv().await.unwrap();
    assert_eq!(captured.path, "/chat/completions");
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"data: first\n\n"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), body.next())
            .await
            .is_err(),
        "response must not end while upstream stays open"
    );
    upstream
        .chunks
        .send(Bytes::from_static(b"data: second\n\n"))
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second, Bytes::from_static(b"data: second\n\n"));
}

#[tokio::test]
async fn rejects_unsafe_hosts_origins_missing_key_and_invalid_requests_before_upstream() {
    let mut upstream = mock(StatusCode::OK).await;
    let keyless = app(&upstream.base, None);
    let mut foreign_host = request("/v1/chat/completions", Body::from("{}"));
    foreign_host
        .headers_mut()
        .insert(header::HOST, "evil.example".parse().unwrap());
    assert_eq!(
        keyless
            .clone()
            .oneshot(foreign_host)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut browser = request("/v1/chat/completions", Body::from("{}"));
    browser
        .headers_mut()
        .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
    assert_eq!(
        keyless.clone().oneshot(browser).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let locked = app(&upstream.base, Some("local-secret"));
    assert_eq!(
        locked
            .oneshot(request("/v1/chat/completions", Body::from("{}")))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        keyless
            .clone()
            .oneshot(request("/v1/chat/completions", Body::from("not-json")))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut no_type = request("/v1/chat/completions", Body::from("{}"));
    no_type.headers_mut().remove(header::CONTENT_TYPE);
    assert_eq!(
        keyless.clone().oneshot(no_type).await.unwrap().status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let huge = vec![b'x'; 16 * 1024 * 1024 + 1];
    assert_eq!(
        keyless
            .oneshot(request("/v1/chat/completions", Body::from(huge)))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), upstream.requests.recv())
            .await
            .is_err(),
        "rejected requests must not reach upstream"
    );
}
