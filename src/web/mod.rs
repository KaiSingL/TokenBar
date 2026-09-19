//! HTTP usage dashboard for mobile/desktop browsers.
//!
//! Binds loopback by default so Cloudflare Access + Tunnel can front it.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

use crate::account::{mask_secret, CredentialOutcome};
use crate::app::{AppEvent, AppState};
use crate::error::AppError;
use crate::model::{
    Account, AccountStatus, ProviderKind, SessionEntry, UsageSnapshot, UsageWindow,
};
use crate::tui::widgets::format_reset;

const INDEX_HTML: &str = include_str!("static/index.html");
const APP_JS: &str = include_str!("static/app.js");
const ICON_SVG: &[u8] = include_bytes!("static/icon.svg");
const FAVICON_PNG: &[u8] = include_bytes!("static/favicon.png");
const FAVICON_SVG: &[u8] = include_bytes!("static/favicon.svg");

/// Bump when static assets change (sha256 prefix of
/// icon.svg|favicon.png|favicon.svg|app.js).
/// Injected as `?v=` so Cloudflare/mobile treat a deploy as a new cache key.
const ASSET_V: &str = "88817de6fc";

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
        .replace("/app.js\"", &format!("/app.js?v={ASSET_V}\""))
}

#[derive(Clone)]
struct WebState {
    app: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
    /// Data dir (for sessions.json) and auth.toml path, so the credential
    /// endpoint can persist a new key.
    data_dir: PathBuf,
    config_path: PathBuf,
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
    /// Non-secret credential descriptor; `masked` never holds the full value.
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<CredentialDto>,
    /// Optional tappable destination for this state (workspace subscribe page,
    /// console login). Rendered as a real link so it survives a phone viewport
    /// without the URL being broken mid-token inside the note text.
    #[serde(skip_serializing_if = "Option::is_none")]
    action_url: Option<String>,
}

