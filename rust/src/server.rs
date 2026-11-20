use std::sync::Arc;

use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::auth::CopilotAuth;

// Request JSON is buffered for header derivation and minor normalization; responses are streamed.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    auth: Arc<CopilotAuth>,
    client: reqwest::Client,
    api_key: Option<String>,
    device_id: String,
}

pub fn router(auth: Arc<CopilotAuth>, api_key: Option<String>) -> Result<Router> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let state = AppState {
        auth,
        client,
        api_key,
        device_id: Uuid::new_v4().to_string(),
    };
    Ok(Router::new()
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
        .with_state(state))
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
        if let Some(n) = object.get("max_output_tokens").and_then(Value::as_u64) {
            if n > 0 && n < 16 {
                object.insert("max_output_tokens".into(), json!(16));
            }
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
    let credential = match state.auth.credential().await {
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
    if !body.is_empty() {
        upstream = upstream
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    if path == "/v1/messages" {
        for name in ["anthropic-version", "anthropic-beta"] {
            if let Some(value) = headers.get(name) {
                upstream = upstream.header(name, value);
            }
        }
    }
    let upstream = match upstream.send().await {
        Ok(r) => r,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Copilot request failed"),
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
}
