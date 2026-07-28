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

/// Ensure `name` exists in auth.toml. Returns `true` if a new account was added.
pub fn ensure_account(path: &Path, name: &str) -> Result<bool, AppError> {
    let mut config = load_config_or_default(path)?;
    if config.accounts.iter().any(|a| a.name == name) {
        return Ok(false);
    }
    config.accounts.push(Account {
        name: name.to_string(),
        provider: ProviderKind::OpenCodeGo,
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
    fn ensure_account_creates_file_and_account() {
        let dir = temp_dir();
        let path = dir.join("auth.toml");
        assert!(ensure_account(&path, "kibashi").unwrap());
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].name, "kibashi");
        assert_eq!(cfg.accounts[0].provider, ProviderKind::OpenCodeGo);
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
        });
        cfg.refresh_interval_secs = 90;
        save_config(&path, &cfg).unwrap();

        assert!(ensure_account(&path, "newone").unwrap());
        assert!(!ensure_account(&path, "newone").unwrap());
        assert!(!ensure_account(&path, "existing").unwrap());

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.refresh_interval_secs, 90);
        assert_eq!(loaded.accounts.len(), 2);
        assert!(loaded.accounts.iter().any(|a| a.name == "existing"));
        assert!(loaded.accounts.iter().any(|a| a.name == "newone"));
        let _ = fs::remove_dir_all(&dir);
    }
}
