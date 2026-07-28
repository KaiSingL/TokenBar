use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod api;
mod app;
mod config;
mod error;
mod model;
mod session;
mod tui;

#[derive(Parser)]
#[command(name = "tokenbar", about = "TUI monitor for AI subscription plan limits")]
struct Cli {
    #[arg(short, long, help = "Override config file path")]
    config: Option<String>,

    #[arg(long, help = "Override data directory (default: ~/.config/tokenbar or %APPDATA%/tokenbar)")]
    data_dir: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage session cookies
    Session {
        #[command(subcommand)]
        action: SessionCommands,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    /// Store a session cookie for an account
    Set {
        /// Account name (must match auth.toml)
        name: String,

        /// Session cookie value
        #[arg(short, long)]
        cookie: Option<String>,

        /// Import cookie from a browser export file
        #[arg(long)]
        json_file_path: Option<String>,
    },
    /// Remove a session entry
    Rm {
        /// Account name
        name: String,
    },
    /// List session entries (no cookie values)
    Status,
    /// Show session details for all accounts
    Export,
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

    let data_dir = config::resolve_data_dir(cli.data_dir.as_deref())?;
    let config_path = if let Some(ref override_path) = cli.config {
        std::path::PathBuf::from(override_path)
    } else {
        config::resolve_config_path(&data_dir)
    };

    match cli.command {
        Some(Commands::Session { action }) => {
            run_session_command(action, &data_dir)?;
            Ok(())
        }
        None => {
            run_tui(&config_path, &data_dir).await
        }
    }
}

fn run_session_command(cmd: SessionCommands, data_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_path = session::resolve_sessions_path(data_dir);
    let mut sessions = session::load_sessions(&sessions_path)?;

    match cmd {
        SessionCommands::Set { name, cookie, json_file_path } => {
            let cookie_val = if let Some(c) = cookie {
                c
            } else if let Some(path) = json_file_path {
                let contents = std::fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read {path}: {e}"))?;
                contents.trim().to_string()
            } else {
                return Err("Either --cookie or --json-file-path is required".into());
            };

            sessions.sessions.insert(name.clone(), model::SessionEntry {
                cookie: cookie_val,
                workspace_id: None,
                updated_at: chrono::Utc::now(),
            });
            session::save_sessions(&sessions_path, &sessions)?;
            info!("Session stored for account '{}'", name);
        }
        SessionCommands::Rm { name } => {
            if sessions.sessions.remove(&name).is_some() {
                session::save_sessions(&sessions_path, &sessions)?;
                info!("Session removed for account '{}'", name);
            } else {
                info!("No session found for account '{}'", name);
            }
        }
        SessionCommands::Status => {
            if sessions.sessions.is_empty() {
                println!("No sessions stored.");
            } else {
                println!("Sessions:");
                for name in sessions.sessions.keys() {
                    println!("  {name}");
                }
            }
        }
        SessionCommands::Export => {
            if sessions.sessions.is_empty() {
                println!("No sessions stored.");
            } else {
                println!("Session details:");
                for (name, entry) in &sessions.sessions {
                    let age = chrono::Utc::now()
                        .signed_duration_since(entry.updated_at);
                    let wid = entry.workspace_id.as_deref().unwrap_or("(discover on next poll)");
                    println!("  {name}:");
                    println!("    cookie: ({} chars)", entry.cookie.len());
                    println!("    workspace_id: {wid}");
                    println!("    updated: {} ({} ago)", entry.updated_at.format("%Y-%m-%d %H:%M:%S"), age);
                }
            }
        }
    }

    Ok(())
}

async fn run_tui(config_path: &std::path::Path, data_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    debug!("Loading config from {}", config_path.display());

    let app_config = config::load_config(config_path)?;

    info!(
        "Loaded {} account(s) from config",
        app_config.accounts.len()
    );

    let state = Arc::new(RwLock::new(app::AppState::new(app_config.clone())));
    let (event_tx, event_rx) = mpsc::channel::<app::AppEvent>(64);

    let poller = app::Poller::new(state.clone(), event_rx, &app_config, data_dir);

    let poller_handle = tokio::spawn(async move {
        poller.run().await;
    });

    let tui_result = tui::run_tui(state.clone(), event_tx.clone()).await;

    let _ = poller_handle.await;

    tui_result
}
