//! GitHub device login, credential persistence and Copilot token exchange.
//!
//! Secrets are intentionally not logged or included in Debug output. A
//! [`CopilotAuth`] instance serializes refreshes and caches each issued token.
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Response, Url, redirect::Policy};
use serde::Deserialize;
use tokio::sync::Mutex;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const MAX_RESPONSE: usize = 1024 * 1024;
const USER_AGENT: &str = "GitHubCopilotChat/0.72.0";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Network endpoints used for device login and token exchange.
/// Alternate endpoints should only be supplied from trusted configuration.
#[derive(Clone)]
pub struct AuthConfig {
    pub github_base: String,
    pub github_api_base: String,
    pub client_id: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            github_base: "https://github.com".into(),
            github_api_base: "https://api.github.com".into(),
            client_id: CLIENT_ID.into(),
        }
    }
}

#[derive(Clone)]
pub struct GitHubAuth {
    config: AuthConfig,
    client: Client,
}

/// Device code response. Display only `user_code` and `verification_uri`.
#[derive(Deserialize)]
pub struct DeviceCode {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
struct PollResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    token: String,
    expires_at: u64,
    refresh_in: u64,
    endpoints: Option<ExchangeEndpoints>,
}

#[derive(Deserialize)]
struct ExchangeEndpoints {
    api: Option<String>,
}

/// A short-lived token and the API origin it was issued for.
/// Avoid printing either field in logs.
#[derive(Clone)]
pub struct CopilotCredential {
    pub token: String,
    pub api_base: String,
}

struct CachedCredential {
    credential: CopilotCredential,
    refresh_at: Instant,
    expires_at: SystemTime,
    expires_monotonic: Instant,
}

impl CachedCredential {
    fn is_valid(&self) -> bool {
        Instant::now() < self.expires_monotonic && SystemTime::now() < self.expires_at
    }
}

enum ExchangeFailure {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

fn is_transient_transport(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_body()
}

/// A concurrency-safe, lazily refreshed Copilot token cache.
pub struct CopilotAuth {
    auth: GitHubAuth,
    github_token: String,
    cached: Mutex<Option<CachedCredential>>,
}

impl GitHubAuth {
    pub fn new(config: AuthConfig) -> Result<Self> {
        if std::env::var("COPILOT_API_OAUTH_APP").is_ok_and(|value| !value.trim().is_empty()) {
            bail!("alternate OAuth apps are not supported by the Rust port yet");
        }
        if std::env::var("COPILOT_API_ENTERPRISE_URL").is_ok_and(|value| !value.trim().is_empty()) {
            bail!("GitHub Enterprise authentication is not supported by the Rust port yet");
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .context("building GitHub HTTP client")?;
        Ok(Self { config, client })
    }

    /// Environment takes precedence over the existing upstream-compatible file.
    pub fn resolve_github_token(&self) -> Result<Option<String>> {
        if let Ok(value) = std::env::var("COPILOT_API_GITHUB_TOKEN")
            && !value.trim().is_empty()
        {
            return Ok(Some(value.trim().to_owned()));
        }
        let path = github_token_path()?;
        match std::fs::read_to_string(path) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading GitHub credential file"),
        }
    }

    /// Atomically store a long-lived token with owner-only permissions on Unix.
    pub fn save_github_token(&self, token: &str) -> Result<()> {
        let token = token.trim();
        if token.is_empty() {
            bail!("cannot save an empty GitHub token");
        }
        write_secret(&github_token_path()?, token)
    }

    pub async fn request_device_code(&self) -> Result<DeviceCode> {
        let response = self
            .client
            .post(format!(
                "{}/login/device/code",
                self.config.github_base.trim_end_matches('/')
            ))
            .header("accept", "application/json")
            .json(&serde_json::json!({"client_id": self.config.client_id, "scope": "read:user"}))
            .send()
            .await
            .context("requesting GitHub device code")?;
        let device: DeviceCode = parse_json(response, "GitHub device code").await?;
        if device.device_code.is_empty() || device.user_code.is_empty() || device.expires_in == 0 {
            bail!("GitHub returned an invalid device code");
        }
        Ok(device)
    }

