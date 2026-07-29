use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "opencode_go", alias = "open_code_go")]
    OpenCodeGo,
    #[serde(rename = "zai", alias = "z_ai")]
    Zai,
    #[serde(rename = "grok", alias = "xai")]
    Grok,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenCodeGo => "opencode_go",
            Self::Zai => "zai",
            Self::Grok => "grok",
        }
    }

    /// Human-facing label for TUI / web UI.
    pub fn display_label(self) -> &'static str {
        match self {
            Self::OpenCodeGo => "OpenCode Go",
            Self::Zai => "Zai",
            Self::Grok => "Grok",
        }
    }

    pub fn parse_cli(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "opencode_go" | "open_code_go" | "opencodego" => Ok(Self::OpenCodeGo),
            "zai" | "z_ai" | "z.ai" => Ok(Self::Zai),
            "grok" | "xai" | "x.ai" => Ok(Self::Grok),
            other => Err(format!(
                "Unknown provider '{other}'. Expected: opencode_go, zai, grok"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub provider: ProviderKind,
    /// API key for token-based providers (e.g. zai). Omitted for cookie providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_fetches: usize,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

fn default_refresh_interval() -> u64 {
    60
}
fn default_request_timeout() -> u64 {
    15
}
fn default_max_concurrent() -> usize {
    4
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 60,
            request_timeout_secs: 15,
            max_concurrent_fetches: 4,
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sessions {
    #[serde(default)]
    pub sessions: HashMap<String, SessionEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// OpenCode Go session cookie. Empty for token-based providers.
    #[serde(default)]
    pub cookie: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Grok / xAI OAuth access token (from `grok login` auth.json `key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Grok / xAI user id (`x-userid` header for billing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl SessionEntry {
    /// Grok session via OAuth bearer token (preferred) or legacy cookie.
    pub fn has_grok_session(&self) -> bool {
        self.access_token
            .as_ref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
            || !self.cookie.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct UsageWindow {
    pub usage_percent: f64,
    pub reset_in_sec: u64,
    /// Optional display label (e.g. "5h", "Weekly"). Falls back to Rolling/Weekly/Monthly.
    pub label: Option<String>,
}

impl UsageWindow {
    pub fn new(usage_percent: f64, reset_in_sec: u64) -> Self {
        Self {
            usage_percent,
            reset_in_sec,
            label: None,
        }
    }

    pub fn with_label(
        usage_percent: f64,
        reset_in_sec: u64,
        label: impl Into<String>,
    ) -> Self {
        Self {
            usage_percent,
            reset_in_sec,
            label: Some(label.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UsageSnapshot {
    #[allow(dead_code)]
    pub account_name: String,
    pub rolling: UsageWindow,
    pub weekly: Option<UsageWindow>,
    pub monthly: Option<UsageWindow>,
    #[allow(dead_code)]
    pub updated_at: DateTime<Utc>,
    #[allow(dead_code)]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AccountStatus {
    NoSession,
    Loading,
    Ready(UsageSnapshot),
    Stale {
        last: UsageSnapshot,
        error: String,
        failed_at: DateTime<Utc>,
    },
    Error {
        message: String,
        failed_at: DateTime<Utc>,
    },
}
