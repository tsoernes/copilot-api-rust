//! Explicitly experimental pass-through routes for GitHub Copilot Fusion.
//!
//! The task API is undocumented. Upstream hosts and paths are fixed here;
//! callers cannot supply a URL or arbitrary path. Every route requires the
//! gateway's configured local API key, and request/response contents are never
//! logged.

use std::time::Duration;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use reqwest::Url;

use crate::server::AppState;

const GITHUB_API_BASE: &str = "https://api.github.com";
const MODELS_PATH: &str = "/agents/swe/models";
const TASKS_PATH: &str = "/cmc_internal/api/agents/tasks";
const ENVIRONMENTS_PATH: &str = "/cmc_internal/api/agents/environments";
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_QUERY_BYTES: usize = 4096;
const MAX_RESOURCE_ID_BYTES: usize = 256;
const UPSTREAM_CONTENT_TYPE: &str = "text/plain;charset=UTF-8";

#[derive(Clone, Copy, Debug)]
struct FusionError {
    status: StatusCode,
    message: &'static str,
}

impl IntoResponse for FusionError {
    fn into_response(self) -> Response {
        crate::server::error(self.status, self.message)
    }
}

fn github_api_base(state: &AppState) -> &str {
    #[cfg(test)]
    if let Some(credential) = &state.test_credential {
        return &credential.api_base;
    }
    #[cfg(not(test))]
    let _ = state;
    GITHUB_API_BASE
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/experimental/fusion/models", get(model_catalog))
        .route(
            "/experimental/fusion/tasks",
            get(task_list).post(task_create),
        )
        .route("/experimental/fusion/tasks/{task_id}", get(task_detail))
        .route(
            "/experimental/fusion/tasks/{task_id}/events",
            get(task_events),
        )
        .route(
            "/experimental/fusion/environments/{environment_id}",
            get(environment_detail),
        )
        .route(
            "/experimental/fusion/environments/{environment_id}/connect",
            get(environment_connect),
        )
}

fn require_local_api_key(state: &AppState, headers: &HeaderMap) -> Result<(), FusionError> {
    let Some(key) = state.api_key.as_deref() else {
        return Err(FusionError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Experimental Fusion routes require COPILOT_API_KEY",
        });
    };
    if !crate::server::authorized(headers, Some(key)) {
        return Err(FusionError {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid or missing API key",
        });
    }
    Ok(())
}

fn query(uri: &Uri) -> Result<Option<String>, FusionError> {
    let Some(query) = uri.query() else {
        return Ok(None);
    };
    if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Err(FusionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid query string",
        });
    }
    Ok(Some(query.to_owned()))
}

fn resource_path(base: &str, id: &str, suffix: &str) -> Result<String, FusionError> {
    if id.is_empty()
        || id.len() > MAX_RESOURCE_ID_BYTES
        || id == "."
        || id == ".."
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(FusionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Fusion resource identifier",
        });
    }
    Ok(format!("{base}/{id}{suffix}"))
}

async fn model_catalog(State(state): State<AppState>, uri: Uri, headers: HeaderMap) -> Response {
    if let Err(error) = require_local_api_key(&state, &headers) {
        return error.into_response();
    }
    let query = match query(&uri) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let credential = match state.credential().await {
        Ok(credential) => credential,
        Err(_) => {
            return crate::server::error(
                StatusCode::BAD_GATEWAY,
                "Could not obtain Copilot credentials",
            );
        }
    };
    forward(
        state,
        "fusion.models",
        &credential.api_base,
        MODELS_PATH,
        query.as_deref(),
        Method::GET,
        None,
    )
    .await
}

async fn task_list(State(state): State<AppState>, uri: Uri, headers: HeaderMap) -> Response {
    if let Err(error) = require_local_api_key(&state, &headers) {
        return error.into_response();
    }
    let query = match query(&uri) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let base = github_api_base(&state).to_owned();
    forward(
        state,
        "fusion.task_list",
        &base,
        TASKS_PATH,
        query.as_deref(),
        Method::GET,
        None,
    )
    .await
}

async fn task_create(State(state): State<AppState>, request: Request) -> Response {
    if let Err(error) = require_local_api_key(&state, request.headers()) {
        return error.into_response();
    }
    if !is_task_content_type(request.headers()) {
        return crate::server::error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json or text/plain",
        );
    }
    let query = match query(request.uri()) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let body = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => {
            return crate::server::error(StatusCode::BAD_REQUEST, "Could not read request body");
        }
    };
    let root = body.iter().find(|byte| !byte.is_ascii_whitespace());
    if root != Some(&b'{') || serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_err() {
        return crate::server::error(StatusCode::BAD_REQUEST, "Expected a JSON object");
    }
    let base = github_api_base(&state).to_owned();
    forward(
        state,
        "fusion.task_create",
        &base,
        TASKS_PATH,
        query.as_deref(),
        Method::POST,
        Some(body),
    )
    .await
}

