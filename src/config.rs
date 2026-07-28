use std::path::PathBuf;

use crate::error::AppError;
use crate::model::AppConfig;

pub fn resolve_config_path(override_path: Option<&str>) -> Result<PathBuf, AppError> {
    if let Some(path) = override_path {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| AppError::Config("Cannot determine config directory".into()))?;
    let path = base.join("tokenbar").join("auth.toml");
    Ok(path)
}

pub fn load_config(path: &std::path::Path) -> Result<AppConfig, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!(
            "Config file not found: {}",
            path.display()
        )));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("Failed to read config: {e}")))?;
    let mut config: AppConfig =
        toml::from_str(&contents).map_err(|e| AppError::Config(format!("Invalid TOML: {e}")))?;
    for account in &mut config.accounts {
        account.cookie = normalize_cookie(&account.cookie);
        if account.cookie.is_empty() {
            return Err(AppError::Config(format!(
                "Account '{}' has empty cookie after normalization",
                account.name
            )));
        }
    }
    Ok(config)
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
}
