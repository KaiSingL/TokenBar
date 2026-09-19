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
    // Atomic-ish: write a unique sibling temp file, then rename over the real
    // path so a crash mid-write cannot truncate auth.toml.
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AppError::Config(format!("Invalid config path: {}", path.display())))?;
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, contents)
        .map_err(|e| AppError::Config(format!("Failed to write config: {e}")))?;
    std::fs::rename(&tmp, path)
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

/// Upsert an account that authenticates with an api_key into auth.toml
/// (provider-agnostic). Preserves every other account and field.
/// Returns whether the account was newly created.
pub fn upsert_api_key_account(
    path: &Path,
    name: &str,
    provider: ProviderKind,
    api_key: &str,
) -> Result<bool, AppError> {
    let mut config = load_config_or_default(path)?;
    let key = api_key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::Config("api_key must not be empty".into()));
    }

    if let Some(account) = config.accounts.iter_mut().find(|a| a.name == name) {
        account.provider = provider;
        account.api_key = Some(key);
        save_config(path, &config)?;
        return Ok(false);
    }

    config.accounts.push(Account {
        name: name.to_string(),
        provider,
        api_key: Some(key),
    });
    save_config(path, &config)?;
    Ok(true)
}

/// Upsert a ZAI account with api_key into auth.toml.
/// Returns whether the account was newly created.
pub fn upsert_zai_account(path: &Path, name: &str, api_key: &str) -> Result<bool, AppError> {
    upsert_api_key_account(path, name, ProviderKind::Zai, api_key)
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

    #[test]
    fn upsert_api_key_account_preserves_other_accounts_and_fields() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        let cfg = AppConfig {
            refresh_interval_secs: 120,
            accounts: vec![Account {
                name: "cookie-go".into(),
                provider: ProviderKind::OpenCodeGo,
                api_key: None,
            }],
            ..AppConfig::default()
        };
        save_config(&path, &cfg).unwrap();

        // Trims the key; creates a new account.
        assert!(
            upsert_api_key_account(&path, "go2", ProviderKind::OpenCodeGo, " sk-new ").unwrap()
        );
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.refresh_interval_secs, 120);
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].name, "cookie-go");
        assert!(loaded.accounts[0].api_key.is_none());
        assert_eq!(loaded.accounts[1].name, "go2");
        assert_eq!(loaded.accounts[1].provider, ProviderKind::OpenCodeGo);
        assert_eq!(loaded.accounts[1].api_key.as_deref(), Some("sk-new"));

        // Second upsert updates in place, no duplicates.
        assert!(!upsert_api_key_account(&path, "go2", ProviderKind::OpenCodeGo, "sk-rot").unwrap());
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[1].api_key.as_deref(), Some("sk-rot"));

        // No stray temp files left behind by the atomic write.
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_api_key_account_rejects_empty_key() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        let err = upsert_api_key_account(&path, "a", ProviderKind::Zai, "   ").unwrap_err();
        assert!(err.to_string().contains("api_key must not be empty"));
        assert!(!path.exists(), "rejected key must not create auth.toml");
        let _ = fs::remove_dir_all(&dir);
    }
}
