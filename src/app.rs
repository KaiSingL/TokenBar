use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{RwLock, mpsc, Semaphore};
use tracing::{error, info};

use crate::api;
use crate::error::AppError;
use crate::model::{Account, AccountStatus, AppConfig, UsageSnapshot};

pub enum AppEvent {
    Refresh,
    Quit,
}

pub struct AppState {
    pub accounts: Vec<Account>,
    pub statuses: Vec<AccountStatus>,
    pub config: AppConfig,
    pub last_refresh: Option<chrono::DateTime<Utc>>,
    pub is_refreshing: bool,
    pub tick_count: u64,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let count = config.accounts.len();
        Self {
            statuses: vec![AccountStatus::Loading; count],
            accounts: config.accounts.clone(),
            config,
            last_refresh: None,
            is_refreshing: false,
            tick_count: 0,
        }
    }
}

pub struct Poller {
    state: Arc<RwLock<AppState>>,
    client: reqwest::Client,
    event_rx: mpsc::Receiver<AppEvent>,
    refresh_interval: Duration,
    request_timeout: Duration,
    max_concurrent: usize,
}

impl Poller {
    pub fn new(
        state: Arc<RwLock<AppState>>,
        event_rx: mpsc::Receiver<AppEvent>,
        config: &AppConfig,
    ) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("Failed to build reqwest client");

        Self {
            state,
            client,
            event_rx,
            refresh_interval: Duration::from_secs(config.refresh_interval_secs),
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            max_concurrent: config.max_concurrent_fetches.max(1),
        }
    }

    pub async fn run(mut self) {
        info!("Poller started (interval={:?})", self.refresh_interval);

        self.refresh_all().await;
        let mut ticker = tokio::time::interval(self.refresh_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.refresh_all().await;
                }
                Some(event) = self.event_rx.recv() => {
                    match event {
                        AppEvent::Refresh => {
                            self.refresh_all().await;
                        }
                        AppEvent::Quit => {
                            info!("Poller stopping");
                            break;
                        }

                    }
                }
            }
        }
    }

    async fn refresh_all(&mut self) {
        let accounts = {
            let state = self.state.read().await;
            state.accounts.clone()
        };

        if accounts.is_empty() {
            return;
        }

        {
            let mut state = self.state.write().await;
            state.is_refreshing = true;
        }

        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::with_capacity(accounts.len());

        for (i, account) in accounts.into_iter().enumerate() {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Semaphore closed");
            let client = self.client.clone();
            let timeout = self.request_timeout;
            let state = self.state.clone();

            let handle = tokio::spawn(async move {
                let _permit = permit;
                let result = fetch_single_account(&account, &client, timeout).await;
                let mut state = state.write().await;
                match result {
                    Ok(snapshot) => {
                        state.statuses[i] = AccountStatus::Ready(snapshot);
                    }
                    Err(e) => {
                        error!("Account '{}' fetch failed: {e}", account.name);
                        let already_had_ready = matches!(&state.statuses[i], AccountStatus::Ready(_));
                        if already_had_ready {
                            if let AccountStatus::Ready(ref last) = state.statuses[i] {
                                state.statuses[i] = AccountStatus::Stale {
                                    last: last.clone(),
                                    error: e.to_string(),
                                    failed_at: Utc::now(),
                                };
                            }
                        } else {
                            state.statuses[i] = AccountStatus::Error {
                                message: e.to_string(),
                                failed_at: Utc::now(),
                            };
                        }
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        {
            let mut state = self.state.write().await;
            state.last_refresh = Some(Utc::now());
            state.is_refreshing = false;
        }

        info!("Refresh cycle completed");
    }
}

async fn fetch_single_account(
    account: &Account,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<UsageSnapshot, AppError> {
    let provider = api::opencodego::OpenCodeGoProvider::new(client.clone(), timeout);
    provider.fetch_usage(account).await
}
