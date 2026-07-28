use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

use crate::error::AppError;
use crate::model::{UsageSnapshot, UsageWindow};

const DEFAULT_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

pub struct ZaiProvider {
    client: Client,
    timeout: Duration,
}

impl ZaiProvider {
    pub fn new(client: Client, timeout: Duration) -> Self {
        Self { client, timeout }
    }

    pub async fn fetch_usage(
        &self,
        account_name: &str,
        api_key: &str,
    ) -> Result<UsageSnapshot, AppError> {
        debug!("fetch_usage zai for account {}", account_name);

        let key = api_key.trim();
        if key.is_empty() {
            return Err(AppError::InvalidCredentials);
        }

        let resp = self
            .client
            .get(DEFAULT_QUOTA_URL)
            .header("Authorization", format!("Bearer {key}"))
            .header("Accept", "application/json")
            .timeout(self.timeout)
            .send()
            .await
            .map_err(AppError::Network)?;

        let status = resp.status();
        let body = resp.text().await.map_err(AppError::Network)?;

        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(AppError::InvalidCredentials);
        }
        if !status.is_success() {
            return Err(AppError::Api(format!(
                "z.ai HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        if body.trim().is_empty() {
            return Err(AppError::Parse(
                "Empty z.ai response body. Check API key and region (global api.z.ai).".into(),
            ));
        }

        parse_quota_response(&body, account_name)
    }
}

#[derive(Debug, Deserialize)]
struct QuotaResponse {
    code: i64,
    msg: Option<String>,
    success: bool,
    data: Option<QuotaData>,
}

#[derive(Debug, Deserialize)]
struct QuotaData {
    #[serde(default)]
    limits: Vec<LimitRaw>,
}

#[derive(Debug, Clone, Deserialize)]
struct LimitRaw {
    #[serde(rename = "type")]
    limit_type: String,
    unit: i64,
    number: i64,
    usage: Option<i64>,
    #[serde(rename = "currentValue")]
    current_value: Option<i64>,
    remaining: Option<i64>,
    percentage: Option<f64>,
    #[serde(rename = "nextResetTime")]
    next_reset_time: Option<i64>,
}

#[derive(Debug, Clone)]
struct LimitEntry {
    limit_type: LimitType,
    unit: LimitUnit,
    number: i64,
    usage_percent: f64,
    reset_in_sec: u64,
    window_minutes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitType {
    Tokens,
    Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitUnit {
    Unknown,
    Days,
    Hours,
    Minutes,
    Weeks,
}

fn parse_quota_response(body: &str, account_name: &str) -> Result<UsageSnapshot, AppError> {
    let resp: QuotaResponse = serde_json::from_str(body)
        .map_err(|e| AppError::Parse(format!("z.ai JSON: {e}")))?;

    if !(resp.success && resp.code == 200) {
        let msg = resp
            .msg
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| format!("z.ai quota API returned code {}", resp.code));
        if msg.to_ascii_lowercase().contains("auth")
            || msg.to_ascii_lowercase().contains("token")
            || msg.to_ascii_lowercase().contains("unauthor")
        {
            return Err(AppError::InvalidCredentials);
        }
        return Err(AppError::Api(msg));
    }

    let data = resp
        .data
        .ok_or_else(|| AppError::Parse("z.ai response missing data".into()))?;

    let mut token_limits = Vec::new();
    let mut time_limit: Option<LimitEntry> = None;

    for raw in &data.limits {
        if let Some(entry) = to_limit_entry(raw) {
            match entry.limit_type {
                LimitType::Tokens => token_limits.push(entry),
                LimitType::Time => time_limit = Some(entry),
            }
        }
    }

    // Sort tokens by window ascending (shortest first = 5h session).
    token_limits.sort_by_key(|e| e.window_minutes.unwrap_or(i64::MAX));

    let (rolling_src, weekly_src) = match (token_limits.len(), time_limit) {
        (0, None) => {
            return Err(AppError::Parse(
                "z.ai response has no TOKENS_LIMIT or TIME_LIMIT".into(),
            ));
        }
        (0, Some(time)) => {
            // Only time limit → show as weekly (no monthly).
            (None, Some(time))
        }
        (1, time) => {
            let t = token_limits.into_iter().next().unwrap();
            (Some(t), time)
        }
        (_, time) => {
            // ≥2 token limits: shortest → rolling (5h), longest → weekly.
            // Ignore TIME_LIMIT for weekly when two token windows exist.
            let short = token_limits.first().cloned().unwrap();
            let long = token_limits.last().cloned().unwrap();
            let _ = time;
            (Some(short), Some(long))
        }
    };

    let rolling = match rolling_src {
        Some(e) => to_window(&e, "5h"),
        None => {
            // Only weekly-style limit; put a zero rolling so snapshot stays valid.
            // Prefer showing the time limit as weekly only — use it as rolling if alone.
            let w = weekly_src
                .as_ref()
                .map(|e| to_window(e, "Weekly"))
                .unwrap_or_else(|| UsageWindow::with_label(0.0, 0, "5h"));
            return Ok(UsageSnapshot {
                account_name: account_name.to_string(),
                rolling: w,
                weekly: None,
                monthly: None,
                updated_at: Utc::now(),
                workspace_id: None,
            });
        }
    };

    // Secondary meter is always labeled Weekly (never monthly for ZAI).
    let weekly = weekly_src.map(|e| {
        UsageWindow::with_label(e.usage_percent, e.reset_in_sec, "Weekly")
    });

    Ok(UsageSnapshot {
        account_name: account_name.to_string(),
        rolling,
        weekly,
        monthly: None, // ZAI: never monthly
        updated_at: Utc::now(),
        workspace_id: None,
    })
}

fn to_limit_entry(raw: &LimitRaw) -> Option<LimitEntry> {
    let limit_type = match raw.limit_type.as_str() {
        "TOKENS_LIMIT" => LimitType::Tokens,
        "TIME_LIMIT" => LimitType::Time,
        _ => return None,
    };
    let unit = match raw.unit {
        1 => LimitUnit::Days,
        3 => LimitUnit::Hours,
        5 => LimitUnit::Minutes,
        6 => LimitUnit::Weeks,
        _ => LimitUnit::Unknown,
    };
    let window_minutes = window_minutes(unit, raw.number);
    let usage_percent = used_percent(raw);
    let reset_in_sec = reset_in_sec(raw.next_reset_time);

    Some(LimitEntry {
        limit_type,
        unit,
        number: raw.number,
        usage_percent,
        reset_in_sec,
        window_minutes,
    })
}

fn window_minutes(unit: LimitUnit, number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    Some(match unit {
        LimitUnit::Minutes => number,
        LimitUnit::Hours => number * 60,
        LimitUnit::Days => number * 24 * 60,
        LimitUnit::Weeks => number * 7 * 24 * 60,
        LimitUnit::Unknown => return None,
    })
}

fn used_percent(raw: &LimitRaw) -> f64 {
    if let Some(limit) = raw.usage.filter(|u| *u > 0) {
        let mut used_raw: Option<i64> = None;
        if let Some(remaining) = raw.remaining {
            let from_remaining = limit - remaining;
            used_raw = Some(match raw.current_value {
                Some(cv) => from_remaining.max(cv),
                None => from_remaining,
            });
        } else if let Some(cv) = raw.current_value {
            used_raw = Some(cv);
        }
        if let Some(used) = used_raw {
            let used = used.clamp(0, limit);
            let pct = (used as f64 / limit as f64) * 100.0;
            return pct.clamp(0.0, 100.0);
        }
    }
    raw.percentage.unwrap_or(0.0).clamp(0.0, 100.0)
}

fn reset_in_sec(next_reset_ms: Option<i64>) -> u64 {
    let Some(ms) = next_reset_ms else {
        return 0;
    };
    let now_ms = Utc::now().timestamp_millis();
    if ms <= now_ms {
        return 0;
    }
    ((ms - now_ms) / 1000) as u64
}

fn window_label(entry: &LimitEntry) -> Option<String> {
    if entry.number <= 0 {
        return None;
    }
    match entry.unit {
        LimitUnit::Minutes => Some(format!("{}m", entry.number)),
        LimitUnit::Hours => Some(format!("{}h", entry.number)),
        LimitUnit::Days => Some(format!("{}d", entry.number)),
        LimitUnit::Weeks => Some(format!("{}w", entry.number)),
        LimitUnit::Unknown => None,
    }
}

fn to_window(entry: &LimitEntry, fallback_label: &str) -> UsageWindow {
    let label = window_label(entry).unwrap_or_else(|| fallback_label.to_string());
    // Prefer fixed "5h" for short rolling token windows around 5 hours.
    let label = if entry.limit_type == LimitType::Tokens {
        match entry.window_minutes {
            Some(m) if (4 * 60..6 * 60).contains(&m) => "5h".to_string(),
            _ => label,
        }
    } else {
        label
    };
    UsageWindow::with_label(entry.usage_percent, entry.reset_in_sec, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_two_tokens_and_time() -> String {
        let now_ms = Utc::now().timestamp_millis();
        let reset_5h = now_ms + 3 * 3600 * 1000;
        let reset_week = now_ms + 5 * 86_400 * 1000;
        format!(
            r#"{{
            "code": 200,
            "success": true,
            "msg": "ok",
            "data": {{
                "planName": "Coding Plan",
                "limits": [
                    {{
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 1000,
                        "currentValue": 120,
                        "remaining": 880,
                        "percentage": 12,
                        "nextResetTime": {reset_5h}
                    }},
                    {{
                        "type": "TOKENS_LIMIT",
                        "unit": 6,
                        "number": 1,
                        "usage": 10000,
                        "currentValue": 2600,
                        "remaining": 7400,
                        "percentage": 26,
                        "nextResetTime": {reset_week}
                    }},
                    {{
                        "type": "TIME_LIMIT",
                        "unit": 1,
                        "number": 30,
                        "usage": 100,
                        "currentValue": 5,
                        "remaining": 95,
                        "percentage": 5,
                        "nextResetTime": {reset_week}
                    }}
                ]
            }}
        }}"#
        )
    }

    #[test]
    fn parse_maps_5h_and_weekly_no_monthly() {
        let snap = parse_quota_response(&sample_two_tokens_and_time(), "me").unwrap();
        assert!((snap.rolling.usage_percent - 12.0).abs() < 0.01);
        assert_eq!(snap.rolling.label.as_deref(), Some("5h"));
        let weekly = snap.weekly.expect("weekly");
        assert!((weekly.usage_percent - 26.0).abs() < 0.01);
        assert!(snap.monthly.is_none());
        assert!(snap.rolling.reset_in_sec > 0);
    }

    #[test]
    fn parse_single_token_plus_time() {
        let now_ms = Utc::now().timestamp_millis();
        let body = format!(
            r#"{{
            "code": 200, "success": true,
            "data": {{
                "limits": [
                    {{
                        "type": "TOKENS_LIMIT", "unit": 3, "number": 5,
                        "usage": 100, "remaining": 50, "percentage": 50,
                        "nextResetTime": {now_ms}
                    }},
                    {{
                        "type": "TIME_LIMIT", "unit": 1, "number": 30,
                        "usage": 100, "remaining": 90, "percentage": 10,
                        "nextResetTime": {now_ms}
                    }}
                ]
            }}
        }}"#
        );
        let snap = parse_quota_response(&body, "a").unwrap();
        assert!((snap.rolling.usage_percent - 50.0).abs() < 0.01);
        let weekly = snap.weekly.unwrap();
        assert!((weekly.usage_percent - 10.0).abs() < 0.01);
        assert_eq!(weekly.label.as_deref(), Some("Weekly"));
        assert!(snap.monthly.is_none());
    }

    #[test]
    fn parse_rejects_error_code() {
        let body = r#"{"code":401,"success":false,"msg":"unauthorized","data":null}"#;
        let err = parse_quota_response(body, "a").unwrap_err();
        assert!(matches!(err, AppError::InvalidCredentials | AppError::Api(_)));
    }

    #[test]
    fn used_percent_from_remaining() {
        let raw = LimitRaw {
            limit_type: "TOKENS_LIMIT".into(),
            unit: 3,
            number: 5,
            usage: Some(1000),
            current_value: None,
            remaining: Some(750),
            percentage: Some(99.0),
            next_reset_time: None,
        };
        assert!((used_percent(&raw) - 25.0).abs() < 0.01);
    }
}