#[derive(Serialize, Clone)]
struct CredentialDto {
    kind: &'static str,
    masked: String,
    /// Where the key comes from when it was resolved from a store, e.g.
    /// `hermes:lks00` / `opencode`. `None` for pasted keys and cookies.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Serialize)]
struct MeterDto {
    label: String,
    usage_percent: f64,
    reset_in_sec: u64,
    reset_label: String,
}

#[derive(Serialize)]
struct KeySourceDto {
    store: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Store bookkeeping (display-only metadata), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    /// The `api_key_source` value to adopt this entry, e.g. `hermes:lks00`.
    source_ref: String,
}

/// Serve the usage dashboard on `bind` (prefer `127.0.0.1`).
pub async fn run_server(
    app: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
    data_dir: PathBuf,
    config_path: PathBuf,
    bind: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = WebState {
        app,
        event_tx,
        data_dir,
        config_path,
    };

    let app_router = Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(status_handler))
        .route("/api/refresh", post(refresh_handler))
        .route("/api/accounts/{name}/credential", post(credential_handler))
        .route("/api/key-sources", get(key_sources_handler))
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
        .route(
            "/app.js",
            get(|| async {
                static_bytes(APP_JS.as_bytes(), "application/javascript; charset=utf-8")
            }),
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
    eprintln!("  POST /api/accounts/<name>/credential  store api key");
    eprintln!("  GET  /api/key-sources  labels in local key stores (no values)");

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
    let sessions =
        crate::session::load_sessions(&crate::session::resolve_sessions_path(&state.data_dir))
            .map(|s| s.sessions)
            .unwrap_or_default();
    let accounts = app
        .accounts
        .iter()
        .zip(app.statuses.iter())
        .map(|(account, status)| {
            let session = sessions.get(&account.name);
            // Resolve the OpenCode Go key (explicit or from a store) so the
            // credential card can show where it comes from. The value itself
            // never leaves credential_dto masked.
            let key = crate::api::resolve_opencode_api_key(account);
            account_dto(account, status, session, key.as_ref())
        })
        .collect();

    Json(StatusResponse {
        last_refresh: app.last_refresh,
        is_refreshing: app.is_refreshing,
        refresh_interval_secs: app.config.refresh_interval_secs,
        accounts,
    })
}

/// GET /api/key-sources — what the local stores actually contain for
/// OpenCode Go, so the dashboard can offer adoption targets: labels only,
/// never values, and env-sourced entries reported as not usable here.
async fn key_sources_handler() -> Json<serde_json::Value> {
    let sources: Vec<KeySourceDto> = crate::keysource::list_available_default()
        .into_iter()
        .map(|e| {
            let source_ref = e.source_ref();
            KeySourceDto {
                store: e.store,
                label: e.label,
                usable: e.usable,
                reason: e.reason,
                status: e.status,
                source_ref,
            }
        })
        .collect();
    Json(serde_json::json!({ "sources": sources }))
}

#[derive(Deserialize)]
struct CredentialRequest {
    kind: String,
    value: String,
}

/// POST /api/accounts/{name}/credential — validate an API key (pasted, or
/// adopted from a local key store via `api_key_source`) against the live
/// opencode.ai usage API and store it when accepted.
///
/// `{"kind":"api_key","value":"sk-…"}` stores the key itself;
/// `{"kind":"key_source","value":"hermes:lks00"}` stores only the reference
/// (resolved and validated first, so a bad label fails before writing).
///
/// Requires the `X-TokenBar: 1` custom header so a cross-site form cannot
/// rewrite credentials (custom headers are not submittable cross-site). The
/// key is never logged and never returned in full.
async fn credential_handler(
    State(state): State<WebState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CredentialRequest>,
) -> Response {
    // CSRF guard.
    if headers.get("x-tokenbar").and_then(|v| v.to_str().ok()) != Some("1") {
        return cred_err(StatusCode::FORBIDDEN, "missing X-TokenBar: 1 header");
    }
    let kind = match req.kind.as_str() {
        "api_key" => CredentialKind::ApiKey,
        "key_source" => CredentialKind::KeySource,
        _ => {
            return cred_err(
                StatusCode::BAD_REQUEST,
                "unsupported credential kind (expected api_key or key_source)",
            )
        }
    };
    let value = req.value.trim().to_string();
    if value.is_empty() {
        return cred_err(StatusCode::BAD_REQUEST, "value must not be empty");
    }

    // Unknown account → 404; non-opencode_go accounts don't use this path.
    let provider = {
        let app = state.app.read().await;
        match app.accounts.iter().find(|a| a.name == name) {
            None => return cred_err(StatusCode::NOT_FOUND, &format!("unknown account '{name}'")),
            Some(account) => account.provider,
        }
    };
    if provider != ProviderKind::OpenCodeGo {
        return cred_err(
            StatusCode::BAD_REQUEST,
            "web credential entry is only supported for opencode_go accounts",
        );
    }

    // For a key source, a bad label / missing store is a bad *value*: 400,
    // surfaced before anything is written. Rejected by the live API (401)
    // also maps to 400 below, exactly like the pasted-key path.
    if kind == CredentialKind::KeySource {
        if let Err(e) = crate::keysource::resolve(&value) {
            return cred_err(StatusCode::BAD_REQUEST, &e.to_string());
        }
    }

    let set_result = match kind {
        CredentialKind::ApiKey => {
            crate::account::set_api_key_credential(
                &state.data_dir,
                &state.config_path,
                &name,
                &value,
            )
            .await
        }
        CredentialKind::KeySource => {
            let client = reqwest::Client::builder().build();
            match client {
                Ok(client) => {
                    crate::account::set_key_source_credential(
                        &state.data_dir,
                        &state.config_path,
                        &name,
                        &value,
                        client,
                        std::time::Duration::from_secs(15),
                    )
                    .await
                }
                Err(e) => Err(AppError::Network(e)),
            }
        }
    };

    match set_result {
        Ok(outcome) => {
            // Surface the new credential to the poller (it re-reads
            // state.accounts on every refresh) and kick a refresh now,
            // through the same event queue /api/refresh uses.
            {
                let mut app = state.app.write().await;
                let (api_key, api_key_source) = match kind {
                    CredentialKind::ApiKey => (Some(value.clone()), None),
                    // Adoption drops the pasted key (config layer does the
                    // same) so the source actually takes effect in-memory.
                    CredentialKind::KeySource => (None, Some(value.clone())),
                };
                for account in app.accounts.iter_mut() {
                    if account.name == name {
                        account.api_key = api_key.clone();
                        account.api_key_source = api_key_source.clone();
                    }
                }
                for account in app.config.accounts.iter_mut() {
                    if account.name == name {
                        account.api_key = api_key.clone();
                        account.api_key_source = api_key_source.clone();
                    }
                }
            }
            let _ = state.event_tx.try_send(AppEvent::Refresh);

            let (subscribed, note) = match outcome {
                CredentialOutcome::Stored { .. } => (
                    true,
                    match kind {
                        CredentialKind::ApiKey => "api key stored — refreshing usage".to_string(),
                        CredentialKind::KeySource => {
                            format!("api key from {value} stored — refreshing usage")
                        }
                    },
                ),
                CredentialOutcome::StoredNotSubscribed => (
                    false,
                    match kind {
                        CredentialKind::ApiKey => {
                            "key stored, but it has no OpenCode Go subscription".to_string()
                        }
                        CredentialKind::KeySource => format!(
                            "key from {value} stored, but it has no OpenCode Go subscription"
                        ),
                    },
                ),
            };
            // Never echo the secret: pasted keys come back masked, a key
            // source has no secret here at all (only its reference).
            let masked = match kind {
                CredentialKind::ApiKey => mask_secret(&value),
                CredentialKind::KeySource => value.clone(),
            };
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "masked": masked,
                    "subscribed": subscribed,
                    "note": note,
                })),
            )
                .into_response()
        }
        Err(AppError::Login(reason)) => cred_err(StatusCode::BAD_REQUEST, &reason),
        Err(AppError::Config(reason)) => cred_err(StatusCode::INTERNAL_SERVER_ERROR, &reason),
        Err(e) => cred_err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CredentialKind {
    ApiKey,
    KeySource,
}

