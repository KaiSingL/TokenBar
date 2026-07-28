use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use tracing::{debug, info, warn};
use wry::{PageLoadEvent, WebViewBuilder};

use crate::config;
use crate::error::AppError;
use crate::model::{ProviderKind, SessionEntry};
use crate::session;

const OPENCODE_LOGIN_URL: &str = "https://opencode.ai/auth";
const COOKIE_POLL_MS: u64 = 750;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// Minimum length for a real OpenCode iron-session `auth` cookie value.
const MIN_AUTH_COOKIE_LEN: usize = 80;
/// Require the same candidate on this many consecutive polls before accepting.
const STABLE_POLLS_REQUIRED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebLoginKind {
    OpenCode,
    Grok,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedSession {
    cookie: String,
    /// OpenCode workspace id; empty for Grok cookie sessions.
    workspace_id: String,
}

enum UserEvent {
    PollCookies,
}

pub fn run_login_flow(
    account_name: &str,
    force: bool,
    provider: ProviderKind,
    api_key: Option<String>,
    data_dir: &Path,
    config_path: &Path,
) -> Result<(), AppError> {
    match provider {
        ProviderKind::Zai => run_zai_login(account_name, api_key, config_path),
        ProviderKind::OpenCodeGo => {
            run_opencode_login(account_name, force, data_dir, config_path)
        }
        ProviderKind::Grok => run_grok_login(account_name, force, data_dir, config_path),
    }
}

fn run_grok_login(
    account_name: &str,
    force: bool,
    data_dir: &Path,
    config_path: &Path,
) -> Result<(), AppError> {
    use crate::api::grok::oauth;

    if config::ensure_account(config_path, account_name, ProviderKind::Grok)? {
        println!(
            "Added account '{account_name}' (grok) to {}",
            config_path.display()
        );
    }

    let sessions_path = session::resolve_sessions_path(data_dir);
    let mut sessions = session::load_sessions(&sessions_path)?;
    if sessions
        .sessions
        .get(account_name)
        .is_some_and(|e| e.has_grok_session())
        && !force
    {
        return Err(AppError::Login(format!(
            "Account '{account_name}' already has a Grok session. Use --force to overwrite."
        )));
    }

    let client_id = oauth::client_id();
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Login(format!("HTTP client: {e}")))?;

    println!("Starting Grok device login (auth.x.ai)...");
    println!("  Account: {account_name}");
    let device = oauth::request_device_code(&http, &client_id)?;
    let verify_url = oauth::browser_url(&device).to_string();

    println!();
    println!("  1. A browser window will open (or visit the URL below).");
    println!("  2. Sign in and approve access.");
    if !device.user_code.is_empty() {
        println!("  3. If asked for a code, enter:  {}", device.user_code);
    }
    println!("  URL: {verify_url}");
    println!();

    let partition_dir = login_partition_dir(data_dir, &format!("grok-{account_name}"));
    if partition_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&partition_dir) {
            warn!(
                "Failed to clear webview partition {}: {e}",
                partition_dir.display()
            );
        }
    }
    std::fs::create_dir_all(&partition_dir).map_err(AppError::Io)?;

    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
    let (tok_tx, tok_rx) = std::sync::mpsc::channel();
    let poll_client_id = client_id.clone();
    let device_code = device.device_code.clone();
    let interval = device.interval_secs;
    let expires_in = device.expires_in;
    std::thread::spawn(move || {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build();
        let result = match http {
            Ok(c) => oauth::poll_device_token(
                &c,
                &poll_client_id,
                &device_code,
                interval,
                expires_in,
                &cancel_rx,
            ),
            Err(e) => Err(AppError::Login(format!("HTTP client: {e}"))),
        };
        let _ = tok_tx.send(result);
    });

    let browse_result =
        open_device_oauth_webview(&partition_dir, account_name, &verify_url, &tok_rx);
    let _ = cancel_tx.send(());

    let tokens = match browse_result {
        Ok(t) => t,
        Err(e) => {
            if let Ok(Ok(t)) = tok_rx.try_recv() {
                t
            } else {
                return Err(e);
            }
        }
    };

    sessions.sessions.insert(
        account_name.to_string(),
        SessionEntry {
            cookie: String::new(),
            workspace_id: None,
            access_token: Some(tokens.access_token.clone()),
            refresh_token: tokens.refresh_token.clone(),
            expires_at: tokens.expires_at,
            email: tokens.email.clone(),
            user_id: tokens.user_id.clone(),
            updated_at: Utc::now(),
        },
    );
    session::save_sessions(&sessions_path, &sessions)?;

    println!("Login successful!");
    println!("  Account: {account_name}");
    if let Some(email) = &tokens.email {
        println!("  Email: {email}");
    }
    if let Some(uid) = &tokens.user_id {
        println!("  User: {uid}");
    }
    if let Some(exp) = tokens.expires_at {
        println!("  Expires: {}", exp.to_rfc3339());
    }
    println!("  Token: stored ({} chars)", tokens.access_token.len());

    Ok(())
}

