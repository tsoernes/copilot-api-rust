use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    auth::CopilotAuth,
    quota::QuotaClient,
    usage::{UsageEvent, UsageStore},
    usage_stream::UsageStream,
};

// Request JSON is buffered for header derivation and minor normalization; responses are streamed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) auth: Arc<CopilotAuth>,
    pub(crate) client: reqwest::Client,
    pub(crate) api_key: Option<String>,
    device_id: String,
    quota: Option<Arc<QuotaClient>>,
    usage: Option<Arc<UsageStore>>,
    #[cfg(test)]
    pub(crate) test_credential: Option<crate::auth::CopilotCredential>,
}

pub fn router(
    auth: Arc<CopilotAuth>,
    api_key: Option<String>,
    quota: Arc<QuotaClient>,
    usage: Arc<UsageStore>,
) -> Result<Router> {
    let mut state = new_state(auth, api_key)?;
    state.quota = Some(quota);
    state.usage = Some(usage);
    Ok(build_router(state))
}

fn new_state(auth: Arc<CopilotAuth>, api_key: Option<String>) -> Result<AppState> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()?;
    Ok(AppState {
        auth,
        client,
        api_key,
        device_id: Uuid::new_v4().to_string(),
        quota: None,
        usage: None,
        #[cfg(test)]
        test_credential: None,
    })
}

#[cfg(test)]
pub(crate) fn router_with_test_credential(
    api_base: String,
    token: String,
    api_key: Option<String>,
) -> Result<Router> {
    let github = crate::auth::GitHubAuth::new(crate::auth::AuthConfig::default())?;
    let auth = Arc::new(CopilotAuth::new(github, "test-github-token".into())?);
    let mut state = new_state(auth, api_key)?;
    state.test_credential = Some(crate::auth::CopilotCredential { api_base, token });
    Ok(build_router(state))
}

#[cfg(test)]
pub(crate) fn router_with_test_usage(
    api_base: String,
    api_key: Option<String>,
    usage: Arc<UsageStore>,
) -> Result<Router> {
    let github = crate::auth::GitHubAuth::new(crate::auth::AuthConfig::default())?;
    let auth = Arc::new(CopilotAuth::new(github, "test-github-token".into())?);
    let mut state = new_state(auth, api_key)?;
    state.test_credential = Some(crate::auth::CopilotCredential {
        api_base,
        token: "test-copilot-token".into(),
    });
    state.usage = Some(usage);
    Ok(build_router(state))
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "copilot-api-rust (partial port)" }))
        .route("/health", get(health))
        .route("/usage-viewer", get(usage_viewer))
        .route("/usage-viewer/", get(usage_viewer))
        .route("/usage", get(account_usage))
        .route("/usage/{provider}", get(subscription_usage))
        .route("/token-usage", get(token_summary))
        .route("/token-usage/daily", get(token_daily))
        .route("/token-usage/events", get(token_events))
        .route("/token-usage/sessions", get(token_sessions))
        .route("/token-usage/rates", get(token_rates))
        .route("/models", get(proxy))
        .route("/v1/models", get(proxy))
        .route("/chat/completions", post(proxy))
        .route("/v1/chat/completions", post(proxy))
        .route("/embeddings", post(proxy))
        .route("/v1/embeddings", post(proxy))
        .route("/responses", post(proxy))
        .route("/v1/responses", post(proxy))
        .route("/v1/messages", post(proxy))
        .merge(crate::fusion::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_browser_access,
        ))
        .with_state(state)
}

async fn health() -> Response {
    let mut response =
        axum::Json(json!({"service":"copilot-api-rust","status":"ok"})).into_response();
    response.headers_mut().insert(
        HeaderName::from_static("x-copilot-api-service"),
        HeaderValue::from_static("copilot-api-rust"),
    );
    response
}

impl AppState {
    pub(crate) async fn credential(&self) -> Result<crate::auth::CopilotCredential> {
        #[cfg(test)]
        if let Some(credential) = &self.test_credential {
            return Ok(credential.clone());
        }
        self.auth.credential().await
    }
}

