pub mod grok;
pub mod opencodego;
pub mod zai;

use std::time::Duration;

use crate::error::AppError;
use crate::model::{Account, ProviderKind, SessionEntry, UsageSnapshot};

/// Fetch usage for an account using provider-specific credentials.
pub async fn fetch_for_account(
    account: &Account,
    session: Option<&SessionEntry>,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<UsageSnapshot, AppError> {
    match account.provider {
        ProviderKind::OpenCodeGo => {
            let entry = session.ok_or(AppError::InvalidCredentials)?;
            opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                .fetch_usage(
                    &account.name,
                    &entry.cookie,
                    entry.workspace_id.as_deref(),
                )
                .await
        }
        ProviderKind::Zai => {
            let api_key = resolve_zai_api_key(account).ok_or(AppError::InvalidCredentials)?;
            zai::ZaiProvider::new(client.clone(), timeout)
                .fetch_usage(&account.name, &api_key)
                .await
        }
        ProviderKind::Grok => {
            let entry = session.ok_or(AppError::InvalidCredentials)?;
            grok::GrokProvider::new(client.clone(), timeout)
                .fetch_usage(&account.name, entry)
                .await
        }
    }
}

pub fn has_credentials(account: &Account, session: Option<&SessionEntry>) -> bool {
    match account.provider {
        ProviderKind::OpenCodeGo => session.map(|s| !s.cookie.trim().is_empty()).unwrap_or(false),
        ProviderKind::Zai => resolve_zai_api_key(account).is_some(),
        ProviderKind::Grok => session.map(|s| s.has_grok_session()).unwrap_or(false),
    }
}

pub fn resolve_zai_api_key(account: &Account) -> Option<String> {
    if let Some(key) = account
        .api_key
        .as_ref()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return Some(key);
    }
    std::env::var("Z_AI_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}
