use std::path::Path;
use std::time::Duration;

use chrono::Utc;

use crate::api::opencodego::{KeyCheck, OpenCodeGoProvider};
use crate::error::AppError;
use crate::model::{ProviderKind, SessionEntry, UsageSnapshot};

/// Timeout for the pre-store validation call (matches the default
/// `request_timeout_secs`).
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome of storing an API-key credential.
#[derive(Debug)]
pub enum CredentialOutcome {
    /// 200 — key accepted by opencode.ai and stored; the windows it returned.
    Stored { snapshot: UsageSnapshot },
    /// 403 — key is valid but has no Go subscription; stored anyway.
    /// Informational, not a failure.
    StoredNotSubscribed,
}

/// Validate `api_key` against the live OpenCode Go usage API and persist it
/// for `name` only when the key is usable:
///
/// - 200 → stored, subscribed (windows returned for display)
/// - 403 → stored, but no Go subscription (reported plainly)
/// - 401 → NOT stored, clear rejection
/// - network error → NOT stored, `AppError::Network` propagated
///
/// The write is atomic-ish (temp file + rename via `save_config`) and
/// preserves every other account and field in auth.toml.
pub async fn set_api_key_credential(
    data_dir: &Path,
    config_path: &Path,
    name: &str,
    api_key: &str,
) -> Result<CredentialOutcome, AppError> {
    let key = api_key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::Config("api_key must not be empty".into()));
    }

    let client = reqwest::Client::builder()
        .build()
        .map_err(AppError::Network)?;
    let check = OpenCodeGoProvider::new(client, VALIDATE_TIMEOUT)
        .check_api_key(&key)
        .await?;

    // `data_dir` is accepted for symmetry with the other credential entry
    // points; an API key lives in auth.toml only, no session file is touched.
    let _ = data_dir;

    store_api_key_credential(config_path, name, &key, check)
}

/// Store-side of [`set_api_key_credential`]: no network I/O, so the
/// write / no-write decision is unit-testable.
fn store_api_key_credential(
    config_path: &Path,
    name: &str,
    api_key: &str,
    check: KeyCheck,
) -> Result<CredentialOutcome, AppError> {
    match check {
        KeyCheck::Rejected => Err(AppError::Login(
            "key rejected by opencode.ai (401) — not stored".into(),
        )),
        KeyCheck::NotSubscribed => {
            crate::config::upsert_api_key_account(
                config_path,
                name,
                ProviderKind::OpenCodeGo,
                api_key,
            )?;
            Ok(CredentialOutcome::StoredNotSubscribed)
        }
        KeyCheck::Subscribed(snapshot) => {
            crate::config::upsert_api_key_account(
                config_path,
                name,
                ProviderKind::OpenCodeGo,
                api_key,
            )?;
            Ok(CredentialOutcome::Stored { snapshot })
        }
    }
}

/// Adopt a key from an external store (`hermes:<label>` / `opencode`) instead
/// of pasting one. The source is resolved FIRST, so a bad label or missing
/// store file fails before anything is written or any request is sent; the
/// resolved key is then validated exactly like the pasted-key path:
///
/// - 200 → stored (subscribed)
/// - 403 → stored, but no Go subscription (informational)
/// - 401 → NOT stored, clear rejection
/// - network error → NOT stored
///
/// Only the `api_key_source` reference is persisted — the key itself is
/// never copied into auth.toml and is re-read at fetch time, so rotating it
/// in the store is picked up automatically. A previously pasted `api_key` on
/// this account is dropped: an explicit key always wins over a source, so
/// keeping it would leave the adoption inert.
pub async fn set_key_source_credential(
    data_dir: &Path,
    config_path: &Path,
    name: &str,
    source: &str,
    client: reqwest::Client,
    timeout: Duration,
) -> Result<CredentialOutcome, AppError> {
    let src = source.trim().to_string();
    if src.is_empty() {
        return Err(AppError::Config("api_key_source must not be empty".into()));
    }
    // Bad label / missing store fails here, before any write or request.
    let resolved = crate::keysource::resolve(&src)?;

    let check = OpenCodeGoProvider::new(client, timeout)
        .check_api_key(&resolved.key)
        .await?;

    // `data_dir` is accepted for symmetry with the other credential entry
    // points; a key source lives in auth.toml only, no session file.
    let _ = data_dir;

    store_key_source_credential(config_path, name, &src, check)
}

/// Store-side of [`set_key_source_credential`]: no network I/O, so the
/// write / no-write decision is unit-testable.
fn store_key_source_credential(
    config_path: &Path,
    name: &str,
    source: &str,
    check: KeyCheck,
) -> Result<CredentialOutcome, AppError> {
    match check {
        KeyCheck::Rejected => Err(AppError::Login(
            "key rejected by opencode.ai (401) — not stored".into(),
        )),
        KeyCheck::NotSubscribed => {
            persist_key_source(config_path, name, source)?;
            Ok(CredentialOutcome::StoredNotSubscribed)
        }
        KeyCheck::Subscribed(snapshot) => {
            persist_key_source(config_path, name, source)?;
            Ok(CredentialOutcome::Stored { snapshot })
        }
    }
}

