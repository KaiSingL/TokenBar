use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, mpsc, Semaphore};
use tracing::{error, info, warn};

use crate::api;
use crate::error::AppError;
use crate::model::{Account, AccountStatus, AppConfig, SessionEntry};
use crate::session;

pub enum AppEvent {
    Refresh,
    Quit,
}

pub struct AppState {
    pub accounts: Vec<Account>,
    pub statuses: Vec<AccountStatus>,
    pub config: AppConfig,
    pub last_refresh: Option<DateTime<Utc>>,
    pub is_refreshing: bool,
    pub tick_count: u64,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let count = config.accounts.len();
        Self {
            statuses: vec![AccountStatus::NoSession; count],
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
    #[allow(dead_code)]
    sessions_path: PathBuf,
    sessions: HashMap<String, SessionEntry>,
}

impl Poller {
    pub fn new(
        state: Arc<RwLock<AppState>>,
        event_rx: mpsc::Receiver<AppEvent>,
        config: &AppConfig,
        data_dir: &std::path::Path,
    ) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("Failed to build reqwest client");

        let sessions_path = session::resolve_sessions_path(data_dir);
        let sessions = session::load_sessions(&sessions_path)
            .map(|s| s.sessions)
            .unwrap_or_default();

        Self {
            state,
            client,
            event_rx,
            refresh_interval: Duration::from_secs(config.refresh_interval_secs),
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            max_concurrent: config.max_concurrent_fetches.max(1),
            sessions_path,
            sessions,
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
        let account_count = accounts.len();
        let mut handles = Vec::with_capacity(account_count);

        for (i, account) in accounts.into_iter().enumerate() {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Semaphore closed");

            let client = self.client.clone();
            let timeout = self.request_timeout;
            let state = self.state.clone();

            let session_entry = self.sessions.get(&account.name).cloned();

            if session_entry.is_none() {
                let mut state = state.write().await;
                state.statuses[i] = AccountStatus::NoSession;
                continue;
            }

            {
                let mut state = state.write().await;
                state.statuses[i] = AccountStatus::Loading;
            }

            let handle = tokio::spawn(async move {
                let _permit = permit;
                let entry = session_entry.unwrap();
                let workspace_id = entry.workspace_id.as_deref();

                let result = api::opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                    .fetch_usage(&account.name, &entry.cookie, workspace_id)
                    .await;

                let mut state = state.write().await;
                match result {
                    Ok(snapshot) => {
                        state.statuses[i] = AccountStatus::Ready(snapshot);
                    }
                    Err(e) => {
                        error!("Account '{}' fetch failed: {e}", account.name);
                        match &e {
                            AppError::InvalidCredentials => {
                                warn!("Account '{}' has invalid credentials, clearing session", account.name);
                                state.statuses[i] = AccountStatus::NoSession;
                            }
                            _ => {
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
                    }
                }
            });
            handles.push(handle);

            // Small stagger between account launches
            if i < account_count.saturating_sub(1) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
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