/// Open device verification URL; return when token poll succeeds.
fn open_device_oauth_webview(
    partition_dir: &Path,
    account_name: &str,
    verify_url: &str,
    tok_rx: &std::sync::mpsc::Receiver<Result<crate::api::grok::oauth::OAuthTokens, AppError>>,
) -> Result<crate::api::grok::oauth::OAuthTokens, AppError> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("TokenBar — Login to Grok (device auth)")
        .with_inner_size(tao::dpi::LogicalSize::new(980.0, 720.0))
        .build(&event_loop)
        .map_err(|e| AppError::Login(format!("Failed to create window: {e}")))?;

    let mut web_context = wry::WebContext::new(Some(partition_dir.to_path_buf()));
    let builder = WebViewBuilder::new_with_web_context(&mut web_context);

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let builder = {
        use wry::WebViewBuilderExtDarwin;
        builder.with_data_store_identifier(login_data_store_id(
            account_name,
            WebLoginKind::Grok,
        ))
    };
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = account_name;

    let webview = builder
        .build(&window)
        .map_err(|e| AppError::Login(format_webview_error(e)))?;
    clear_webview_cookies(&webview);
    let _ = webview.load_url(verify_url);

    let proxy_tick = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(COOKIE_POLL_MS));
        if proxy_tick.send_event(UserEvent::PollCookies).is_err() {
            break;
        }
    });

    let result_slot: Arc<Mutex<Option<Result<crate::api::grok::oauth::OAuthTokens, String>>>> =
        Arc::new(Mutex::new(None));
    let result_for_loop = result_slot.clone();
    let started = Instant::now();

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if result_for_loop
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            *control_flow = ControlFlow::Exit;
            return;
        }

        match event {
            Event::UserEvent(UserEvent::PollCookies) => {
                if started.elapsed() > LOGIN_TIMEOUT {
                    if let Ok(mut slot) = result_for_loop.lock() {
                        *slot = Some(Err("Login timed out after 10 minutes".into()));
                    }
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                match tok_rx.try_recv() {
                    Ok(Ok(tokens)) => {
                        info!("Device OAuth tokens received");
                        let _ = webview.evaluate_script(
                            r#"document.title = "TokenBar — Login successful";"#,
                        );
                        if let Ok(mut slot) = result_for_loop.lock() {
                            *slot = Some(Ok(tokens));
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                    Ok(Err(e)) => {
                        if let Ok(mut slot) = result_for_loop.lock() {
                            *slot = Some(Err(e.to_string()));
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if let Ok(mut slot) = result_for_loop.lock() {
                            *slot = Some(Err("OAuth poller ended".into()));
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Ok(mut slot) = result_for_loop.lock() {
                    if slot.is_none() {
                        *slot = Some(Err("Login cancelled — window closed".into()));
                    }
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    let result = result_slot
        .lock()
        .map_err(|_| AppError::Login("Internal lock poisoned".into()))?
        .take();
    match result {
        Some(Ok(tokens)) => Ok(tokens),
        Some(Err(msg)) => Err(AppError::Login(msg)),
        None => Err(AppError::Login("Login ended unexpectedly".into())),
    }
}

fn run_zai_login(
    account_name: &str,
    api_key: Option<String>,
    config_path: &Path,
) -> Result<(), AppError> {
    let key = api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .or_else(|| {
            std::env::var("Z_AI_API_KEY")
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
        })
        .ok_or_else(|| {
            AppError::Login(
                "z.ai requires an API key. Pass --api-key or set Z_AI_API_KEY.".into(),
            )
        })?;

    let created = config::upsert_zai_account(config_path, account_name, &key)?;
    if created {
        println!(
            "Added account '{account_name}' (zai) to {}",
            config_path.display()
        );
    } else {
        println!("Updated API key for account '{account_name}' (zai)");
    }
    println!("  Key: stored ({} chars)", key.len());
    println!("  Endpoint: https://api.z.ai (personal / global)");
    Ok(())
}

fn run_opencode_login(
    account_name: &str,
    force: bool,
    data_dir: &Path,
    config_path: &Path,
) -> Result<(), AppError> {
    if config::ensure_account(config_path, account_name, ProviderKind::OpenCodeGo)? {
        println!(
            "Added account '{account_name}' (opencode_go) to {}",
            config_path.display()
        );
    }

    let sessions_path = session::resolve_sessions_path(data_dir);
    let mut sessions = session::load_sessions(&sessions_path)?;
    if sessions.sessions.contains_key(account_name) && !force {
        return Err(AppError::Login(format!(
            "Account '{account_name}' already has a session. Use --force to overwrite."
        )));
    }

    println!("Opening browser window for OpenCode console login...");
    println!("  Account: {account_name}");
    println!("  Complete login until you reach a workspace page.");
    println!("  Window closes automatically only after a verified session is captured.");
    println!();

    let partition_dir = login_partition_dir(data_dir, account_name);
    // Always start clean so leftover cookies cannot false-trigger success.
    // On Windows/Linux this wipes the WebView2/WebKitGTK user-data dir.
    // On macOS WKWebView ignores this path — isolation uses data_store_identifier.
    if partition_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&partition_dir) {
            warn!(
                "Failed to clear webview partition {}: {e}",
                partition_dir.display()
            );
        }
    }
    std::fs::create_dir_all(&partition_dir).map_err(AppError::Io)?;

    let captured = open_login_webview(&partition_dir, account_name, WebLoginKind::OpenCode)?;

    sessions.sessions.insert(
        account_name.to_string(),
        SessionEntry {
            cookie: captured.cookie.clone(),
            workspace_id: Some(captured.workspace_id.clone()),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            email: None,
            user_id: None,
            updated_at: Utc::now(),
        },
    );
    session::save_sessions(&sessions_path, &sessions)?;

    println!("Login successful!");
    println!("  Account: {account_name}");
    println!("  Cookie: stored ({} chars)", captured.cookie.len());
    println!("  Workspace: {}", captured.workspace_id);

    Ok(())
}

fn login_partition_dir(data_dir: &Path, account_name: &str) -> PathBuf {
    let safe: String = account_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir.join("webview").join(format!("login-{safe}"))
}

/// Stable 16-byte id for WKWebsiteDataStore (macOS 14+).
/// Different accounts must not share cookies; path-based WebContext is a no-op on WKWebView.
fn login_data_store_id(account_name: &str, kind: WebLoginKind) -> [u8; 16] {
    // Two independent FNV-1a 64-bit streams → 128-bit identifier (stable across runs).
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let kind_tag: &[u8] = match kind {
        WebLoginKind::OpenCode => b"opencode",
        WebLoginKind::Grok => b"grok",
    };
    let mut h0 = FNV_OFFSET;
    let mut h1 = FNV_OFFSET ^ 0x9e3779b97f4a7c15;
    for b in b"tokenbar.login.v1\0"
        .iter()
        .chain(kind_tag.iter())
        .chain(b"\0".iter())
        .chain(account_name.as_bytes())
    {
        h0 ^= u64::from(*b);
        h0 = h0.wrapping_mul(FNV_PRIME);
        h1 ^= u64::from(*b).rotate_left(13);
        h1 = h1.wrapping_mul(FNV_PRIME);
    }
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&h0.to_le_bytes());
    id[8..].copy_from_slice(&h1.to_le_bytes());
    id
}

fn clear_webview_cookies(webview: &wry::WebView) {
    match webview.cookies() {
        Ok(cookies) => {
            for c in cookies {
                if let Err(e) = webview.delete_cookie(&c) {
                    debug!("failed to delete cookie {}: {e}", c.name());
                }
            }
        }
        Err(e) => warn!("failed to list cookies for wipe: {e}"),
    }
}

fn open_login_webview(
    partition_dir: &Path,
    account_name: &str,
    kind: WebLoginKind,
) -> Result<CapturedSession, AppError> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (title, start_url) = match kind {
        WebLoginKind::OpenCode => ("TokenBar — Login to OpenCode", OPENCODE_LOGIN_URL),
        WebLoginKind::Grok => {
            // Grok uses open_oauth_webview; this arm is unused.
            ("TokenBar — Login to Grok", OPENCODE_LOGIN_URL)
        }
    };

    let window = WindowBuilder::new()
        .with_title(title)
        .with_inner_size(tao::dpi::LogicalSize::new(980.0, 720.0))
        .build(&event_loop)
        .map_err(|e| AppError::Login(format!("Failed to create window: {e}")))?;

    let mut web_context = wry::WebContext::new(Some(partition_dir.to_path_buf()));

    let latest_url = Arc::new(Mutex::new(String::from(start_url)));
    let url_for_nav = latest_url.clone();
    let url_for_load = latest_url.clone();
    let proxy_for_load = proxy.clone();

    // Do not navigate until cookies are wiped (avoids auto-login on first paint).
    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_navigation_handler(move |url| {
            if let Ok(mut guard) = url_for_nav.lock() {
                *guard = url;
            }
            true
        })
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, PageLoadEvent::Finished) {
                if let Ok(mut guard) = url_for_load.lock() {
                    *guard = url;
                }
                let _ = proxy_for_load.send_event(UserEvent::PollCookies);
            }
        });

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let builder = {
        use wry::WebViewBuilderExtDarwin;
        let store_id = login_data_store_id(account_name, kind);
        debug!(
            "macOS login data_store_identifier={:02x?}",
            store_id
        );
        builder.with_data_store_identifier(store_id)
    };

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = account_name;

    let webview = builder
        .build(&window)
        .map_err(|e| AppError::Login(format_webview_error(e)))?;

    // Fresh login surface: drop any leftover cookies in this partition/store.
    clear_webview_cookies(&webview);
    let _ = webview.load_url(start_url);

    let started = Instant::now();
    let result_slot: Arc<Mutex<Option<Result<CapturedSession, String>>>> =
        Arc::new(Mutex::new(None));
    let result_for_loop = result_slot.clone();
    let webview = Arc::new(webview);
    let webview_poll = webview.clone();
    let latest_url_poll = latest_url.clone();

    // Candidate must appear STABLE_POLLS_REQUIRED times in a row.
    let pending: Arc<Mutex<Option<(CapturedSession, u8)>>> = Arc::new(Mutex::new(None));
    let pending_poll = pending.clone();

    let proxy_tick = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(COOKIE_POLL_MS));
        if proxy_tick.send_event(UserEvent::PollCookies).is_err() {
            break;
        }
    });

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if result_for_loop
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            *control_flow = ControlFlow::Exit;
            return;
        }

        match event {
            Event::UserEvent(UserEvent::PollCookies) => {
                if started.elapsed() > LOGIN_TIMEOUT {
                    if let Ok(mut slot) = result_for_loop.lock() {
                        *slot = Some(Err("Login timed out after 10 minutes".into()));
                    }
                    *control_flow = ControlFlow::Exit;
                    return;
                }

                let url = latest_url_poll
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();

                match try_capture_session(webview_poll.as_ref(), &url, kind) {
                    Ok(Some(session)) => {
                        let ready = match pending_poll.lock() {
                            Ok(mut pending) => match pending.as_mut() {
                                Some((prev, count)) if prev == &session => {
                                    *count = count.saturating_add(1);
                                    *count >= STABLE_POLLS_REQUIRED
                                }
                                _ => {
                                    *pending = Some((session.clone(), 1));
                                    false
                                }
                            },
                            Err(_) => false,
                        };

                        if ready {
                            info!(
                                "Session captured ({} chars), workspace={}",
                                session.cookie.len(),
                                session.workspace_id
                            );
                            let _ = webview_poll.evaluate_script(
                                r#"document.title = "TokenBar — Login successful";"#,
                            );
                            if let Ok(mut slot) = result_for_loop.lock() {
                                *slot = Some(Ok(session));
                            }
                            *control_flow = ControlFlow::Exit;
                        } else {
                            debug!(
                                "Candidate session seen (workspace={}); waiting for stability",
                                session.workspace_id
                            );
                        }
                    }
                    Ok(None) => {
                        if let Ok(mut pending) = pending_poll.lock() {
                            *pending = None;
                        }
                    }
                    Err(e) => {
                        warn!("cookie poll error: {e}");
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Ok(mut slot) = result_for_loop.lock() {
                    if slot.is_none() {
                        *slot = Some(Err("Login cancelled — window closed".into()));
                    }
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    let result = result_slot
        .lock()
        .map_err(|_| AppError::Login("Internal lock poisoned".into()))?
        .take();

    match result {
        Some(Ok(session)) => Ok(session),
        Some(Err(msg)) => Err(AppError::Login(msg)),
        None => Err(AppError::Login("Login ended unexpectedly".into())),
    }
}

fn try_capture_session(
    webview: &wry::WebView,
    current_url: &str,
    kind: WebLoginKind,
) -> Result<Option<CapturedSession>, String> {
    let cookie_url = match kind {
        WebLoginKind::OpenCode => "https://opencode.ai/",
        WebLoginKind::Grok => "https://grok.com/",
    };
    let cookies = webview
        .cookies_for_url(cookie_url)
        .or_else(|_| webview.cookies())
        .map_err(|e| format!("Failed to read cookies: {e}"))?;

    let cookie_pairs: Vec<(String, String)> = cookies
        .iter()
        .map(|c| (c.name().to_string(), c.value().to_string()))
        .collect();

    let webview_url = webview.url().ok();
    let result = match kind {
        WebLoginKind::OpenCode => {
            evaluate_opencode_capture(current_url, webview_url.as_deref(), &cookie_pairs)
        }
        WebLoginKind::Grok => evaluate_grok_capture(current_url, webview_url.as_deref(), &cookie_pairs),
    };

    if result.is_none() {
        debug!(
            "poll skip url={} cookies=[{}]",
            current_url,
            cookie_names_csv(&cookie_pairs)
        );
    }

    Ok(result)
}

/// Pure capture decision — unit-tested without a webview.
fn evaluate_opencode_capture(
    current_url: &str,
    webview_url: Option<&str>,
    cookies: &[(String, String)],
) -> Option<CapturedSession> {
    if looks_like_login_intermediate(current_url) {
        return None;
    }
    if is_auth_flow_url(current_url) {
        return None;
    }
    if !current_url.contains("opencode.ai") {
        return None;
    }

    let workspace_id = extract_workspace_id(current_url)
        .or_else(|| webview_url.and_then(extract_workspace_id))?;

    let auth_value = find_auth_cookie_value(cookies)?;
    if !is_valid_auth_cookie_value(auth_value) {
        return None;
    }

    let header = build_cookie_header(cookies);
    if header.is_empty() {
        return None;
    }

    Some(CapturedSession {
        cookie: header,
        workspace_id,
    })
}

/// Grok session: on grok.com with `sso` or `sso-rw` cookie (CodexBar convention).
fn evaluate_grok_capture(
    current_url: &str,
    webview_url: Option<&str>,
    cookies: &[(String, String)],
) -> Option<CapturedSession> {
    if looks_like_login_intermediate(current_url) {
        return None;
    }
    let on_grok = current_url.contains("grok.com")
        || webview_url.is_some_and(|u| u.contains("grok.com"));
    if !on_grok {
        return None;
    }
    // Still on xAI auth host only — wait for return to grok.com.
    if current_url.contains("accounts.x.ai") || current_url.contains("auth.x.ai") {
        return None;
    }

    let has_sso = cookies.iter().any(|(n, v)| {
        let name = n.to_ascii_lowercase();
        (name == "sso" || name == "sso-rw") && !v.trim().is_empty()
    });
    if !has_sso {
        return None;
    }

    let header = build_cookie_header(cookies);
    if header.is_empty() {
        return None;
    }

    Some(CapturedSession {
        cookie: header,
        workspace_id: String::new(),
    })
}

fn find_auth_cookie_value(cookies: &[(String, String)]) -> Option<&str> {
    cookies
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("auth"))
        .map(|(_, v)| v.as_str())
}

fn is_valid_auth_cookie_value(value: &str) -> bool {
    let v = value.trim();
    if v.len() < MIN_AUTH_COOKIE_LEN {
        return false;
    }
    // OpenCode uses iron-session sealed cookies (Fe26.2**…)
    v.starts_with("Fe26.") || v.len() >= 120
}

fn build_cookie_header(cookies: &[(String, String)]) -> String {
    cookies
        .iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn cookie_names_csv(cookies: &[(String, String)]) -> String {
    cookies
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Any URL still in the login/auth flow — never capture here.
fn is_auth_flow_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    // Path segment /auth (not e.g. "author")
    if lower.contains("/auth/")
        || lower.contains("/auth?")
        || lower.ends_with("/auth")
        || lower.contains("opencode.ai/auth")
    {
        // Allow only if somehow also on workspace (shouldn't happen)
        if extract_workspace_id(url).is_some() {
            return false;
        }
        return true;
    }
    lower.contains("/login")
        || lower.contains("sign-in")
        || lower.contains("signin")
        || lower.contains("sign_in")
}

fn looks_like_login_intermediate(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("accounts.google")
        || lower.contains("github.com/login")
        || lower.contains("github.com/session")
        || lower.contains("auth/authorize")
        || lower.contains("oauth/authorize")
        || lower.contains("oauth2/")
}

fn extract_workspace_id(url: &str) -> Option<String> {
    let marker = "/workspace/";
    let idx = url.find(marker)?;
    let rest = &url[idx + marker.len()..];
    let id = rest.split(['/', '?', '#']).next().unwrap_or("");
    if id.starts_with("wrk_") && id.len() > 8 {
        Some(id.to_string())
    } else {
        None
    }
}

fn format_webview_error(err: wry::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("webview2") || lower.contains("edge") {
        format!(
            "{msg}\n\nWebView2 runtime may be missing. Install:\n  https://developer.microsoft.com/microsoft-edge/webview2/"
        )
    } else {
        format!("Failed to create webview: {msg}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_auth() -> String {
        format!("Fe26.2**{}", "a".repeat(200))
    }

    fn cookies_with_auth(auth: &str) -> Vec<(String, String)> {
        vec![
            ("auth".into(), auth.into()),
            ("_ga".into(), "GA1.1.x".into()),
        ]
    }

    #[test]
    fn reject_auth_landing_page() {
        let c = cookies_with_auth(&long_auth());
        assert!(evaluate_opencode_capture("https://opencode.ai/auth", None, &c).is_none());
        assert!(evaluate_opencode_capture("https://opencode.ai/auth?", None, &c).is_none());
        assert!(evaluate_opencode_capture("https://opencode.ai/auth/callback", None, &c).is_none());
        assert!(evaluate_opencode_capture("https://opencode.ai/auth/cli", None, &c).is_none());
    }

    #[test]
    fn reject_oauth_intermediates() {
        let c = cookies_with_auth(&long_auth());
        assert!(evaluate_opencode_capture(
            "https://accounts.google.com/o/oauth2/auth",
            None,
            &c
        )
        .is_none());
        assert!(evaluate_opencode_capture(
            "https://github.com/login/oauth/authorize",
            None,
            &c
        )
        .is_none());
    }

    #[test]
    fn reject_homepage_without_workspace() {
        let c = cookies_with_auth(&long_auth());
        assert!(evaluate_opencode_capture("https://opencode.ai/", None, &c).is_none());
        assert!(evaluate_opencode_capture("https://opencode.ai/go", None, &c).is_none());
    }

    #[test]
    fn reject_short_or_missing_auth_cookie() {
        let url = "https://opencode.ai/workspace/wrk_01ABC123XYZ/go";
        assert!(evaluate_opencode_capture(url, None, &[]).is_none());
        assert!(evaluate_opencode_capture(
            url,
            None,
            &[("auth".into(), "short".into())]
        )
        .is_none());
        assert!(evaluate_opencode_capture(
            url,
            None,
            &[("session".into(), long_auth())]
        )
        .is_none());
    }

    #[test]
    fn accept_workspace_with_valid_auth() {
        let auth = long_auth();
        let c = cookies_with_auth(&auth);
        let url = "https://opencode.ai/workspace/wrk_01KE4QRVQMJPHQVJTNJZFJ76G7/go";
        let captured = evaluate_opencode_capture(url, None, &c).expect("should capture");
        assert_eq!(
            captured.workspace_id,
            "wrk_01KE4QRVQMJPHQVJTNJZFJ76G7"
        );
        assert!(captured.cookie.contains("auth=Fe26.2**"));
    }

    #[test]
    fn accept_workspace_id_from_webview_url_fallback() {
        let auth = long_auth();
        let c = cookies_with_auth(&auth);
        // Navigation handler lag: current_url still intermediate host check passes
        // only if current is on opencode and not auth — use workspace-less opencode
        // with webview_url holding workspace.
        // Actually evaluate requires workspace from current OR webview.
        // current must not be auth flow. Homepage + webview workspace works.
        let captured = evaluate_opencode_capture(
            "https://opencode.ai/",
            Some("https://opencode.ai/workspace/wrk_01ABCDEFGH1234567890/go"),
            &c,
        )
        .expect("should capture via webview url");
        assert_eq!(captured.workspace_id, "wrk_01ABCDEFGH1234567890");
    }

    #[test]
    fn extract_workspace_id_variants() {
        assert_eq!(
            extract_workspace_id("https://opencode.ai/workspace/wrk_01ABC/go"),
            Some("wrk_01ABC".into())
        );
        assert_eq!(
            extract_workspace_id("https://opencode.ai/workspace/wrk_01ABC?x=1"),
            Some("wrk_01ABC".into())
        );
        assert_eq!(
            extract_workspace_id("https://opencode.ai/workspace/notvalid"),
            None
        );
        assert_eq!(extract_workspace_id("https://opencode.ai/auth"), None);
    }

    #[test]
    fn is_valid_auth_cookie_value_rules() {
        assert!(!is_valid_auth_cookie_value("short"));
        assert!(!is_valid_auth_cookie_value(&"x".repeat(100))); // long but not Fe26 and < 120
        assert!(is_valid_auth_cookie_value(&long_auth()));
        assert!(is_valid_auth_cookie_value(&"z".repeat(120)));
    }

    #[test]
    fn login_data_store_id_stable_and_unique() {
        let a = login_data_store_id("Personal", WebLoginKind::OpenCode);
        let b = login_data_store_id("Personal", WebLoginKind::OpenCode);
        let c = login_data_store_id("Work", WebLoginKind::OpenCode);
        let d = login_data_store_id("Personal", WebLoginKind::Grok);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(a, [0u8; 16]);
    }

    #[test]
    fn grok_capture_requires_sso_on_grok_com() {
        let cookies = vec![
            ("sso".into(), "abc".into()),
            ("other".into(), "x".into()),
        ];
        assert!(evaluate_grok_capture(
            "https://accounts.x.ai/sign-in",
            None,
            &cookies
        )
        .is_none());
        assert!(evaluate_grok_capture("https://grok.com/", None, &[]).is_none());
        let captured =
            evaluate_grok_capture("https://grok.com/?_s=usage", None, &cookies).expect("ok");
        assert!(captured.cookie.contains("sso=abc"));
        assert!(captured.workspace_id.is_empty());
    }

    #[test]
    fn grok_capture_accepts_sso_rw() {
        let cookies = vec![("sso-rw".into(), "tok".into())];
        let captured =
            evaluate_grok_capture("https://grok.com/chat", None, &cookies).expect("ok");
        assert!(captured.cookie.contains("sso-rw=tok"));
    }
}