fn cred_err(status: StatusCode, reason: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": reason })),
    )
        .into_response()
}

async fn refresh_handler(State(state): State<WebState>) -> Response {
    match state.event_tx.try_send(AppEvent::Refresh) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "refresh queue full" })),
        )
            .into_response(),
    }
}

fn account_dto(
    account: &Account,
    status: &AccountStatus,
    session: Option<&SessionEntry>,
    key: Option<&crate::keysource::ResolvedKey>,
) -> AccountDto {
    let credential = credential_dto(account, session, key);
    let action_url = action_url_for(account, status);
    match status {
        AccountStatus::NoSession => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "no_session",
            meters: Vec::new(),
            error: Some(no_session_hint(account)),
            failed_at: None,
            credential,
            action_url,
        },
        AccountStatus::NotSubscribed { workspace_id } => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "not_subscribed",
            meters: Vec::new(),
            error: Some(not_subscribed_hint(workspace_id.as_deref())),
            failed_at: None,
            credential,
            action_url,
        },
        AccountStatus::Loading => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "loading",
            meters: Vec::new(),
            error: None,
            failed_at: None,
            credential,
            action_url,
        },
        AccountStatus::Ready(snapshot) => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "ready",
            meters: meters_from_snapshot(snapshot),
            error: None,
            failed_at: None,
            credential,
            action_url,
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
            credential,
            action_url,
        },
        AccountStatus::Error { message, failed_at } => AccountDto {
            name: account.name.clone(),
            provider: account.provider.as_str().into(),
            status: "error",
            meters: Vec::new(),
            error: Some(message.clone()),
            failed_at: Some(*failed_at),
            credential,
            action_url,
        },
    }
}

