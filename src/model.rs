use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "opencodego")]
    OpenCodeGo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub provider: ProviderKind,
    pub cookie: String,
    pub workspace_id: Option<String>,
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

fn default_refresh_interval() -> u64 { 60 }
fn default_request_timeout() -> u64 { 15 }
fn default_max_concurrent() -> usize { 4 }

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

#[derive(Debug, Clone)]
pub struct UsageWindow {
    pub usage_percent: f64,
    pub reset_in_sec: u64,
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
