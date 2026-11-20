use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use copilot_api_rust::{
    auth::{AuthConfig, CopilotAuth, GitHubAuth},
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
    /// Serve the supported Copilot-backed API routes.
    Serve(ServerArgs),
    /// Start the server and its interactive monitoring dashboard together.
    Monitor(ServerArgs),
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
            let token = github.resolve_github_token()?.context(
                "no GitHub token; run `copilot-api-rust auth` or set COPILOT_API_GITHUB_TOKEN",
            )?;
            let quota = Arc::new(QuotaClient::new(token.clone())?);
            let auth = Arc::new(CopilotAuth::new(github, token)?);
            let db_path = usage_db_path()?;
            let usage =
                Arc::new(UsageStore::open(&db_path).context("opening Rust usage database")?);
            let app = server::router(auth, api_key.clone(), quota, usage)?;
            let listener = tokio::net::TcpListener::bind(listen).await?;
            if monitoring {
                let address = listener.local_addr()?;
                let ip = match address.ip() {
                    IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    ip => ip,
                };
                let endpoint = format!("http://{}", SocketAddr::new(ip, address.port()));
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let mut server_task = tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await
                });
                let result = monitor::run(&endpoint, api_key, &mut server_task, logs).await;
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
