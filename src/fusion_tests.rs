use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
    routing::any,
};
use futures_util::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc};
use tower::ServiceExt;

use crate::server::router_with_test_credential;

struct Captured {
    method: Method,
    path_and_query: String,
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
    let body = to_bytes(body, usize::MAX).await.unwrap();
    state
        .captured
        .send(Captured {
            method: parts.method,
            path_and_query: parts.uri.to_string(),
            headers: parts.headers,
            body,
        })
        .unwrap();
    let rx = state.chunks.lock().await.take().unwrap();
    let chunks = stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), rx))
    });
    Response::builder()
        .status(state.status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::RETRY_AFTER, "9")
        .header(header::SET_COOKIE, "server=private")
        .body(Body::from_stream(chunks))
        .unwrap()
}

struct Mock {
    base: String,
    requests: mpsc::UnboundedReceiver<Captured>,
    chunks: Option<mpsc::UnboundedSender<Bytes>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Mock {
    fn send_chunk(&self, chunk: Bytes) {
        self.chunks.as_ref().unwrap().send(chunk).unwrap();
    }

    fn close_chunks(&mut self) {
        self.chunks.take();
    }
}

async fn mock(status: StatusCode) -> Mock {
    let (captured, requests) = mpsc::unbounded_channel();
    let (chunks, chunk_rx) = mpsc::unbounded_channel();
    let state = Arc::new(MockState {
        captured,
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
        chunks: Some(chunks),
        server,
    }
}

fn app(base: &str, api_key: Option<&str>) -> Router {
    router_with_test_credential(
        base.to_owned(),
        "test-copilot-token-that-must-not-be-forwarded".into(),
        api_key.map(str::to_owned),
    )
    .unwrap()
}

fn request(method: Method, path: &str, body: Body) -> Request {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost:4141")
        .header(header::AUTHORIZATION, "Bearer local-test-key")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn model_catalog_uses_oauth_and_integration_headers_and_streams_safely() {
    let mut upstream = mock(StatusCode::OK).await;
    upstream.send_chunk(Bytes::from_static(b"{\"object\":"));
    let mut req = request(
        Method::GET,
        "/experimental/fusion/models?limit=3",
        Body::empty(),
    );
    req.headers_mut()
        .insert(header::COOKIE, "session=local-secret".parse().unwrap());
    req.headers_mut()
        .insert("x-api-key", "another-local-secret".parse().unwrap());
    req.headers_mut()
        .insert("copilot-integration-id", "client-value".parse().unwrap());
    req.headers_mut()
        .insert("x-client-application", "client-value".parse().unwrap());

    let response = app(&upstream.base, Some("local-test-key"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response.headers().get(header::SET_COOKIE).is_none());

    let captured = tokio::time::timeout(Duration::from_secs(2), upstream.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(captured.method, Method::GET);
    assert_eq!(captured.path_and_query, "/agents/swe/models?limit=3");
    assert_eq!(
        captured.headers[header::AUTHORIZATION],
        "Bearer test-github-token"
    );
    assert_eq!(captured.headers["copilot-integration-id"], "copilot-web");
    assert_eq!(
        captured.headers["x-client-application"],
        "copilot-cloud-web"
    );
    assert!(captured.headers.get(header::COOKIE).is_none());
    assert!(captured.headers.get("x-api-key").is_none());

    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"{\"object\":"));
}

#[tokio::test]
async fn task_creation_forwards_explicit_json_and_preserves_created_status() {
    let mut upstream = mock(StatusCode::CREATED).await;
    upstream.send_chunk(Bytes::from_static(b"{\"id\":\"task-1\"}"));
    let body = r#"{"environment_id":"env-1","prompt":"synthetic task"}"#;
    let mut req = request(Method::POST, "/experimental/fusion/tasks", Body::from(body));
    req.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    req.headers_mut()
        .insert(header::COOKIE, "session=local-secret".parse().unwrap());

    let response = app(&upstream.base, Some("local-test-key"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = tokio::time::timeout(Duration::from_secs(2), upstream.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(captured.method, Method::POST);
    assert_eq!(captured.path_and_query, "/cmc_internal/api/agents/tasks");
    assert_eq!(captured.body, Bytes::copy_from_slice(body.as_bytes()));
    assert_eq!(
        captured.headers[header::CONTENT_TYPE],
        "text/plain;charset=UTF-8"
    );
    assert_eq!(
        captured.headers[header::AUTHORIZATION],
        "Bearer test-github-token"
    );
    assert_eq!(captured.headers["copilot-integration-id"], "copilot-web");
    assert_eq!(
        captured.headers["x-client-application"],
        "copilot-cloud-web"
    );
    assert!(captured.headers.get(header::COOKIE).is_none());
    assert!(captured.headers.get("x-api-key").is_none());

    upstream.close_chunks();
    let response_body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(response_body, Bytes::from_static(b"{\"id\":\"task-1\"}"));
}

#[tokio::test]
async fn task_and_environment_reads_map_only_to_observed_fixed_paths() {
    let cases = [
        (
            "/experimental/fusion/tasks?state=active",
            "/cmc_internal/api/agents/tasks?state=active",
        ),
        (
            "/experimental/fusion/tasks/task-42",
            "/cmc_internal/api/agents/tasks/task-42",
        ),
        (
            "/experimental/fusion/tasks/task-42/events",
            "/cmc_internal/api/agents/tasks/task-42/events",
        ),
        (
            "/experimental/fusion/environments/env-7",
            "/cmc_internal/api/agents/environments/env-7",
        ),
        (
            "/experimental/fusion/environments/env-7/connect",
            "/cmc_internal/api/agents/environments/env-7/connect",
        ),
    ];
    for (local_path, expected_path) in cases {
        let mut upstream = mock(StatusCode::ACCEPTED).await;
        upstream.send_chunk(Bytes::from_static(b"{}"));
        let response = app(&upstream.base, Some("local-test-key"))
            .oneshot(request(Method::GET, local_path, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{local_path}");
        let captured = tokio::time::timeout(Duration::from_secs(2), upstream.requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(captured.method, Method::GET, "{local_path}");
        assert_eq!(captured.path_and_query, expected_path, "{local_path}");
        assert_eq!(
            captured.headers[header::AUTHORIZATION],
            "Bearer test-github-token",
            "{local_path}"
        );
    }
}

#[tokio::test]
async fn fusion_routes_require_a_configured_local_api_key() {
    let upstream = mock(StatusCode::OK).await;
    let response = app(&upstream.base, None)
        .oneshot(request(
            Method::GET,
            "/experimental/fusion/tasks",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn fusion_routes_reject_invalid_local_api_keys_without_upstream_call() {
    let mut upstream = mock(StatusCode::OK).await;
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/experimental/fusion/tasks")
        .header(header::HOST, "localhost:4141")
        .header(header::AUTHORIZATION, "Bearer wrong-key")
        .body(Body::empty())
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", "wrong-key".parse().unwrap());
    let response = app(&upstream.base, Some("local-test-key"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), upstream.requests.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn task_detail_preserves_upstream_not_found_status_and_filters_cookie() {
    let mut upstream = mock(StatusCode::NOT_FOUND).await;
    upstream.send_chunk(Bytes::from_static(b"{\"error\":\"not found\"}"));
    let mut req = request(
        Method::GET,
        "/experimental/fusion/tasks/missing-task",
        Body::empty(),
    );
    req.headers_mut()
        .insert(header::COOKIE, "session=private".parse().unwrap());
    let response = app(&upstream.base, Some("local-test-key"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::RETRY_AFTER], "9");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let captured = upstream.requests.recv().await.unwrap();
    assert_eq!(
        captured.path_and_query,
        "/cmc_internal/api/agents/tasks/missing-task"
    );
    assert!(captured.headers.get(header::COOKIE).is_none());
    upstream.close_chunks();
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(body, Bytes::from_static(b"{\"error\":\"not found\"}"));
}

#[tokio::test]
async fn task_create_rejects_non_object_json_without_upstream_call() {
    let mut upstream = mock(StatusCode::CREATED).await;
    let mut req = request(Method::POST, "/experimental/fusion/tasks", Body::from("[]"));
    req.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    let response = app(&upstream.base, Some("local-test-key"))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), upstream.requests.recv())
            .await
            .is_err()
    );
}