/// Persist `api_key_source` for `name` and drop this account's pasted
/// `api_key` so the source actually takes effect (explicit keys win).
fn persist_key_source(config_path: &Path, name: &str, source: &str) -> Result<(), AppError> {
    crate::config::set_account_key_source(
        config_path,
        name,
        ProviderKind::OpenCodeGo,
        Some(source),
    )?;
    crate::config::clear_account_api_key(config_path, name)
}

/// Mask a secret for display: at most first 7 chars + "…" + last 4.
/// Never returns the input and never the middle of a long secret.
pub fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.trim().chars().collect();
    if chars.len() <= 11 {
        // Too short to expose head+tail safely; show a stub instead.
        if chars.is_empty() {
            return "(none)".to_string();
        }
        let head: String = chars.iter().take(2).collect();
        return format!("{head}…");
    }
    let first: String = chars.iter().take(7).collect();
    let last: String = chars[chars.len() - 4..].iter().collect();
    format!("{first}…{last}")
}

/// Remove one or more accounts from auth.toml and their sessions from
/// sessions.json in one pass. Idempotent: names that don't exist are reported,
/// not fatal. Returns one human-readable line per requested name.
pub fn rm_accounts(
    config_path: &Path,
    sessions_path: &Path,
    names: &[String],
) -> Result<Vec<String>, AppError> {
    let mut config = crate::config::load_config_or_default(config_path)?;
    let mut sessions = crate::session::load_sessions(sessions_path)?;
    let mut lines = Vec::new();
    let mut config_dirty = false;
    let mut sessions_dirty = false;

    for name in names {
        let before = config.accounts.len();
        config.accounts.retain(|a| a.name != *name);
        let in_config = config.accounts.len() != before;
        let had_session = sessions.sessions.remove(name).is_some();

        if in_config {
            config_dirty = true;
        }
        if had_session {
            sessions_dirty = true;
        }

        match (in_config, had_session) {
            (true, true) => lines.push(format!("Removed account '{name}' and its session")),
            (true, false) => lines.push(format!("Removed account '{name}' (no session stored)")),
            (false, true) => lines.push(format!(
                "No account '{name}' in auth.toml; removed orphan session"
            )),
            (false, false) => lines.push(format!("Account '{name}' not found")),
        }
    }

    if config_dirty {
        crate::config::save_config(config_path, &config)?;
    }
    if sessions_dirty {
        crate::session::save_sessions(sessions_path, &sessions)?;
    }
    Ok(lines)
}

/// Remove only the session for an account, keeping the account in auth.toml.
pub fn logout_account(sessions_path: &Path, name: &str) -> Result<String, AppError> {
    let mut sessions = crate::session::load_sessions(sessions_path)?;
    if sessions.sessions.remove(name).is_some() {
        crate::session::save_sessions(sessions_path, &sessions)?;
        Ok(format!(
            "Logged out '{name}' — session removed, account kept"
        ))
    } else {
        Ok(format!("No session found for '{name}'"))
    }
}

