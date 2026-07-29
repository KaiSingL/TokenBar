use std::fs::OpenOptions;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod api;
mod app;
mod config;
mod error;
mod login;
mod model;
mod session;
mod tui;
mod web;

#[derive(Parser)]
#[command(name = "tokenbar", about = "TUI monitor for AI subscription plan limits")]
struct Cli {
    #[arg(short, long, help = "Override config file path")]
    config: Option<String>,

    #[arg(long, help = "Override data directory (default: ~/.config/tokenbar or %APPDATA%/tokenbar)")]
    data_dir: Option<String>,

    /// Enable debug logging (poll skips, API traces, etc.)
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show accounts and session status
    Status,
    /// Serve mobile-friendly web usage dashboard (loopback by default)
    Serve {
        /// Bind address (use 127.0.0.1 for Cloudflare Tunnel private sites)
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// TCP port
        #[arg(long, short = 'p', default_value_t = 8790)]
        port: u16,
    },
    /// Manage session cookies
    Session {
        #[command(subcommand)]
        action: SessionCommands,
    },
    /// Log in / store credentials for an account
    Login {
        /// Account name (added to auth.toml if missing)
        name: String,
        /// Provider: opencode_go (default), zai, or grok
        #[arg(long, default_value = "opencode_go")]
        provider: String,
        /// z.ai API key (or set Z_AI_API_KEY). Ignored for opencode_go.
        #[arg(long)]
        api_key: Option<String>,
        /// Overwrite existing OpenCode session cookie
        #[arg(long)]
        force: bool,
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
    let cli = Cli::parse();

    let data_dir = config::resolve_data_dir(cli.data_dir.as_deref())?;
    let config_path = if let Some(ref override_path) = cli.config {
        std::path::PathBuf::from(override_path)
    } else {
        config::resolve_config_path(&data_dir)
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            if cli.debug {
                EnvFilter::new("warn,tokenbar=debug")
            } else {
                EnvFilter::new("info")
            }
        });

    match cli.command {
        Some(Commands::Status) => {
            init_console_tracing(env_filter);
            print_status(&config_path, &data_dir)?;
            Ok(())
        }
        Some(Commands::Serve { bind, port }) => {
            init_file_tracing(&data_dir, env_filter)?;
            run_serve(&config_path, &data_dir, &bind, port).await
        }
        Some(Commands::Session { action }) => {
            init_console_tracing(env_filter);
            run_session_command(action, &data_dir, &config_path)?;
            Ok(())
        }
        Some(Commands::Login {
            name,
            provider,
            api_key,
            force,
        }) => {
            init_console_tracing(env_filter);
            let provider = model::ProviderKind::parse_cli(&provider)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            login::run_login_flow(&name, force, provider, api_key, &data_dir, &config_path)?;
            Ok(())
        }
        None => {
            init_file_tracing(&data_dir, env_filter)?;
            run_tui(&config_path, &data_dir).await
        }
    }
}