// This gateway has no browser UI. Reject browser Origins even with an API key,
// and reject untrusted Host/authority values when no key protects the server.
// The latter mitigates DNS rebinding: a hostile hostname must not be allowed to
// resolve to a keyless loopback listener and access it as its own origin.
async fn protect_browser_access(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return error(StatusCode::FORBIDDEN, "Browser origins are not supported");
    }
    if state.api_key.is_none() && !loopback_authority(&request) {
        return error(
            StatusCode::FORBIDDEN,
            "A loopback Host is required without an API key",
        );
    }
    next.run(request).await
}

fn is_loopback_authority(authority: &str) -> bool {
    let Ok(authority) = authority.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    let Some(suffix) = authority.as_str().strip_prefix(authority.host()) else {
        return false;
    };
    if !suffix.is_empty()
        && !suffix
            .strip_prefix(':')
            .is_some_and(|port| port.parse::<u16>().is_ok())
    {
        return false;
    }
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn loopback_authority(request: &Request) -> bool {
    let hosts = request.headers().get_all(header::HOST);
    let mut hosts = hosts.iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return false;
    }
    let host_ok = host.map(|h| h.to_str().is_ok_and(is_loopback_authority));
    let uri_ok = request
        .uri()
        .authority()
        .map(|a| is_loopback_authority(a.as_str()));
    // At least one authority is required, and neither may contradict the rule.
    (host_ok.is_some() || uri_ok.is_some()) && host_ok.unwrap_or(true) && uri_ok.unwrap_or(true)
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let value = values.next();
    if values.next().is_some() {
        return false;
    }
    value.and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    })
}

fn anthropic_beta(headers: &HeaderMap, payload: &Value) -> Option<String> {
    const ALLOWED: &[&str] = &[
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        "advanced-tool-use-2025-11-20",
        "extended-cache-ttl-2025-04-11",
        "mid-conversation-output-config-2026-07-01",
        "mid-conversation-tool-changes-2026-07-01",
        "per-turn-control-2026-07-01",
    ];
    if let Some(value) = headers.get("anthropic-beta") {
        let value = value.to_str().ok()?;
        let allowed: Vec<_> = value
            .split(',')
            .map(str::trim)
            .filter(|v| ALLOWED.contains(v))
            .collect();
        return (!allowed.is_empty()).then(|| allowed.join(","));
    }
    let thinking = payload.get("thinking")?;
    if thinking.get("type").and_then(Value::as_str) != Some("adaptive")
        && thinking
            .get("budget_tokens")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
    {
        return Some("interleaved-thinking-2025-05-14".into());
    }
    None
}

pub(crate) fn authorized(headers: &HeaderMap, key: Option<&str>) -> bool {
    let Some(key) = key else { return true };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let anthropic = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    [bearer, anthropic]
        .into_iter()
        .flatten()
        .any(|candidate| bool::from(candidate.as_bytes().ct_eq(key.as_bytes())))
}

pub(crate) fn error(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(json!({"error": {"message": message}}))).into_response()
}

async fn usage_viewer() -> Response {
    // Keep the bundled viewer, but default to the origin that served it instead
    // of assuming localhost:4141, which breaks on custom ports and 127.0.0.1.
    static PAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let page = PAGE.get_or_init(|| {
        include_str!("../assets/usage-viewer.html")
            .replace(
                "const DEFAULT_ENDPOINT = \"http://localhost:4141/usage\";",
                "const DEFAULT_ENDPOINT = window.location.origin + \"/usage\";",
            )
            .replace(
                "if (endpointFromUrl) {\n            if (state.activeTab",
                "if (initialEndpoint) {\n            if (state.activeTab",
            )
    });
    ([(header::CACHE_CONTROL, "no-store")], Html(page.as_str())).into_response()
}

async fn account_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, state.api_key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Invalid or missing API key");
    }
    let Some(quota) = &state.quota else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Usage is unavailable");
    };
    match quota.fetch_raw().await {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error":"Failed to fetch Copilot usage"})),
        )
            .into_response(),
    }
}

async fn subscription_usage(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&headers, state.api_key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Invalid or missing API key");
    }
    if provider != "github-copilot" {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error":"Unsupported subscription provider"})),
        )
            .into_response();
    }
    let Some(quota) = &state.quota else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Usage is unavailable");
    };
    match quota.fetch_normalized().await {
        Ok(value) => {
            let value = if let Some(usage) = state.usage.clone() {
                let fallback = value.clone();
                match tokio::task::spawn_blocking(move || {
                    let mut value = value;
                    usage.record_quota_snapshots(&mut value)?;
                    Ok::<_, anyhow::Error>(value)
                })
                .await
                {
                    Ok(Ok(value)) => value,
                    _ => {
                        tracing::warn!("Could not persist quota samples; omitting runout estimate");
                        fallback
                    }
                }
            } else {
                value
            };
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error":"Failed to fetch subscription usage"})),
        )
            .into_response(),
    }
}

