use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use copilot_api_rust::{
    auth::{AuthConfig, CopilotAuth, GitHubAuth},
    codex_catalog,
    monitor::{self, MemoryLog},
    quota::QuotaClient,
    server,
    usage::UsageStore,
};
use std::{
    io::IsTerminal,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

#[derive(Parser)]
#[command(version, about = "GitHub Copilot gateway (incremental Rust port)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with GitHub using the device flow.
    Auth,
    /// Refresh Codex's model catalogue from this gateway's available models.
    UpdateCodexModels(CodexModelsArgs),
    /// Serve the supported Copilot-backed API routes.
    Serve(ServerArgs),
    /// Start the server and its interactive monitoring dashboard together.
    Monitor(ServerArgs),
}

#[derive(Args)]
struct CodexModelsArgs {
    #[arg(long, default_value = "127.0.0.1:4141")]
    listen: SocketAddr,
    /// Shared client key, provided via environment to avoid shell history.
    #[arg(long, env = "COPILOT_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    /// Set Codex's top-level model_catalog_json setting to the refreshed catalog.
    #[arg(long)]
    activate: bool,
}

#[derive(Args)]
struct ServerArgs {
    #[arg(long, default_value = "127.0.0.1:4141")]
    listen: SocketAddr,
    /// Shared client key, provided via environment to avoid shell history.
    #[arg(long, env = "COPILOT_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let monitoring = matches!(cli.command, Command::Monitor(_));
    let logs = MemoryLog::default();
    if monitoring {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(logs.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    }
    let github = GitHubAuth::new(AuthConfig::default())?;
    match cli.command {
        Command::Auth => {
            let code = github.request_device_code().await?;
            println!(
                "Open {} and enter code {}",
                code.verification_uri, code.user_code
            );
            let token = github.poll_device_token(&code).await?;
            github.save_github_token(&token)?;
            println!("GitHub token saved.");
        }
        Command::UpdateCodexModels(CodexModelsArgs {
            listen,
            api_key,
            activate,
        }) => {
            if listen.port() == 0 {
                bail!("--listen must use a fixed port for the running gateway");
            }
            if api_key.as_ref().is_some_and(|key| key.is_empty()) {
                bail!("API key must not be empty");
            }
            let endpoint = local_endpoint(listen);
            let summary =
                codex_catalog::refresh_from_endpoint(&endpoint, api_key, activate).await?;
            println!(
                "Codex catalogue refreshed: {} models, {} excluded; {}; {}.",
                summary.model_ids.len(),
                summary.excluded_count,
                if summary.active {
                    "active in Codex config"
                } else {
                    "catalogue written but not active"
                },
                if summary.config_updated {
                    "config updated"
                } else {
                    "config unchanged"
                },
            );
            println!("Catalog: {}", summary.catalog_path.display());
        }
        Command::Serve(ServerArgs { listen, api_key })
        | Command::Monitor(ServerArgs { listen, api_key }) => {
            if monitoring && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
                bail!("monitor requires an interactive terminal on stdin and stdout");
            }
            if monitoring && !listen.ip().is_loopback() && !listen.ip().is_unspecified() {
                bail!("monitor requires a loopback or wildcard listen address");
            }
            if api_key.as_ref().is_some_and(|k| k.is_empty()) {
                bail!("API key must not be empty");
            }
            if !listen.ip().is_loopback() && api_key.is_none() {
                bail!("a client API key is required when listening on a non-loopback address");
            }
            let endpoint = local_endpoint(listen);
            if monitoring && wait_for_existing_gateway(listen).await.is_some() {
                return run_attached_monitor(&endpoint, api_key, logs).await;
            }
            let token = github.resolve_github_token()?.context(
                "no GitHub token; run `copilot-api-rust auth` or set COPILOT_API_GITHUB_TOKEN",
            )?;
            let quota = Arc::new(QuotaClient::new(token.clone())?);
            let auth = Arc::new(CopilotAuth::new(github, token)?);
            let db_path = usage_db_path()?;
            let usage =
                Arc::new(UsageStore::open(&db_path).context("opening Rust usage database")?);
            let app = server::router(auth, api_key.clone(), quota, usage)?;
            let listener = match tokio::net::TcpListener::bind(listen).await {
                Ok(listener) => listener,
                Err(error) if monitoring && error.kind() == std::io::ErrorKind::AddrInUse => {
                    if wait_for_existing_gateway(listen).await.is_some() {
                        return run_attached_monitor(&endpoint, api_key, logs).await;
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "cannot monitor {endpoint}: the address is in use, but no Copilot API Rust health endpoint was found"
                        )
                    });
                }
                Err(error) => return Err(error.into()),
            };
            if monitoring {
                let endpoint = local_endpoint(listener.local_addr()?);
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let mut server_task = tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await
                });
                launch_startup_codex_catalog_refresh(endpoint.clone(), api_key.clone());
                let result = monitor::run(&endpoint, api_key, &mut server_task, logs, false).await;
                let _ = shutdown_tx.send(());
                if !server_task.is_finished() {
                    // Bound shutdown time if clients hold long-running streams open.
                    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut server_task)
                        .await
                        .is_err()
                    {
                        server_task.abort();
                    }
                }
                return result;
            }
            let server_endpoint = local_endpoint(listener.local_addr()?);
            launch_startup_codex_catalog_refresh(server_endpoint, api_key.clone());
            eprintln!("Listening on http://{}", listener.local_addr()?);
            if listener.local_addr()?.ip().is_loopback() {
                eprintln!(
                    "Usage viewer: http://{}/usage-viewer",
                    listener.local_addr()?
                );
            }
            eprintln!("Usage database: {}", db_path.display());
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}

