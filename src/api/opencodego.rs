use std::time::Duration;

use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{UsageSnapshot, UsageWindow};

const BASE_URL: &str = "https://opencode.ai";
const SERVER_URL: &str = "https://opencode.ai/_server";
const USAGE_API_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Coarse classification of an OpenCode Go usage page body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// Usage payload present (`rollingUsage` with a `usagePercent`).
    Subscribed,
    /// Rendered console payload without a usage payload — the upsell page.
    NotSubscribed,
    /// Neither marker matched; treat as a parse failure or a client-only shell.
    Unknown,
}

/// Pure classifier for the SSR usage page, in priority order:
///
/// 1. usage payload (`rollingUsage` with a `usagePercent`, same check as the
///    parser) → `Subscribed`
/// 2. rendered console payload without usage (at least two of `customerID:`,
///    `referralCode:`, `liteSubscriptionID:`, and a `wrk_` workspace id) →
///    `NotSubscribed`
/// 3. otherwise → `Unknown`
fn classify_usage_page(text: &str) -> PageKind {
    if parse_subscription_json(text, "", "", Utc::now()).is_some()
        || extract_double(
            r#"rollingUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
            text,
        )
        .is_some()
    {
        return PageKind::Subscribed;
    }

    let console_markers = [
        text.contains("customerID:"),
        text.contains("referralCode:"),
        text.contains("liteSubscriptionID:"),
        text.contains("wrk_"),
    ]
    .iter()
    .filter(|&&found| found)
    .count();

    if console_markers >= 2 {
        return PageKind::NotSubscribed;
    }

    PageKind::Unknown
}