/// A tappable destination for the states that have one: the workspace's Go page
/// when a subscription is missing (or a no-session opencode account, where the
/// useful next step is signing in and minting a key). Only http(s) URLs that we
/// construct ourselves are ever emitted, so the dashboard can render them as
/// plain links.
fn action_url_for(account: &Account, status: &AccountStatus) -> Option<String> {
    match (account.provider, status) {
        (ProviderKind::OpenCodeGo, AccountStatus::NotSubscribed { workspace_id }) => workspace_id
            .as_deref()
            .filter(|w| w.starts_with("wrk_"))
            .map(|w| format!("https://opencode.ai/workspace/{w}/go")),
        (ProviderKind::OpenCodeGo, AccountStatus::NoSession) => {
            Some("https://opencode.ai/console".to_string())
        }
        _ => None,
    }
}

/// Non-secret credential descriptor for the dashboard: which kind is stored
/// and a masked hint. A key resolved from a store reports `kind: "api_key"`
/// with `source` naming the store (e.g. `hermes:lks00`) so the card shows
/// where the key comes from — still never the value. A configured source
/// that cannot be resolved here (bad label, env-sourced Hermes entry) is
/// reported as `kind: "key_source"` with the raw reference, mirroring the
/// fetcher (which surfaces the broken source instead of falling back to a
/// cookie).
fn credential_dto(
    account: &Account,
    session: Option<&SessionEntry>,
    key: Option<&crate::keysource::ResolvedKey>,
) -> Option<CredentialDto> {
    // An explicitly stored key (any provider) — masked, no store provenance.
    if let Some(k) = account
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return Some(CredentialDto {
            kind: "api_key",
            masked: mask_secret(k),
            source: None,
        });
    }
    if let Some(resolved) = key {
        return Some(CredentialDto {
            kind: "api_key",
            masked: mask_secret(&resolved.key),
            source: resolved.source.clone(),
        });
    }
    if account.provider != ProviderKind::OpenCodeGo {
        return None;
    }
    if let Some(source) = account
        .api_key_source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(CredentialDto {
            kind: "key_source",
            masked: source.to_string(),
            source: Some(source.to_string()),
        });
    }
    if session
        .map(|s| !s.cookie.trim().is_empty())
        .unwrap_or(false)
    {
        return Some(CredentialDto {
            kind: "cookie",
            masked: "cookie (browser session)".into(),
            source: None,
        });
    }
    None
}

fn no_session_hint(account: &Account) -> String {
    match account.provider {
        crate::model::ProviderKind::Zai => {
            format!(
                "No API key — tokenbar account login {} --provider zai --api-key …",
                account.name
            )
        }
        crate::model::ProviderKind::OpenCodeGo => {
            format!("No session — tokenbar account login {}", account.name)
        }
        crate::model::ProviderKind::Grok => {
            format!(
                "No session — tokenbar account login {} --provider grok",
                account.name
            )
        }
    }
}

