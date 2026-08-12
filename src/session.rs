use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::model::Sessions;

pub fn resolve_sessions_path(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions.json")
}

pub fn load_sessions(path: &Path) -> Result<Sessions, AppError> {
    if !path.exists() {
        return Ok(Sessions {
            sessions: std::collections::HashMap::new(),
        });
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("Failed to read sessions: {e}")))?;
    let mut sessions: Sessions = serde_json::from_str(&contents)
        .map_err(|e| AppError::Config(format!("Invalid sessions.json: {e}")))?;
    for entry in sessions.sessions.values_mut() {
        entry.cookie = normalize_cookie(&entry.cookie);
        if let Some(t) = entry.access_token.as_mut() {
            *t = t.trim().to_string();
            if t.is_empty() {
                entry.access_token = None;
            }
        }
        if let Some(t) = entry.refresh_token.as_mut() {
            *t = t.trim().to_string();
            if t.is_empty() {
                entry.refresh_token = None;
            }
        }
    }
    Ok(sessions)
}

pub fn save_sessions(path: &Path, sessions: &Sessions) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError::Io(e))?;
    }
    let contents = serde_json::to_string_pretty(sessions)
        .map_err(|e| AppError::Config(format!("Failed to serialize sessions: {e}")))?;
    std::fs::write(path, &contents)
        .map_err(|e| AppError::Config(format!("Failed to write sessions: {e}")))?;
    Ok(())
}

fn normalize_cookie(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = if let Some(rest) = trimmed.strip_prefix("Cookie:") {
        rest
    } else {
        trimmed
    };
    let without_quotes = without_prefix
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    without_quotes.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionEntry;
    use chrono::Utc;

    #[test]
    fn test_normalize_cookie_strips_cookie_prefix() {
        assert_eq!(
            normalize_cookie("Cookie: _opencode_session=abc123"),
            "_opencode_session=abc123"
        );
    }

    #[test]
    fn test_normalize_cookie_strips_whitespace() {
        assert_eq!(
            normalize_cookie("  _opencode_session=abc123  "),
            "_opencode_session=abc123"
        );
    }

    #[test]
    fn test_normalize_cookie_strips_quotes() {
        assert_eq!(
            normalize_cookie("\"_opencode_session=abc123\""),
            "_opencode_session=abc123"
        );
    }

    #[test]
    fn test_normalize_cookie_already_clean() {
        assert_eq!(
            normalize_cookie("_opencode_session=abc123"),
            "_opencode_session=abc123"
        );
    }

    #[test]
    fn test_normalize_cookie_empty() {
        assert_eq!(normalize_cookie(""), "");
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("tokenbar_test_sessions");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sessions.json");

        let mut sessions = Sessions {
            sessions: std::collections::HashMap::new(),
        };
        sessions.sessions.insert(
            "Personal".into(),
            SessionEntry {
                cookie: "_opencode_session=abc123".into(),
                workspace_id: Some("wrk_test123".into()),
                access_token: None,
                refresh_token: None,
                expires_at: None,
                email: None,
                user_id: None,
                updated_at: Utc::now(),
            },
        );
        save_sessions(&path, &sessions).unwrap();
        let loaded = load_sessions(&path).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(
            loaded.sessions["Personal"].cookie,
            "_opencode_session=abc123"
        );
        assert_eq!(
            loaded.sessions["Personal"].workspace_id,
            Some("wrk_test123".into())
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
