pub mod auth;
pub mod billing;
pub mod oauth;

use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use tracing::debug;

use crate::error::AppError;
use crate::model::{SessionEntry, UsageSnapshot, UsageWindow};

pub struct GrokProvider {
    client: Client,
    timeout: Duration,
}

impl GrokProvider {
    pub fn new(client: Client, timeout: Duration) -> Self {
        Self { client, timeout }
    }

    pub async fn fetch_usage(
        &self,
        account_name: &str,
        session: &SessionEntry,
    ) -> Result<UsageSnapshot, AppError> {
        debug!("fetch_usage grok for account {}", account_name);

        let token = session
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AppError::InvalidCredentials)?;

        if session.expires_at.is_some_and(|exp| Utc::now() >= exp) {
            return Err(AppError::InvalidCredentials);
        }

        let snap = billing::fetch_credits(
            &self.client,
            token,
            session.user_id.as_deref(),
            self.timeout,
        )
        .await?;

        let reset_in_sec = snap
            .resets_at
            .map(|at| {
                let delta = at.signed_duration_since(Utc::now());
                delta.num_seconds().max(0) as u64
            })
            .unwrap_or(0);

        Ok(UsageSnapshot {
            account_name: account_name.to_string(),
            rolling: UsageWindow::with_label(
                snap.used_percent,
                reset_in_sec,
                snap.period_label,
            ),
            weekly: None,
            monthly: None,
            updated_at: Utc::now(),
            workspace_id: None,
        })
    }
}
