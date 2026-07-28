pub mod opencodego;

use crate::error::AppError;
use crate::model::{Account, ProviderKind, UsageSnapshot};

#[allow(dead_code)]
pub async fn fetch_for_account(
    account: &Account,
    client: &reqwest::Client,
    timeout: std::time::Duration,
) -> Result<UsageSnapshot, AppError> {
    match account.provider {
        ProviderKind::OpenCodeGo => {
            opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                .fetch_usage(account)
                .await
        }
    }
}
