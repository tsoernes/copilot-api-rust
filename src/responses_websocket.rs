//! Optional Responses WebSocket transport from local Codex clients through to
//! GitHub Copilot. WebSocket payloads are normalized with the same sanitizer as
//! HTTP Responses requests and relayed as individual JSON events.
use std::time::Instant;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{
    Message as UpstreamMessage, client::IntoClientRequest, protocol::WebSocketConfig,
};

use crate::{
    server::{AppState, PreparedPayload, build_upstream_headers, prepare_payload},
    session_id,
    usage::{UsageEvent, UsageStore},
    usage_observer::{StreamTerminal, UsageObserver},
};

const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

type UpstreamSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct UpstreamConnection {
    socket: UpstreamSocket,
    model: String,
}

pub(crate) async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    socket: WebSocketUpgrade,
) -> Response {
    if !state.responses_websockets {
        return crate::server::error(
            axum::http::StatusCode::NOT_FOUND,
            "Responses WebSocket transport is disabled",
        );
    }
    if !crate::server::authorized(&headers, state.api_key.as_deref()) {
        return crate::server::error(
            axum::http::StatusCode::UNAUTHORIZED,
            "Invalid or missing API key",
        );
    }
    let local_headers = headers.clone();
    socket
        .max_message_size(usize::MAX)
        .max_frame_size(usize::MAX)
        .on_upgrade(move |socket| serve_connection(state, local_headers, socket))
}

async fn serve_connection(state: AppState, headers: HeaderMap, mut downstream: WebSocket) {
    let mut upstream: Option<UpstreamConnection> = None;
    loop {
        let Some(message) = downstream.recv().await else {
            return;
        };
        let Ok(message) = message else {
            tracing::debug!("local Responses WebSocket failed while receiving");
            return;
        };
        match message {
            Message::Text(text) => {
                if let Err(error) = handle_request(
                    &state,
                    &headers,
                    text.as_bytes(),
                    &mut upstream,
                    &mut downstream,
                )
                .await
                {
                    tracing::warn!(error = %error, "Responses WebSocket request failed");
                    let frame = serde_json::json!({
                        "type": "error",
                        "error": {"type": "server_error", "message": safe_ws_error(&error)}
                    });
                    let _ = downstream
                        .send(Message::Text(frame.to_string().into()))
                        .await;
                }
            }
            Message::Binary(_bytes) => {
                let frame = serde_json::json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error", "message": "Binary Responses WebSocket frames are unsupported"}
                });
                let _ = downstream
                    .send(Message::Text(frame.to_string().into()))
                    .await;
            }
            Message::Ping(payload) => {
                if downstream.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => return,
        }
    }
}

