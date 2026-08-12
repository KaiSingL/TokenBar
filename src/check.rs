use std::collections::HashMap;
use std::path::Path;

use crate::error::AppError;
use crate::model::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Err,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
}

impl Finding {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Ok,
            text: text.into(),
        }
    }
    fn warn(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            text: text.into(),
        }
    }
    fn err(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Err,
            text: text.into(),
        }
    }
}

/// Semantic validation of a successfully parsed config.
/// Syntax errors are caught by the parser (line/column included) before this runs.
pub fn validate_config(cfg: &crate::model::AppConfig) -> Vec<Finding> {
    let mut out = Vec::new();

    if cfg.accounts.is_empty() {
        out.push(Finding::warn(
            "auth.toml: no accounts configured (add one with: tokenbar login <name>)",
        ));
    }

    // Empty / duplicate account names (hand-edits can dup; the CLI path guards itself).
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for a in &cfg.accounts {
        *by_name.entry(a.name.as_str()).or_default() += 1;
    }
    let mut names: Vec<&str> = by_name.keys().copied().collect();
    names.sort();
    for name in names {
        let n = by_name[name];
        if name.is_empty() {
            out.push(Finding::err(
                "auth.toml: account with empty name (name = \"\")",
            ));
        } else if n > 1 {
            out.push(Finding::err(format!(
                "auth.toml: duplicate account name '{name}' ({n} entries) — rename or remove duplicates"
            )));
        }
    }

    for a in &cfg.accounts {
        match a.provider {
            ProviderKind::Zai => {
                let has_key = a
                    .api_key
                    .as_ref()
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                if !has_key {
                    out.push(Finding::warn(format!(
                        "auth.toml: '{}' has no api_key (set it in auth.toml or via env Z_AI_API_KEY)",
                        a.name
                    )));
                }
            }
            _ => {}
        }
    }

    if cfg.refresh_interval_secs == 0 {
        out.push(Finding::warn(
            "auth.toml: refresh_interval_secs = 0 would poll in a tight loop; set ≥ 1",
        ));
    }
    if cfg.request_timeout_secs == 0 {
        out.push(Finding::warn(
            "auth.toml: request_timeout_secs = 0 makes requests time out instantly; set ≥ 1",
        ));
    }

    out
}

/// Gather all findings for the given config + sessions files. No printing, no exit —
/// pure so it is testable.
pub fn collect_findings(config_path: &Path, data_dir: &Path) -> Vec<Finding> {
    let mut out = Vec::new();

    // ---- auth.toml ----
    let config = if config_path.exists() {
        match crate::config::load_config(config_path) {
            Ok(cfg) => {
                let names: Vec<&str> = cfg.accounts.iter().map(|a| a.name.as_str()).collect();
                out.push(Finding::ok(format!(
                    "auth.toml: valid — {} account(s){}",
                    cfg.accounts.len(),
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", names.join(", "))
                    }
                )));
                out.extend(validate_config(&cfg));
                Some(cfg)
            }
            Err(e) => {
                out.push(Finding::err(format!("auth.toml: {e}")));
                None
            }
        }
    } else {
        out.push(Finding::err(format!(
            "auth.toml not found: {} (run `tokenbar serve` or `tokenbar login <name>` to create it)",
            config_path.display()
        )));
        None
    };

    // ---- sessions.json ----
    let sessions_path = crate::session::resolve_sessions_path(data_dir);
    if sessions_path.exists() {
        match crate::session::load_sessions(&sessions_path) {
            Ok(s) => {
                let mut names: Vec<&str> = s.sessions.keys().map(|k| k.as_str()).collect();
                names.sort();
                out.push(Finding::ok(format!(
                    "sessions.json: valid — {} session(s){}",
                    names.len(),
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", names.join(", "))
                    }
                )));
                if let Some(cfg) = &config {
                    let known: std::collections::HashSet<&str> =
                        cfg.accounts.iter().map(|a| a.name.as_str()).collect();
                    for name in names {
                        if !known.contains(name) {
                            out.push(Finding::warn(format!(
                                "sessions.json: orphan session '{name}' — no matching account in auth.toml (remove with: tokenbar account logout {name})"
                            )));
                        }
                    }
                }
            }
            Err(e) => out.push(Finding::err(format!("sessions.json: {e}"))),
        }
    } else {
        out.push(Finding::ok(
            "sessions.json: not found (no sessions stored yet)",
        ));
    }

    out
}

