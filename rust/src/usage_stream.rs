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
    usage_observer::UsageObserver,
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
        let protocol_error = observer.protocol_error();
        let Some(observed) = observer.finish() else {
            return;
        };
        let Some(mut event) = self.event.take() else {
            return;
        };
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
        event.first_token_ms = observed.first_token_ms;
        event.duration_ms =
            Some(i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX));
        event.request_outcome = Some(
            if disposition == "cancelled" {
                "cancelled"
            } else if disposition == "error" || protocol_error || !self.http_success {
                "error"
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
