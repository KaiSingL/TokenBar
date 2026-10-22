use std::fs::OpenOptions;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

mod account;
mod api;
mod app;
mod check;
mod config;
mod error;
mod keysource;
mod login;
mod model;
mod pidfile;
mod session;
mod tui;
mod web;

#[derive(Parser)]
#[command(
    name = "tokenbar",
    about = "TUI monitor for AI subscription plan limits"
)]
struct Cli {
    #[arg(short, long, help = "Override config file path")]
    config: Option<String>,

    #[arg(
        long,
        help = "Override data directory (default: ~/.config/tokenbar or %APPDATA%/tokenbar)"
    )]
    data_dir: Option<String>,

    /// Enable debug logging (poll skips, API traces, etc.)
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Serve mobile-friendly web usage dashboard (loopback by default)
    Serve {
        /// Bind address (use 127.0.0.1 for Cloudflare Tunnel private sites)
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// TCP port
        #[arg(long, short = 'p', default_value_t = 8790)]
        port: u16,
    },
    /// Validate auth.toml (and sessions.json) without starting the server
    Check,
    /// Restart a running `serve` daemon (SIGHUP; launchd KeepAlive respawns it)
    Restart,
    /// Manage accounts: list, login, logout, or remove
    Account {
        #[command(subcommand)]
        action: AccountCommands,
    },
}

#[derive(Subcommand)]
enum AccountCommands {
    /// Show accounts and their session state
    List {
        /// Include session details (email, expiry, token lengths)
        #[arg(long)]
        details: bool,
    },
    /// Add an account and obtain a session (interactive browser flow,
    /// or inject a cookie with --cookie / --json-file-path)
    Login {
        /// Account name (added to auth.toml if missing)
        name: String,
        /// Provider: opencode_go (default), zai, or grok
        #[arg(long, default_value = "opencode_go")]
        provider: String,
        /// API key (z.ai, or opencode_go — validated against the live API).
        /// For z.ai you can also set Z_AI_API_KEY.
        #[arg(long)]
        api_key: Option<String>,
        /// Adopt an OpenCode Go API key from a local store instead of pasting
        /// one: `hermes:<label>` (Hermes credential pool) or `opencode`
        /// (opencode's own auth.json). Validated against the live API; only
        /// the reference is stored, the key is re-read at fetch time.
        #[arg(long)]
        key_source: Option<String>,
        /// Store a manually captured session cookie instead of the browser flow
        #[arg(long)]
        cookie: Option<String>,
        /// Import the cookie value from a file instead of --cookie
        #[arg(long)]
        json_file_path: Option<String>,
        /// Overwrite an existing session
        #[arg(long)]
        force: bool,
    },
    /// Drop the session for an account but keep it in auth.toml
    Logout {
        /// Account name
        name: String,
    },
    /// Remove account(s) from auth.toml and their sessions
    Rm {
        /// Account name(s)
        #[arg(required = true)]
        names: Vec<String>,
    },
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

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if cli.debug {
            EnvFilter::new("warn,tokenbar=debug")
        } else {
            EnvFilter::new("info")
        }
    });

    match cli.command {
        Some(Commands::Check) => {
            init_console_tracing(env_filter);
            match check::run_check(&config_path, &data_dir) {
                Ok(()) => Ok(()),
                Err(_) => std::process::exit(1),
            }
        }
        Some(Commands::Restart) => run_restart(&data_dir).await,
        Some(Commands::Serve { bind, port }) => {
            init_file_tracing(&data_dir, env_filter)?;
            run_serve(&config_path, &data_dir, &bind, port).await
        }
        Some(Commands::Account { action }) => {
            init_console_tracing(env_filter);
            run_account_command(action, &config_path, &data_dir).await?;
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
    p.display_label()
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
    details: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let app_config = config::load_config_or_default(config_path)?;
    let sessions = session::load_sessions(&session::resolve_sessions_path(data_dir))?;

    if app_config.accounts.is_empty() && sessions.sessions.is_empty() {
        println!("No accounts configured.");
        println!("  Add one with: tokenbar account login <name>");
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
                        "  {:<16}  {:<12}  no api key  (run: tokenbar account login {} --provider zai --api-key …)",
                        account.name,
                        provider_label(account.provider),
                        account.name
                    );
                }
            }
            model::ProviderKind::OpenCodeGo => {
                let has_pasted_key = account
                    .api_key
                    .as_ref()
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                let source = account
                    .api_key_source
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if has_pasted_key {
                    println!(
                        "  {:<16}  {:<12}  api key ok  (pasted --api-key)",
                        account.name,
                        provider_label(account.provider),
                    );
                } else if let Some(source) = source {
                    // Reference into an external store; show whether it
                    // resolves right now (plus display-only bookkeeping).
                    match keysource::resolve(source) {
                        Ok(rk) => {
                            let status = rk.status.as_deref().unwrap_or("");
                            let sep = if status.is_empty() { "" } else { " · " };
                            println!(
                                "  {:<16}  {:<12}  api key ok  ({source}{sep}{status})",
                                account.name,
                                provider_label(account.provider),
                            );
                        }
                        Err(e) => {
                            println!(
                                "  {:<16}  {:<12}  key source broken  ({source}: {e})",
                                account.name,
                                provider_label(account.provider),
                            );
                        }
                    }
                } else {
                    match sessions.sessions.get(&account.name) {
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
                                "  {:<16}  {:<12}  no session  (run: tokenbar account login {})",
                                account.name,
                                provider_label(account.provider),
                                account.name
                            );
                        }
                    }
                }
            }
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
                        "  {:<16}  {:<12}  no session  (run: tokenbar account login {} --provider grok)",
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

    if details {
        println!();
        println!("Session details:");
        if sessions.sessions.is_empty() {
            println!("  (none)");
        }
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

    Ok(())
}