/// The client-only /console SPA shell served when the usage page redirects to
/// login: mount point + asset bundle references, and no SSR payload at all.
fn is_console_client_shell(text: &str) -> bool {
    text.contains(r#"<div id="app">"#)
        && text.contains("/console/assets/")
        && classify_usage_page(text) == PageKind::Unknown
}

/// Live-API verdict for a candidate OpenCode Go API key.
/// Keyed off the HTTP status; the response body is advisory only.
#[derive(Debug)]
pub enum KeyCheck {
    /// 200 — key accepted and subscribed; the usage snapshot it returned.
    Subscribed(UsageSnapshot),
    /// 403 — key is valid but has no Go subscription.
    NotSubscribed,
    /// 401 — key rejected.
    Rejected,
}

pub struct OpenCodeGoProvider {
    client: Client,
    timeout: Duration,
}

impl OpenCodeGoProvider {
    pub fn new(client: Client, timeout: Duration) -> Self {
        Self { client, timeout }
    }

    pub async fn fetch_usage(
        &self,
        account_name: &str,
        cookie: &str,
        workspace_id_override: Option<&str>,
    ) -> Result<UsageSnapshot, AppError> {
        debug!("fetch_usage for account {}", account_name);

        let workspace_id = if let Some(w) = workspace_id_override {
            if is_valid_workspace_id(w) {
                w.to_string()
            } else {
                self.fetch_workspace_id(cookie).await?
            }
        } else {
            self.fetch_workspace_id(cookie).await?
        };

        let page_text = self.fetch_usage_page(&workspace_id, cookie).await?;

        let now = Utc::now();
        let snapshot = parse_subscription(&page_text, account_name, &workspace_id, now)?;

        Ok(snapshot)
    }

    /// Fetch usage through the official JSON API with an API key
    /// (`GET /zen/go/v1/usage`, `Authorization: Bearer <key>`).
    /// No session cookie or workspace id required.
    pub async fn fetch_usage_via_api(
        &self,
        account_name: &str,
        api_key: &str,
    ) -> Result<UsageSnapshot, AppError> {
        debug!("fetch_usage_via_api for account {}", account_name);

        let key = api_key.trim();
        if key.is_empty() {
            return Err(AppError::InvalidCredentials);
        }

        let (status, body) = self.usage_api_get(key).await?;
        if !(200..300).contains(&status) {
            return Err(map_api_status(status, &body));
        }
        parse_usage_body(&body, account_name, Utc::now())
    }

    /// Validate a candidate API key against the live usage endpoint without
    /// persisting anything. 401/403 are verdicts here, not errors.
    pub async fn check_api_key(&self, api_key: &str) -> Result<KeyCheck, AppError> {
        let (status, body) = self.usage_api_get(api_key.trim()).await?;
        match status {
            401 => Ok(KeyCheck::Rejected),
            403 => Ok(KeyCheck::NotSubscribed),
            s if (200..300).contains(&s) => Ok(KeyCheck::Subscribed(parse_usage_body(
                &body,
                "validate",
                Utc::now(),
            )?)),
            s => Err(map_api_status(s, &body)),
        }
    }

    /// Raw GET of the usage API. Returns (status, body); the key is never
    /// logged.
    async fn usage_api_get(&self, api_key: &str) -> Result<(u16, String), AppError> {
        let resp = self
            .client
            .get(USAGE_API_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(AppError::Network)?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(AppError::Network)?;
        Ok((status, body))
    }

    async fn fetch_workspace_id(&self, cookie: &str) -> Result<String, AppError> {
        debug!("Fetching workspace ID");
        let text = self
            .fetch_server_text(WORKSPACES_SERVER_ID, None, "GET", cookie, BASE_URL)
            .await?;

        if looks_signed_out(&text) {
            return Err(AppError::InvalidCredentials);
        }

        let mut ids = parse_workspace_ids(&text);
        if ids.is_empty() {
            ids = parse_workspace_ids_from_json(&text);
        }

        if ids.is_empty() {
            warn!("Workspace IDs empty after GET; retrying with POST");
            let fallback = self
                .fetch_server_text(WORKSPACES_SERVER_ID, Some("[]"), "POST", cookie, BASE_URL)
                .await?;
            if looks_signed_out(&fallback) {
                return Err(AppError::InvalidCredentials);
            }
            ids = parse_workspace_ids(&fallback);
            if ids.is_empty() {
                ids = parse_workspace_ids_from_json(&fallback);
            }
            if ids.is_empty() {
                return Err(AppError::Parse("Missing workspace id.".into()));
            }
            return Ok(ids.swap_remove(0));
        }

        Ok(ids.swap_remove(0))
    }

    async fn fetch_usage_page(&self, workspace_id: &str, cookie: &str) -> Result<String, AppError> {
        let url = format!("{BASE_URL}/workspace/{workspace_id}/go");
        let text = fetch_page_text(&self.client, &url, cookie, self.timeout).await?;

        if looks_signed_out(&text) {
            return Err(AppError::InvalidCredentials);
        }

        match classify_usage_page(&text) {
            PageKind::Subscribed => Ok(text),
            PageKind::NotSubscribed => {
                warn!("Workspace {workspace_id} has no OpenCode Go subscription");
                Err(AppError::NotSubscribed)
            }
            PageKind::Unknown => {
                error!("Usage page payload missing usage fields");
                Err(AppError::Parse("Missing usage fields.".into()))
            }
        }
    }

    async fn fetch_server_text(
        &self,
        server_id: &str,
        args: Option<&str>,
        method: &str,
        cookie: &str,
        referer: &str,
    ) -> Result<String, AppError> {
        let url = server_request_url(server_id, args, method);
        let instance_id = format!("server-fn:{}", Uuid::new_v4());

        let req = if method.to_uppercase() != "GET" {
            let mut r = self
                .client
                .post(&url)
                .header("Cookie", cookie)
                .header("X-Server-Id", server_id)
                .header("X-Server-Instance", &instance_id)
                .header("User-Agent", USER_AGENT)
                .header("Origin", BASE_URL)
                .header("Referer", referer)
                .header(
                    "Accept",
                    "text/javascript, application/json;q=0.9, */*;q=0.8",
                )
                .header("Content-Type", "application/json")
                .timeout(self.timeout);
            if let Some(body) = args {
                r = r.body(body.to_owned());
            }
            r
        } else {
            self.client
                .get(&url)
                .header("Cookie", cookie)
                .header("X-Server-Id", server_id)
                .header("X-Server-Instance", &instance_id)
                .header("User-Agent", USER_AGENT)
                .header("Origin", BASE_URL)
                .header("Referer", referer)
                .header(
                    "Accept",
                    "text/javascript, application/json;q=0.9, */*;q=0.8",
                )
                .timeout(self.timeout)
        };

        let resp = req.send().await?;
        let status = resp.status();
        let body_text = resp.text().await?;

        if status != 200 {
            if looks_signed_out(&body_text) {
                return Err(AppError::InvalidCredentials);
            }
            if status == 401 || status == 403 {
                return Err(AppError::InvalidCredentials);
            }
            if let Some(msg) = extract_server_error_message(&body_text) {
                return Err(AppError::Api(format!("HTTP {status}: {msg}")));
            }
            return Err(AppError::Api(format!("HTTP {status}")));
        }

        Ok(body_text)
    }
}

async fn fetch_page_text(
    client: &Client,
    url: &str,
    cookie: &str,
    timeout: Duration,
) -> Result<String, AppError> {
    let resp = client
        .get(url)
        .header("Cookie", cookie)
        .header("User-Agent", USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .timeout(timeout)
        .send()
        .await?;

    // The client follows redirects; landing on /console/login means the
    // session cookie is expired/invalid, even though the final status is 200.
    let final_path = resp.url().path().to_string();
    let status = resp.status();
    let body_text = resp.text().await?;

    if final_path.starts_with("/console/login") {
        warn!("Usage page redirected to console login ({final_path})");
        return Err(AppError::InvalidCredentials);
    }

    if status != 200 {
        if looks_signed_out(&body_text) {
            return Err(AppError::InvalidCredentials);
        }
        if status == 401 || status == 403 {
            return Err(AppError::InvalidCredentials);
        }
        if let Some(msg) = extract_server_error_message(&body_text) {
            return Err(AppError::Api(format!("HTTP {status}: {msg}")));
        }
        return Err(AppError::Api(format!("HTTP {status}")));
    }

    // Backstop: a 200 body that is the client-only console shell (no SSR
    // payload) also means we were bounced to login without a literal
    // /console/login redirect target.
    if is_console_client_shell(&body_text) {
        warn!("Usage page returned the client-only console shell");
        return Err(AppError::InvalidCredentials);
    }

    Ok(body_text)
}

fn server_request_url(server_id: &str, args: Option<&str>, method: &str) -> String {
    if method.to_uppercase() != "GET" {
        return SERVER_URL.to_string();
    }
    let mut url = format!("{SERVER_URL}?id={server_id}");
    if let Some(a) = args {
        if !a.is_empty() {
            url.push_str("&args=");
            url.push_str(&urlencoding(a));
        }
    }
    url
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("auth/authorize")
        || lower.contains("not associated with an account")
        || lower.contains("actor of type \"public\"")
}

fn extract_server_error_message(text: &str) -> Option<String> {
    if let Ok(val) = serde_json::from_str::<Value>(text) {
        if let Some(obj) = val.as_object() {
            for key in &["message", "error", "detail"] {
                if let Some(v) = obj.get(*key) {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

fn missing_usage_fields() -> AppError {
    AppError::Parse("Missing usage fields.".to_string())
}

/// Parse a 200 body from the official usage API into a snapshot.
///
/// Requires the `usage` object with rolling/weekly/monthly windows, each
/// carrying a numeric `percent` (already 0–100 — never normalized).
/// `resetsAt` becomes a countdown from `now`, clamped at 0. Labels are
/// Rolling/Weekly/Monthly, identical to what the cookie scraper emits.
fn parse_usage_body(
    body: &str,
    account_name: &str,
    now: DateTime<Utc>,
) -> Result<UsageSnapshot, AppError> {
    let val: Value = serde_json::from_str(body).map_err(|_| missing_usage_fields())?;
    let usage = val
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(missing_usage_fields)?;

    let rolling = api_usage_window(usage.get("rolling"), "Rolling", now)?;
    let weekly = api_usage_window(usage.get("weekly"), "Weekly", now)?;
    let monthly = api_usage_window(usage.get("monthly"), "Monthly", now)?;

    Ok(UsageSnapshot {
        account_name: account_name.to_string(),
        rolling,
        weekly: Some(weekly),
        monthly: Some(monthly),
        updated_at: now,
        workspace_id: None,
    })
}

/// One window of the usage API. A missing window or a missing/non-numeric
/// `percent` is the same failure class as the scraper: `Missing usage fields.`
fn api_usage_window(
    val: Option<&Value>,
    label: &str,
    now: DateTime<Utc>,
) -> Result<UsageWindow, AppError> {
    let dict = val
        .and_then(Value::as_object)
        .ok_or_else(missing_usage_fields)?;
    let percent = double_from_value(dict.get("percent")).ok_or_else(missing_usage_fields)?;
    let reset_in_sec = date_from_value(dict.get("resetsAt"))
        .map(|ts| (ts - now.timestamp()).max(0) as u64)
        .unwrap_or(0);
    Ok(UsageWindow::with_label(
        percent.clamp(0.0, 100.0),
        reset_in_sec,
        label,
    ))
}

/// Map a non-2xx usage-API status to an error. Keyed off the status only;
/// the body is advisory (403 bodies are documented, not captured).
fn map_api_status(status: u16, body: &str) -> AppError {
    match status {
        401 => AppError::InvalidCredentials,
        403 => AppError::NotSubscribed,
        _ => match extract_api_error_message(body) {
            Some(msg) => AppError::Api(format!("HTTP {status}: {msg}")),
            None => AppError::Api(format!("HTTP {status}")),
        },
    }
}

/// Error-message extraction for the usage API, in the same style as
/// `extract_server_error_message` but also understanding the nested
/// `{"error": {"message": …}}` envelope the API uses.
fn extract_api_error_message(text: &str) -> Option<String> {
    let val: Value = serde_json::from_str(text).ok()?;
    let obj = val.as_object()?;
    if let Some(error) = obj.get("error") {
        if let Some(msg) = error
            .as_object()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return Some(msg.to_string());
        }
        if let Some(s) = error.as_str() {
            return Some(s.to_string());
        }
    }
    for key in &["message", "detail"] {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn parse_workspace_ids(text: &str) -> Vec<String> {
    let re = match Regex::new(r#"id\s*:\s*"(wrk_[^"]+)""#) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    re.captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

fn parse_workspace_ids_from_json(text: &str) -> Vec<String> {
    let val: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut ids = Vec::new();
    collect_workspace_ids(&val, &mut ids);
    ids
}

fn collect_workspace_ids(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::Object(map) => {
            for v in map.values() {
                collect_workspace_ids(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_workspace_ids(v, out);
            }
        }
        Value::String(s) => {
            if s.starts_with("wrk_") && !out.contains(s) {
                out.push(s.clone());
            }
        }
        _ => {}
    }
}

fn is_valid_workspace_id(id: &str) -> bool {
    let trimmed = id.trim();
    trimmed.starts_with("wrk_") && trimmed.len() > 4
}

fn parse_subscription(
    text: &str,
    account_name: &str,
    workspace_id: &str,
    now: DateTime<Utc>,
) -> Result<UsageSnapshot, AppError> {
    if let Some(snapshot) = parse_subscription_json(text, account_name, workspace_id, now) {
        return Ok(snapshot);
    }

    let rolling_percent = match extract_double(
        r#"rollingUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
        text,
    ) {
        Some(percent) => percent,
        None => match classify_usage_page(text) {
            PageKind::NotSubscribed => return Err(AppError::NotSubscribed),
            PageKind::Subscribed | PageKind::Unknown => {
                return Err(AppError::Parse("Missing usage fields.".into()))
            }
        },
    };
    let rolling_reset = extract_int(r#"rollingUsage[^}]*?resetInSec\s*:\s*([0-9]+)"#, text)
        .ok_or_else(|| AppError::Parse("Missing usage fields.".into()))?;

    let weekly_percent = extract_double(
        r#"weeklyUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
        text,
    );
    let weekly_reset = extract_int(r#"weeklyUsage[^}]*?resetInSec\s*:\s*([0-9]+)"#, text);

    let monthly_percent = extract_double(
        r#"monthlyUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
        text,
    );
    let monthly_reset = extract_int(r#"monthlyUsage[^}]*?resetInSec\s*:\s*([0-9]+)"#, text);

    let weekly = if weekly_percent.is_some() && weekly_reset.is_some() {
        Some(UsageWindow::new(
            normalize_percent(weekly_percent.unwrap()),
            weekly_reset.unwrap() as u64,
        ))
    } else {
        None
    };

    let monthly = if monthly_percent.is_some() || monthly_reset.is_some() {
        Some(UsageWindow::new(
            normalize_percent(monthly_percent.unwrap_or(0.0)),
            monthly_reset.unwrap_or(0) as u64,
        ))
    } else {
        None
    };

    Ok(UsageSnapshot {
        account_name: account_name.to_string(),
        rolling: UsageWindow::new(normalize_percent(rolling_percent), rolling_reset as u64),
        weekly,
        monthly,
        updated_at: now,
        workspace_id: Some(workspace_id.to_string()),
    })
}

fn parse_subscription_json(
    text: &str,
    account_name: &str,
    workspace_id: &str,
    now: DateTime<Utc>,
) -> Option<UsageSnapshot> {
    let val: Value = serde_json::from_str(text).ok()?;
    let obj = val.as_object()?;

    if let Some(snapshot) = parse_usage_dict(obj, account_name, workspace_id, now, None) {
        return Some(snapshot);
    }

    for key in &["data", "result", "usage", "billing", "payload"] {
        if let Some(nested) = obj.get(*key).and_then(|v| v.as_object()) {
            if let Some(snapshot) = parse_usage_dict(nested, account_name, workspace_id, now, None)
            {
                return Some(snapshot);
            }
        }
    }

    parse_usage_nested(obj, account_name, workspace_id, now, 0, None)
}

fn parse_usage_dict(
    dict: &serde_json::Map<String, Value>,
    account_name: &str,
    workspace_id: &str,
    now: DateTime<Utc>,
    inherited_renews_at: Option<i64>,
) -> Option<UsageSnapshot> {
    let renews_at = date_from_value(dict.get("renewAt"))
        .or_else(|| date_from_value(dict.get("renew_at")))
        .or(inherited_renews_at);

    if let Some(usage) = dict.get("usage").and_then(|v| v.as_object()) {
        return parse_usage_dict(usage, account_name, workspace_id, now, renews_at);
    }

    let rolling_keys = [
        "rollingUsage",
        "rolling",
        "rolling_usage",
        "rollingWindow",
        "rolling_window",
    ];
    let weekly_keys = [
        "weeklyUsage",
        "weekly",
        "weekly_usage",
        "weeklyWindow",
        "weekly_window",
    ];
    let monthly_keys = [
        "monthlyUsage",
        "monthly",
        "monthly_usage",
        "monthlyWindow",
        "monthly_window",
    ];

    let rolling = first_dict(dict, &rolling_keys)?;
    let weekly = first_dict(dict, &weekly_keys);
    let monthly = first_dict(dict, &monthly_keys);

    build_snapshot(
        &rolling,
        weekly,
        monthly,
        account_name,
        workspace_id,
        now,
        renews_at,
    )
}

fn first_dict<'a>(
    dict: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    for key in keys {
        if let Some(val) = dict.get(*key).and_then(|v| v.as_object()) {
            return Some(val);
        }
    }
    None
}

fn parse_usage_nested(
    dict: &serde_json::Map<String, Value>,
    account_name: &str,
    workspace_id: &str,
    now: DateTime<Utc>,
    depth: usize,
    inherited_renews_at: Option<i64>,
) -> Option<UsageSnapshot> {
    if depth > 3 {
        return None;
    }
    let renews_at = date_from_value(dict.get("renewAt"))
        .or_else(|| date_from_value(dict.get("renew_at")))
        .or(inherited_renews_at);

    let mut rolling: Option<&serde_json::Map<String, Value>> = None;
    let mut weekly: Option<&serde_json::Map<String, Value>> = None;
    let mut monthly: Option<&serde_json::Map<String, Value>> = None;

    for (key, value) in dict {
        let sub = value.as_object()?;
        let lower = key.to_lowercase();
        if lower.contains("rolling")
            || lower.contains("hour")
            || lower.contains("5h")
            || lower.contains("5-hour")
        {
            rolling = Some(sub);
        } else if lower.contains("weekly") || lower.contains("week") {
            weekly = Some(sub);
        } else if lower.contains("monthly") || lower.contains("month") {
            monthly = Some(sub);
        }
    }

    if let Some(rolling) = rolling {
        if let Some(snapshot) = build_snapshot(
            rolling,
            weekly,
            monthly,
            account_name,
            workspace_id,
            now,
            renews_at,
        ) {
            return Some(snapshot);
        }
    }

    for value in dict.values() {
        if let Some(sub) = value.as_object() {
            if let Some(snapshot) =
                parse_usage_nested(sub, account_name, workspace_id, now, depth + 1, renews_at)
            {
                return Some(snapshot);
            }
        }
    }

    None
}

fn build_snapshot(
    rolling: &serde_json::Map<String, Value>,
    weekly: Option<&serde_json::Map<String, Value>>,
    monthly: Option<&serde_json::Map<String, Value>>,
    account_name: &str,
    workspace_id: &str,
    now: DateTime<Utc>,
    _renews_at: Option<i64>,
) -> Option<UsageSnapshot> {
    let rolling_window = parse_window(rolling, now)?;
    let weekly_window = weekly.and_then(|w| parse_window(w, now));
    let monthly_window = monthly.and_then(|m| parse_window(m, now));

    Some(UsageSnapshot {
        account_name: account_name.to_string(),
        rolling: UsageWindow::new(rolling_window.0, rolling_window.1 as u64),
        weekly: weekly_window.map(|(p, r)| UsageWindow::new(p, r as u64)),
        monthly: monthly_window.map(|(p, r)| UsageWindow::new(p, r as u64)),
        updated_at: now,
        workspace_id: Some(workspace_id.to_string()),
    })
}

fn parse_window(dict: &serde_json::Map<String, Value>, now: DateTime<Utc>) -> Option<(f64, i64)> {
    let percent_keys = [
        "usagePercent",
        "usedPercent",
        "percentUsed",
        "percent",
        "usage_percent",
        "used_percent",
        "utilization",
        "utilizationPercent",
        "utilization_percent",
        "usage",
    ];

    let mut percent: Option<f64> = None;
    for key in &percent_keys {
        if let Some(v) = double_from_value(dict.get(*key)) {
            percent = Some(v);
            break;
        }
    }

    if percent.is_none() {
        let used_keys = ["used", "usage", "consumed", "count", "usedTokens"];
        let limit_keys = ["limit", "total", "quota", "max", "cap", "tokenLimit"];
        let used = used_keys
            .iter()
            .find_map(|k| double_from_value(dict.get(*k)));
        let limit = limit_keys
            .iter()
            .find_map(|k| double_from_value(dict.get(*k)));
        if let (Some(u), Some(l)) = (used, limit) {
            if l > 0.0 {
                percent = Some((u / l) * 100.0);
            }
        }
    }

    let mut percent = percent?;
    // opencode.ai reports usagePercent already on a 0-100 scale (1 = 1%).
    // Do NOT apply a fraction->percent heuristic: a raw value of 1.0 means
    // 1% used, not 100%.
    percent = percent.clamp(0.0, 100.0);

    let reset_keys = [
        "resetInSec",
        "resetInSeconds",
        "resetSeconds",
        "reset_sec",
        "reset_in_sec",
        "resetsInSec",
        "resetsInSeconds",
        "resetIn",
        "resetSec",
    ];

    let mut reset_in_sec: Option<i64> = None;
    for key in &reset_keys {
        if let Some(v) = int_from_value(dict.get(*key)) {
            reset_in_sec = Some(v);
            break;
        }
    }

    if reset_in_sec.is_none() {
        let reset_at_keys = [
            "resetAt",
            "resetsAt",
            "reset_at",
            "resets_at",
            "nextReset",
            "next_reset",
            "renewAt",
            "renew_at",
        ];
        for key in &reset_at_keys {
            if let Some(dt) = date_from_value(dict.get(*key)) {
                let interval = dt - now.timestamp();
                if interval > 0 {
                    reset_in_sec = Some(interval);
                } else {
                    reset_in_sec = Some(0);
                }
                break;
            }
        }
    }

    let resolved = reset_in_sec.map(|v| v.max(0)).unwrap_or(0);
    Some((percent, resolved))
}

fn double_from_value(val: Option<&Value>) -> Option<f64> {
    match val? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn int_from_value(val: Option<&Value>) -> Option<i64> {
    match val? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn date_from_value(val: Option<&Value>) -> Option<i64> {
    match val? {
        Value::Number(n) => {
            let f = n.as_f64()?;
            if f > 1_000_000_000_000.0 {
                Some((f / 1000.0) as i64)
            } else if f > 1_000_000_000.0 {
                Some(f as i64)
            } else {
                None
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if let Ok(n) = trimmed.parse::<f64>() {
                return date_from_value(Some(&Value::Number(
                    serde_json::Number::from_f64(n)
                        .unwrap_or_else(|| serde_json::Number::from_f64(0.0).unwrap()),
                )));
            }
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
                return Some(dt.timestamp());
            }
            None
        }
        _ => None,
    }
}

fn normalize_percent(val: f64) -> f64 {
    // opencode.ai reports usagePercent already on a 0-100 scale (1 = 1%).
    val.clamp(0.0, 100.0)
}

fn extract_double(pattern: &str, text: &str) -> Option<f64> {
    let re = Regex::new(pattern).ok()?;
    let cap = re.captures(text)?;
    cap.get(1)?.as_str().parse::<f64>().ok()
}

fn extract_int(pattern: &str, text: &str) -> Option<i64> {
    let re = Regex::new(pattern).ok()?;
    let cap = re.captures(text)?;
    cap.get(1)?.as_str().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_SUBSCRIBED: &str =
        include_str!("../../tests/fixtures/opencodego/subscribed.html");
    const FIXTURE_NOT_SUBSCRIBED: &str =
        include_str!("../../tests/fixtures/opencodego/not_subscribed.html");
    const FIXTURE_LOGIN_SHELL: &str =
        include_str!("../../tests/fixtures/opencodego/login_shell.html");
    const FIXTURE_API_USAGE_OK: &str =
        include_str!("../../tests/fixtures/opencodego/api_usage_ok.json");
    const FIXTURE_API_USAGE_401: &str =
        include_str!("../../tests/fixtures/opencodego/api_usage_unauthorized_401.json");
    const FIXTURE_API_USAGE_403: &str =
        include_str!("../../tests/fixtures/opencodego/api_usage_entitlement_403.json");

    #[test]
    fn test_classify_subscribed_fixture() {
        assert_eq!(
            classify_usage_page(FIXTURE_SUBSCRIBED),
            PageKind::Subscribed
        );
    }

    #[test]
    fn test_parse_subscribed_fixture_integer_percent() {
        // Regression: usagePercent is integer percent (0.7 means 0.7%).
        let snapshot =
            parse_subscription(FIXTURE_SUBSCRIBED, "acc", "wrk_test", Utc::now()).unwrap();
        assert!((snapshot.rolling.usage_percent - 0.7).abs() < 0.001);
        assert_eq!(snapshot.rolling.reset_in_sec, 13204);
        let weekly = snapshot.weekly.expect("weekly window");
        assert!((weekly.usage_percent - 24.7).abs() < 0.001);
        assert_eq!(weekly.reset_in_sec, 121171);
        let monthly = snapshot.monthly.expect("monthly window");
        assert!((monthly.usage_percent - 36.1).abs() < 0.001);
        assert_eq!(monthly.reset_in_sec, 1479961);
    }

    #[test]
    fn test_classify_not_subscribed_fixture() {
        assert_eq!(
            classify_usage_page(FIXTURE_NOT_SUBSCRIBED),
            PageKind::NotSubscribed
        );
    }

    #[test]
    fn test_parse_not_subscribed_fixture_is_not_subscribed_error() {
        let err = parse_subscription(FIXTURE_NOT_SUBSCRIBED, "acc", "wrk_test", Utc::now())
            .expect_err("not-subscribed page must not parse");
        assert!(matches!(err, AppError::NotSubscribed));
    }

    #[test]
    fn test_classify_login_shell_is_unknown() {
        // The client-only console shell must never be mistaken for a
        // subscribed or not-subscribed SSR page; it is handled as a
        // credentials/shell problem before parsing.
        let kind = classify_usage_page(FIXTURE_LOGIN_SHELL);
        assert_ne!(kind, PageKind::Subscribed);
        assert_ne!(kind, PageKind::NotSubscribed);
        assert_eq!(kind, PageKind::Unknown);
        assert!(is_console_client_shell(FIXTURE_LOGIN_SHELL));
    }

    #[test]
    fn test_parse_synthetic_unknown_body_is_parse_error() {
        let err = parse_subscription(
            "<html><body>hello</body></html>",
            "acc",
            "wrk_test",
            Utc::now(),
        )
        .expect_err("markerless body must not parse");
        assert!(matches!(err, AppError::Parse(ref msg) if msg == "Missing usage fields."));
    }

    #[test]
    fn test_looks_signed_out_login() {
        assert!(looks_signed_out("<title>Login</title>"));
    }

    #[test]
    fn test_looks_signed_out_sign_in() {
        assert!(looks_signed_out("Sign In to continue"));
    }

    #[test]
    fn test_looks_signed_out_clean() {
        assert!(!looks_signed_out("dashboard content here"));
    }

    #[test]
    fn test_normalize_percent_already_normal() {
        assert!((normalize_percent(43.0) - 43.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_percent_low_usage_stays_percent() {
        // Regression: opencode.ai usagePercent is already 0-100 scale.
        // A raw value of 1.0 means 1% used, NOT 100%.
        assert!((normalize_percent(1.0) - 1.0).abs() < 0.001);
        assert!((normalize_percent(0.5) - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_normalize_percent_clamp() {
        assert!((normalize_percent(150.0) - 100.0).abs() < 0.001);
        assert!((normalize_percent(-10.0) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_is_valid_workspace_id() {
        assert!(is_valid_workspace_id("wrk_abc123"));
        assert!(!is_valid_workspace_id(""));
        assert!(!is_valid_workspace_id("abc123"));
    }

    #[test]
    fn test_extract_double_simple() {
        let text = r#"rollingUsage: { usagePercent: 43.5 }"#;
        let val = extract_double(
            r#"rollingUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
            text,
        );
        assert!((val.unwrap() - 43.5).abs() < 0.001);
    }

    #[test]
    fn test_extract_int_simple() {
        let text = r#"rollingUsage: { resetInSec: 3600 }"#;
        let val = extract_int(r#"rollingUsage[^}]*?resetInSec\s*:\s*([0-9]+)"#, text);
        assert_eq!(val.unwrap(), 3600);
    }

    #[test]
    fn test_parse_window_from_json() {
        let mut dict = serde_json::Map::new();
        dict.insert(
            "usagePercent".to_string(),
            Value::Number(serde_json::Number::from_f64(43.5).unwrap()),
        );
        dict.insert(
            "resetInSec".to_string(),
            Value::Number(serde_json::Number::from(9200)),
        );
        let now = Utc::now();
        let (p, r) = parse_window(&dict, now).unwrap();
        assert!((p - 43.5).abs() < 0.001);
        assert_eq!(r, 9200);
    }

    #[test]
    fn test_parse_window_used_limit() {
        let mut dict = serde_json::Map::new();
        dict.insert(
            "used".to_string(),
            Value::Number(serde_json::Number::from_f64(430.0).unwrap()),
        );
        dict.insert(
            "limit".to_string(),
            Value::Number(serde_json::Number::from_f64(1000.0).unwrap()),
        );
        dict.insert(
            "resetInSec".to_string(),
            Value::Number(serde_json::Number::from(3600)),
        );
        let now = Utc::now();
        let (p, r) = parse_window(&dict, now).unwrap();
        assert!((p - 43.0).abs() < 0.001);
        assert_eq!(r, 3600);
    }

    #[test]
    fn test_parse_window_low_percent() {
        // Regression: usagePercent of 1 (1% used) must NOT become 100%.
        let mut dict = serde_json::Map::new();
        dict.insert(
            "usagePercent".to_string(),
            Value::Number(serde_json::Number::from_f64(1.0).unwrap()),
        );
        dict.insert(
            "resetInSec".to_string(),
            Value::Number(serde_json::Number::from(9200)),
        );
        let now = Utc::now();
        let (p, r) = parse_window(&dict, now).unwrap();
        assert!((p - 1.0).abs() < 0.001);
        assert_eq!(r, 9200);
    }

    #[test]
    fn test_extract_server_error_message_json() {
        let text = r#"{"message": "Something went wrong"}"#;
        assert_eq!(
            extract_server_error_message(text),
            Some("Something went wrong".into())
        );
    }

    #[test]
    fn test_parse_workspace_ids_regex() {
        let text = r#"id: "wrk_abc123def456""#;
        let ids = parse_workspace_ids(text);
        assert_eq!(ids, vec!["wrk_abc123def456"]);
    }

    #[test]
    fn test_parse_workspace_ids_from_json() {
        let text = r#"{"workspaces": [{"id": "wrk_abc123"}, {"id": "wrk_def456"}]}"#;
        let ids = parse_workspace_ids_from_json(text);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"wrk_abc123".to_string()));
        assert!(ids.contains(&"wrk_def456".to_string()));
    }

    // ---- official usage API (api key path) ----

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-19T17:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn test_parse_usage_api_ok_fixture() {
        // Expectations derived from the fixture's resetsAt vs the fixed now:
        // rolling  2026-09-19T18:00:33.055Z - 17:00:00Z = 3633s
        // weekly   2026-09-21T00:00:00.055Z - 17:00:00Z = 111600s
        // monthly  2026-10-06T17:26:30.055Z - 17:00:00Z = 1470390s
        let snap = parse_usage_body(FIXTURE_API_USAGE_OK, "go-api", fixed_now()).unwrap();
        assert_eq!(snap.account_name, "go-api");
        assert!((snap.rolling.usage_percent - 2.0).abs() < 0.001);
        assert_eq!(snap.rolling.label.as_deref(), Some("Rolling"));
        assert_eq!(snap.rolling.reset_in_sec, 3633);
        let weekly = snap.weekly.expect("weekly window");
        assert!((weekly.usage_percent - 25.0).abs() < 0.001);
        assert_eq!(weekly.label.as_deref(), Some("Weekly"));
        assert_eq!(weekly.reset_in_sec, 111_600);
        let monthly = snap.monthly.expect("monthly window");
        assert!((monthly.usage_percent - 36.0).abs() < 0.001);
        assert_eq!(monthly.label.as_deref(), Some("Monthly"));
        assert_eq!(monthly.reset_in_sec, 1_470_390);
        assert!(snap.workspace_id.is_none());
    }

    #[test]
    fn test_parse_usage_api_percent_not_normalized() {
        // percent is already 0-100: 2 must stay 2 (not 0.02, not 200).
        let snap = parse_usage_body(FIXTURE_API_USAGE_OK, "a", fixed_now()).unwrap();
        assert!((snap.rolling.usage_percent - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_usage_api_past_resets_clamp_to_zero() {
        let now = DateTime::parse_from_rfc3339("2026-09-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let snap = parse_usage_body(FIXTURE_API_USAGE_OK, "a", now).unwrap();
        assert_eq!(snap.rolling.reset_in_sec, 0);
        assert_eq!(snap.weekly.unwrap().reset_in_sec, 86_400);
    }

    #[test]
    fn test_parse_usage_api_missing_usage_object() {
        for body in [
            "{}",
            r#"{"data": 1}"#,
            "[]",
            "not json at all",
            FIXTURE_API_USAGE_401,
        ] {
            let err = parse_usage_body(body, "a", fixed_now()).unwrap_err();
            assert!(
                matches!(err, AppError::Parse(ref m) if m == "Missing usage fields."),
                "body {body:?} should be Missing usage fields., got {err:?}"
            );
        }
    }

    #[test]
    fn test_parse_usage_api_missing_window_or_bad_percent() {
        let missing_weekly = r#"{"usage":{"rolling":{"percent":2,"resetsAt":"2026-09-19T18:00:33.055Z"},"monthly":{"percent":36,"resetsAt":"2026-10-06T17:26:30.055Z"}}}"#;
        let err = parse_usage_body(missing_weekly, "a", fixed_now()).unwrap_err();
        assert!(matches!(err, AppError::Parse(ref m) if m == "Missing usage fields."));

        let bad_percent = r#"{"usage":{"rolling":{"percent":"high","resetsAt":"2026-09-19T18:00:33.055Z"},"weekly":{"percent":25,"resetsAt":"2026-09-21T00:00:00.055Z"},"monthly":{"percent":36,"resetsAt":"2026-10-06T17:26:30.055Z"}}}"#;
        let err = parse_usage_body(bad_percent, "a", fixed_now()).unwrap_err();
        assert!(matches!(err, AppError::Parse(ref m) if m == "Missing usage fields."));
    }

    #[test]
    fn test_map_api_status_401_is_invalid_credentials() {
        assert!(matches!(
            map_api_status(401, FIXTURE_API_USAGE_401),
            AppError::InvalidCredentials
        ));
    }

    #[test]
    fn test_map_api_status_403_is_not_subscribed() {
        // Keyed off the status; the synthesized body must not matter
        // (also: it has comment lines and is not valid JSON).
        assert!(matches!(
            map_api_status(403, FIXTURE_API_USAGE_403),
            AppError::NotSubscribed
        ));
        assert!(matches!(map_api_status(403, ""), AppError::NotSubscribed));
    }

    #[test]
    fn test_map_api_status_other_is_api_error_with_message() {
        let err = map_api_status(500, FIXTURE_API_USAGE_401);
        assert!(matches!(err, AppError::Api(ref m) if m == "HTTP 500: Unauthorized"));
        let err = map_api_status(502, "<html>bad gateway</html>");
        assert!(matches!(err, AppError::Api(ref m) if m == "HTTP 502"));
    }
}