    /// Wait for the user to authorize the device code. Dropping the future cancels polling.
    pub async fn poll_device_token(&self, device: &DeviceCode) -> Result<String> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(device.expires_in))
            .ok_or_else(|| anyhow!("invalid device code expiry"))?;
        let mut interval = device.interval.max(1);
        loop {
            if Instant::now() >= deadline {
                bail!("GitHub device authorization expired; sign in again");
            }
            let request = self
                .client
                .post(format!(
                    "{}/login/oauth/access_token",
                    self.config.github_base.trim_end_matches('/')
                ))
                .header("accept", "application/json")
                .json(&serde_json::json!({
                    "client_id": self.config.client_id,
                    "device_code": device.device_code,
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
                }))
                .send();
            let response = match tokio::time::timeout_at(deadline.into(), request).await {
                Ok(response) => response,
                Err(_) => bail!("GitHub device authorization expired; sign in again"),
            };
            if let Ok(response) = response
                && response.status().is_success()
            {
                let result: PollResponse = tokio::time::timeout_at(
                    deadline.into(),
                    parse_json(response, "GitHub device authorization"),
                )
                .await
                .map_err(|_| anyhow!("GitHub device authorization expired; sign in again"))??;
                if let Some(token) = result.access_token.filter(|s| !s.is_empty()) {
                    return Ok(token);
                }
                match result.error.as_deref() {
                    Some("slow_down") => {
                        interval = interval.saturating_add(5).max(result.interval.unwrap_or(0))
                    }
                    Some("authorization_pending") | None => {}
                    Some(error) => bail!(
                        "GitHub device authorization failed: {}",
                        result.error_description.as_deref().unwrap_or(error)
                    ),
                }
            }
            let delay = Duration::from_secs(interval.saturating_add(1));
            tokio::time::sleep(delay.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
    }
}

impl CopilotAuth {
    pub fn new(auth: GitHubAuth, github_token: String) -> Result<Self> {
        if github_token.trim().is_empty() {
            bail!("empty GitHub token");
        }
        Ok(Self {
            auth,
            github_token,
            cached: Mutex::new(None),
        })
    }

    pub async fn credential(&self) -> Result<CopilotCredential> {
        let mut cache = self.cached.lock().await;
        if let Some(cached) = cache.as_ref()
            && cached.is_valid()
            && Instant::now() < cached.refresh_at
        {
            return Ok(cached.credential.clone());
        }
        match self.exchange().await {
            Ok(new) => {
                let credential = new.credential.clone();
                *cache = Some(new);
                Ok(credential)
            }
            Err(ExchangeFailure::Transient(error)) => {
                if let Some(cached) = cache.as_mut() {
                    // A request can itself take long enough for the old token to expire.
                    if cached.is_valid() {
                        cached.refresh_at = Instant::now() + Duration::from_secs(5);
                        return Ok(cached.credential.clone());
                    }
                }
                Err(error)
            }
            Err(ExchangeFailure::Permanent(error)) => Err(error),
        }
    }

    async fn exchange(&self) -> std::result::Result<CachedCredential, ExchangeFailure> {
        let response = self
            .auth
            .client
            .get(format!(
                "{}/copilot_internal/v2/token",
                self.auth.config.github_api_base.trim_end_matches('/')
            ))
            .header("authorization", format!("token {}", self.github_token))
            .header("user-agent", USER_AGENT)
            .header("x-github-api-version", "2025-04-01")
            .header("x-vscode-user-agent-library-version", "electron-fetch")
            .send()
            .await
            .map_err(|error| {
                let transient = is_transient_transport(&error);
                let error = anyhow!(error).context("exchanging GitHub token for Copilot token");
                if transient {
                    ExchangeFailure::Transient(error)
                } else {
                    ExchangeFailure::Permanent(error)
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            let error = anyhow!("Copilot token exchange returned HTTP {status}");
            return Err(if status.as_u16() == 429 || status.is_server_error() {
                ExchangeFailure::Transient(error)
            } else {
                ExchangeFailure::Permanent(error)
            });
        }
        let response: ExchangeResponse = parse_json(response, "Copilot token exchange")
            .await
            .map_err(|error| {
                let transient = error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(is_transient_transport)
                });
                if transient {
                    ExchangeFailure::Transient(error)
                } else {
                    ExchangeFailure::Permanent(error)
                }
            })?;
        self.cache_response(response)
            .map_err(ExchangeFailure::Permanent)
    }

