//! Observe and persist usage while forwarding the original upstream bytes.
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use bytes::Bytes;
use futures_util::Stream;
use tokio::task::JoinHandle;

use crate::{
    usage::{UsageEvent, UsageStore},
    usage_observer::{StreamTerminal, UsageObserver},
};

pub(crate) struct UsageStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    observer: Option<UsageObserver>,
    event: Option<UsageEvent>,
    store: Arc<UsageStore>,
    start: Instant,
    http_success: bool,
    pending: Option<JoinHandle<()>>,
    done: bool,
}

impl UsageStream {
    pub(crate) fn new(
        inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        store: Arc<UsageStore>,
        event: UsageEvent,
        sse: bool,
        start: Instant,
        http_success: bool,
    ) -> Self {
        let observer = UsageObserver::new(&event.endpoint, sse, start);
        Self {
            inner: Box::pin(inner),
            observer: Some(observer),
            event: Some(event),
            store,
            start,
            http_success,
            pending: None,
            done: false,
        }
    }

    fn save(&mut self, disposition: &str) {
        let Some(observer) = self.observer.take() else {
            return;
        };
        let terminal = observer.terminal();
        let expects_terminal = observer.expects_terminal();
        if observer.monthly_quota_exceeded() {
            tracing::error!(
                "Copilot reported monthly quota exhausted; automatic payer switching is unavailable. Use Quota > b to open GitHub billing settings."
            );
        }
        let mut observed = observer.finish();
        if observed.is_none() && disposition != "error" && self.http_success {
            return;
        }
        let Some(mut event) = self.event.take() else {
            return;
        };
        let protocol_error = observed
            .as_ref()
            .is_some_and(|observed| observed.protocol_error);
        if let Some(observed) = observed.take() {
            event.input_tokens = observed.input_tokens.unwrap_or(0);
            event.output_tokens = observed.output_tokens.unwrap_or(0);
            event.cache_read_input_tokens = observed.cache_read_input_tokens.unwrap_or(0);
            event.cache_creation_input_tokens = observed.cache_creation_input_tokens.unwrap_or(0);
            event.total_tokens = observed.total_tokens.unwrap_or_else(|| {
                event
                    .input_tokens
                    .saturating_add(event.output_tokens)
                    .saturating_add(event.cache_read_input_tokens)
                    .saturating_add(event.cache_creation_input_tokens)
            });
            event.total_nano_aiu = observed.total_nano_aiu;
            if !observed.token_details.is_empty() {
                event.token_details = Some(observed.token_details);
            }
            event.first_token_ms = observed.first_token_ms;
        }
        event.duration_ms =
            Some(i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX));
        event.request_outcome = Some(
            if disposition == "error" || protocol_error || !self.http_success {
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
        let store = self.store.clone();
        // Waiting for this task at normal EOF gives callers read-after-completion
        // consistency without doing SQLite I/O on the async runtime thread.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            self.pending = Some(handle.spawn_blocking(move || {
                if let Err(error) = store.record(event) {
                    tracing::error!(%error, "failed to persist token usage");
                }
            }));
        } else if let Err(error) = store.record(event) {
            tracing::error!(%error, "failed to persist token usage");
        }
    }
}