/// Print findings and return Ok(()) when valid. Caller exits non-zero on Err.
pub fn run_check(config_path: &Path, data_dir: &Path) -> Result<(), AppError> {
    let findings = collect_findings(config_path, data_dir);
    let mut n_err = 0usize;
    let mut n_warn = 0usize;
    for f in &findings {
        let mark = match f.severity {
            Severity::Ok => "✓",
            Severity::Warn => {
                n_warn += 1;
                "⚠"
            }
            Severity::Err => {
                n_err += 1;
                "✗"
            }
        };
        println!("{mark} {}", f.text);
    }
    if n_err == 0 {
        if n_warn == 0 {
            println!("OK — config is valid");
        } else {
            println!("OK — config is valid ({n_warn} warning(s))");
        }
        Ok(())
    } else {
        println!("FAILED — {n_err} error(s), {n_warn} warning(s)");
        Err(AppError::Config("check failed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, AppConfig};

    fn account(name: &str, provider: ProviderKind) -> Account {
        Account {
            name: name.into(),
            provider,
            api_key: None,
        }
    }

    fn config_with(accounts: Vec<Account>) -> AppConfig {
        AppConfig {
            refresh_interval_secs: 60,
            request_timeout_secs: 15,
            max_concurrent_fetches: 4,
            accounts,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-check-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_config_no_findings() {
        let cfg = config_with(vec![account("a", ProviderKind::OpenCodeGo)]);
        assert!(validate_config(&cfg).is_empty());
    }

    #[test]
    fn duplicate_names_flagged() {
        let cfg = config_with(vec![
            account("a", ProviderKind::OpenCodeGo),
            account("a", ProviderKind::Zai),
        ]);
        let out = validate_config(&cfg);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Err && f.text.contains("duplicate")));
    }

    #[test]
    fn empty_name_flagged() {
        let cfg = config_with(vec![account("", ProviderKind::OpenCodeGo)]);
        let out = validate_config(&cfg);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Err && f.text.contains("empty name")));
    }

    #[test]
    fn zai_without_key_warns() {
        let cfg = config_with(vec![account("z", ProviderKind::Zai)]);
        let out = validate_config(&cfg);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Warn && f.text.contains("no api_key")));
    }

    #[test]
    fn zero_intervals_warn() {
        let mut cfg = config_with(vec![account("a", ProviderKind::OpenCodeGo)]);
        cfg.refresh_interval_secs = 0;
        cfg.request_timeout_secs = 0;
        let out = validate_config(&cfg);
        assert!(out.iter().any(|f| {
            f.severity == Severity::Warn && f.text.contains("refresh_interval_secs = 0")
        }));
        assert!(out.iter().any(|f| {
            f.severity == Severity::Warn && f.text.contains("request_timeout_secs = 0")
        }));
    }

    #[test]
    fn empty_accounts_warn() {
        let out = validate_config(&config_with(vec![]));
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Warn && f.text.contains("no accounts")));
    }

    #[test]
    fn collect_flags_bad_toml() {
        let dir = temp_dir();
        let cfg_path = dir.join("auth.toml");
        std::fs::write(&cfg_path, "refresh_interval_secs = \n[[accounts]]\n").unwrap();
        let out = collect_findings(&cfg_path, &dir);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Err && f.text.starts_with("auth.toml")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_flags_missing_config() {
        let dir = temp_dir();
        let cfg_path = dir.join("auth.toml");
        let out = collect_findings(&cfg_path, &dir);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Err && f.text.contains("not found")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_flags_orphan_session() {
        let dir = temp_dir();
        let cfg_path = dir.join("auth.toml");
        std::fs::write(
            &cfg_path,
            "[[accounts]]\nname = \"a\"\nprovider = \"opencode_go\"\n",
        )
        .unwrap();
        let sessions_path = dir.join("sessions.json");
        std::fs::write(
            &sessions_path,
            "{\"sessions\":{\"ghost\":{\"cookie\":\"x\",\"updated_at\":\"2026-08-12T00:00:00Z\"}}}",
        )
        .unwrap();
        let out = collect_findings(&cfg_path, &dir);
        assert!(out
            .iter()
            .any(|f| f.severity == Severity::Warn && f.text.contains("orphan session 'ghost'")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_ok_for_valid_setup() {
        let dir = temp_dir();
        let cfg_path = dir.join("auth.toml");
        std::fs::write(
            &cfg_path,
            "[[accounts]]\nname = \"a\"\nprovider = \"opencode_go\"\n\n[[accounts]]\nname = \"z\"\nprovider = \"zai\"\napi_key = \"k\"\n",
        )
        .unwrap();
        let out = collect_findings(&cfg_path, &dir);
        assert!(
            !out.iter().any(|f| f.severity == Severity::Err),
            "unexpected errors: {:?}",
            out.iter()
                .filter(|f| f.severity == Severity::Err)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
