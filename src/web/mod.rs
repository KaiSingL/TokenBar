//! HTTP usage dashboard for mobile/desktop browsers.
//!
//! Binds loopback by default so Cloudflare Access + Tunnel can front it.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

use crate::app::{AppEvent, AppState};
use crate::model::{Account, AccountStatus, UsageSnapshot, UsageWindow};
use crate::tui::widgets::format_reset;

const INDEX_HTML: &str = include_str!("static/index.html");
const ICON_SVG: &[u8] = include_bytes!("static/icon.svg");
const FAVICON_PNG: &[u8] = include_bytes!("static/favicon.png");
const FAVICON_SVG: &[u8] = include_bytes!("static/favicon.svg");

/// Bump when static icons change (sha256 prefix of icon.svg|favicon.png|favicon.svg).
/// Injected as `?v=` so Cloudflare/mobile treat a deploy as a new cache key.
const ASSET_V: &str = "96e089cec1";

fn static_bytes(body: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // private: do not let shared CDN edges hold a copy across clients.
            // must-revalidate: after max-age, revalidate with origin (tunnel).
            // URL ?v=ASSET_V is the real bust when bytes change.
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600, must-revalidate"),
            ),
        ],
        body,
    )
        .into_response()
}

/// Inject content-addressed `?v=` on icon URLs so a new deploy is a new cache key.
fn index_html() -> String {
    INDEX_HTML
        .replace("/icon.svg\"", &format!("/icon.svg?v={ASSET_V}\""))
        .replace("/favicon.png\"", &format!("/favicon.png?v={ASSET_V}\""))
        .replace("/favicon.svg\"", &format!("/favicon.svg?v={ASSET_V}\""))
        .replace(
            "/apple-touch-icon.png\"",
            &format!("/apple-touch-icon.png?v={ASSET_V}\""),
        )
}

#[derive(Clone)]
struct WebState {
    app: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
}

#[derive(Serialize)]
struct StatusResponse {
    last_refresh: Option<DateTime<Utc>>,
    is_refreshing: bool,
    refresh_interval_secs: u64,
    accounts: Vec<AccountDto>,
}

#[derive(Serialize)]
struct AccountDto {
    name: String,
    provider: String,
    status: &'static str,
    meters: Vec<MeterDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct MeterDto {
    label: String,
    usage_percent: f64,
    reset_in_sec: u64,
    reset_label: String,
}

/// Serve the usage dashboard on `bind` (prefer `127.0.0.1`).
pub async fn run_server(
    app: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
    bind: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = WebState { app, event_tx };

    let app_router = Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(status_handler))
        .route("/api/refresh", post(refresh_handler))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route(
            "/icon.svg",
            get(|| async { static_bytes(ICON_SVG, "image/svg+xml; charset=utf-8") }),
        )
        .route(
            "/favicon.png",
            get(|| async { static_bytes(FAVICON_PNG, "image/png") }),
        )
        .route(
            "/favicon.ico",
            get(|| async { static_bytes(FAVICON_PNG, "image/png") }),
        )
        .route(
            "/favicon.svg",
            get(|| async { static_bytes(FAVICON_SVG, "image/svg+xml; charset=utf-8") }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async { static_bytes(FAVICON_PNG, "image/png") }),
        )
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state);

    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    info!("TokenBar web dashboard listening on http://{local}");
    eprintln!("TokenBar web dashboard → http://{local}");
    eprintln!("  GET  /            mobile dashboard");
    eprintln!("  GET  /api/status  JSON snapshot");
    eprintln!("  POST /api/refresh force poll");

    axum::serve(listener, app_router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("ctrl_c handler error: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => tracing::warn!("SIGTERM handler error: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("Web server shutting down");
}

async fn index_handler() -> Html<String> {
    Html(index_html())
}

async fn status_handler(State(state): State<WebState>) -> Json<StatusResponse> {
    let app = state.app.read().await;
    let accounts = app
        .accounts
        .iter()
        .zip(app.statuses.iter())
        .map(|(account, status)| account_dto(account, status))
        .collect();

    Json(StatusResponse {
        last_refresh: app.last_refresh,
        is_refreshing: app.is_refreshing,
        refresh_interval_secs: app.config.refresh_interval_secs,
        accounts,
    })
}

async fn refresh_handler(State(state): State<WebState>) -> Response {
    match state.event_tx.try_send(AppEvent::Refresh) {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "refresh queue full" })),
        )
            .into_response(),
    }
}

fn account_dto(account: &Account, status: &AccountStatus) -> AccountDto {
    match status {
        AccountStatus::NoSession => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "no_session",
            meters: Vec::new(),
            error: Some(no_session_hint(account)),
            failed_at: None,
        },
        AccountStatus::Loading => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "loading",
            meters: Vec::new(),
            error: None,
            failed_at: None,
        },
        AccountStatus::Ready(snapshot) => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "ready",
            meters: meters_from_snapshot(snapshot),
            error: None,
            failed_at: None,
        },
        AccountStatus::Stale {
            last,
            error,
            failed_at,
        } => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "stale",
            meters: meters_from_snapshot(last),
            error: Some(error.clone()),
            failed_at: Some(*failed_at),
        },
        AccountStatus::Error { message, failed_at } => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "error",
            meters: Vec::new(),
            error: Some(message.clone()),
            failed_at: Some(*failed_at),
        },
    }
}

fn no_session_hint(account: &Account) -> String {
    match account.provider {
        crate::model::ProviderKind::Zai => {
            format!("No API key — tokenbar login {} --provider zai --api-key …", account.name)
        }
        crate::model::ProviderKind::OpenCodeGo => {
            format!("No session — tokenbar login {}", account.name)
        }
        crate::model::ProviderKind::Grok => {
            format!("No session — tokenbar login {} --provider grok", account.name)
        }
    }
}

fn meters_from_snapshot(snapshot: &UsageSnapshot) -> Vec<MeterDto> {
    let mut out = Vec::with_capacity(3);
    out.push(meter_dto(
        snapshot
            .rolling
            .label
            .clone()
            .unwrap_or_else(|| "Rolling".into()),
        &snapshot.rolling,
    ));
    if let Some(ref w) = snapshot.weekly {
        out.push(meter_dto(
            w.label.clone().unwrap_or_else(|| "Weekly".into()),
            w,
        ));
    }
    if let Some(ref m) = snapshot.monthly {
        out.push(meter_dto(
            m.label.clone().unwrap_or_else(|| "Monthly".into()),
            m,
        ));
    }
    out
}

fn meter_dto(label: String, window: &UsageWindow) -> MeterDto {
    MeterDto {
        label,
        usage_percent: window.usage_percent.clamp(0.0, 100.0),
        reset_in_sec: window.reset_in_sec,
        reset_label: format_reset(window.reset_in_sec),
    }
}