#[derive(Clone, Copy)]
enum UsageQuery {
    Summary,
    Daily,
    Events,
    Sessions,
    Rates,
}

async fn query_usage(
    state: AppState,
    headers: HeaderMap,
    query: HashMap<String, String>,
    kind: UsageQuery,
) -> Response {
    if !authorized(&headers, state.api_key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Invalid or missing API key");
    }
    let Some(usage) = state.usage else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Usage storage is unavailable",
        );
    };
    let period = query
        .get("period")
        .cloned()
        .unwrap_or_else(|| "today".into());
    let page = query
        .get("page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1);
    let page_size = query
        .get("page_size")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(20)
        .min(100);
    let session_id = query.get("session_id").cloned();
    if matches!(kind, UsageQuery::Rates)
        && session_id
            .as_ref()
            .is_some_and(|id| id.len() > 1024 || id.chars().any(char::is_control))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid session identifier");
    }
    let result = tokio::task::spawn_blocking(move || match kind {
        UsageQuery::Summary => usage.summary(&period),
        UsageQuery::Daily => usage.daily(&period),
        UsageQuery::Events => usage.events(&period, page, page_size),
        UsageQuery::Sessions => usage.sessions(&period, page, page_size),
        UsageQuery::Rates => usage.rates(session_id.as_deref()),
    })
    .await;
    match result {
        Ok(Ok(value)) => ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response(),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Usage query failed"),
    }
}

async fn record_failed_request(
    usage: Option<Arc<UsageStore>>,
    event: Option<UsageEvent>,
    started: Instant,
) {
    let (Some(usage), Some(mut event)) = (usage, event) else {
        return;
    };
    let request_id = event.request_id.clone().unwrap_or_default();
    event.duration_ms = Some(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
    event.request_outcome = Some("error".into());
    if !matches!(
        tokio::task::spawn_blocking(move || usage.record(event)).await,
        Ok(Ok(()))
    ) {
        tracing::error!(request_id = %request_id, "failed to persist failed Copilot request");
    }
}

async fn token_summary(
    State(s): State<AppState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    query_usage(s, h, q, UsageQuery::Summary).await
}
async fn token_daily(
    State(s): State<AppState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    query_usage(s, h, q, UsageQuery::Daily).await
}
async fn token_events(
    State(s): State<AppState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    query_usage(s, h, q, UsageQuery::Events).await
}
async fn token_sessions(
    State(s): State<AppState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    query_usage(s, h, q, UsageQuery::Sessions).await
}
async fn token_rates(
    State(s): State<AppState>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    query_usage(s, h, q, UsageQuery::Rates).await
}

fn upstream_path(path: &str) -> &str {
    match path {
        "/v1/models" => "/models",
        "/v1/chat/completions" => "/chat/completions",
        "/v1/embeddings" => "/embeddings",
        "/v1/responses" => "/responses",
        _ => path,
    }
}

fn contains_image(v: &Value) -> bool {
    match v {
        Value::Array(values) => values.iter().any(contains_image),
        Value::Object(map) => {
            map.get("type")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "image" || s == "image_url")
                || map.values().any(contains_image)
        }
        _ => false,
    }
}

struct PreparedPayload {
    body: Bytes,
    vision: bool,
    initiator: &'static str,
    model: String,
    anthropic_beta: Option<String>,
    stripped_opaque_reasoning: usize,
}

impl PreparedPayload {
    fn empty() -> Self {
        Self {
            body: Bytes::new(),
            vision: false,
            initiator: "user",
            model: "unknown".into(),
            anthropic_beta: None,
            stripped_opaque_reasoning: 0,
        }
    }
}

