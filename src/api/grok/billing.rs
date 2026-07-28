//! Grok Build billing via CLI chat proxy (same as official `x.ai/billing`).

use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

use crate::error::AppError;

const DEFAULT_PROXY_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const TOKEN_AUTH_HEADER: &str = "xai-grok-cli";

#[derive(Debug, Clone, PartialEq)]
pub struct GrokBillingSnapshot {
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub period_label: String,
}

fn proxy_base() -> String {
    std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PROXY_BASE.to_string())
}

pub async fn fetch_credits(
    client: &Client,
    access_token: &str,
    user_id: Option<&str>,
    timeout: Duration,
) -> Result<GrokBillingSnapshot, AppError> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(AppError::InvalidCredentials);
    }

    let url = format!("{}/billing?format=credits", proxy_base());
    let mut req = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("X-XAI-Token-Auth", TOKEN_AUTH_HEADER)
        .header("Accept", "application/json")
        .header("User-Agent", "TokenBar")
        .timeout(timeout);

    if let Some(uid) = user_id.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.header("x-userid", uid);
    }

    let resp = req.send().await.map_err(AppError::Network)?;
    let status = resp.status();
    let body = resp.text().await.map_err(AppError::Network)?;

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(AppError::InvalidCredentials);
    }
    if !status.is_success() {
        return Err(AppError::Api(format!(
            "Grok billing HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }

    parse_billing_json(&body)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfigResponse {
    config: Option<BillingConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    credit_usage_percent: Option<f64>,
    current_period: Option<UsagePeriod>,
    monthly_limit: Option<Cent>,
    used: Option<Cent>,
    billing_period_end: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsagePeriod {
    #[serde(rename = "type")]
    period_type: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Cent {
    #[serde(default)]
    val: i64,
}

pub fn parse_billing_json(body: &str) -> Result<GrokBillingSnapshot, AppError> {
    let resp: BillingConfigResponse = serde_json::from_str(body)
        .map_err(|e| AppError::Parse(format!("Grok billing JSON: {e}")))?;
    let config = resp
        .config
        .ok_or_else(|| AppError::Parse("Grok billing response missing config".into()))?;

    let used_percent = if let Some(p) = config.credit_usage_percent {
        p.clamp(0.0, 100.0)
    } else if let (Some(limit), Some(used)) = (config.monthly_limit, config.used) {
        if limit.val > 0 {
            ((used.val as f64) / (limit.val as f64) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    } else if config.current_period.is_some() || config.billing_period_end.is_some() {
        0.0
    } else {
        return Err(AppError::Parse(
            "Grok billing missing creditUsagePercent and monthlyLimit/used".into(),
        ));
    };

    let end_raw = config
        .current_period
        .as_ref()
        .and_then(|p| p.end.clone())
        .or(config.billing_period_end);

    let resets_at = end_raw.as_deref().and_then(parse_rfc3339);

    let period_label = config
        .current_period
        .as_ref()
        .and_then(|p| p.period_type.as_deref())
        .map(period_type_label)
        .unwrap_or_else(|| "Weekly".to_string());

    debug!(
        "grok billing used_percent={used_percent:.2} label={period_label} resets_at={:?}",
        resets_at
    );

    Ok(GrokBillingSnapshot {
        used_percent,
        resets_at,
        period_label,
    })
}

fn period_type_label(t: &str) -> String {
    let u = t.to_ascii_uppercase();
    if u.contains("WEEK") {
        "Weekly".into()
    } else if u.contains("MONTH") {
        "Monthly".into()
    } else {
        "Weekly".into()
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_credits_config_weekly() {
        let json = r#"{
            "config": {
                "creditUsagePercent": 42.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-06-01T00:00:00Z",
                    "end": "2026-06-08T00:00:00Z"
                }
            }
        }"#;
        let snap = parse_billing_json(json).unwrap();
        assert!((snap.used_percent - 42.5).abs() < 0.01);
        assert_eq!(snap.period_label, "Weekly");
        assert!(snap.resets_at.is_some());
    }

    #[test]
    fn parse_legacy_cents() {
        let json = r#"{
            "config": {
                "monthlyLimit": {"val": 2000},
                "used": {"val": 500},
                "billingPeriodEnd": "2026-07-01T00:00:00Z"
            }
        }"#;
        let snap = parse_billing_json(json).unwrap();
        assert!((snap.used_percent - 25.0).abs() < 0.01);
        assert!(snap.resets_at.is_some());
    }

    #[test]
    fn parse_null_config_errors() {
        assert!(parse_billing_json(r#"{"config":null}"#).is_err());
    }

    #[test]
    fn parse_zero_usage_no_credit_percent() {
        let json = r#"{
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-28T14:22:04.516115+00:00",
                    "end": "2026-08-04T14:22:04.516115+00:00"
                },
                "onDemandCap": {"val": 0},
                "onDemandUsed": {"val": 0},
                "isUnifiedBillingUser": true,
                "prepaidBalance": {"val": 0},
                "topUpMethod": "TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
                "billingPeriodStart": "2026-07-28T14:22:04.516115+00:00",
                "billingPeriodEnd": "2026-08-04T14:22:04.516115+00:00"
            }
        }"#;
        let snap = parse_billing_json(json).unwrap();
        assert!((snap.used_percent - 0.0).abs() < 0.01);
        assert_eq!(snap.period_label, "Weekly");
        assert!(snap.resets_at.is_some());
    }
}