    fn cache_response(&self, response: ExchangeResponse) -> Result<CachedCredential> {
        if response.token.is_empty() {
            bail!("Copilot returned an empty token");
        }
        let api = response
            .endpoints
            .and_then(|e| e.api)
            .unwrap_or_else(|| "https://api.githubcopilot.com".into());
        let api_base = trusted_copilot_endpoint(&api)?;
        let expires_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(response.expires_at))
            .ok_or_else(|| anyhow!("invalid Copilot token expiry"))?;
        let until_expiry = expires_at
            .duration_since(SystemTime::now())
            .map_err(|_| anyhow!("Copilot returned an expired token"))?;
        if until_expiry.is_zero() {
            bail!("Copilot returned an expired token");
        }
        let now = Instant::now();
        let expires_monotonic = now
            .checked_add(until_expiry)
            .ok_or_else(|| anyhow!("invalid Copilot token expiry"))?;
        // Refresh before expiration and never later than GitHub's refresh interval.
        let lifetime = Duration::from_secs(response.refresh_in.max(1))
            .min(until_expiry.saturating_sub(Duration::from_secs(30)));
        let credential = CopilotCredential {
            token: response.token,
            api_base,
        };
        let refresh_at = now
            .checked_add(lifetime)
            .ok_or_else(|| anyhow!("invalid Copilot token expiry"))?;
        Ok(CachedCredential {
            credential,
            refresh_at,
            expires_at,
            expires_monotonic,
        })
    }
}

