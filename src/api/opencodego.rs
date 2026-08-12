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
const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

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

        if parse_subscription_json(&text, "", "", Utc::now()).is_none()
            && extract_double(
                r#"rollingUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
                &text,
            )
            .is_none()
        {
            error!("Usage page payload missing usage fields");
            return Err(AppError::Parse("Missing usage fields.".into()));
        }

        Ok(text)
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

    let rolling_percent = extract_double(
        r#"rollingUsage[^}]*?usagePercent\s*:\s*([0-9]+(?:\.[0-9]+)?)"#,
        text,
    )
    .ok_or_else(|| AppError::Parse("Missing usage fields.".into()))?;
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
}
