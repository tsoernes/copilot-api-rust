//! Bounded, read-only client for the local monitoring endpoints.
//!
//! The key is held only as a sensitive header; it is never put into a URL,
//! debug output, errors, or a redirect. GitHub quota refreshes are intentionally
//! separate from local history refreshes because they make a remote request.
use std::{net::IpAddr, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde_json::Value;

const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_SESSION_ID_BYTES: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub struct MonitorClient {
    client: Client,
    base: Url,
    authorization: Option<HeaderValue>,
}

/// Each endpoint can fail independently without hiding data from the others.
#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub summary: Result<Value, String>,
    pub daily: Result<Value, String>,
    pub events: Result<Value, String>,
    pub sessions: Result<Value, String>,
    pub rates: Result<Value, String>,
    pub session_rates: Option<Result<Value, String>>,
}

impl MonitorClient {
    /// Accept only an explicitly chosen loopback origin. No URL credentials,
    /// paths, query strings, redirects or ambient proxy are permitted.
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<Self> {
        let base =
            Url::parse(base_url).map_err(|_| anyhow::anyhow!("invalid monitor server URL"))?;
        let host = base.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let is_loopback = host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !matches!(base.scheme(), "http" | "https")
            || !is_loopback
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            bail!("monitor server must be a loopback HTTP(S) origin without credentials or path");
        }
        let authorization = match api_key {
            Some(key) => {
                if key.is_empty() || key.len() > 4096 {
                    bail!("monitor API key is empty or too long");
                }
                let mut value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                    anyhow::anyhow!("monitor API key contains invalid header characters")
                })?;
                value.set_sensitive(true);
                Some(value)
            }
            None => None,
        };
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(TIMEOUT)
            .user_agent("copilot-api-rust-monitor/0.1.0")
            .build()
            .context("building local monitoring client")?;
        Ok(Self {
            client,
            base,
            authorization,
        })
    }

    /// Refresh local aggregates, independent event/session pages, and rolling rates.
    pub async fn fetch_usage(
        &self,
        period: &str,
        event_page: usize,
        session_page: usize,
        page_size: usize,
        selected_session: Option<&str>,
    ) -> UsageSnapshot {
        if !matches!(
            period,
            "today" | "this_week" | "last_7_days" | "this_month" | "last_30_days" | "lifetime"
        ) {
            let error = Err("Invalid usage period".to_owned());
            return UsageSnapshot {
                summary: error.clone(),
                daily: error.clone(),
                events: error.clone(),
                sessions: error.clone(),
                rates: error.clone(),
                session_rates: selected_session.map(|_| error),
            };
        }
        let event_page = event_page.clamp(1, i32::MAX as usize).to_string();
        let session_page = session_page.clamp(1, i32::MAX as usize).to_string();
        let size = page_size.clamp(1, 100).to_string();
        let period_params = [("period", period)];
        let event_params = [
            ("period", period),
            ("page", event_page.as_str()),
            ("page_size", size.as_str()),
        ];
        let session_params = [
            ("period", period),
            ("page", session_page.as_str()),
            ("page_size", size.as_str()),
        ];
        let session_rate = async {
            match selected_session {
                None => None,
                Some(id) if id.len() > MAX_SESSION_ID_BYTES || id.chars().any(char::is_control) => {
                    Some(Err("Invalid session identifier".into()))
                }
                Some(id) => Some(self.fetch("token-usage/rates", &[("session_id", id)]).await),
            }
        };
        let (summary, daily, events, sessions, rates, session_rates) = tokio::join!(
            self.fetch("token-usage", &period_params),
            self.fetch("token-usage/daily", &period_params),
            self.fetch("token-usage/events", &event_params),
            self.fetch("token-usage/sessions", &session_params),
            self.fetch("token-usage/rates", &[]),
            session_rate,
        );
        UsageSnapshot {
            summary,
            daily,
            events,
            sessions,
            rates,
            session_rates,
        }
    }

    /// This route fetches live quota from GitHub; the caller controls frequency.
    pub async fn fetch_quota(&self) -> Result<Value, String> {
        self.fetch("usage/github-copilot", &[]).await
    }

    /// Fetch account-specific Copilot model metadata through the local gateway.
    pub async fn fetch_models(&self) -> Result<Value, String> {
        self.fetch("models", &[]).await
    }

    async fn fetch(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        // `path` is an internal static string, not an external URL.
        let mut url = self.base.clone();
        url.set_path(path);
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params.iter().copied());
        }
        let mut request = self.client.get(url);
        if let Some(value) = &self.authorization {
            request = request.header(AUTHORIZATION, value.clone());
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                "Server request timed out"
            } else {
                "Could not connect to monitoring server"
            }
            .to_owned()
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => "Authentication failed (HTTP 401)".to_owned(),
                403 => "Access denied (HTTP 403)".to_owned(),
                code => format!("Server returned HTTP {code}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE as u64)
        {
            return Err("Monitoring response exceeded 2 MiB".into());
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "Could not read monitoring response".to_owned())?;
            if chunk.len() > MAX_RESPONSE.saturating_sub(bytes.len()) {
                return Err("Monitoring response exceeded 2 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Invalid monitoring response JSON".to_owned())?;
        if !value.is_object() {
            return Err("Invalid monitoring response format".into());
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::{Request, State},
        http::{HeaderMap, StatusCode, header},
        response::Response,
        routing::get,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn validates_loopback_origin_and_never_echoes_secrets() {
        for origin in [
            "http://127.0.0.1:4141",
            "http://localhost:4141/",
            "https://[::1]:4141",
        ] {
            assert!(MonitorClient::new(origin, Some("key".into())).is_ok());
        }
        for origin in [
            "http://example.org",
            "https://example.org",
            "http://127.0.0.1/path",
            "http://secret@localhost/",
            "http://localhost/?q=secret",
            "http://localhost/#secret",
            "file:///tmp/test",
            "not a url",
        ] {
            assert!(
                MonitorClient::new(origin, None).is_err(),
                "accepted {origin}"
            );
        }
        let error = MonitorClient::new("http://localhost", Some("top-secret\n".into()))
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("top-secret"));
    }

    async fn spawn(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), task)
    }

    #[tokio::test]
    async fn fetches_independently_with_safe_key_and_bounded_pagination() {
        async fn handler(request: Request) -> Response {
            assert_eq!(
                request.headers()[header::AUTHORIZATION],
                "Bearer top-secret"
            );
            let path = request.uri().path();
            if path.ends_with("/daily") {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("private error top-secret"))
                    .unwrap();
            }
            if path.ends_with("/events") {
                assert!(request.uri().query().unwrap().contains("page_size=100"));
                assert!(request.uri().query().unwrap().contains("page=1"));
            }
            if path.ends_with("/sessions") {
                assert!(request.uri().query().unwrap().contains("page_size=100"));
                assert!(request.uri().query().unwrap().contains("page=7"));
            }
            if path.ends_with("/rates") {
                let url = Url::parse(&format!("http://localhost{}", request.uri())).unwrap();
                let query: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                let session = query.get("session_id").cloned();
                if let Some(session) = &session {
                    assert_eq!(session, "session alpha &?=+ø");
                    assert_eq!(query.len(), 1);
                }
                return Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"path":path,"session_id":session}).to_string(),
                    ))
                    .unwrap();
            }
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!("{{\"path\":\"{path}\"}}")))
                .unwrap()
        }
        let app = Router::new().fallback(get(handler));
        let (base, task) = spawn(app).await;
        let client = MonitorClient::new(&base, Some("top-secret".into())).unwrap();
        let result = client
            .fetch_usage("today", 0, 7, 1000, Some("session alpha &?=+ø"))
            .await;
        assert_eq!(result.summary.unwrap()["path"], "/token-usage");
        assert_eq!(result.events.unwrap()["path"], "/token-usage/events");
        assert_eq!(result.sessions.unwrap()["path"], "/token-usage/sessions");
        assert_eq!(result.rates.unwrap()["path"], "/token-usage/rates");
        assert_eq!(
            result.session_rates.unwrap().unwrap()["session_id"],
            "session alpha &?=+ø"
        );
        let error = result.daily.unwrap_err();
        assert_eq!(error, "Server returned HTTP 500");
        assert!(!error.contains("top-secret"));
        assert_eq!(
            client.fetch_quota().await.unwrap()["path"],
            "/usage/github-copilot"
        );
        assert_eq!(client.fetch_models().await.unwrap()["path"], "/models");
        assert!(
            client
                .fetch_usage("bad", 1, 1, 20, None)
                .await
                .summary
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn session_rate_selection_is_optional_and_invalid_ids_are_not_sent() {
        async fn handler(State(hits): State<Arc<AtomicUsize>>, request: Request) -> Response {
            if request.uri().path() == "/token-usage/rates" {
                hits.fetch_add(1, Ordering::SeqCst);
                assert!(
                    request.uri().query().is_none(),
                    "invalid session must not be sent"
                );
            }
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap()
        }
        let hits = Arc::new(AtomicUsize::new(0));
        let (base, task) = spawn(
            Router::new()
                .fallback(get(handler))
                .with_state(hits.clone()),
        )
        .await;
        let client = MonitorClient::new(&base, None).unwrap();
        let result = client.fetch_usage("today", 1, 1, 20, None).await;
        assert!(result.rates.is_ok());
        assert!(result.session_rates.is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        for invalid in ["s\ntoken".to_owned(), "x".repeat(MAX_SESSION_ID_BYTES + 1)] {
            let result = client.fetch_usage("today", 1, 1, 20, Some(&invalid)).await;
            assert!(result.rates.is_ok());
            assert_eq!(
                result.session_rates.unwrap().unwrap_err(),
                "Invalid session identifier"
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        task.abort();
    }

    #[tokio::test]
    async fn denies_redirects_and_does_not_send_key_to_destination() {
        async fn destination(State(hits): State<Arc<AtomicUsize>>, _: HeaderMap) -> &'static str {
            hits.fetch_add(1, Ordering::SeqCst);
            "{}"
        }
        let hits = Arc::new(AtomicUsize::new(0));
        let (destination_url, destination_task) = spawn(
            Router::new()
                .fallback(get(destination))
                .with_state(hits.clone()),
        )
        .await;
        let app = Router::new().fallback(get(move || {
            let location = destination_url.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, location)],
                )
            }
        }));
        let (base, task) = spawn(app).await;
        let client = MonitorClient::new(&base, Some("top-secret".into())).unwrap();
        assert_eq!(
            client.fetch_quota().await.unwrap_err(),
            "Server returned HTTP 307"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        task.abort();
        destination_task.abort();
    }

    #[tokio::test]
    async fn rejects_invalid_and_oversized_responses() {
        let app = Router::new().fallback(get(|request: Request| async move {
            match request.uri().path() {
                "/token-usage" => "not json".to_owned(),
                "/token-usage/daily" => "[]".to_owned(),
                _ => "x".repeat(MAX_RESPONSE + 1),
            }
        }));
        let (base, task) = spawn(app).await;
        let result = MonitorClient::new(&base, None)
            .unwrap()
            .fetch_usage("today", 1, 1, 20, None)
            .await;
        assert_eq!(
            result.summary.unwrap_err(),
            "Invalid monitoring response JSON"
        );
        assert_eq!(
            result.daily.unwrap_err(),
            "Invalid monitoring response format"
        );
        assert_eq!(
            result.events.unwrap_err(),
            "Monitoring response exceeded 2 MiB"
        );
        task.abort();
    }
}
