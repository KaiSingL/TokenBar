pub mod opencodego;

use std::time::Duration;

use crate::error::AppError;
use crate::model::{Account, ProviderKind, UsageSnapshot};

#[allow(dead_code)]
pub async fn fetch_for_account(
    account: &Account,
    cookie: &str,
    workspace_id: Option<&str>,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<UsageSnapshot, AppError> {
    match account.provider {
        ProviderKind::OpenCodeGo => {
            opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                .fetch_usage(&account.name, cookie, workspace_id)
                .await
        }
    }
}