fn usage_db_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("COPILOT_API_RUST_USAGE_DB").filter(|p| !p.is_empty()) {
        return Ok(path.into());
    }
    let home = match std::env::var_os("COPILOT_API_HOME").filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => dirs::home_dir()
            .context("home directory not available")?
            .join(".local/share/copilot-api"),
    };
    Ok(home.join("rust-usage.sqlite3"))
}

fn local_endpoint(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

async fn wait_for_existing_gateway(listen: SocketAddr) -> Option<String> {
    if listen.port() == 0 {
        return None;
    }
    let endpoint = local_endpoint(listen);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(250))
        .timeout(Duration::from_millis(500))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;

    for attempt in 0..5 {
        if is_copilot_api_rust(&client, &endpoint).await {
            return Some(endpoint);
        }
        if attempt < 4 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    None
}

async fn is_copilot_api_rust(client: &reqwest::Client, endpoint: &str) -> bool {
    let Ok(response) = client.get(format!("{endpoint}/health")).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .headers()
        .get("x-copilot-api-service")
        .and_then(|value| value.to_str().ok())
        == Some("copilot-api-rust")
}

async fn run_attached_monitor(
    endpoint: &str,
    api_key: Option<String>,
    logs: MemoryLog,
) -> Result<()> {
    let mut no_owned_server =
        tokio::spawn(async { std::future::pending::<std::io::Result<()>>().await });
    let result = monitor::run(endpoint, api_key, &mut no_owned_server, logs, true).await;
    no_owned_server.abort();
    result
}

fn launch_startup_codex_catalog_refresh(endpoint: String, api_key: Option<String>) {
    tokio::spawn(async move {
        // Let Axum enter its accept loop before making the local models request.
        tokio::time::sleep(Duration::from_millis(300)).await;
        match codex_catalog::refresh_from_endpoint(&endpoint, api_key, true).await {
            Ok(summary) => tracing::info!(
                models = summary.model_ids.len(),
                excluded = summary.excluded_count,
                active = summary.active,
                config_updated = summary.config_updated,
                "Updated Codex model catalog at API startup"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                "Could not refresh Codex model catalog at API startup; gateway will continue"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    #[test]
    fn monitor_endpoint_uses_loopback_for_wildcard_listeners() {
        assert_eq!(
            local_endpoint("0.0.0.0:4141".parse().unwrap()),
            "http://127.0.0.1:4141"
        );
        assert_eq!(
            local_endpoint("[::]:4141".parse().unwrap()),
            "http://[::1]:4141"
        );
    }

    #[tokio::test]
    async fn monitor_recognizes_only_the_copilot_api_health_marker() {
        let app = Router::new().route(
            "/health",
            get(|| async { ([("x-copilot-api-service", "copilot-api-rust")], "ok") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = format!("http://{address}");
        let client = reqwest::Client::new();
        assert!(is_copilot_api_rust(&client, &endpoint).await);
        server.abort();
    }
}
