use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use copilot_api_rust::{
    auth::{AuthConfig, CopilotAuth, GitHubAuth},
    server,
};
use std::{net::SocketAddr, sync::Arc};

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
    Serve {
        #[arg(long, default_value = "127.0.0.1:4141")]
        listen: SocketAddr,
        /// Shared client key, provided via environment to avoid shell history.
        #[arg(long, env = "COPILOT_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
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
        Command::Serve { listen, api_key } => {
            if api_key.as_ref().is_some_and(|k| k.is_empty()) {
                bail!("API key must not be empty");
            }
            if !listen.ip().is_loopback() && api_key.is_none() {
                bail!("a client API key is required when listening on a non-loopback address");
            }
            let token = github.resolve_github_token()?.context(
                "no GitHub token; run `copilot-api-rust auth` or set COPILOT_API_GITHUB_TOKEN",
            )?;
            let auth = Arc::new(CopilotAuth::new(github, token)?);
            let app = server::router(auth, api_key)?;
            let listener = tokio::net::TcpListener::bind(listen).await?;
            eprintln!("Listening on http://{}", listener.local_addr()?);
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}