async fn parse_json<T: serde::de::DeserializeOwned>(
    mut response: Response,
    description: &str,
) -> Result<T> {
    if !response.status().is_success() {
        bail!("{} returned HTTP {}", description, response.status());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("reading GitHub response")? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            bail!("{} response is too large", description);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {} response", description))
}

fn trusted_copilot_endpoint(value: &str) -> Result<String> {
    let parsed = Url::parse(value).context("invalid Copilot API endpoint")?;
    let host = parsed.host_str().unwrap_or("");
    if parsed.scheme() != "https"
        || !(host == "githubcopilot.com" || host.ends_with(".githubcopilot.com"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        bail!("untrusted Copilot API endpoint");
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

fn github_token_path() -> Result<PathBuf> {
    let home = match std::env::var_os("COPILOT_API_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::home_dir()
            .ok_or_else(|| anyhow!("home directory not available"))?
            .join(".local/share/copilot-api"),
    };
    let oauth = std::env::var("COPILOT_API_OAUTH_APP").unwrap_or_default();
    // Never allow an environment-controlled path component to escape APP_DIR.
    if !oauth.is_empty() && oauth != "opencode" {
        bail!("unsupported COPILOT_API_OAUTH_APP");
    }
    let prefix = if std::env::var("COPILOT_API_ENTERPRISE_URL").is_ok_and(|s| !s.trim().is_empty())
    {
        "ent_"
    } else {
        ""
    };
    Ok(home.join(oauth).join(format!("{prefix}github_token")))
}

fn write_secret(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("credential path has no parent"))?;
    std::fs::create_dir_all(parent).context("creating credential directory")?;
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".github_token.{}.{}.tmp", std::process::id(), id));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .context("creating credential temporary file")?;
    let result = (|| -> Result<()> {
        file.write_all(contents.as_bytes())
            .context("writing credential")?;
        file.sync_all().context("syncing credential")?;
        std::fs::rename(&tmp, path).context("installing credential")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn mock_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        mock_server_status(responses.into_iter().map(|body| (200, body)).collect())
    }

    fn mock_server_status(
        responses: Vec<(u16, String)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    input.extend_from_slice(&buffer[..n]);
                    if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(input).unwrap());
                write!(stream, "HTTP/1.1 {status} mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            requests
        });
        (format!("http://{addr}"), handle)
    }

    fn test_auth(base: String) -> GitHubAuth {
        GitHubAuth::new(AuthConfig {
            github_base: base.clone(),
            github_api_base: base,
            client_id: CLIENT_ID.into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn device_login_uses_expected_protocol() {
        let (base, server) = mock_server(vec![
            r#"{"device_code":"private-device","user_code":"ABCD","verification_uri":"https://github.com/login/device","expires_in":60,"interval":1}"#.into(),
            r#"{"access_token":"private-token"}"#.into(),
        ]);
        let auth = test_auth(base);
        let device = auth.request_device_code().await.unwrap();
        assert_eq!(device.user_code, "ABCD");
        assert_eq!(
            auth.poll_device_token(&device).await.unwrap(),
            "private-token"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /login/device/code "));
        assert!(requests[0].contains("read:user"));
        assert!(requests[1].starts_with("POST /login/oauth/access_token "));
        assert!(requests[1].contains("urn:ietf:params:oauth:grant-type:device_code"));
    }

    #[tokio::test]
    async fn exchange_uses_headers_and_caches_token() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let (base, server) = mock_server(vec![format!(
            r#"{{"token":"copilot-secret","refresh_in":600,"expires_at":{expiry},"endpoints":{{"api":"https://api.enterprise.githubcopilot.com"}}}}"#
        )]);
        let auth = CopilotAuth::new(test_auth(base), "github-secret".into()).unwrap();
        let (first, second) = tokio::join!(auth.credential(), auth.credential());
        let first = first.unwrap();
        assert_eq!(first.token, "copilot-secret");
        assert_eq!(second.unwrap().token, "copilot-secret");
        assert_eq!(first.api_base, "https://api.enterprise.githubcopilot.com");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /copilot_internal/v2/token "));
        assert!(requests[0].contains("authorization: token github-secret"));
        assert!(requests[0].contains("x-github-api-version: 2025-04-01"));
    }

    #[tokio::test]
    async fn exchange_rejects_untrusted_endpoint_and_expired_token() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let (base, server) = mock_server(vec![format!(
            r#"{{"token":"secret","refresh_in":10,"expires_at":{expiry},"endpoints":{{"api":"https://evil.example"}}}}"#
        )]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        assert!(auth.credential().await.is_err());
        server.join().unwrap();
        let (base, server) = mock_server(vec![
            r#"{"token":"secret","refresh_in":10,"expires_at":1}"#.into(),
        ]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        assert!(auth.credential().await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn refreshes_after_interval_and_uses_default_endpoint() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let (base, server) = mock_server(vec![
            format!(r#"{{"token":"first","refresh_in":1,"expires_at":{expiry}}}"#),
            format!(r#"{{"token":"second","refresh_in":600,"expires_at":{expiry}}}"#),
        ]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        let first = auth.credential().await.unwrap();
        assert_eq!(first.api_base, "https://api.githubcopilot.com");
        assert_eq!(first.token, "first");
        tokio::time::sleep(Duration::from_millis(1050)).await;
        assert_eq!(auth.credential().await.unwrap().token, "second");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn temporary_refresh_failure_uses_valid_cache_then_retries() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let (base, server) = mock_server_status(vec![
            (
                200,
                format!(r#"{{"token":"first","refresh_in":600,"expires_at":{expiry}}}"#),
            ),
            (503, "down".into()),
            (
                200,
                format!(r#"{{"token":"second","refresh_in":600,"expires_at":{expiry}}}"#),
            ),
        ]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        assert_eq!(auth.credential().await.unwrap().token, "first");
        auth.cached.lock().await.as_mut().unwrap().refresh_at = Instant::now();
        assert_eq!(auth.credential().await.unwrap().token, "first");
        // The retry delay prevents every concurrent request hammering the endpoint.
        assert_eq!(auth.credential().await.unwrap().token, "first");
        auth.cached.lock().await.as_mut().unwrap().refresh_at = Instant::now();
        assert_eq!(auth.credential().await.unwrap().token, "second");
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn expired_cache_and_nontransient_errors_never_fallback() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let body = format!(r#"{{"token":"first","refresh_in":600,"expires_at":{expiry}}}"#);
        let (base, server) = mock_server_status(vec![(200, body.clone()), (503, "down".into())]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        auth.credential().await.unwrap();
        {
            let mut cache = auth.cached.lock().await;
            let cached = cache.as_mut().unwrap();
            // A forward wall-clock change must invalidate a token even before refresh_at.
            cached.expires_at = SystemTime::now() - Duration::from_secs(1);
        }
        assert!(auth.credential().await.is_err());
        server.join().unwrap();

        let (base, server) = mock_server_status(vec![(200, body), (401, "denied".into())]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        auth.credential().await.unwrap();
        auth.cached.lock().await.as_mut().unwrap().refresh_at = Instant::now();
        let error = match auth.credential().await {
            Ok(_) => panic!("expected authorization error"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("401"));
        assert!(!error.to_string().contains("secret"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn transport_failure_uses_valid_cache_but_not_expired_cache() {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let (base, server) = mock_server(vec![format!(
            r#"{{"token":"first","refresh_in":600,"expires_at":{expiry}}}"#
        )]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        auth.credential().await.unwrap();
        server.join().unwrap();
        auth.cached.lock().await.as_mut().unwrap().refresh_at = Instant::now();
        assert_eq!(auth.credential().await.unwrap().token, "first");
        {
            let mut cache = auth.cached.lock().await;
            let cached = cache.as_mut().unwrap();
            cached.expires_monotonic = Instant::now();
        }
        assert!(auth.credential().await.is_err());
    }

    #[tokio::test]
    async fn device_denial_and_expiry_are_errors() {
        let (base, server) = mock_server(vec![r#"{"error":"access_denied"}"#.into()]);
        let auth = test_auth(base);
        let device = DeviceCode {
            device_code: "private".into(),
            user_code: "public".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 30,
            interval: 1,
        };
        assert!(auth.poll_device_token(&device).await.is_err());
        server.join().unwrap();
        let expired = DeviceCode {
            expires_in: 0,
            ..device
        };
        assert!(auth.poll_device_token(&expired).await.is_err());
    }

    #[tokio::test]
    async fn exchange_rejects_oversized_and_invalid_json() {
        let (base, server) = mock_server(vec!["x".repeat(MAX_RESPONSE + 1)]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        assert!(auth.credential().await.is_err());
        server.join().unwrap();
        let (base, server) = mock_server(vec!["not json".into()]);
        let auth = CopilotAuth::new(test_auth(base), "secret".into()).unwrap();
        assert!(auth.credential().await.is_err());
        server.join().unwrap();
    }

    #[test]
    fn validates_credential_endpoint() {
        assert_eq!(
            trusted_copilot_endpoint("https://api.business.githubcopilot.com/").unwrap(),
            "https://api.business.githubcopilot.com"
        );
        for invalid in [
            "http://api.githubcopilot.com",
            "https://evilgithubcopilot.com",
            "https://api.githubcopilot.com.evil.test",
            "https://attacker@api.githubcopilot.com",
            "https://api.githubcopilot.com:444",
            "https://api.githubcopilot.com/path",
        ] {
            assert!(trusted_copilot_endpoint(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn stores_secret_privately_and_overwrites_atomically() {
        let root = std::env::temp_dir().join(format!(
            "copilot-auth-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("github_token");
        write_secret(&path, "first").unwrap();
        write_secret(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
