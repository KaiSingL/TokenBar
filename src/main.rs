use std::sync::Arc;

use clap::Parser;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod api;
mod app;
mod config;
mod error;
mod model;
mod tui;

#[derive(Parser)]
#[command(name = "tokenbar", about = "TUI monitor for AI subscription plan limits")]
struct Cli {
    #[arg(short, long, help = "Path to auth.toml config file")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,tokenbar=debug")),
        )
        .init();

    let cli = Cli::parse();

    let config_path = config::resolve_config_path(cli.config.as_deref())?;
    debug!("Loading config from {}", config_path.display());

    let app_config = config::load_config(&config_path)?;

    info!(
        "Loaded {} account(s) from config",
        app_config.accounts.len()
    );

    let state = Arc::new(RwLock::new(app::AppState::new(app_config.clone())));
    let (event_tx, event_rx) = mpsc::channel::<app::AppEvent>(64);

    let poller = app::Poller::new(state.clone(), event_rx, &app_config);

    let poller_handle = tokio::spawn(async move {
        poller.run().await;
    });

    let tui_result = tui::run_tui(state.clone(), event_tx.clone()).await;

    let _ = poller_handle.await;

    tui_result
}
