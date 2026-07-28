use std::path::PathBuf;

use crate::error::AppError;
use crate::model::AppConfig;

pub fn resolve_data_dir(override_path: Option<&str>) -> Result<PathBuf, AppError> {
    if let Some(path) = override_path {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| AppError::Config("Cannot determine config directory".into()))?;
    Ok(base.join("tokenbar"))
}

pub fn resolve_config_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("auth.toml")
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
    let config: AppConfig =
        toml::from_str(&contents).map_err(|e| AppError::Config(format!("Invalid TOML: {e}")))?;
    Ok(config)
}