impl Stream for UsageStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use std::future::Future;
        if let Some(task) = self.pending.as_mut() {
            return match Pin::new(task).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    if let Err(error) = result {
                        tracing::error!(%error, "usage persistence task failed");
                    }
                    self.pending = None;
                    self.done = true;
                    Poll::Ready(None)
                }
            };
        }
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(observer) = &mut self.observer {
                    observer.observe(&bytes);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                let request_id = self
                    .event
                    .as_ref()
                    .and_then(|event| event.request_id.as_deref())
                    .unwrap_or("unknown");
                let endpoint = self
                    .event
                    .as_ref()
                    .map(|event| event.endpoint.as_str())
                    .unwrap_or("unknown");
                let error_class = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else {
                    "stream"
                };
                tracing::error!(
                    request_id,
                    endpoint,
                    error_class,
                    "Copilot response stream failed"
                );
                self.save("error");
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.save("completed");
                if self.pending.is_some() {
                    self.poll_next(cx)
                } else {
                    self.done = true;
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for UsageStream {
    fn drop(&mut self) {
        if !self.done && self.observer.is_some() {
            self.save("cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{StreamExt, stream};
    use std::time::Duration;

    async fn dropped_outcome(
        endpoint: &str,
        fragments: Vec<&'static [u8]>,
        http_success: bool,
    ) -> String {
        let store = Arc::new(UsageStore::in_memory().unwrap());
        let count = fragments.len();
        let source = stream::iter(
            fragments
                .into_iter()
                .map(|bytes| Ok::<Bytes, reqwest::Error>(Bytes::from_static(bytes))),
        )
        .chain(stream::pending());
        let event = UsageEvent {
            endpoint: endpoint.into(),
            model: "mock".into(),
            ..UsageEvent::default()
        };
        let mut observed = UsageStream::new(
            source,
            store.clone(),
            event,
            true,
            Instant::now(),
            http_success,
        );
        for _ in 0..count {
            observed.next().await.unwrap().unwrap();
        }
        // Simulates a real SSE client closing immediately after a complete frame,
        // while the upstream body is still open and has not produced EOF.
        drop(observed);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let events = store.events("lifetime", 1, 10).unwrap();
                if events["total"] == 1 {
                    return events["items"][0]["request_outcome"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn clients_stopping_after_protocol_terminal_are_not_cancelled() {
        let chat = vec![
            b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n".as_slice(),
            b"data: [DO".as_slice(),
            b"NE]\n\n".as_slice(),
        ];
        assert_eq!(
            dropped_outcome("chat_completions", chat, true).await,
            "completed"
        );
        let responses = vec![b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("responses", responses, true).await,
            "completed"
        );
        let incomplete = vec![b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("responses", incomplete, true).await,
            "incomplete"
        );
        let incomplete = vec![b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"incomplete\",\"usage\":{\"input_tokens\":10}}}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("responses", incomplete, true).await,
            "incomplete"
        );
        let messages = vec![b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n".as_slice(), b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("messages", messages, true).await,
            "completed"
        );
    }

    #[tokio::test]
    async fn early_drop_and_incomplete_terminal_remain_cancelled() {
        let fragments = vec![
            b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n".as_slice(),
            b"data: [DONE]\n".as_slice(),
        ];
        assert_eq!(
            dropped_outcome("chat_completions", fragments, true).await,
            "cancelled"
        );
        // A bare [DONE] before any valid chat chunk cannot be trusted as success.
        let fragments = vec![
            b"data: [DONE]\n\n".as_slice(),
            b"data: {\"usage\":{\"prompt_tokens\":10}}\n\n".as_slice(),
        ];
        assert_eq!(
            dropped_outcome("chat_completions", fragments, true).await,
            "cancelled"
        );
        // A terminal name without the required response object is not completion.
        let fragments = vec![b"data: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":10}}}\n\n".as_slice(), b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("responses", fragments, true).await,
            "cancelled"
        );
        let fragments = vec![b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n".as_slice(), b"event: message_stop\ndata: {}\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("messages", fragments, true).await,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn errors_take_precedence_over_terminal_and_drop() {
        let fragments = vec![b"data: {\"usage\":{\"prompt_tokens\":10}}\n\nevent: error\ndata: {\"type\":\"error\"}\n\ndata: [DONE]\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("chat_completions", fragments, true).await,
            "error"
        );
        let fragments =
            vec![b"data: {\"usage\":{\"prompt_tokens\":10}}\n\ndata: [DONE]\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("chat_completions", fragments, false).await,
            "error"
        );
        let fragments = vec![b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"usage\":{\"input_tokens\":10}}}\n\n".as_slice()];
        assert_eq!(dropped_outcome("responses", fragments, true).await, "error");
        let fragments = vec![b"data: {\"usage\":{\"prompt_tokens\":10}}\n\ndata: {\"error\":{\"message\":\"failed\"}}\n\ndata: [DONE]\n\n".as_slice()];
        assert_eq!(
            dropped_outcome("chat_completions", fragments, true).await,
            "error"
        );
        let fragments = vec![b"event: response.failed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10}}}\n\n".as_slice()];
        assert_eq!(dropped_outcome("responses", fragments, true).await, "error");
        let fragments = vec![b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"failed\",\"usage\":{\"input_tokens\":10}}}\n\n".as_slice()];
        assert_eq!(dropped_outcome("responses", fragments, true).await, "error");
    }

    #[tokio::test]
    async fn eof_parses_json_errors_and_preserves_incomplete_state() {
        for (endpoint, data, sse, outcome) in [
            ("chat_completions", b"{\"usage\":{\"prompt_tokens\":4},\"error\":{\"message\":\"fail\"}}".as_slice(), false, "error"),
            ("chat_completions", b"data: {\"usage\":{\"prompt_tokens\":4}}\n\n".as_slice(), true, "incomplete"),
            ("responses", b"event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":4}}}\n\n".as_slice(), true, "incomplete"),
            ("responses", b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":4}}}\n\n".as_slice(), true, "incomplete"),
        ] {
            let store = Arc::new(UsageStore::in_memory().unwrap());
            let source = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from_static(data))]);
            let event = UsageEvent { endpoint: endpoint.into(), model: "mock".into(), ..UsageEvent::default() };
            let mut observed = UsageStream::new(source, store.clone(), event, sse, Instant::now(), true);
            observed.next().await.unwrap().unwrap();
            assert!(observed.next().await.is_none());
            let events = store.events("lifetime", 1, 10).unwrap();
            assert_eq!(events["items"][0]["request_outcome"], outcome);
        }
    }

    #[tokio::test]
    async fn non_success_without_usage_is_recorded_as_error() {
        let store = Arc::new(UsageStore::in_memory().unwrap());
        let source = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from_static(
            b"upstream unavailable",
        ))]);
        let event = UsageEvent {
            endpoint: "responses".into(),
            model: "mock".into(),
            request_id: Some("failed-response".into()),
            ..UsageEvent::default()
        };
        let mut observed =
            UsageStream::new(source, store.clone(), event, false, Instant::now(), false);
        while observed.next().await.is_some() {}

        let events = store.events("lifetime", 1, 10).unwrap();
        assert_eq!(events["total"], 1);
        assert_eq!(events["items"][0]["request_outcome"], "error");
        assert_eq!(events["items"][0]["total_tokens"], 0);
        assert_eq!(events["items"][0]["request_id"], "failed-response");
    }

    #[tokio::test]
    async fn protocol_error_without_usage_is_recorded_as_error() {
        let store = Arc::new(UsageStore::in_memory().unwrap());
        let source = stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from_static(
            br#"{"error":{"message":"failed"}}"#,
        ))]);
        let event = UsageEvent {
            endpoint: "responses".into(),
            model: "mock".into(),
            ..UsageEvent::default()
        };
        let mut observed =
            UsageStream::new(source, store.clone(), event, false, Instant::now(), true);
        while observed.next().await.is_some() {}

        let events = store.events("lifetime", 1, 10).unwrap();
        assert_eq!(events["total"], 1);
        assert_eq!(events["items"][0]["request_outcome"], "error");
        assert_eq!(events["items"][0]["total_tokens"], 0);
    }
}