async fn run_account_command(
    cmd: AccountCommands,
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        AccountCommands::List { details } => {
            print_status(config_path, data_dir, details)?;
        }
        AccountCommands::Login {
            name,
            provider,
            api_key,
            key_source,
            cookie,
            json_file_path,
            force,
        } => {
            let provider = model::ProviderKind::parse_cli(&provider)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            if api_key.is_some() && key_source.is_some() {
                return Err(
                    "--api-key and --key-source are mutually exclusive (the pasted key would always win)"
                        .into(),
                );
            }
            if let Some(source) = key_source.as_deref() {
                if provider != model::ProviderKind::OpenCodeGo {
                    return Err(format!(
                        "--key-source is only supported for opencode_go accounts (got {})",
                        provider.as_str()
                    )
                    .into());
                }
                store_opencode_go_key_source(&name, source, data_dir, config_path).await?;
                return Ok(());
            }
            let cookie_val = if let Some(c) = cookie {
                Some(c)
            } else if let Some(path) = json_file_path {
                let contents =
                    std::fs::read_to_string(&path).map_err(|e| -> Box<dyn std::error::Error> {
                        format!("Failed to read {path}: {e}").into()
                    })?;
                Some(contents.trim().to_string())
            } else {
                None
            };
            if let Some(cookie) = cookie_val {
                if provider != model::ProviderKind::OpenCodeGo {
                    return Err(format!(
                        "--cookie is only supported for opencode_go accounts (got {})",
                        provider.as_str()
                    )
                    .into());
                }
                account::store_session_cookie(data_dir, config_path, &name, cookie)?;
            } else if provider == model::ProviderKind::OpenCodeGo && api_key.is_some() {
                store_opencode_go_api_key(
                    &name,
                    api_key.as_deref().unwrap_or_default(),
                    data_dir,
                    config_path,
                )
                .await?;
            } else {
                login::run_login_flow(&name, force, provider, api_key, data_dir, config_path)?;
            }
        }
        AccountCommands::Logout { name } => {
            println!(
                "{}",
                account::logout_account(&session::resolve_sessions_path(data_dir), &name)?
            );
        }
        AccountCommands::Rm { names } => {
            for line in account::rm_accounts(
                config_path,
                &session::resolve_sessions_path(data_dir),
                &names,
            )? {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// CLI path for `tokenbar restart`: find the running `serve` daemon via the
/// pidfile, SIGHUP it, and confirm the restart by waiting for the pidfile to
/// be rewritten with a NEW pid (launchd KeepAlive respawns the daemon).
async fn run_restart(data_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(unix))]
    {
        Err("`tokenbar restart` is Unix-only (it restarts via SIGHUP)".into())
    }
    #[cfg(unix)]
    {
        let Some(info) = pidfile::read(data_dir).await? else {
            return Err(format!(
                "no running server found (missing {}); start one with `tokenbar serve`",
                pidfile::path(data_dir).display()
            )
            .into());
        };

        let old = info.pid;
        if !process_alive(old) {
            // launchctl kickstart -k kills with SIGKILL, so the daemon cannot
            // clean its pidfile up; a stale entry means nothing to signal.
            return Err(format!(
                "stale pidfile: pid {old} is not running; no live daemon to restart (start one with `tokenbar serve`)"
            )
            .into());
        }

        if unsafe { libc::kill(old, libc::SIGHUP) } != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("failed to signal pid {old}: {err}").into());
        }

        let old_addr = info.addr.clone();
        eprintln!("signalled pid {old} ({old_addr}), waiting for restart...");

        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some(new) = pidfile::read(data_dir).await? {
                if new.pid != old {
                    println!(
                        "restarted: old pid {old} -> new pid {} ({})",
                        new.pid, new.addr
                    );
                    return Ok(());
                }
            }
        }

        Err(format!(
            "sent SIGHUP to pid {old}, but no new pidfile appeared within 10s. If the server was NOT started under launchd, nothing respawns it (a foreground `tokenbar serve` simply exits). Under launchd check: launchctl print gui/$(id -u)/com.kaising.tokenbar"
        )
        .into())
    }
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // kill(pid, 0) errors: ESRCH = gone, EPERM = alive but owned by someone else.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// CLI path for `account login <name> --provider opencode_go --api-key …`:
/// validate against the live usage API, then store (no webview, no cookie).
async fn store_opencode_go_api_key(
    name: &str,
    api_key: &str,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match account::set_api_key_credential(data_dir, config_path, name, api_key).await {
        Ok(account::CredentialOutcome::Stored { snapshot }) => {
            println!("API key stored for account '{name}' (opencode_go)");
            println!(
                "  Usage: rolling {:.0}% (resets {}), weekly {:.0}%, monthly {:.0}%",
                snapshot.rolling.usage_percent,
                tui::widgets::format_reset(snapshot.rolling.reset_in_sec),
                snapshot
                    .weekly
                    .as_ref()
                    .map(|w| w.usage_percent)
                    .unwrap_or(0.0),
                snapshot
                    .monthly
                    .as_ref()
                    .map(|m| m.usage_percent)
                    .unwrap_or(0.0),
            );
            Ok(())
        }
        Ok(account::CredentialOutcome::StoredNotSubscribed) => {
            println!("API key stored for account '{name}' (opencode_go)");
            println!("  Note: key is valid but has no OpenCode Go subscription (403)");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// CLI path for `account login <name> --provider opencode_go --key-source …`:
/// resolve the source (a bad label fails before anything is written),
/// validate against the live usage API, then persist the reference only.
async fn store_opencode_go_key_source(
    name: &str,
    source: &str,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder().build()?;
    let outcome = account::set_key_source_credential(
        data_dir,
        config_path,
        name,
        source,
        client,
        std::time::Duration::from_secs(15),
    )
    .await;
    match outcome {
        Ok(account::CredentialOutcome::Stored { snapshot }) => {
            println!("API key from {source} stored for account '{name}' (opencode_go)");
            println!("  Key stays in its store; tokenbar re-reads it at fetch time.");
            println!(
                "  Usage: rolling {:.0}% (resets {}), weekly {:.0}%, monthly {:.0}%",
                snapshot.rolling.usage_percent,
                tui::widgets::format_reset(snapshot.rolling.reset_in_sec),
                snapshot
                    .weekly
                    .as_ref()
                    .map(|w| w.usage_percent)
                    .unwrap_or(0.0),
                snapshot
                    .monthly
                    .as_ref()
                    .map(|m| m.usage_percent)
                    .unwrap_or(0.0),
            );
            Ok(())
        }
        Ok(account::CredentialOutcome::StoredNotSubscribed) => {
            println!("API key from {source} stored for account '{name}' (opencode_go)");
            println!("  Key stays in its store; tokenbar re-reads it at fetch time.");
            println!("  Note: key is valid but has no OpenCode Go subscription (403)");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
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

    let addr_label = format!("{bind_host}:{port}");
    match pidfile::write(data_dir, std::process::id() as i32, &addr_label).await {
        Ok(()) => info!("Pidfile written at {}", pidfile::path(data_dir).display()),
        Err(e) => warn!("Could not write serve pidfile: {e}"),
    }

    let serve_result =
        run_server_until_signal(state, event_tx.clone(), data_dir, config_path, bind).await;

    let _ = event_tx.send(app::AppEvent::Quit).await;
    let _ = poller_handle.await;

    let _ = pidfile::remove(data_dir).await;

    serve_result
}

/// Run the web server until completion OR SIGHUP. On SIGHUP (a restart
/// request, e.g. `tokenbar restart`) the future returns Ok(()) so `run_serve`
/// runs its normal clean-shutdown path; under launchd KeepAlive the agent
/// immediately respawns the daemon, which is the actual restart. Without
/// launchd (foreground run) SIGHUP just stops the process; `tokenbar restart`
/// reports that.
#[cfg(unix)]
async fn run_server_until_signal(
    state: Arc<RwLock<app::AppState>>,
    event_tx: mpsc::Sender<app::AppEvent>,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
    bind: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hangup = signal(SignalKind::hangup()).map_err(|e| -> Box<dyn std::error::Error> {
        format!("install SIGHUP handler: {e}").into()
    })?;

    tokio::select! {
        res = web::run_server(
            state,
            event_tx,
            data_dir.to_path_buf(),
            config_path.to_path_buf(),
            bind,
        ) => res,
        _ = hangup.recv() => {
            info!("SIGHUP received, exiting cleanly for restart");
            Ok(())
        }
    }
}

#[cfg(not(unix))]
async fn run_server_until_signal(
    state: Arc<RwLock<app::AppState>>,
    event_tx: mpsc::Sender<app::AppEvent>,
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
    bind: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    web::run_server(
        state,
        event_tx,
        data_dir.to_path_buf(),
        config_path.to_path_buf(),
        bind,
    )
    .await
}