fn is_task_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value.to_str().is_ok_and(|value| {
        matches!(
            value.split(';').next().unwrap_or_default().trim(),
            "application/json" | "text/plain"
        )
    })
}

async fn task_detail(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    resource_get(
        state,
        &headers,
        &uri,
        "fusion.task_detail",
        TASKS_PATH,
        &task_id,
        "",
    )
    .await
}

async fn task_events(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    resource_get(
        state,
        &headers,
        &uri,
        "fusion.task_events",
        TASKS_PATH,
        &task_id,
        "/events",
    )
    .await
}

async fn environment_detail(
    State(state): State<AppState>,
    Path(environment_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    resource_get(
        state,
        &headers,
        &uri,
        "fusion.environment_detail",
        ENVIRONMENTS_PATH,
        &environment_id,
        "",
    )
    .await
}

async fn environment_connect(
    State(state): State<AppState>,
    Path(environment_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    resource_get(
        state,
        &headers,
        &uri,
        "fusion.environment_connect",
        ENVIRONMENTS_PATH,
        &environment_id,
        "/connect",
    )
    .await
}

async fn resource_get(
    state: AppState,
    headers: &HeaderMap,
    uri: &Uri,
    endpoint: &'static str,
    base_path: &str,
    id: &str,
    suffix: &str,
) -> Response {
    if let Err(error) = require_local_api_key(&state, headers) {
        return error.into_response();
    }
    let query = match query(uri) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let path = match resource_path(base_path, id, suffix) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let base = github_api_base(&state).to_owned();
    forward(
        state,
        endpoint,
        &base,
        &path,
        query.as_deref(),
        Method::GET,
        None,
    )
    .await
}

async fn forward(
    state: AppState,
    endpoint: &'static str,
    base: &str,
    path: &str,
    query: Option<&str>,
    method: Method,
    body: Option<Bytes>,
) -> Response {
    let mut url = match Url::parse(base) {
        Ok(url) => url,
        Err(_) => {
            return crate::server::error(
                StatusCode::BAD_GATEWAY,
                "Invalid configured upstream endpoint",
            );
        }
    };
    url.set_path(path);
    url.set_query(query);

    let mut request = state
        .client
        .request(method, url)
        .bearer_auth(state.auth.github_oauth_token())
        .header("copilot-integration-id", "copilot-web")
        .header("x-client-application", "copilot-cloud-web");
    if let Some(body) = body {
        request = request
            .header(header::CONTENT_TYPE, UPSTREAM_CONTENT_TYPE)
            .body(body);
    }

    let upstream = match tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(request_error)) => {
            let error_class = if request_error.is_timeout() {
                "timeout"
            } else if request_error.is_connect() {
                "connect"
            } else {
                "transport"
            };
            tracing::warn!(endpoint, error_class, "Fusion upstream request failed");
            return crate::server::error(StatusCode::BAD_GATEWAY, "Fusion request failed");
        }
        Err(_) => {
            tracing::warn!(
                endpoint,
                timeout_ms = RESPONSE_HEADERS_TIMEOUT.as_millis(),
                "Fusion response headers timed out"
            );
            return crate::server::error(
                StatusCode::GATEWAY_TIMEOUT,
                "Fusion response headers timed out",
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        tracing::warn!(
            endpoint,
            upstream_status = status.as_u16(),
            "Fusion upstream returned a non-success status"
        );
    }
    let headers = upstream.headers().clone();
    let body = Body::from_stream(upstream.bytes_stream());
    let mut response = Response::builder().status(status);
    for name in [header::CONTENT_TYPE, header::RETRY_AFTER] {
        if let Some(value) = headers.get(&name) {
            response = response.header(name, value);
        }
    }
    response = response.header(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    match response.body(body) {
        Ok(response) => response,
        Err(_) => crate::server::error(StatusCode::BAD_GATEWAY, "Invalid Fusion response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_ids_are_limited_to_a_single_safe_path_segment() {
        assert_eq!(
            resource_path(TASKS_PATH, "task-123_a.b", "/events").unwrap(),
            "/cmc_internal/api/agents/tasks/task-123_a.b/events"
        );
        assert!(resource_path(TASKS_PATH, "../other", "").is_err());
        assert!(resource_path(TASKS_PATH, "%2fother", "").is_err());
        assert!(resource_path(TASKS_PATH, "..", "").is_err());
        assert!(resource_path(TASKS_PATH, &"x".repeat(MAX_RESOURCE_ID_BYTES + 1), "").is_err());
    }
}