async fn handle_request(
    state: &AppState,
    headers: &HeaderMap,
    text: &[u8],
    upstream: &mut Option<UpstreamConnection>,
    downstream: &mut WebSocket,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut value: Value =
        serde_json::from_slice(text).map_err(|_| anyhow::anyhow!("Invalid JSON message"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Expected a JSON object"))?;
    if object.get("type").and_then(Value::as_str) != Some("response.create") {
        anyhow::bail!("Expected a response.create message");
    }
    if let Some(response) = object.remove("response") {
        let response = response
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("response must be an object"))?
            .clone();
        object.extend(response);
    }
    object.remove("type");
    object.insert("stream".into(), Value::Bool(true));
    let body = serde_json::to_vec(&value)?;
    let prepared = prepare_payload("/responses", body.into(), headers)
        .map_err(|_| anyhow::anyhow!("Expected a Responses JSON object"))?;
    let PreparedPayload {
        body,
        model,
        initiator,
        vision,
        ..
    } = prepared;
    let id = uuid::Uuid::new_v4().to_string();
    let event = state.usage.as_ref().map(|_| UsageEvent {
        model: model.clone(),
        endpoint: "responses".into(),
        session_id: session_id::resolve(headers, &id),
        trace_id: id.clone(),
        request_id: Some(id.clone()),
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .chars()
            .take(64)
            .collect(),
        ..UsageEvent::default()
    });
    let mut observer = UsageObserver::new("responses", true, started);

    let credential = state.credential().await?;
    let mut wire = serde_json::from_slice::<Value>(&body)?;
    wire.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Expected a Responses JSON object"))?
        .insert("type".into(), Value::String("response.create".into()));
    let request_text = serde_json::to_string(&wire)?;

    if upstream
        .as_ref()
        .is_some_and(|connection| connection.model != model)
        && let Some(mut previous) = upstream.take()
    {
        let _ = previous.socket.close(None).await;
    }
    if upstream.is_none() {
        *upstream = Some(UpstreamConnection {
            socket: connect_upstream(
                state,
                &credential.api_base,
                &credential.token,
                &id,
                headers,
                vision,
                initiator,
            )
            .await?,
            model,
        });
    }
    let socket = &mut upstream.as_mut().expect("connected").socket;
    socket
        .send(UpstreamMessage::Text(request_text.into()))
        .await?;

    let mut terminal = false;
    let mut protocol_error = false;
    loop {
        let next = tokio::time::timeout(IDLE_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("Upstream WebSocket idle timeout"))?;
        let Some(next) = next else {
            *upstream = None;
            anyhow::bail!("Upstream WebSocket closed before completion");
        };
        match next? {
            UpstreamMessage::Text(text) => {
                let bounded = text.as_bytes();
                if let Ok(parsed) = serde_json::from_slice::<Value>(bounded) {
                    let kind = parsed.get("type").and_then(Value::as_str);
                    protocol_error |= matches!(kind, Some("error" | "response.failed"));
                    observer.observe(b"data: ");
                    observer.observe(bounded);
                    observer.observe(b"\n\n");
                    terminal = matches!(
                        kind,
                        Some(
                            "response.completed"
                                | "response.failed"
                                | "response.incomplete"
                                | "error"
                        )
                    );
                }
                if downstream
                    .send(Message::Text(text.to_string().into()))
                    .await
                    .is_err()
                {
                    *upstream = None;
                    persist_usage(state.usage.as_ref(), event, observer, started, "cancelled")
                        .await;
                    return Ok(());
                }
                if terminal {
                    break;
                }
            }
            UpstreamMessage::Binary(bytes) => {
                let frame = bytes.to_vec();
                if downstream
                    .send(Message::Binary(frame.into()))
                    .await
                    .is_err()
                {
                    *upstream = None;
                    persist_usage(state.usage.as_ref(), event, observer, started, "cancelled")
                        .await;
                    return Ok(());
                }
            }
            UpstreamMessage::Ping(payload) => socket.send(UpstreamMessage::Pong(payload)).await?,
            UpstreamMessage::Pong(_) | UpstreamMessage::Frame(_) => {}
            UpstreamMessage::Close(_) => {
                *upstream = None;
                anyhow::bail!("Upstream WebSocket closed before completion");
            }
        }
    }
    persist_usage(
        state.usage.as_ref(),
        event,
        observer,
        started,
        if protocol_error { "error" } else { "completed" },
    )
    .await;
    Ok(())
}

async fn connect_upstream(
    state: &AppState,
    api_base: &str,
    token: &str,
    request_id: &str,
    client_headers: &HeaderMap,
    vision: bool,
    initiator: &str,
) -> anyhow::Result<UpstreamSocket> {
    let ws_base = if api_base.starts_with("https://") {
        api_base.replacen("https://", "wss://", 1)
    } else {
        api_base.replacen("http://", "ws://", 1)
    };
    let url = format!("{}/responses", ws_base.trim_end_matches('/'));
    let mut request = url.into_client_request()?;
    request.headers_mut().extend(build_upstream_headers(
        state,
        token,
        request_id,
        client_headers,
        vision,
        true,
        initiator,
    ));
    let mut config = WebSocketConfig::default();
    config.max_message_size = None;
    config.max_frame_size = None;
    let (socket, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Upstream WebSocket connect timeout"))??;
    Ok(socket)
}

async fn persist_usage(
    store: Option<&std::sync::Arc<UsageStore>>,
    event: Option<UsageEvent>,
    observer: UsageObserver,
    started: Instant,
    disposition: &str,
) {
    let (Some(store), Some(mut event)) = (store.cloned(), event) else {
        return;
    };
    let terminal = observer.terminal();
    let expects_terminal = observer.expects_terminal();
    let observed = observer.finish();
    let protocol_error = observed.as_ref().is_some_and(|value| value.protocol_error);
    if let Some(observed) = observed {
        event.input_tokens = observed.input_tokens.unwrap_or(0);
        event.output_tokens = observed.output_tokens.unwrap_or(0);
        event.cache_read_input_tokens = observed.cache_read_input_tokens.unwrap_or(0);
        event.cache_creation_input_tokens = observed.cache_creation_input_tokens.unwrap_or(0);
        event.total_tokens = observed.total_tokens.unwrap_or(0);
        event.total_nano_aiu = observed.total_nano_aiu;
        event.token_details =
            (!observed.token_details.is_empty()).then_some(observed.token_details);
        event.error_code = observed.provider_error_code;
        event.error_message = observed.provider_error_message;
    }
    event.duration_ms = Some(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
    event.request_outcome = Some(
        if disposition == "error" || protocol_error {
            "error"
        } else if terminal == Some(StreamTerminal::Incomplete) {
            "incomplete"
        } else if disposition == "cancelled" && terminal != Some(StreamTerminal::Completed) {
            "cancelled"
        } else if disposition == "completed" && expects_terminal && terminal.is_none() {
            "incomplete"
        } else {
            "completed"
        }
        .into(),
    );
    if let Err(error) = tokio::task::spawn_blocking(move || store.record(event)).await {
        tracing::error!(%error, "WebSocket usage persistence task failed");
    }
}

fn safe_ws_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("Invalid")
        || message.contains("Expected")
        || message.contains("closed")
        || message.contains("timeout")
    {
        message
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect()
    } else {
        "Copilot WebSocket request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server, usage::UsageStore};
    use axum::Router;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    #[tokio::test]
    async fn relays_responses_websocket_end_to_end_and_records_usage() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (stream, _) = upstream_listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = match request {
                ClientMessage::Text(text) => serde_json::from_str(&text).unwrap(),
                other => panic!("unexpected upstream frame: {other:?}"),
            };
            assert_eq!(request["type"], "response.create");
            assert_eq!(request["stream"], true);
            assert_eq!(request["model"], "mai-code-2");
            assert_eq!(request["input"].as_array().unwrap().len(), 1);
            socket
                .send(ClientMessage::Text(
                    json!({"type":"response.created","response":{"id":"resp-ws"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket
                .send(ClientMessage::Text(
                    json!({"type":"response.completed","response":{"id":"resp-ws","status":"completed","usage":{"input_tokens":8,"output_tokens":3,"total_tokens":11}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });

        let store = Arc::new(UsageStore::in_memory().unwrap());
        let app = server::router_with_test_websockets(
            format!("http://{upstream_addr}"),
            None,
            store.clone(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("ws://{address}/responses");
        let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        client
            .send(ClientMessage::Text(
                json!({
                    "type":"response.create",
                    "model":"mai-code-2",
                    "input":[
                        {"type":"function_call","call_id":"call-old","name":"exec","arguments":"opaque","encrypted_function_args":["secret"]},
                        {"type":"message","role":"user","content":"hello"}
                    ]
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let created = client.next().await.unwrap().unwrap();
        let completed = client.next().await.unwrap().unwrap();
        assert!(matches!(created, ClientMessage::Text(_)));
        let completed: Value = match completed {
            ClientMessage::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("unexpected downstream frame: {other:?}"),
        };
        assert_eq!(completed["type"], "response.completed");

        upstream_task.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let events = store.events("7d", 1, 10).unwrap();
        assert_eq!(events["items"][0]["endpoint"], "responses");
        assert_eq!(events["items"][0]["request_outcome"], "completed");
        assert_eq!(events["items"][0]["total_tokens"], 11);
        client.close(None).await.unwrap();
        server_task.abort();
    }

    #[tokio::test]
    async fn disabled_responses_websocket_returns_not_found() {
        let app: Router =
            server::router_with_test_credential("http://127.0.0.1:1".into(), "unused".into(), None)
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = tokio_tungstenite::connect_async(format!("ws://{address}/responses"))
            .await
            .expect_err("disabled WebSocket must reject upgrade");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND)
            }
            other => panic!("unexpected error: {other}"),
        }
        server_task.abort();
    }
}
