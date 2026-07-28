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
use crate::model::SessionEntry;
use crate::session;

const LOGIN_URL: &str = "https://opencode.ai/auth";
const COOKIE_POLL_MS: u64 = 750;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
struct CapturedSession {
    cookie: String,
    workspace_id: Option<String>,
}

enum UserEvent {
    PollCookies,
}

pub fn run_login_flow(
    account_name: &str,
    force: bool,
    data_dir: &Path,
    config_path: &Path,
) -> Result<(), AppError> {
    let app_config = config::load_config(config_path)?;
    if !app_config.accounts.iter().any(|a| a.name == account_name) {
        return Err(AppError::Login(format!(
            "Account '{account_name}' not found in auth.toml. Add it first."
        )));
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
    println!("  Log in at opencode.ai — window closes automatically on success.");
    println!();

    let partition_dir = login_partition_dir(data_dir, account_name);
    std::fs::create_dir_all(&partition_dir).map_err(AppError::Io)?;

    let captured = open_login_webview(&partition_dir)?;

    sessions.sessions.insert(
        account_name.to_string(),
        SessionEntry {
            cookie: captured.cookie.clone(),
            workspace_id: captured.workspace_id.clone(),
            updated_at: Utc::now(),
        },
    );
    session::save_sessions(&sessions_path, &sessions)?;

    println!("Login successful!");
    println!("  Account: {account_name}");
    println!("  Cookie: stored ({} chars)", captured.cookie.len());
    if let Some(ref wid) = captured.workspace_id {
        println!("  Workspace: {wid}");
    } else {
        println!("  Workspace: (will discover on next poll)");
    }

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

fn open_login_webview(partition_dir: &Path) -> Result<CapturedSession, AppError> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("TokenBar — Login to OpenCode")
        .with_inner_size(tao::dpi::LogicalSize::new(980.0, 720.0))
        .build(&event_loop)
        .map_err(|e| AppError::Login(format!("Failed to create window: {e}")))?;

    let mut web_context = wry::WebContext::new(Some(partition_dir.to_path_buf()));

    let latest_url = Arc::new(Mutex::new(String::from(LOGIN_URL)));
    let url_for_nav = latest_url.clone();
    let url_for_load = latest_url.clone();
    let proxy_for_load = proxy.clone();

    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(LOGIN_URL)
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

    let webview = builder
        .build(&window)
        .map_err(|e| AppError::Login(format_webview_error(e)))?;

    // Fresh partition data dir already isolates accounts; avoid wiping mid-login.
    let _ = webview.load_url(LOGIN_URL);

    let started = Instant::now();
    let result_slot: Arc<Mutex<Option<Result<CapturedSession, String>>>> =
        Arc::new(Mutex::new(None));
    let result_for_loop = result_slot.clone();
    let webview = Arc::new(webview);
    let webview_poll = webview.clone();
    let latest_url_poll = latest_url.clone();

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

                match try_capture_session(webview_poll.as_ref(), &url) {
                    Ok(Some(session)) => {
                        info!(
                            "Session captured ({} chars)",
                            session.cookie.len()
                        );
                        let _ = webview_poll.evaluate_script(
                            r#"document.title = "TokenBar — Login successful";"#,
                        );
                        if let Ok(mut slot) = result_for_loop.lock() {
                            *slot = Some(Ok(session));
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                    Ok(None) => {}
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
) -> Result<Option<CapturedSession>, String> {
    if looks_like_login_intermediate(current_url) {
        return Ok(None);
    }

    let cookies = webview
        .cookies_for_url("https://opencode.ai/")
        .or_else(|_| webview.cookies())
        .map_err(|e| format!("Failed to read cookies: {e}"))?;

    if cookies.is_empty() || !has_session_cookie(&cookies) {
        return Ok(None);
    }

    // Only capture once we're back on opencode.ai after auth (not mid-OAuth).
    let host_ok = current_url.contains("opencode.ai");
    if !host_ok {
        return Ok(None);
    }

    // Prefer authenticated surfaces; also accept /auth after session cookies exist
    // when redirected to workspace or console home.
    let authenticated_surface = current_url.contains("/workspace/")
        || current_url.contains("/go")
        || (current_url.contains("opencode.ai")
            && !looks_like_auth_page(current_url));

    // If still on /auth but session cookie is set, wait until navigation leaves pure login.
    if current_url.contains("/auth") && !current_url.contains("/workspace/") {
        // Some flows land on /auth while already logged in — require session cookie
        // and that we are not on oauth intermediate. Capture after cookies appear.
        // Delay capture on bare /auth to avoid racing password page.
        if !current_url.ends_with("/auth") && !current_url.contains("/auth?") {
            // deeper auth paths may still be login
            if looks_like_auth_page(current_url) {
                return Ok(None);
            }
        } else {
            // bare /auth: only capture if we also see workspace-ish cookie richness
            // wait for redirect to workspace when possible
            return Ok(None);
        }
    }

    if !authenticated_surface && !current_url.contains("/workspace/") {
        return Ok(None);
    }

    let header = build_cookie_header(&cookies);
    if header.is_empty() {
        return Ok(None);
    }

    let workspace_id = extract_workspace_id(current_url)
        .or_else(|| webview.url().ok().and_then(|u| extract_workspace_id(&u)));

    // Require workspace path OR enough cookie signal after leaving auth
    if workspace_id.is_none() && current_url.contains("/auth") {
        return Ok(None);
    }

    // If no workspace in URL yet but we have session cookies on a non-auth page, capture.
    if workspace_id.is_none() && looks_like_auth_page(current_url) {
        return Ok(None);
    }

    debug!(
        "Captured session cookie ({} chars), workspace={:?}",
        header.len(),
        workspace_id
    );

    Ok(Some(CapturedSession {
        cookie: header,
        workspace_id,
    }))
}

fn has_session_cookie(cookies: &[cookie::Cookie<'static>]) -> bool {
    cookies.iter().any(|c| {
        let name = c.name().to_ascii_lowercase();
        name == "auth"
            || name.contains("session")
            || name.contains("opencode")
            || name.starts_with("sb-")
            || name.contains("token")
    })
}

fn build_cookie_header(cookies: &[cookie::Cookie<'static>]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn looks_like_auth_page(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("/login")
        || lower.contains("sign-in")
        || lower.contains("signin")
        || lower.contains("auth/authorize")
        || lower.contains("accounts.google")
        || lower.contains("github.com/login")
        || lower.contains("github.com/session")
}

fn looks_like_login_intermediate(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("accounts.google")
        || lower.contains("github.com/login")
        || lower.contains("github.com/session")
        || lower.contains("auth/authorize")
        || lower.contains("oauth/authorize")
}

fn extract_workspace_id(url: &str) -> Option<String> {
    let marker = "/workspace/";
    let idx = url.find(marker)?;
    let rest = &url[idx + marker.len()..];
    let id = rest.split(['/', '?', '#']).next().unwrap_or("");
    if id.starts_with("wrk_") && id.len() > 4 {
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
