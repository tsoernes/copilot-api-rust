//! GitHub Copilot account usage and the upstream subscription-usage projection.
//!
//! The long-lived GitHub token is sent only to the GitHub API; it must never be
//! exposed to a caller, logged, or redirected to another origin.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use futures_util::StreamExt;
use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn quota_reset_timestamp(reset: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(reset)
        .ok()
        .map(|date| date.timestamp())
        .or_else(|| {
            NaiveDate::parse_from_str(reset, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date: NaiveDateTime| date.and_utc().timestamp())
        })
}

/// Fetches current quota information directly from GitHub on demand.
/// The service does not maintain a local quota cache or estimate billing.
pub struct QuotaClient {
    client: Client,
    url: Url,
    github_token: String,
}

impl QuotaClient {
    pub fn new(github_token: String) -> Result<Self> {
        Self::with_url(
            github_token,
            Url::parse("https://api.github.com/copilot_internal/user")?,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_test_url(github_token: String, url: &str) -> Result<Self> {
        Self::with_url(github_token, Url::parse(url)?)
    }

    fn with_url(github_token: String, url: Url) -> Result<Self> {
        if github_token.trim().is_empty() {
            bail!("empty GitHub token");
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("building GitHub usage client")?;
        Ok(Self {
            client,
            url,
            github_token,
        })
    }

    /// Fetch the original GitHub JSON payload, as the upstream `/usage` route does.
    pub async fn fetch_raw(&self) -> Result<Value> {
        let response = self
            .client
            .get(self.url.clone())
            .header("authorization", format!("token {}", self.github_token))
            .header("user-agent", "GitHubCopilotChat/0.72.0")
            .header("x-github-api-version", "2025-04-01")
            .header("x-vscode-user-agent-library-version", "electron-fetch")
            .send()
            .await
            .context("fetching GitHub Copilot usage")?;
        let status = response.status();
        if !status.is_success() {
            bail!("GitHub Copilot usage returned HTTP {status}");
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            bail!("GitHub Copilot usage response exceeds size limit");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading GitHub Copilot usage")?;
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                bail!("GitHub Copilot usage response exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("decoding GitHub Copilot usage JSON")
    }

    /// Fetch and project the upstream-compatible `/usage/github-copilot` result.
    pub async fn fetch_normalized(&self) -> Result<Value> {
        normalize_copilot_usage(self.fetch_raw().await?)
    }
}

#[derive(Deserialize)]
struct Usage {
    copilot_plan: Option<String>,
    quota_reset_date: Option<String>,
    quota_snapshots: Option<Snapshots>,
    token_based_billing: Option<bool>,
}

#[derive(Default, Deserialize)]
struct Snapshots {
    premium_interactions: Option<Quota>,
    chat: Option<Quota>,
    completions: Option<Quota>,
}

#[derive(Deserialize)]
struct Quota {
    entitlement: Option<f64>,
    remaining: Option<f64>,
    quota_remaining: Option<f64>,
    percent_remaining: Option<f64>,
    unlimited: Option<bool>,
}

/// Match the upstream GitHub Copilot subscription projection without exposing
/// account login or other private fields from the raw upstream response.
pub fn normalize_copilot_usage(value: Value) -> Result<Value> {
    let usage: Usage = serde_json::from_value(value).context("invalid Copilot usage payload")?;
    let snapshots = usage.quota_snapshots.unwrap_or_default();
    let mut windows = Vec::new();
    for (id, label, quota) in [
        (
            "premium_interactions",
            "Premium",
            snapshots.premium_interactions,
        ),
        ("chat", "Chat", snapshots.chat),
        ("completions", "Completions", snapshots.completions),
    ] {
        let Some(quota) = quota else { continue };
        let remaining = quota.remaining.or(quota.quota_remaining);
        let limit = quota.entitlement;
        let percent = quota.percent_remaining.or_else(|| {
            limit
                .zip(remaining)
                .and_then(|(limit, remaining)| (limit > 0.0).then_some((remaining / limit) * 100.0))
        });
        let unlimited = quota.unlimited.unwrap_or(false);
        if !unlimited && percent.is_none() && remaining.is_none() && limit.is_none() {
            continue;
        }
        let reached = !unlimited && percent.is_some_and(|percent| percent <= 0.0);
        let mut window = json!({
            "id": format!("copilot:{id}"), "kind": "usage", "name": label,
            "unlimited": unlimited, "unit": "requests", "limitReached": reached,
        });
        let object = window.as_object_mut().expect("constructed object");
        if !unlimited && let Some(percent) = percent {
            object.insert("usedPercent".into(), json!(100.0 - percent));
        }
        if let (Some(limit), Some(remaining)) = (limit, remaining) {
            object.insert("used".into(), json!((limit - remaining).max(0.0)));
        }
        if let Some(limit) = limit {
            object.insert("limit".into(), json!(limit));
        }
        // Preserve the actual reported values instead of reconstructing a
        // balance from rounded percentages or clamped consumption.
        if let Some(remaining) = remaining {
            object.insert("remaining".into(), json!(remaining));
        }
        if let Some(quota_remaining) = quota.quota_remaining {
            object.insert("reportedQuotaRemaining".into(), json!(quota_remaining));
        }
        if let Some(ref reset) = usage.quota_reset_date {
            object.insert("resetDate".into(), json!(reset));
            if let Some(reset_at) = quota_reset_timestamp(reset) {
                object.insert("resetAt".into(), json!(reset_at));
            }
        }
        windows.push(window);
    }
    let reached = windows.iter().any(|window| window["limitReached"] == true);
    let mut result =
        json!({ "provider": "github-copilot", "windows": windows, "limitReached": reached });
    if let Some(plan) = usage.copilot_plan {
        result["planType"] = json!(plan);
    }
    if let Some(token_based) = usage.token_based_billing {
        result["tokenBasedBilling"] = json!(token_based);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::{Request, State},
        http::{HeaderMap, StatusCode},
        response::Response,
        routing::get,
    };
    use tokio::{net::TcpListener, sync::mpsc};

    fn sample() -> Value {
        json!({"login":"private-login", "copilot_plan":"individual", "quota_reset_date":"2026-11-01", "quota_snapshots": {
            "premium_interactions": {"entitlement":1500, "remaining":1049, "percent_remaining":69.9},
            "chat":{"entitlement":100, "quota_remaining":0}, "completions":{"unlimited":true}
        }})
    }

    #[test]
    fn normalizes_upstream_subscription_shape() {
        let value = normalize_copilot_usage(sample()).unwrap();
        assert_eq!(value["provider"], "github-copilot");
        assert_eq!(value["planType"], "individual");
        assert_eq!(value["limitReached"], true);
        let windows = value["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0]["id"], "copilot:premium_interactions");
        assert_eq!(windows[0]["used"], 451.0);
        assert_eq!(windows[0]["resetDate"], "2026-11-01");
        assert_eq!(
            windows[0]["resetAt"], 1_793_491_200,
            "date-only reset values are projected at the UTC day boundary"
        );
        assert!((windows[0]["usedPercent"].as_f64().unwrap() - 30.1).abs() < 1e-10);
        assert_eq!(windows[1]["usedPercent"], 100.0);
        assert_eq!(windows[1]["limitReached"], true);
        assert_eq!(windows[2]["unlimited"], true);
        assert!(windows[2].get("usedPercent").is_none());
        assert!(!value.to_string().contains("private-login"));
    }

    #[test]
    fn handles_nulls_empty_and_overage_and_rejects_bad_types() {
        let value = normalize_copilot_usage(json!({"quota_snapshots":{
            "premium_interactions":{"percent_remaining":25}, "chat":{},
            "completions":{"entitlement":100,"remaining":150}
        }}))
        .unwrap();
        assert_eq!(value["windows"].as_array().unwrap().len(), 2);
        assert_eq!(value["windows"][0]["usedPercent"], 75.0);
        assert!(value["windows"][0].get("limit").is_none());
        assert_eq!(value["windows"][1]["used"], 0.0);
        assert_eq!(
            normalize_copilot_usage(json!({"quota_snapshots":null})).unwrap()["windows"],
            json!([])
        );
        assert!(
            normalize_copilot_usage(
                json!({"quota_snapshots":{"chat":{"percent_remaining":"invalid"}}})
            )
            .is_err()
        );
        assert!(normalize_copilot_usage(json!({"copilot_plan":7})).is_err());
        assert!(QuotaClient::new("  ".into()).is_err());
    }

    #[test]
    fn parses_date_only_and_precise_quota_reset_values() {
        assert_eq!(quota_reset_timestamp("2026-11-01"), Some(1_793_491_200));
        assert_eq!(
            quota_reset_timestamp("2026-11-01T14:30:00+00:00"),
            Some(1_793_543_400)
        );
        assert_eq!(quota_reset_timestamp("not-a-date"), None);
    }

    #[test]
    fn preserves_separately_reported_balances_and_billing_mode() {
        let value = normalize_copilot_usage(json!({"token_based_billing":true,"quota_snapshots":{
            "premium_interactions":{"entitlement":1000,"remaining":600,"quota_remaining":600.25,"percent_remaining":60.0}
        }})).unwrap();
        assert_eq!(value["tokenBasedBilling"], true);
        assert_eq!(value["windows"][0]["remaining"], 600.0);
        assert_eq!(value["windows"][0]["reportedQuotaRemaining"], 600.25);
        assert_eq!(value["windows"][0]["used"], 400.0);
        let only_percent = normalize_copilot_usage(
            json!({"quota_snapshots":{"premium_interactions":{"percent_remaining":50}}}),
        )
        .unwrap();
        assert!(only_percent["windows"][0].get("remaining").is_none());
        assert!(only_percent.get("tokenBasedBilling").is_none());
    }

    #[derive(Clone)]
    struct MockState {
        sender: mpsc::UnboundedSender<(String, HeaderMap)>,
        status: StatusCode,
        body: String,
        location: Option<String>,
    }

    async fn handler(State(state): State<MockState>, request: Request) -> Response {
        state
            .sender
            .send((request.uri().path().into(), request.headers().clone()))
            .unwrap();
        let mut response = Response::builder().status(state.status);
        if let Some(location) = state.location {
            response = response.header("location", location);
        }
        response.body(Body::from(state.body)).unwrap()
    }

    async fn mock(
        status: StatusCode,
        body: String,
        location: Option<String>,
    ) -> (
        Url,
        mpsc::UnboundedReceiver<(String, HeaderMap)>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let app = Router::new()
            .route("/copilot_internal/user", get(handler))
            .with_state(MockState {
                sender,
                status,
                body,
                location,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/copilot_internal/user",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, receiver, task)
    }

    #[tokio::test]
    async fn fetches_raw_and_normalized_with_github_auth_headers() {
        let (url, mut rx, task) = mock(StatusCode::OK, sample().to_string(), None).await;
        let client = QuotaClient::with_url("private-token".into(), url).unwrap();
        let raw = client.fetch_raw().await.unwrap();
        assert_eq!(raw["login"], "private-login");
        let (path, headers) = rx.recv().await.unwrap();
        assert_eq!(path, "/copilot_internal/user");
        assert_eq!(headers["authorization"], "token private-token");
        assert_eq!(headers["x-github-api-version"], "2025-04-01");
        assert_eq!(headers["user-agent"], "GitHubCopilotChat/0.72.0");
        assert_eq!(
            client.fetch_normalized().await.unwrap()["provider"],
            "github-copilot"
        );
        task.abort();
    }

    #[tokio::test]
    async fn refuses_redirects_errors_bad_json_and_oversize() {
        for (status, body, location) in [
            (
                StatusCode::FOUND,
                "".into(),
                Some("https://attacker.example/".into()),
            ),
            (StatusCode::UNAUTHORIZED, "secret error body".into(), None),
            (StatusCode::OK, "not-json".into(), None),
            (StatusCode::OK, "x".repeat(MAX_RESPONSE_BYTES + 1), None),
        ] {
            let (url, mut rx, task) = mock(status, body, location).await;
            let client = QuotaClient::with_url("private-token".into(), url).unwrap();
            let error = client.fetch_raw().await.unwrap_err().to_string();
            assert!(!error.contains("private-token"));
            assert!(!error.contains("secret error body"));
            assert!(rx.recv().await.is_some());
            task.abort();
        }
    }
}