fn init_console_tracing(env_filter: EnvFilter) {
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

fn init_file_tracing(
    data_dir: &std::path::Path,
    env_filter: EnvFilter,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(data_dir)?;
    let log_path = data_dir.join("tokenbar.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(file)
        .with_ansi(false)
        .init();
    // Use eprintln only before alternate screen; once TUI starts logs go to file.
    eprintln!("Logging to {}", log_path.display());
    Ok(())
}

fn provider_label(p: model::ProviderKind) -> &'static str {
    p.as_str()
}

fn format_age(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

fn print_status(
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let app_config = config::load_config_or_default(config_path)?;
    let sessions = session::load_sessions(&session::resolve_sessions_path(data_dir))?;

    if app_config.accounts.is_empty() && sessions.sessions.is_empty() {
        println!("No accounts configured.");
        println!("  Add one with: tokenbar login <name>");
        return Ok(());
    }

    println!("Accounts:");
    let mut known = std::collections::HashSet::new();
    for account in &app_config.accounts {
        known.insert(account.name.as_str());
        match account.provider {
            model::ProviderKind::Zai => {
                let has_key = account
                    .api_key
                    .as_ref()
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false)
                    || std::env::var("Z_AI_API_KEY")
                        .map(|k| !k.trim().is_empty())
                        .unwrap_or(false);
                if has_key {
                    let src = if account
                        .api_key
                        .as_ref()
                        .map(|k| !k.trim().is_empty())
                        .unwrap_or(false)
                    {
                        "auth.toml"
                    } else {
                        "env Z_AI_API_KEY"
                    };
                    println!(
                        "  {:<16}  {:<12}  api key ok  ({src})",
                        account.name,
                        provider_label(account.provider),
                    );
                } else {
                    println!(
                        "  {:<16}  {:<12}  no api key  (run: tokenbar login {} --provider zai --api-key …)",
                        account.name,
                        provider_label(account.provider),
                        account.name
                    );
                }
            }
            model::ProviderKind::OpenCodeGo => match sessions.sessions.get(&account.name) {
                Some(entry) if !entry.cookie.trim().is_empty() => {
                    let wid = entry
                        .workspace_id
                        .as_deref()
                        .unwrap_or("(discover on next poll)");
                    let age = chrono::Utc::now().signed_duration_since(entry.updated_at);
                    println!(
                        "  {:<16}  {:<12}  session ok  workspace {}  updated {} ago",
                        account.name,
                        provider_label(account.provider),
                        wid,
                        format_age(age.num_seconds())
                    );
                }
                _ => {
                    println!(
                        "  {:<16}  {:<12}  no session  (run: tokenbar login {})",
                        account.name,
                        provider_label(account.provider),
                        account.name
                    );
                }
            },
            model::ProviderKind::Grok => match sessions.sessions.get(&account.name) {
                Some(entry) if entry.has_grok_session() => {
                    let age = chrono::Utc::now().signed_duration_since(entry.updated_at);
                    let email = entry.email.as_deref().unwrap_or("-");
                    let tok = entry
                        .access_token
                        .as_ref()
                        .map(|t| format!("token {} chars", t.len()))
                        .unwrap_or_else(|| "session".into());
                    println!(
                        "  {:<16}  {:<12}  session ok  {email}  {tok}  updated {} ago",
                        account.name,
                        provider_label(account.provider),
                        format_age(age.num_seconds())
                    );
                }
                _ => {
                    println!(
                        "  {:<16}  {:<12}  no session  (run: tokenbar login {} --provider grok)",
                        account.name,
                        provider_label(account.provider),
                        account.name
                    );
                }
            },
        }
    }

    let orphans: Vec<_> = sessions
        .sessions
        .keys()
        .filter(|name| !known.contains(name.as_str()))
        .collect();
    if !orphans.is_empty() {
        println!();
        println!("Orphan sessions (not in auth.toml):");
        for name in orphans {
            println!("  {name}");
        }
    }

    Ok(())
}

fn run_session_command(
    cmd: SessionCommands,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_path = session::resolve_sessions_path(data_dir);
    let mut sessions = session::load_sessions(&sessions_path)?;

    match cmd {
        SessionCommands::Set {
            name,
            cookie,
            json_file_path,
        } => {
            let cookie_val = if let Some(c) = cookie {
                c
            } else if let Some(path) = json_file_path {
                let contents = std::fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read {path}: {e}"))?;
                contents.trim().to_string()
            } else {
                return Err("Either --cookie or --json-file-path is required".into());
            };

            sessions.sessions.insert(
                name.clone(),
                model::SessionEntry {
                    cookie: cookie_val,
                    workspace_id: None,
                    access_token: None,
                    refresh_token: None,
                    expires_at: None,
                    email: None,
                    user_id: None,
                    updated_at: chrono::Utc::now(),
                },
            );
            session::save_sessions(&sessions_path, &sessions)?;
            info!("Session stored for account '{name}'");
            println!("Session stored for account '{name}'");
        }
        SessionCommands::Rm { name } => {
            if sessions.sessions.remove(&name).is_some() {
                session::save_sessions(&sessions_path, &sessions)?;
                info!("Session removed for account '{name}'");
                println!("Session removed for account '{name}'");
            } else {
                println!("No session found for account '{name}'");
            }
        }
        SessionCommands::Status => {
            print_status(config_path, data_dir)?;
        }
        SessionCommands::Export => {
            if sessions.sessions.is_empty() {
                println!("No sessions stored.");
            } else {
                println!("Session details:");
                for (name, entry) in &sessions.sessions {
                    let age = chrono::Utc::now().signed_duration_since(entry.updated_at);
                    let wid = entry
                        .workspace_id
                        .as_deref()
                        .unwrap_or("(discover on next poll)");
                    println!("  {name}:");
                    if !entry.cookie.is_empty() {
                        println!("    cookie: ({} chars)", entry.cookie.len());
                        println!("    workspace_id: {wid}");
                    }
                    if let Some(tok) = &entry.access_token {
                        println!("    access_token: ({} chars)", tok.len());
                    }
                    if let Some(email) = &entry.email {
                        println!("    email: {email}");
                    }
                    if let Some(exp) = entry.expires_at {
                        println!(
                            "    expires_at: {}",
                            exp.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S")
                        );
                    }
                    println!(
                        "    updated: {} ({} ago)",
                        entry
                            .updated_at
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S"),
                        format_age(age.num_seconds())
                    );
                }
            }
        }
    }

    Ok(())
}

async fn run_tui(
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("Loading config from {}", config_path.display());

    let app_config = config::load_or_create_config(config_path)?;

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

async fn run_serve(
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
    bind_host: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("Loading config from {}", config_path.display());

    let app_config = config::load_or_create_config(config_path)?;
    info!(
        "Loaded {} account(s) from config (web serve)",
        app_config.accounts.len()
    );

    let state = Arc::new(RwLock::new(app::AppState::new(app_config.clone())));
    let (event_tx, event_rx) = mpsc::channel::<app::AppEvent>(64);

    let poller = app::Poller::new(state.clone(), event_rx, &app_config, data_dir);
    let poller_handle = tokio::spawn(async move {
        poller.run().await;
    });

    let bind: std::net::SocketAddr = format!("{bind_host}:{port}")
        .parse()
        .map_err(|e| format!("Invalid bind address {bind_host}:{port}: {e}"))?;

    let serve_result = web::run_server(state, event_tx.clone(), bind).await;

    let _ = event_tx.send(app::AppEvent::Quit).await;
    let _ = poller_handle.await;

    serve_result
}