fn not_subscribed_hint(_workspace_id: Option<&str>) -> String {
    // The URL travels separately as `action_url` so the dashboard can render it
    // as a tappable link instead of breaking a long id mid-token on a phone.
    "No OpenCode Go subscription on this workspace".to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keysource::ResolvedKey;
    use crate::model::Account;

    fn go_account(api_key: Option<&str>) -> Account {
        Account {
            name: "go".into(),
            provider: ProviderKind::OpenCodeGo,
            api_key: api_key.map(str::to_string),
            api_key_source: None,
        }
    }

    fn source_account(source: Option<&str>) -> Account {
        Account {
            name: "go".into(),
            provider: ProviderKind::OpenCodeGo,
            api_key: None,
            api_key_source: source.map(str::to_string),
        }
    }

    fn cookie_session(cookie: &str) -> SessionEntry {
        SessionEntry {
            cookie: cookie.into(),
            workspace_id: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            email: None,
            user_id: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn credential_dto_masks_api_key() {
        let key = "sk-3YLDrouterMIDDLEsunshine9XzQ";
        let dto = credential_dto(&go_account(Some(key)), None, None).expect("credential");
        assert_eq!(dto.kind, "api_key");
        assert!(dto.source.is_none(), "pasted keys have no store source");
        assert_ne!(dto.masked, key, "masked must never equal the key");
        assert!(!dto.masked.contains("routerMIDDLE"));
        assert!(!dto.masked.contains("sunshine"));
    }

    #[test]
    fn credential_dto_store_key_reports_source_and_mask() {
        let resolved = ResolvedKey {
            key: "sk-3YLDrouterMIDDLEsunshine9XzQ".into(),
            source: Some("hermes:lks00".into()),
            store: "hermes",
            label: Some("lks00".into()),
            status: None,
        };
        let dto = credential_dto(&source_account(Some("hermes:lks00")), None, Some(&resolved))
            .expect("credential");
        assert_eq!(dto.kind, "api_key");
        assert_eq!(dto.source.as_deref(), Some("hermes:lks00"));
        assert_eq!(dto.masked, "sk-3YLD…9XzQ");
        // Serialized form carries provenance but never the key itself.
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("hermes:lks00"), "{json}");
        assert!(
            !json.contains("sk-3YLDrouter"),
            "DTO serialization leaked the key: {json}"
        );
    }

    #[test]
    fn credential_dto_unresolvable_source_shows_reference_not_cookie() {
        // Broken source + stored cookie: the row shows the source (what the
        // fetcher will actually try), not a silent cookie fallback.
        let dto = credential_dto(
            &source_account(Some("hermes:ghost")),
            Some(&cookie_session("auth=Fe26.2**sessionsecret")),
            None,
        )
        .expect("credential");
        assert_eq!(dto.kind, "key_source");
        assert_eq!(dto.masked, "hermes:ghost");
        assert!(!dto.masked.contains("sessionsecret"));
    }

    #[test]
    fn credential_dto_cookie_session_has_no_secret() {
        let dto = credential_dto(
            &go_account(None),
            Some(&cookie_session("auth=Fe26.2**secret")),
            None,
        )
        .expect("credential");
        assert_eq!(dto.kind, "cookie");
        assert_eq!(dto.masked, "cookie (browser session)");
        assert!(!dto.masked.contains("secret"));
    }

    #[test]
    fn credential_dto_blank_key_falls_back_to_cookie() {
        let dto = credential_dto(
            &go_account(Some("   ")),
            Some(&cookie_session("auth=x")),
            None,
        )
        .expect("credential");
        assert_eq!(dto.kind, "cookie");
    }

    #[test]
    fn credential_dto_none_when_nothing_stored() {
        assert!(credential_dto(&go_account(None), None, None).is_none());
        assert!(credential_dto(&go_account(None), Some(&cookie_session("")), None).is_none());
        assert!(credential_dto(&go_account(None), Some(&cookie_session("  ")), None).is_none());
    }

    #[test]
    fn account_dto_serialization_contains_no_secrets() {
        // End-to-end over the DTO layer: a source-resolved key plus a cookie
        // session must serialize to /api/status-shaped JSON without ever
        // containing the dummy key or the cookie value.
        let account = source_account(Some("hermes:lks00"));
        let resolved = ResolvedKey {
            key: "sk-test-hermes-lks00-SECRET".into(),
            source: Some("hermes:lks00".into()),
            store: "hermes",
            label: Some("lks00".into()),
            status: Some("429 GoUsageLimitError".into()),
        };
        let dto = account_dto(
            &account,
            &AccountStatus::Ready(UsageSnapshot {
                account_name: "go".into(),
                rolling: crate::model::UsageWindow::with_label(2.0, 3600, "Rolling"),
                weekly: None,
                monthly: None,
                updated_at: Utc::now(),
                workspace_id: None,
            }),
            Some(&cookie_session("auth=Fe26.2**cookiesecret")),
            Some(&resolved),
        );
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("SECRET"), "leaked the key: {json}");
        assert!(!json.contains("cookiesecret"), "leaked the cookie: {json}");
        assert!(json.contains("hermes:lks00"), "missing provenance: {json}");
        // Status metadata stays display-only: it appears nowhere as a meter.
        assert!(!json.contains("\"meters\":[{\"label\":\"429"), "{json}");
    }
}