fn strip_opaque_responses_state(value: &mut Value) -> usize {
    fn strip(value: &mut Value) -> usize {
        match value {
            Value::Array(values) => values.iter_mut().map(strip).sum(),
            Value::Object(object) => {
                let mut removed = usize::from(object.remove("encrypted_content").is_some());
                for child in object.values_mut() {
                    removed = removed.saturating_add(strip(child));
                }
                removed
            }
            _ => 0,
        }
    }

    let mut removed = 0usize;
    if let Some(input) = value.get_mut("input") {
        removed = removed.saturating_add(strip(input));
        if let Some(items) = input.as_array_mut() {
            let original = items.len();
            items.retain(|item| {
                let is_reasoning = item.get("type").and_then(Value::as_str) == Some("reasoning");
                let has_summary = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .is_some_and(|summary| !summary.is_empty());
                let has_content = item
                    .get("content")
                    .is_some_and(|content| !content.is_null())
                    && item.get("content").is_some_and(|content| {
                        !content.is_array()
                            || content
                                .as_array()
                                .is_some_and(|content| !content.is_empty())
                    });
                !is_reasoning || has_summary || has_content
            });
            removed = removed.saturating_add(original.saturating_sub(items.len()));
        }
    }
    if let Some(include) = value.get_mut("include").and_then(Value::as_array_mut) {
        let original = include.len();
        include.retain(|item| {
            item.as_str() != Some("reasoning.encrypted_content")
                && item.as_str() != Some("encrypted_content")
        });
        removed = removed.saturating_add(original.saturating_sub(include.len()));
    }
    removed
}

fn prepare_payload(path: &str, body: Bytes, headers: &HeaderMap) -> Result<PreparedPayload, ()> {
    let parsed = serde_json::from_slice::<Value>(&body).map_err(|_| ());
    // The parsed Value owns its strings. Drop our reference to the original
    // input before allocating the canonical serialized body; other Bytes
    // owners, if any, may still keep the input allocation alive.
    drop(body);
    let mut value = parsed?;
    if !value.is_object() {
        return Err(());
    }
    let vision = contains_image(&value);
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .chars()
        .take(256)
        .collect();
    let beta = if path == "/v1/messages" {
        anthropic_beta(headers, &value)
    } else {
        None
    };
    let last = value
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last());
    let mut initiator = if matches!(
        last.and_then(|m| m.get("role")).and_then(Value::as_str),
        Some("assistant" | "tool")
    ) {
        "agent"
    } else {
        "user"
    };
    if path == "/v1/messages" {
        initiator = match last {
            Some(m) if m.get("role").and_then(Value::as_str) == Some("user") => {
                match m.get("content") {
                    Some(Value::Array(blocks))
                        if blocks.iter().all(|b| {
                            b.get("type").and_then(Value::as_str) == Some("tool_result")
                        }) =>
                    {
                        "agent"
                    }
                    _ => "user",
                }
            }
            _ => "agent",
        };
    }
    let stripped_opaque_reasoning = if path == "/responses" {
        let object = value.as_object_mut().ok_or(())?;
        object.remove("service_tier");
        if let Some(n) = object.get("max_output_tokens").and_then(Value::as_u64)
            && n > 0
            && n < 16
        {
            object.insert("max_output_tokens".into(), json!(16));
        }
        strip_opaque_responses_state(&mut value)
    } else {
        0
    };
    let body = Bytes::from(serde_json::to_vec(&value).map_err(|_| ())?);
    Ok(PreparedPayload {
        body,
        vision,
        initiator,
        model,
        anthropic_beta: beta,
        stripped_opaque_reasoning,
    })
}