/// Store a manually captured cookie as the account's session, creating the
/// account in auth.toml first so we never leave an orphan session behind.
pub fn store_session_cookie(
    data_dir: &Path,
    config_path: &Path,
    account_name: &str,
    cookie: String,
) -> Result<(), AppError> {
    let cookie = cookie.trim().to_string();
    if cookie.is_empty() {
        return Err(AppError::Login(
            "--cookie must not be empty (use --json-file-path for a file)".into(),
        ));
    }

    let created =
        crate::config::ensure_account(config_path, account_name, ProviderKind::OpenCodeGo)?;
    if created {
        println!(
            "Added account '{account_name}' (opencode_go) to {}",
            config_path.display()
        );
    }

    let sessions_path = crate::session::resolve_sessions_path(data_dir);
    let mut sessions = crate::session::load_sessions(&sessions_path)?;
    sessions.sessions.insert(
        account_name.to_string(),
        SessionEntry {
            cookie: cookie.clone(),
            workspace_id: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            email: None,
            user_id: None,
            updated_at: Utc::now(),
        },
    );
    crate::session::save_sessions(&sessions_path, &sessions)?;
    println!(
        "Session stored for account '{account_name}' ({} chars)",
        cookie.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // pid + per-process counter: wall-clock nanos collide when several tests
        // start in the same tick, which made unrelated tests share one directory
        // and read each other's half-written files under parallel runs.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-account-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(path: &Path, names: &[&str]) {
        let mut s = String::new();
        for n in names {
            s.push_str(&format!(
                "[[accounts]]\nname = \"{n}\"\nprovider = \"opencode_go\"\n\n"
            ));
        }
        std::fs::write(path, s).unwrap();
    }

    fn write_session(path: &Path, names: &[&str]) {
        let entries: Vec<String> = names
            .iter()
            .map(|n| {
                format!("\"{n}\": {{\"cookie\":\"c\",\"updated_at\":\"2026-08-12T00:00:00Z\"}}")
            })
            .collect();
        std::fs::write(path, format!("{{\"sessions\":{{{}}}}}", entries.join(","))).unwrap();
    }

    #[test]
    fn rm_removes_account_and_session() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        let ses = dir.join("sessions.json");
        write_config(&cfg, &["a", "b"]);
        write_session(&ses, &["a", "b"]);

        let lines = rm_accounts(&cfg, &ses, &["a".to_string()]).unwrap();
        assert_eq!(lines, vec!["Removed account 'a' and its session"]);

        let cfg_text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!cfg_text.contains("name = \"a\""));
        assert!(cfg_text.contains("name = \"b\""));
        let ses_text = std::fs::read_to_string(&ses).unwrap();
        assert!(!ses_text.contains("\"a\""));
        assert!(ses_text.contains("\"b\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rm_reports_missing_and_keeps_other_files_untouched() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        let ses = dir.join("sessions.json");
        write_config(&cfg, &["a"]);
        write_session(&ses, &["a"]);
        let before_cfg = std::fs::read_to_string(&cfg).unwrap();
        let before_ses = std::fs::read_to_string(&ses).unwrap();

        let lines = rm_accounts(&cfg, &ses, &["ghost".to_string()]).unwrap();
        assert_eq!(lines, vec!["Account 'ghost' not found"]);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before_cfg);
        assert_eq!(std::fs::read_to_string(&ses).unwrap(), before_ses);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rm_cleans_orphan_session_only() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        let ses = dir.join("sessions.json");
        write_config(&cfg, &["a"]);
        write_session(&ses, &["a", "ghost"]);

        let lines = rm_accounts(&cfg, &ses, &["ghost".to_string()]).unwrap();
        assert_eq!(
            lines,
            vec!["No account 'ghost' in auth.toml; removed orphan session"]
        );
        let ses_text = std::fs::read_to_string(&ses).unwrap();
        assert!(!ses_text.contains("\"ghost\""));
        assert!(ses_text.contains("\"a\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_keeps_account() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        let ses = dir.join("sessions.json");
        write_config(&cfg, &["a"]);
        write_session(&ses, &["a"]);

        let line = logout_account(&ses, "a").unwrap();
        assert!(line.contains("Logged out 'a'"));
        assert!(std::fs::read_to_string(&cfg)
            .unwrap()
            .contains("name = \"a\""));
        assert!(!std::fs::read_to_string(&ses).unwrap().contains("\"a\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_missing_session_reports() {
        let dir = temp_dir();
        let ses = dir.join("sessions.json");
        write_session(&ses, &["a"]);
        let line = logout_account(&ses, "nobody").unwrap();
        assert!(line.contains("No session found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_cookie_creates_account_and_session() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a"]);

        store_session_cookie(&dir, &cfg, "b", "sess-123".into()).unwrap();
        let cfg_text = std::fs::read_to_string(&cfg).unwrap();
        assert!(cfg_text.contains("name = \"b\""));
        let ses = crate::session::load_sessions(&dir.join("sessions.json")).unwrap();
        assert_eq!(ses.sessions["b"].cookie, "sess-123");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_cookie_rejects_empty() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a"]);
        let err = store_session_cookie(&dir, &cfg, "b", "   ".into()).unwrap_err();
        assert!(err.to_string().contains("--cookie must not be empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- API-key credential store path ----

    fn sample_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            account_name: "a".into(),
            rolling: UsageWindow::with_label(2.0, 3633, "Rolling"),
            weekly: Some(UsageWindow::with_label(25.0, 111_600, "Weekly")),
            monthly: Some(UsageWindow::with_label(36.0, 1_470_390, "Monthly")),
            updated_at: Utc::now(),
            workspace_id: None,
        }
    }

    #[test]
    fn store_401_rejected_without_writing() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a", "b"]);
        let before = std::fs::read(&cfg).unwrap();

        let err =
            store_api_key_credential(&cfg, "a", "sk-rejected", KeyCheck::Rejected).unwrap_err();
        assert!(
            err.to_string()
                .contains("key rejected by opencode.ai (401)"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read(&cfg).unwrap(),
            before,
            "a rejected key must leave auth.toml byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_200_writes_key_and_preserves_other_accounts() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a", "b"]);

        let out = store_api_key_credential(
            &cfg,
            "a",
            "sk-live",
            KeyCheck::Subscribed(sample_snapshot()),
        )
        .unwrap();
        match out {
            CredentialOutcome::Stored { snapshot } => {
                assert!((snapshot.rolling.usage_percent - 2.0).abs() < 0.001);
            }
            other => panic!("expected Stored, got {other:?}"),
        }

        let loaded = crate::config::load_config(&cfg).unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].name, "a");
        assert_eq!(loaded.accounts[0].provider, ProviderKind::OpenCodeGo);
        assert_eq!(loaded.accounts[0].api_key.as_deref(), Some("sk-live"));
        assert_eq!(loaded.accounts[1].name, "b");
        assert!(loaded.accounts[1].api_key.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_403_writes_key_but_reports_not_subscribed() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a"]);

        let out =
            store_api_key_credential(&cfg, "a", "sk-freeload", KeyCheck::NotSubscribed).unwrap();
        assert!(matches!(out, CredentialOutcome::StoredNotSubscribed));

        let loaded = crate::config::load_config(&cfg).unwrap();
        assert_eq!(loaded.accounts[0].api_key.as_deref(), Some("sk-freeload"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- key-source credential store path ----

    #[test]
    fn store_key_source_401_leaves_auth_toml_byte_identical() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a", "b"]);
        let before = std::fs::read(&cfg).unwrap();

        let err =
            store_key_source_credential(&cfg, "a", "hermes:lks00", KeyCheck::Rejected).unwrap_err();
        assert!(
            err.to_string()
                .contains("key rejected by opencode.ai (401)"),
            "unexpected error: {err}"
        );
        assert!(
            !err.to_string().contains("sk-"),
            "error must never contain key material: {err}"
        );
        assert_eq!(
            std::fs::read(&cfg).unwrap(),
            before,
            "a rejected source must leave auth.toml byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_key_source_persists_reference_and_preserves_other_accounts() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        // Account "a" has a pasted key that must make way for the adoption;
        // account "b"'s key must survive untouched.
        std::fs::write(
            &cfg,
            "[[accounts]]\nname = \"a\"\nprovider = \"opencode_go\"\napi_key = \"sk-old-pasted\"\n\n\
             [[accounts]]\nname = \"b\"\nprovider = \"zai\"\napi_key = \"sk-theirs\"\n",
        )
        .unwrap();

        let out = store_key_source_credential(
            &cfg,
            "a",
            "hermes:lks00",
            KeyCheck::Subscribed(sample_snapshot()),
        )
        .unwrap();
        assert!(matches!(out, CredentialOutcome::Stored { .. }));

        let loaded = crate::config::load_config(&cfg).unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(
            loaded.accounts[0].api_key_source.as_deref(),
            Some("hermes:lks00")
        );
        assert!(
            loaded.accounts[0].api_key.is_none(),
            "a pasted key would keep winning over the adopted source"
        );
        assert_eq!(loaded.accounts[1].name, "b");
        assert_eq!(loaded.accounts[1].api_key.as_deref(), Some("sk-theirs"));
        assert!(loaded.accounts[1].api_key_source.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_key_source_403_persists_but_reports_not_subscribed() {
        let dir = temp_dir();
        let cfg = dir.join("auth.toml");
        write_config(&cfg, &["a"]);

        let out =
            store_key_source_credential(&cfg, "a", "opencode", KeyCheck::NotSubscribed).unwrap();
        assert!(matches!(out, CredentialOutcome::StoredNotSubscribed));

        let loaded = crate::config::load_config(&cfg).unwrap();
        assert_eq!(
            loaded.accounts[0].api_key_source.as_deref(),
            Some("opencode")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mask_secret_never_reveals_middle_or_full() {
        let key = "sk-3YLDrouterMIDDLEsunshine9XzQ";
        let masked = mask_secret(key);
        assert_ne!(masked, key, "mask must never equal the input");
        assert!(
            !masked.contains("routerMIDDLE"),
            "mask must not contain the middle of a long key"
        );
        assert!(!masked.contains("sunshine"));
        assert!(masked.starts_with("sk-3YLD"));
        assert!(masked.ends_with("9XzQ"));
    }

    #[test]
    fn mask_secret_short_and_edge_inputs() {
        // Short inputs never round-trip the full value.
        assert_ne!(mask_secret("short"), "short");
        assert_ne!(mask_secret("sk-abc"), "sk-abc");
        assert_eq!(mask_secret("  "), "(none)");
        assert_eq!(mask_secret(""), "(none)");
        // Just long enough (12 chars): first 7 + last 4 keeps one char hidden.
        let masked = mask_secret("sk-abcde1234");
        assert_eq!(masked, "sk-abcd…1234");
        assert_ne!(masked, "sk-abcde1234");
    }
}
