use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::model::{Account, AppConfig, ProviderKind};

pub fn resolve_data_dir(override_path: Option<&str>) -> Result<PathBuf, AppError> {
    if let Some(path) = override_path {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| AppError::Config("Cannot determine home directory".into()))?;
    Ok(home.join(".config").join("tokenbar"))
}

pub fn resolve_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auth.toml")
}

pub fn load_config(path: &Path) -> Result<AppConfig, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!(
            "Config file not found: {}",
            path.display()
        )));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("Failed to read config: {e}")))?;
    let config: AppConfig =
        toml::from_str(&contents).map_err(|e| AppError::Config(format!("Invalid TOML: {e}")))?;
    Ok(config)
}

pub fn load_config_or_default(path: &Path) -> Result<AppConfig, AppError> {
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    load_config(path)
}

/// Load config from `path`, creating a default `auth.toml` when missing.
pub fn load_or_create_config(path: &Path) -> Result<AppConfig, AppError> {
    if path.exists() {
        return load_config(path);
    }
    let config = AppConfig::default();
    save_config(path, &config)?;
    Ok(config)
}

pub fn save_config(path: &Path, config: &AppConfig) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let contents = toml::to_string_pretty(config)
        .map_err(|e| AppError::Config(format!("Failed to serialize config: {e}")))?;
    std::fs::write(path, contents)
        .map_err(|e| AppError::Config(format!("Failed to write config: {e}")))?;
    Ok(())
}

/// Ensure `name` exists in auth.toml with the given provider.
/// Returns `true` if a new account was added.
pub fn ensure_account(path: &Path, name: &str, provider: ProviderKind) -> Result<bool, AppError> {
    let mut config = load_config_or_default(path)?;
    if config.accounts.iter().any(|a| a.name == name) {
        return Ok(false);
    }
    config.accounts.push(Account {
        name: name.to_string(),
        provider,
        api_key: None,
    });
    save_config(path, &config)?;
    Ok(true)
}

/// Upsert a ZAI account with api_key into auth.toml.
/// Returns whether the account was newly created.
pub fn upsert_zai_account(path: &Path, name: &str, api_key: &str) -> Result<bool, AppError> {
    let mut config = load_config_or_default(path)?;
    let key = api_key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::Config("api_key must not be empty".into()));
    }

    if let Some(account) = config.accounts.iter_mut().find(|a| a.name == name) {
        account.provider = ProviderKind::Zai;
        account.api_key = Some(key);
        save_config(path, &config)?;
        return Ok(false);
    }

    config.accounts.push(Account {
        name: name.to_string(),
        provider: ProviderKind::Zai,
        api_key: Some(key),
    });
    save_config(path, &config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_or_create_config_writes_default_when_missing() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        assert!(!path.exists());

        let cfg = load_or_create_config(&path).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.refresh_interval_secs, 60);
        assert_eq!(cfg.request_timeout_secs, 15);
        assert_eq!(cfg.max_concurrent_fetches, 4);
        assert!(cfg.accounts.is_empty());

        let reloaded = load_config(&path).unwrap();
        assert_eq!(reloaded.refresh_interval_secs, 60);
        assert!(reloaded.accounts.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_config_leaves_existing_file_unchanged() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        let mut cfg = AppConfig::default();
        cfg.refresh_interval_secs = 90;
        cfg.accounts.push(Account {
            name: "existing".into(),
            provider: ProviderKind::OpenCodeGo,
            api_key: None,
        });
        save_config(&path, &cfg).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let loaded = load_or_create_config(&path).unwrap();
        assert_eq!(loaded.refresh_interval_secs, 90);
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_account_creates_file_and_account() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        assert!(ensure_account(&path, "kibashi", ProviderKind::OpenCodeGo).unwrap());
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].name, "kibashi");
        assert_eq!(cfg.accounts[0].provider, ProviderKind::OpenCodeGo);
        assert!(cfg.accounts[0].api_key.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_account_appends_without_dupes() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        let mut cfg = AppConfig::default();
        cfg.accounts.push(Account {
            name: "existing".into(),
            provider: ProviderKind::OpenCodeGo,
            api_key: None,
        });
        cfg.refresh_interval_secs = 90;
        save_config(&path, &cfg).unwrap();

        assert!(ensure_account(&path, "newone", ProviderKind::OpenCodeGo).unwrap());
        assert!(!ensure_account(&path, "newone", ProviderKind::OpenCodeGo).unwrap());
        assert!(!ensure_account(&path, "existing", ProviderKind::OpenCodeGo).unwrap());

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.refresh_interval_secs, 90);
        assert_eq!(loaded.accounts.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_zai_stores_api_key() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        assert!(upsert_zai_account(&path, "myzai", "secret-key").unwrap());
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].provider, ProviderKind::Zai);
        assert_eq!(cfg.accounts[0].api_key.as_deref(), Some("secret-key"));

        assert!(!upsert_zai_account(&path, "myzai", "new-key").unwrap());
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.accounts[0].api_key.as_deref(), Some("new-key"));
        let _ = fs::remove_dir_all(&dir);
    }
}
