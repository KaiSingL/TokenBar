use std::path::Path;

use chrono::Utc;

use crate::error::AppError;
use crate::model::{ProviderKind, SessionEntry};

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

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-account-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
}
