pub mod auth;
pub mod billing;
pub mod oauth;

use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Client;
use tracing::{debug, warn};

use crate::error::AppError;
use crate::model::{SessionEntry, UsageSnapshot, UsageWindow};

/// Refresh access token this many seconds before hard expiry.
const REFRESH_SKEW: ChronoDuration = ChronoDuration::seconds(120);

pub struct GrokProvider {
    client: Client,
    timeout: Duration,
}

impl GrokProvider {
    pub fn new(client: Client, timeout: Duration) -> Self {
        Self { client, timeout }
    }

    /// Fetch usage. Returns an updated session when tokens were refreshed.
    pub async fn fetch_usage(
        &self,
        account_name: &str,
        session: &SessionEntry,
    ) -> Result<(UsageSnapshot, Option<SessionEntry>), AppError> {
        debug!("fetch_usage grok for account {}", account_name);

        let mut session = session.clone();
        let mut session_touched = false;

        if needs_token_refresh(&session) {
            self.refresh_session(&mut session).await?;
            session_touched = true;
        }

        let token = session
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AppError::InvalidCredentials)?;

        let snap = match billing::fetch_credits(
            &self.client,
            token,
            session.user_id.as_deref(),
            self.timeout,
        )
        .await
        {
            Ok(s) => s,
            Err(AppError::InvalidCredentials) => {
                // Access token rejected; try refresh once if possible.
                if session
                    .refresh_token
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|t| !t.is_empty())
                {
                    warn!("grok billing 401/403 — refreshing token for {account_name}");
                    self.refresh_session(&mut session).await?;
                    session_touched = true;
                    let token = session
                        .access_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .ok_or(AppError::InvalidCredentials)?;
                    billing::fetch_credits(
                        &self.client,
                        token,
                        session.user_id.as_deref(),
                        self.timeout,
                    )
                    .await?
                } else {
                    return Err(AppError::InvalidCredentials);
                }
            }
            Err(e) => return Err(e),
        };

        let reset_in_sec = snap
            .resets_at
            .map(|at| {
                let delta = at.signed_duration_since(Utc::now());
                delta.num_seconds().max(0) as u64
            })
            .unwrap_or(0);

        let snapshot = UsageSnapshot {
            account_name: account_name.to_string(),
            rolling: UsageWindow::with_label(snap.used_percent, reset_in_sec, snap.period_label),
            weekly: None,
            monthly: None,
            updated_at: Utc::now(),
            workspace_id: None,
        };

        Ok((snapshot, session_touched.then_some(session)))
    }

    async fn refresh_session(&self, session: &mut SessionEntry) -> Result<(), AppError> {
        let refresh = session
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AppError::InvalidCredentials)?;

        let tokens =
            oauth::refresh_access_token(&self.client, refresh, self.timeout).await?;

        session.access_token = Some(tokens.access_token);
        if let Some(rt) = tokens.refresh_token {
            session.refresh_token = Some(rt);
        }
        if tokens.expires_at.is_some() {
            session.expires_at = tokens.expires_at;
        }
        session.updated_at = Utc::now();
        debug!(
            "grok token refreshed expires_at={:?}",
            session.expires_at
        );
        Ok(())
    }
}

fn needs_token_refresh(session: &SessionEntry) -> bool {
    let has_refresh = session
        .refresh_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|t| !t.is_empty());
    if !has_refresh {
        return false;
    }
    let Some(exp) = session.expires_at else {
        return false;
    };
    Utc::now() + REFRESH_SKEW >= exp
}
