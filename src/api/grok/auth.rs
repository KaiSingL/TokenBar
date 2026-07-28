//! Optional parsers for `~/.grok/auth.json` (legacy / tests). Login uses the browser.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::AppError;

const OIDC_SCOPE_PREFIX: &str = "https://auth.x.ai::";
const LEGACY_SESSION_SCOPE: &str = "https://accounts.x.ai/sign-in";

/// Credentials from `~/.grok/auth.json` (written by `grok login`).
#[derive(Debug, Clone)]
pub struct GrokCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    pub auth_mode: Option<String>,
    pub team_id: Option<String>,
    pub scope: String,
}

impl GrokCredentials {
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| Utc::now() >= exp)
            .unwrap_or(false)
    }

    pub fn login_method(&self) -> Option<String> {
        match self.auth_mode.as_deref().map(|s| s.to_ascii_lowercase()) {
            Some(ref m) if m == "oidc" => Some("SuperGrok".into()),
            Some(ref m) if m == "session" => Some("session".into()),
            Some(m) => Some(m),
            None => None,
        }
    }
}

pub fn grok_home() -> PathBuf {
    if let Ok(custom) = std::env::var("GROK_HOME") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
}

pub fn auth_file_path() -> PathBuf {
    grok_home().join("auth.json")
}

pub fn load_credentials() -> Result<GrokCredentials, AppError> {
    load_credentials_from_path(&auth_file_path())
}

pub fn load_credentials_from_path(path: &Path) -> Result<GrokCredentials, AppError> {
    if !path.exists() {
        return Err(AppError::Login(format!(
            "Grok auth.json not found at {}. Run `grok login` first.",
            path.display()
        )));
    }
    let data = std::fs::read(path).map_err(AppError::Io)?;
    parse_auth_json(&data)
}

pub fn parse_auth_json(data: &[u8]) -> Result<GrokCredentials, AppError> {
    let root: Value = serde_json::from_slice(data)
        .map_err(|e| AppError::Login(format!("Failed to decode Grok auth.json: {e}")))?;
    let obj = root.as_object().ok_or_else(|| {
        AppError::Login("Invalid Grok auth.json (expected object at root)".into())
    })?;

    let (scope, entry) = select_preferred_entry(obj).ok_or_else(|| {
        AppError::Login(
            "Grok auth.json exists but contains no usable access tokens. Run `grok login`."
                .into(),
        )
    })?;

    let key = entry
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Login("Grok auth.json entry missing non-empty `key`.".into())
        })?;

    Ok(GrokCredentials {
        access_token: key.to_string(),
        refresh_token: entry
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        expires_at: entry.get("expires_at").and_then(parse_json_date),
        email: entry
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        auth_mode: entry
            .get("auth_mode")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        team_id: entry
            .get("team_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        scope,
    })
}

fn select_preferred_entry(
    root: &serde_json::Map<String, Value>,
) -> Option<(String, &serde_json::Map<String, Value>)> {
    let mut oidc: Option<(String, &serde_json::Map<String, Value>)> = None;
    let mut legacy: Option<(String, &serde_json::Map<String, Value>)> = None;

    for (scope, value) in root {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let has_key = entry
            .get("key")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_key {
            continue;
        }
        if scope.starts_with(OIDC_SCOPE_PREFIX) {
            oidc = Some((scope.clone(), entry));
        } else if scope == LEGACY_SESSION_SCOPE || scope.contains("/sign-in") {
            legacy = Some((scope.clone(), entry));
        }
    }

    oidc.or(legacy)
}

fn parse_json_date(value: &Value) -> Option<DateTime<Utc>> {
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // Some writers omit fractional seconds; chrono RFC3339 handles both.
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|ndt| DateTime::from_naive_utc_and_offset(ndt, Utc))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_oidc_and_legacy() -> String {
        r#"{
            "https://auth.x.ai::client-abc": {
                "key": "oidc-token",
                "refresh_token": "oidc-refresh",
                "expires_at": "2099-01-01T00:00:00Z",
                "auth_mode": "oidc",
                "email": "a@x.ai",
                "team_id": "team_1"
            },
            "https://accounts.x.ai/sign-in": {
                "key": "legacy-token",
                "auth_mode": "session",
                "email": "legacy@x.ai"
            }
        }"#
        .into()
    }

    #[test]
    fn prefers_oidc_over_legacy() {
        let creds = parse_auth_json(sample_oidc_and_legacy().as_bytes()).unwrap();
        assert_eq!(creds.access_token, "oidc-token");
        assert_eq!(creds.email.as_deref(), Some("a@x.ai"));
        assert_eq!(creds.login_method().as_deref(), Some("SuperGrok"));
        assert!(!creds.is_expired());
    }

    #[test]
    fn falls_back_to_legacy_when_oidc_missing_key() {
        let raw = r#"{
            "https://auth.x.ai::client-abc": {
                "key": "",
                "email": "bad@x.ai"
            },
            "https://accounts.x.ai/sign-in": {
                "key": "legacy-token",
                "auth_mode": "session",
                "email": "legacy@x.ai"
            }
        }"#;
        let creds = parse_auth_json(raw.as_bytes()).unwrap();
        assert_eq!(creds.access_token, "legacy-token");
        assert_eq!(creds.email.as_deref(), Some("legacy@x.ai"));
    }

    #[test]
    fn missing_tokens_errors() {
        let raw = r#"{"https://auth.x.ai::x": {"email": "n@x.ai"}}"#;
        assert!(parse_auth_json(raw.as_bytes()).is_err());
    }

    #[test]
    fn expired_when_past() {
        let raw = r#"{
            "https://auth.x.ai::c": {
                "key": "t",
                "expires_at": "2020-01-01T00:00:00Z"
            }
        }"#;
        let creds = parse_auth_json(raw.as_bytes()).unwrap();
        assert!(creds.is_expired());
    }
}