async fn proxy(State(state): State<AppState>, request: Request) -> Response {
    if !authorized(request.headers(), state.api_key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Invalid or missing API key");
    }
    let path = upstream_path(request.uri().path()).to_owned();
    let method = request.method().clone();
    let headers = request.headers().clone();
    let prepared = if method == Method::POST {
        if !is_json_content_type(&headers) {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
        // Do not impose a gateway-specific payload cap: Copilot should decide
        // whether a valid request is too large. This is buffered for JSON
        // inspection/normalization, so large requests still consume local RAM.
        let bytes = match to_bytes(request.into_body(), usize::MAX).await {
            Ok(b) => b,
            Err(_) => {
                return error(StatusCode::BAD_REQUEST, "Could not read request body");
            }
        };
        match prepare_payload(&path, bytes, &headers) {
            Ok(v) => v,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Expected a JSON object"),
        }
    } else {
        PreparedPayload::empty()
    };
    let PreparedPayload {
        body,
        vision,
        initiator,
        model,
        anthropic_beta,
        stripped_opaque_reasoning,
    } = prepared;
    let id = Uuid::new_v4().to_string();
    let started = Instant::now();
    if stripped_opaque_reasoning > 0 {
        tracing::debug!(
            request_id = %id,
            endpoint = %path,
            stripped_opaque_fields = stripped_opaque_reasoning,
            "stripped provider-specific encrypted Responses state from request"
        );
    }
    let endpoint = match path.as_str() {
        "/chat/completions" => Some("chat_completions"),
        "/responses" => Some("responses"),
        "/embeddings" => Some("embeddings"),
        "/v1/messages" => Some("messages"),
        _ => None,
    };
    let event = endpoint.map(|endpoint| {
        let session_id = crate::session_id::resolve(&headers, &id);
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .chars()
            .take(64)
            .collect();
        UsageEvent {
            model,
            endpoint: endpoint.into(),
            session_id,
            trace_id: id.clone(),
            request_id: Some(id.clone()),
            user_agent,
            ..UsageEvent::default()
        }
    });
    let credential = match state.credential().await {
        Ok(c) => c,
        Err(_) => {
            tracing::error!(
                request_id = %id,
                endpoint = %path,
                elapsed_ms = started.elapsed().as_millis(),
                "could not obtain Copilot credentials for upstream request"
            );
            record_failed_request(state.usage.clone(), event, started).await;
            return error(
                StatusCode::BAD_GATEWAY,
                "Could not obtain Copilot credentials",
            );
        }
    };
    // The base URL has been validated by the credential module; no client-supplied URL is used.
    let url = format!("{}{}", credential.api_base.trim_end_matches('/'), path);
    let interaction = if path == "/models" {
        "model-access"
    } else {
        "conversation-agent"
    };
    let mut upstream = state
        .client
        .request(method, url)
        .bearer_auth(&credential.token)
        .header("copilot-integration-id", "vscode-chat")
        .header("editor-device-id", &state.device_id)
        .header("editor-version", "vscode/1.106.0")
        .header("editor-plugin-version", "copilot-chat/0.72.0")
        .header(header::USER_AGENT, "GitHubCopilotChat/0.72.0")
        .header("x-github-api-version", "2026-08-01")
        .header("x-request-id", &id)
        .header("x-agent-task-id", &id)
        .header("x-interaction-type", interaction)
        .header("openai-intent", interaction)
        .header("x-initiator", initiator);
    if vision {
        upstream = upstream.header("copilot-vision-request", "true");
    }
    if path == "/v1/messages" {
        if let Some(value) = headers.get("anthropic-version") {
            upstream = upstream.header("anthropic-version", value);
        }
        if let Some(value) = anthropic_beta {
            upstream = upstream.header("anthropic-beta", value);
        }
    }
    if !body.is_empty() {
        upstream = upstream
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    // Limit time to receive response headers, not the response body: SSE can
    // legitimately remain open for much longer than this timeout.
    let upstream = match tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, upstream.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(request_error)) => {
            let error_class = if request_error.is_timeout() {
                "timeout"
            } else if request_error.is_connect() {
                "connect"
            } else {
                "transport"
            };
            tracing::error!(
                request_id = %id,
                endpoint = %path,
                error_class,
                error = %request_error.without_url(),
                elapsed_ms = started.elapsed().as_millis(),
                "Copilot upstream request failed before response headers"
            );
            record_failed_request(state.usage.clone(), event, started).await;
            return error(StatusCode::BAD_GATEWAY, "Copilot request failed");
        }
        Err(_) => {
            tracing::error!(
                request_id = %id,
                endpoint = %path,
                timeout_ms = RESPONSE_HEADERS_TIMEOUT.as_millis(),
                elapsed_ms = started.elapsed().as_millis(),
                "Copilot response headers timed out"
            );
            record_failed_request(state.usage.clone(), event, started).await;
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "Copilot response headers timed out",
            );
        }
    };
    let status = upstream.status();
    if !status.is_success() {
        if status.is_server_error() {
            tracing::error!(
                request_id = %id,
                endpoint = %path,
                upstream_status = status.as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                "Copilot returned a server error status"
            );
        } else {
            tracing::warn!(
                request_id = %id,
                endpoint = %path,
                upstream_status = status.as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                "Copilot returned a non-success HTTP status"
            );
        }
    }
    let sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    let mut response = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::RETRY_AFTER,
        header::CACHE_CONTROL,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value);
        }
    }
    let body = match (event, state.usage) {
        (Some(event), Some(store)) => Body::from_stream(UsageStream::new(
            upstream.bytes_stream(),
            store,
            event,
            sse,
            started,
            status.is_success(),
        )),
        _ => Body::from_stream(upstream.bytes_stream()),
    };
    match response.body(body) {
        Ok(response) => response,
        Err(_) => error(StatusCode::BAD_GATEWAY, "Invalid upstream response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_reports_service_identity_without_client_auth() {
        let app = router_with_test_credential("http://127.0.0.1:1".into(), "unused".into(), None)
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::HOST, "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-copilot-api-service"],
            "copilot-api-rust"
        );
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(value["service"], "copilot-api-rust");
        assert_eq!(value["status"], "ok");
    }

    #[tokio::test]
    async fn upstream_transport_failure_is_logged_and_recorded_as_error() {
        use axum::body::to_bytes;
        use tower::ServiceExt;

        let usage = Arc::new(UsageStore::in_memory().unwrap());
        let app = router_with_test_usage("http://127.0.0.1:1".into(), None, usage.clone()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/responses")
                    .header(header::HOST, "localhost:4141")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"model":"test-model","input":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Copilot request failed")
        );

        let events = usage.events("lifetime", 1, 10).unwrap();
        assert_eq!(events["total"], 1);
        assert_eq!(events["items"][0]["endpoint"], "responses");
        assert_eq!(events["items"][0]["model"], "test-model");
        assert_eq!(events["items"][0]["request_outcome"], "error");
        assert_eq!(events["items"][0]["total_tokens"], 0);
    }

    #[tokio::test]
    async fn quota_routes_require_auth_and_use_github_token() {
        use axum::{body::to_bytes, routing::get};
        let upstream = Router::new().route("/quota", get(|headers: HeaderMap| async move {
            assert_eq!(headers[header::AUTHORIZATION], "token secret-github-token");
            axum::Json(json!({"copilot_plan":"individual","quota_snapshots":{"chat":{"entitlement":100,"remaining":75}}}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let github = crate::auth::GitHubAuth::new(crate::auth::AuthConfig::default()).unwrap();
        let auth = Arc::new(CopilotAuth::new(github, "unused".into()).unwrap());
        let mut state = new_state(auth, Some("client-key".into())).unwrap();
        state.quota = Some(Arc::new(
            QuotaClient::with_test_url(
                "secret-github-token".into(),
                &format!("http://{address}/quota"),
            )
            .unwrap(),
        ));
        state.usage = Some(Arc::new(UsageStore::in_memory().unwrap()));
        let app = build_router(state);
        let request = |path: &str, key: Option<&str>| {
            let mut r = Request::builder()
                .uri(path)
                .header(header::HOST, "localhost");
            if let Some(key) = key {
                r = r.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            r.body(Body::empty()).unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request("/usage", None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let raw = app
            .clone()
            .oneshot(request("/usage", Some("client-key")))
            .await
            .unwrap();
        assert_eq!(raw.status(), StatusCode::OK);
        let raw: Value =
            serde_json::from_slice(&to_bytes(raw.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(raw["copilot_plan"], "individual");
        let normalized = app
            .clone()
            .oneshot(request("/usage/github-copilot", Some("client-key")))
            .await
            .unwrap();
        assert_eq!(normalized.headers()[header::CACHE_CONTROL], "no-store");
        let normalized: Value =
            serde_json::from_slice(&to_bytes(normalized.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(normalized["windows"][0]["usedPercent"], 25.0);
        assert_eq!(
            normalized["windows"][0]["runoutEstimate"],
            "collecting samples"
        );
        assert_eq!(
            app.oneshot(request("/usage/other", Some("client-key")))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        server.abort();
    }

    #[test]
    fn client_key_checks_bearer_and_anthropic_header() {
        let mut h = HeaderMap::new();
        assert!(authorized(&h, None));
        assert!(!authorized(&h, Some("secret")));
        h.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(authorized(&h, Some("secret")));
        assert!(!authorized(&h, Some("wrong")));
        h.clear();
        h.insert("x-api-key", "secret".parse().unwrap());
        assert!(authorized(&h, Some("secret")));
    }

    #[test]
    fn response_normalization_and_chat_initiator() {
        let prepared = prepare_payload(
            "/responses",
            Bytes::from_static(br#"{"max_output_tokens":4,"service_tier":"x"}"#),
            &HeaderMap::new(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(parsed["max_output_tokens"], 16);
        assert!(parsed.get("service_tier").is_none());
        let prepared = prepare_payload(
            "/chat/completions",
            Bytes::from_static(
                br#"{"messages":[{"role":"tool","content":[{"type":"image_url"}]}]}"#,
            ),
            &HeaderMap::new(),
        )
        .unwrap();
        assert!(prepared.vision);
        assert_eq!(prepared.initiator, "agent");
        assert!(
            prepare_payload("/responses", Bytes::from_static(b"[]"), &HeaderMap::new()).is_err()
        );
        assert!(
            prepare_payload(
                "/responses",
                Bytes::from_static(b"not json"),
                &HeaderMap::new()
            )
            .is_err()
        );
        assert_eq!(upstream_path("/v1/responses"), "/responses");
    }

    #[test]
    fn responses_strip_provider_specific_encrypted_reasoning_state() {
        let prepared = prepare_payload(
            "/responses",
            Bytes::from_static(
                br#"{"model":"mai-code-2","include":["reasoning.encrypted_content","usage"],"input":[{"type":"reasoning","id":"rs_opaque","summary":[],"encrypted_content":"secret-state"},{"type":"reasoning","id":"rs_visible","summary":[{"type":"summary_text","text":"kept"}],"encrypted_content":"secret-state-2"},{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
            ),
            &HeaderMap::new(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(prepared.stripped_opaque_reasoning, 4);
        assert_eq!(parsed["include"], json!(["usage"]));
        assert_eq!(parsed["input"].as_array().unwrap().len(), 2);
        assert!(parsed.to_string().contains("rs_visible"));
        assert!(!parsed.to_string().contains("secret-state"));
        assert!(!parsed.to_string().contains("rs_opaque"));
    }

    #[test]
    fn canonical_request_bytes_and_metadata_match_previous_behavior() {
        // Legacy behavior parsed into serde_json::Value then serialized it for
        // every route. Duplicate keys, escaping and numeric representations
        // must continue to follow that same canonicalization.
        for path in [
            "/chat/completions",
            "/embeddings",
            "/v1/messages",
            "/responses",
        ] {
            let raw = br#" { "model":"old", "z":1e1, "model":"m\u00f8del", "messages":[{"role":"user","content":[{"type":"tool_result","content":[{"type":"image"}]}]}], "thinking":{"type":"enabled","budget_tokens":1024}, "service_tier":"priority", "max_output_tokens":4, "z":2.50 } "#;
            let headers = HeaderMap::new();
            let prepared = prepare_payload(path, Bytes::from_static(raw), &headers).unwrap();
            let mut reference: Value = serde_json::from_slice(raw).unwrap();
            if path == "/responses" {
                reference.as_object_mut().unwrap().remove("service_tier");
                reference["max_output_tokens"] = json!(16);
            }
            assert_eq!(
                prepared.body.as_ref(),
                serde_json::to_vec(&reference).unwrap(),
                "{path}"
            );
            assert_eq!(prepared.model, "mødel");
            assert!(prepared.vision);
            assert_eq!(
                prepared.initiator,
                if path == "/v1/messages" {
                    "agent"
                } else {
                    "user"
                }
            );
            assert_eq!(
                prepared.anthropic_beta.as_deref(),
                if path == "/v1/messages" {
                    Some("interleaved-thinking-2025-05-14")
                } else {
                    None
                }
            );
        }
        let model = "ø".repeat(260);
        let body = Bytes::from(serde_json::to_vec(&json!({"model":model})).unwrap());
        let prepared = prepare_payload("/chat/completions", body, &HeaderMap::new()).unwrap();
        assert_eq!(prepared.model.chars().count(), 256);
        assert_eq!(
            prepare_payload(
                "/embeddings",
                Bytes::from_static(b"null"),
                &HeaderMap::new()
            )
            .err(),
            Some(())
        );
        assert_eq!(
            prepare_payload(
                "/embeddings",
                Bytes::from_static(b"{bad"),
                &HeaderMap::new()
            )
            .err(),
            Some(())
        );
    }

    #[tokio::test]
    async fn chunked_and_oversized_json_forwarding_are_preserved() {
        use futures_util::stream;
        use tokio::sync::mpsc;
        use tower::ServiceExt;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let upstream = Router::new().route(
            "/chat/completions",
            post(move |request: Request| {
                let tx = tx.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = to_bytes(body, usize::MAX).await.unwrap();
                    tx.send((parts.headers, body)).unwrap();
                    (StatusCode::ACCEPTED, "ok")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let app =
            router_with_test_credential(format!("http://{addr}"), "copilot-test".into(), None)
                .unwrap();
        let parts = vec![
            Bytes::from_static(b"{\"model\":\"old\",\"model\":"),
            Bytes::from_static(b"\"new\",\"n\":1e1}"),
        ];
        let body = Body::from_stream(stream::iter(
            parts.into_iter().map(Ok::<Bytes, std::io::Error>),
        ));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let (_, forwarded) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let reference: Value =
            serde_json::from_slice(br#"{"model":"old","model":"new","n":1e1}"#).unwrap();
        assert_eq!(forwarded.as_ref(), serde_json::to_vec(&reference).unwrap());
        // A request larger than the former local limit is parsed and accepted;
        // harmless trailing whitespace is removed during JSON canonicalization.
        let mut exact = b"{\"model\":\"boundary\"}".to_vec();
        exact.resize(16 * 1024 * 1024 + 1, b' ');
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(Bytes::from(exact)))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
        let (_, forwarded) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forwarded.as_ref(), br#"{"model":"boundary"}"#);

        let unreadable = Body::from_stream(stream::iter([Err::<Bytes, std::io::Error>(
            std::io::Error::other("broken request body"),
        )]));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(unreadable)
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert!(rx.try_recv().is_err());
        server.abort();
    }

    #[test]
    fn loopback_authority_rejects_rebinding_and_ambiguous_values() {
        for value in [
            "localhost:4141",
            "LOCALHOST",
            "127.0.0.1:4141",
            "[::1]:4141",
        ] {
            assert!(is_loopback_authority(value), "{value}");
        }
        for value in [
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "evil@localhost",
            "[::2]",
            "localhost:abc",
        ] {
            assert!(!is_loopback_authority(value), "{value}");
        }
    }

    #[test]
    fn json_content_type_is_required_and_unambiguous() {
        let mut headers = HeaderMap::new();
        assert!(!is_json_content_type(&headers));
        headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(!is_json_content_type(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            "Application/JSON; charset=utf-8".parse().unwrap(),
        );
        assert!(is_json_content_type(&headers));
        headers.append(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(!is_json_content_type(&headers));
    }

    #[test]
    fn only_supported_anthropic_betas_are_forwarded() {
        let mut headers = HeaderMap::new();
        let thinking = json!({"thinking":{"type":"enabled","budget_tokens":1024}});
        assert_eq!(
            anthropic_beta(&headers, &thinking).as_deref(),
            Some("interleaved-thinking-2025-05-14")
        );
        headers.insert(
            "anthropic-beta",
            "bad, extended-cache-ttl-2025-04-11, unknown"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            anthropic_beta(&headers, &thinking).as_deref(),
            Some("extended-cache-ttl-2025-04-11")
        );
        headers.insert("anthropic-beta", "bad".parse().unwrap());
        assert_eq!(anthropic_beta(&headers, &thinking), None);
        headers.clear();
        assert_eq!(
            anthropic_beta(
                &headers,
                &json!({"thinking":{"type":"adaptive","budget_tokens":1024}})
            ),
            None
        );
    }

    #[tokio::test]
    async fn router_rejects_origins_rebinding_and_non_json_before_proxying() {
        let app =
            router_with_test_credential("http://127.0.0.1:1".into(), "test".into(), None).unwrap();
        let request =
            |method: &str, host: &str, origin: Option<&str>, content_type: Option<&str>| {
                let mut builder = Request::builder()
                    .method(method)
                    .uri("/v1/responses")
                    .header(header::HOST, host);
                if let Some(value) = origin {
                    builder = builder.header(header::ORIGIN, value);
                }
                if let Some(value) = content_type {
                    builder = builder.header(header::CONTENT_TYPE, value);
                }
                builder.body(Body::from("{}")).unwrap()
            };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "localhost:4141",
                Some("http://localhost:4141"),
                Some("application/json"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "attacker.example",
                None,
                Some("application/json"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(request("POST", "127.0.0.1:4141", None, Some("text/plain")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "localhost:4141")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
