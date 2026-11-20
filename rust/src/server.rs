use std::{net::IpAddr, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::auth::CopilotAuth;

// Request JSON is buffered for header derivation and minor normalization; responses are streamed.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct AppState {
    auth: Arc<CopilotAuth>,
    client: reqwest::Client,
    api_key: Option<String>,
    device_id: String,
    #[cfg(test)]
    test_credential: Option<crate::auth::CopilotCredential>,
}

pub fn router(auth: Arc<CopilotAuth>, api_key: Option<String>) -> Result<Router> {
    Ok(build_router(new_state(auth, api_key)?))
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

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "copilot-api-rust (partial port)" }))
        .route("/models", get(proxy))
        .route("/v1/models", get(proxy))
        .route("/chat/completions", post(proxy))
        .route("/v1/chat/completions", post(proxy))
        .route("/embeddings", post(proxy))
        .route("/v1/embeddings", post(proxy))
        .route("/responses", post(proxy))
        .route("/v1/responses", post(proxy))
        .route("/v1/messages", post(proxy))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_browser_access,
        ))
        .with_state(state)
}

impl AppState {
    async fn credential(&self) -> Result<crate::auth::CopilotCredential> {
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

fn anthropic_beta(headers: &HeaderMap, body: &[u8]) -> Option<String> {
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
    let payload: Value = serde_json::from_slice(body).ok()?;
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

fn authorized(headers: &HeaderMap, key: Option<&str>) -> bool {
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

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(json!({"error": {"message": message}}))).into_response()
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

fn prepare_payload(path: &str, body: &[u8]) -> Result<(Vec<u8>, bool, &'static str), ()> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| ())?;
    if !value.is_object() {
        return Err(());
    }
    let vision = contains_image(&value);
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
    if path == "/responses" {
        let object = value.as_object_mut().ok_or(())?;
        object.remove("service_tier");
        if let Some(n) = object.get("max_output_tokens").and_then(Value::as_u64)
            && n > 0
            && n < 16
        {
            object.insert("max_output_tokens".into(), json!(16));
        }
    }
    Ok((
        serde_json::to_vec(&value).map_err(|_| ())?,
        vision,
        initiator,
    ))
}

async fn proxy(State(state): State<AppState>, request: Request) -> Response {
    if !authorized(request.headers(), state.api_key.as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Invalid or missing API key");
    }
    let path = upstream_path(request.uri().path()).to_owned();
    let method = request.method().clone();
    let headers = request.headers().clone();
    let (body, vision, initiator) = if method == Method::POST {
        if !is_json_content_type(&headers) {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
        let bytes = match to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
            Ok(b) => b,
            Err(_) => {
                return error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request body exceeds limit or could not be read",
                );
            }
        };
        match prepare_payload(&path, &bytes) {
            Ok(v) => v,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Expected a JSON object"),
        }
    } else {
        (Vec::new(), false, "user")
    };
    let credential = match state.credential().await {
        Ok(c) => c,
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Could not obtain Copilot credentials",
            );
        }
    };
    // The base URL has been validated by the credential module; no client-supplied URL is used.
    let url = format!("{}{}", credential.api_base.trim_end_matches('/'), path);
    let id = Uuid::new_v4().to_string();
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
        if let Some(value) = anthropic_beta(&headers, &body) {
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
        Ok(Err(_)) => return error(StatusCode::BAD_GATEWAY, "Copilot request failed"),
        Err(_) => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "Copilot response headers timed out",
            );
        }
    };
    let status = upstream.status();
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
    match response.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(response) => response,
        Err(_) => error(StatusCode::BAD_GATEWAY, "Invalid upstream response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

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
        let (body, _, _) = prepare_payload(
            "/responses",
            br#"{"max_output_tokens":4,"service_tier":"x"}"#,
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["max_output_tokens"], 16);
        assert!(parsed.get("service_tier").is_none());
        let (_, image, initiator) = prepare_payload(
            "/chat/completions",
            br#"{"messages":[{"role":"tool","content":[{"type":"image_url"}]}]}"#,
        )
        .unwrap();
        assert!(image);
        assert_eq!(initiator, "agent");
        assert!(prepare_payload("/responses", b"[]").is_err());
        assert!(prepare_payload("/responses", b"not json").is_err());
        assert_eq!(upstream_path("/v1/responses"), "/responses");
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
        let thinking = br#"{"thinking":{"type":"enabled","budget_tokens":1024}}"#;
        assert_eq!(
            anthropic_beta(&headers, thinking).as_deref(),
            Some("interleaved-thinking-2025-05-14")
        );
        headers.insert(
            "anthropic-beta",
            "bad, extended-cache-ttl-2025-04-11, unknown"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            anthropic_beta(&headers, thinking).as_deref(),
            Some("extended-cache-ttl-2025-04-11")
        );
        headers.insert("anthropic-beta", "bad".parse().unwrap());
        assert_eq!(anthropic_beta(&headers, thinking), None);
        headers.clear();
        assert_eq!(
            anthropic_beta(
                &headers,
                br#"{"thinking":{"type":"adaptive","budget_tokens":1024}}"#
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
