//! Resolve OpenCode Go API keys from external local stores via
//! `api_key_source` (e.g. `hermes:lks00` or `opencode`).
//!
//! The key is read at fetch time and never copied into auth.toml, so rotating
//! it in the source store is picked up automatically. Nothing in this module
//! ever prints, logs, or serializes the secret itself — only labels, store
//! names, and (for Hermes) display-only pool bookkeeping such as
//! `last_error_code`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;

/// Pool key Hermes uses for OpenCode Go credentials.
const HERMES_POOL_KEY: &str = "opencode-go";
/// Cap the label list in "unknown label" errors so the message stays readable.
const MAX_LABELS_IN_ERROR: usize = 10;

/// A resolved API key plus safe, displayable provenance.
///
/// `key` is the secret — never print, log, or serialize it. Everything else
/// is safe to show.
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    /// The secret itself. Only for the `Authorization: Bearer` header.
    pub key: String,
    /// Canonical source reference the user wrote / can write again, e.g.
    /// `hermes:lks00` or `opencode`. `None` when the key came from
    /// `account.api_key` directly (no external store).
    pub source: Option<String>,
    /// Store the key was read from: `hermes`, `opencode`, or `auth.toml`
    /// (explicit key, no store).
    #[allow(dead_code)] // provenance kept for display/TUI use; DTO uses `source`
    pub store: &'static str,
    /// Label inside the store (`hermes:<label>`); `None` for the opencode
    /// store and for explicit keys.
    #[allow(dead_code)] // provenance kept for display/TUI use; DTO uses `source`
    pub label: Option<String>,
    /// Store bookkeeping as a short display string (e.g. Hermes
    /// `last_error_code`/`last_error_reason`). Metadata only — never a usage
    /// meter and never this account's error state.
    pub status: Option<String>,
}

impl ResolvedKey {
    /// Wrap an explicit `account.api_key` (no store provenance).
    pub fn explicit(key: &str) -> Self {
        Self {
            key: key.to_string(),
            source: None,
            store: "auth.toml",
            label: None,
            status: None,
        }
    }
}

/// One row of the key-source listing for the dashboard: labels only.
#[derive(Debug, Clone)]
pub struct KeySourceEntry {
    pub store: &'static str,
    pub label: Option<String>,
    /// Resolvable here (a stored, non-blank key exists).
    pub usable: bool,
    /// Why the entry is not usable, or why the store could not be read.
    pub reason: Option<String>,
    /// Store bookkeeping (display only), when present.
    pub status: Option<String>,
}

impl KeySourceEntry {
    /// The `api_key_source` reference for this entry, e.g. `hermes:lks00`.
    pub fn source_ref(&self) -> String {
        match &self.label {
            Some(label) => format!("{}:{}", self.store, label),
            None => self.store.to_string(),
        }
    }
}

/// Default path of the Hermes credential pool: `~/.hermes/auth.json`.
pub fn default_hermes_path() -> Result<PathBuf, AppError> {
    let home = dirs::home_dir().ok_or_else(|| {
        AppError::Config("Cannot determine home directory for ~/.hermes/auth.json".into())
    })?;
    Ok(home.join(".hermes").join("auth.json"))
}

/// Default path of opencode's own auth store:
/// `$OPENCODE_DATA_DIR/auth.json`, else `$XDG_DATA_HOME/opencode/auth.json`,
/// else `~/.local/share/opencode/auth.json` (the order opencode itself uses).
pub fn default_opencode_path() -> Result<PathBuf, AppError> {
    if let Ok(dir) = std::env::var("OPENCODE_DATA_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir).join("auth.json"));
        }
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir).join("opencode").join("auth.json"));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| {
        AppError::Config("Cannot determine home directory for opencode auth.json".into())
    })?;
    Ok(home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("auth.json"))
}

/// Resolve `source` (`hermes:<label>` or `opencode`, case-insensitive prefix,
/// surrounding whitespace trimmed) using the default store paths.
pub fn resolve(source: &str) -> Result<ResolvedKey, AppError> {
    let hermes = default_hermes_path()?;
    let opencode = default_opencode_path()?;
    resolve_with(source, &hermes, &opencode)
}

/// Path-injectable [`resolve`] so tests use fixtures and never read the real
/// `~/.hermes/auth.json` / opencode store.
pub fn resolve_with(
    source: &str,
    hermes_path: &Path,
    opencode_path: &Path,
) -> Result<ResolvedKey, AppError> {
    let src = source.trim();
    let bytes = src.as_bytes();
    if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"hermes:") {
        // The first 7 bytes matched ASCII "hermes:", so index 7 is a valid
        // char boundary; the label keeps its original case.
        resolve_hermes(src[7..].trim(), hermes_path)
    } else if src.eq_ignore_ascii_case("opencode") {
        resolve_opencode(opencode_path)
    } else {
        Err(AppError::Config(format!(
            "unknown api_key_source '{src}' — supported: 'hermes:<label>' (reads ~/.hermes/auth.json) and 'opencode' (reads the opencode auth store)"
        )))
    }
}

/// Cheap syntax check without touching any file: is this a supported
/// `api_key_source` shape? Used by `tokenbar check`.
pub fn source_prefix_supported(source: &str) -> bool {
    let src = source.trim();
    let bytes = src.as_bytes();
    (bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"hermes:"))
        || src.eq_ignore_ascii_case("opencode")
}

/// List what the local stores actually contain for OpenCode Go — labels only,
/// never values. Each store that cannot be read yields a single unusable
/// entry carrying the reason, so the dashboard can show it.
pub fn list_available(hermes_path: &Path, opencode_path: &Path) -> Vec<KeySourceEntry> {
    let mut out = hermes_entries(hermes_path).unwrap_or_else(|e| {
        vec![KeySourceEntry {
            store: "hermes",
            label: None,
            usable: false,
            reason: Some(e.to_string()),
            status: None,
        }]
    });
    out.push(opencode_entry(opencode_path));
    out
}

/// [`list_available`] with the default store paths.
pub fn list_available_default() -> Vec<KeySourceEntry> {
    let (hermes, opencode) = match (default_hermes_path(), default_opencode_path()) {
        (Ok(h), Ok(o)) => (h, o),
        _ => return Vec::new(),
    };
    list_available(&hermes, &opencode)
}

// ---- Hermes ----

#[derive(Debug, Deserialize)]
struct HermesAuth {
    #[serde(default)]
    credential_pool: std::collections::HashMap<String, Vec<HermesCredential>>,
}

#[derive(Debug, Deserialize)]
struct HermesCredential {
    #[serde(default)]
    id: String,
    label: String,
    #[serde(default)]
    auth_type: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    last_status: Option<Value>,
    #[serde(default)]
    last_error_code: Option<Value>,
    #[serde(default)]
    last_error_reason: Option<Value>,
}

fn resolve_hermes(label: &str, path: &Path) -> Result<ResolvedKey, AppError> {
    if label.is_empty() {
        return Err(AppError::Config(format!(
            "hermes key source needs a label, e.g. 'hermes:lks00' (credentials in {})",
            path.display()
        )));
    }

    let entries = hermes_pool(path)?;
    let available = entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>();

    let entry = entries
        .iter()
        .filter(|e| e.label == label)
        .min_by_key(|e| (e.priority.unwrap_or(i64::MAX), e.id.clone()));

    let entry = entry.ok_or_else(|| {
        AppError::Config(format!(
            "no credential labelled '{label}' in {} (available: {})",
            path.display(),
            format_label_list(&available),
        ))
    })?;

    let token = entry
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let token = token.ok_or_else(|| {
        let kind = entry.auth_type.as_deref().unwrap_or("?");
        match entry.source.as_deref() {
            Some(src) if src.starts_with("env:") => AppError::Config(format!(
                "credential '{label}' in {} is env-sourced ({src}) and stores no key here — not resolvable by tokenbar; pick a manual credential",
                path.display()
            )),
            _ => AppError::Config(format!(
                "credential '{label}' in {} (auth_type {kind}) has no usable access_token",
                path.display()
            )),
        }
    })?;

    Ok(ResolvedKey {
        key: token.to_string(),
        source: Some(format!("hermes:{label}")),
        store: "hermes",
        label: Some(label.to_string()),
        status: hermes_status(entry),
    })
}

/// Read and validate the Hermes pool file.
fn hermes_pool(path: &Path) -> Result<Vec<HermesCredential>, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!(
            "hermes credential store not found: {} (expected Hermes' auth.json)",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("cannot read {}: {e}", path.display())))?;
    let mut auth: HermesAuth = serde_json::from_str(&text).map_err(|e| {
        AppError::Config(format!("cannot parse hermes store {}: {e}", path.display()))
    })?;
    Ok(auth
        .credential_pool
        .remove(HERMES_POOL_KEY)
        .unwrap_or_default())
}

/// Listing rows for the Hermes pool: one per credential, labels only.
fn hermes_entries(path: &Path) -> Result<Vec<KeySourceEntry>, AppError> {
    let pool = hermes_pool(path)?;
    Ok(pool
        .into_iter()
        .map(|entry| {
            let status = hermes_status(&entry);
            let usable = entry
                .access_token
                .as_deref()
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false);
            let reason = if usable {
                None
            } else {
                Some(match entry.source.as_deref() {
                    Some(src) if src.starts_with("env:") => {
                        format!("env-sourced ({src}) — no key stored here")
                    }
                    _ => "no stored key".to_string(),
                })
            };
            KeySourceEntry {
                store: "hermes",
                label: Some(entry.label),
                usable,
                reason,
                status,
            }
        })
        .collect())
}

/// Hermes pool bookkeeping as a short display string. Display metadata only:
/// a 429 `GoUsageLimitError` means the key is valid but a window is capped —
/// it is not this account's usage meter or error state.
fn hermes_status(entry: &HermesCredential) -> Option<String> {
    let code = json_str(&entry.last_error_code);
    let reason = json_str(&entry.last_error_reason);
    if let Some(code) = code {
        return Some(match reason {
            Some(reason) => format!("{code} {reason}"),
            None => code,
        });
    }
    json_str(&entry.last_status)
}

fn json_str(v: &Option<Value>) -> Option<String> {
    match v {
        None => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn format_label_list(labels: &[&str]) -> String {
    if labels.is_empty() {
        return "none".to_string();
    }
    let mut shown: Vec<&str> = labels.to_vec();
    shown.sort_unstable();
    shown.dedup();
    if shown.len() > MAX_LABELS_IN_ERROR {
        format!(
            "{} and {} more",
            shown[..MAX_LABELS_IN_ERROR].join(", "),
            shown.len() - MAX_LABELS_IN_ERROR
        )
    } else {
        shown.join(", ")
    }
}

// ---- OpenCode ----

fn resolve_opencode(path: &Path) -> Result<ResolvedKey, AppError> {
    let key = opencode_stored_key(path)?;
    Ok(ResolvedKey {
        key,
        source: Some("opencode".to_string()),
        store: "opencode",
        label: None,
        status: None,
    })
}

/// Read the `opencode-go` API key from opencode's own auth.json.
fn opencode_stored_key(path: &Path) -> Result<String, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!(
            "opencode auth store not found: {} (log in with the opencode CLI to create it)",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("cannot read {}: {e}", path.display())))?;
    let val: Value = serde_json::from_str(&text).map_err(|e| {
        AppError::Config(format!(
            "cannot parse opencode store {}: {e}",
            path.display()
        ))
    })?;

    let entry = val.get(HERMES_POOL_KEY).ok_or_else(|| {
        let providers: Vec<String> = val
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        AppError::Config(format!(
            "no 'opencode-go' entry in {}{}",
            path.display(),
            if providers.is_empty() {
                String::new()
            } else {
                format!(" (providers present: {})", providers.join(", "))
            }
        ))
    })?;

    if let Some(kind) = entry.get("type").and_then(Value::as_str) {
        if kind != "api" {
            return Err(AppError::Config(format!(
                "'opencode-go' entry in {} has type '{kind}', not an api key",
                path.display()
            )));
        }
    }

    let key = entry
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            AppError::Config(format!(
                "'opencode-go' entry in {} has no key value",
                path.display()
            ))
        })?;
    Ok(key.to_string())
}

/// Listing row for the opencode store: a single (unlabelled) entry.
fn opencode_entry(path: &Path) -> KeySourceEntry {
    match opencode_stored_key(path) {
        Ok(_) => KeySourceEntry {
            store: "opencode",
            label: None,
            usable: true,
            reason: None,
            status: None,
        },
        Err(e) => KeySourceEntry {
            store: "opencode",
            label: None,
            usable: false,
            reason: Some(
                e.to_string()
                    .trim_start_matches("Configuration error: ")
                    .to_string(),
            ),
            status: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_HERMES: &str = include_str!("../tests/fixtures/keystores/hermes-auth.json");
    const FIXTURE_OPENCODE: &str = include_str!("../tests/fixtures/keystores/opencode-auth.json");

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // pid + per-process counter: collision-proof under parallel runs.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tokenbar-keysource-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture_store() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = temp_dir();
        let hermes = dir.join("hermes-auth.json");
        let opencode = dir.join("opencode-auth.json");
        std::fs::write(&hermes, FIXTURE_HERMES).unwrap();
        std::fs::write(&opencode, FIXTURE_OPENCODE).unwrap();
        (hermes, opencode)
    }

    // ---- resolution ----

    #[test]
    fn hermes_label_resolves_dummy_key_with_provenance() {
        let (hermes, opencode) = fixture_store();
        let rk = resolve_with("hermes:lks00", &hermes, &opencode).unwrap();
        assert_eq!(rk.key, "sk-test-hermes-lks00");
        assert_eq!(rk.source.as_deref(), Some("hermes:lks00"));
        assert_eq!(rk.store, "hermes");
        assert_eq!(rk.label.as_deref(), Some("lks00"));
        assert!(rk.status.is_none(), "clean entry has no status");
    }

    #[test]
    fn prefix_is_case_insensitive_and_whitespace_trimmed() {
        let (hermes, opencode) = fixture_store();
        for src in ["HERMES:lks00", "  hermes:lks00  ", "\tOpencode "] {
            let rk = resolve_with(src, &hermes, &opencode)
                .unwrap_or_else(|e| panic!("{src} should resolve: {e}"));
            if src.trim().to_ascii_lowercase().starts_with("hermes") {
                assert_eq!(rk.key, "sk-test-hermes-lks00", "for source {src}");
            } else {
                assert_eq!(rk.key, "sk-test-opencode", "for source {src}");
            }
        }
    }

    #[test]
    fn hermes_unknown_label_lists_available_labels() {
        let (hermes, opencode) = fixture_store();
        let err = resolve_with("hermes:ghost", &hermes, &opencode).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no credential labelled 'ghost'"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("lks00"), "available labels missing: {msg}");
        assert!(msg.contains("laut9810"), "available labels missing: {msg}");
        assert!(msg.contains("opencode-ckibashi"), "{msg}");
        assert!(
            !msg.contains("sk-test-"),
            "error must never include the secret: {msg}"
        );
    }

    #[test]
    fn hermes_env_sourced_without_token_is_unusable_not_bearer_blank() {
        let (hermes, opencode) = fixture_store();
        // The env-sourced fixture entry deliberately has no access_token.
        let err = resolve_with("hermes:opencode-ckibashi", &hermes, &opencode).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("env-sourced") || msg.contains("not resolvable"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("env:OPENCODE_GO_API_KEY"),
            "should name the env var: {msg}"
        );
        assert!(
            !msg.contains("sk-test-"),
            "error must never include the secret: {msg}"
        );
    }

    #[test]
    fn hermes_pool_bookkeeping_is_display_metadata_only() {
        let (hermes, opencode) = fixture_store();
        let rk = resolve_with("hermes:capped", &hermes, &opencode).unwrap();
        // The 429 GoUsageLimitError entry still resolves — the key itself is
        // valid; the window cap is Hermes-side display metadata.
        assert_eq!(rk.key, "sk-test-hermes-capped");
        let status = rk.status.expect("status metadata");
        assert!(
            status.contains("429"),
            "status should carry the code: {status}"
        );
        assert!(
            status.contains("GoUsageLimitError"),
            "status should carry the reason: {status}"
        );
    }

    #[test]
    fn hermes_missing_file_is_clear_config_error() {
        let dir = temp_dir();
        let missing = dir.join("nope-hermes.json");
        let opencode = dir.join("nope-opencode.json");
        let err = resolve_with("hermes:lks00", &missing, &opencode).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hermes credential store not found"), "{msg}");
        assert!(
            msg.contains(missing.display().to_string().as_str()),
            "{msg}"
        );
    }

    #[test]
    fn hermes_empty_label_is_rejected() {
        let (hermes, opencode) = fixture_store();
        let err = resolve_with("hermes:", &hermes, &opencode).unwrap_err();
        assert!(err.to_string().contains("needs a label"), "{err}");
    }

    #[test]
    fn opencode_resolves_without_label() {
        let (hermes, opencode) = fixture_store();
        let rk = resolve_with("opencode", &hermes, &opencode).unwrap();
        assert_eq!(rk.key, "sk-test-opencode");
        assert_eq!(rk.source.as_deref(), Some("opencode"));
        assert_eq!(rk.store, "opencode");
        assert!(rk.label.is_none());
    }

    #[test]
    fn opencode_missing_file_is_clear_config_error() {
        let dir = temp_dir();
        let hermes = dir.join("nope-hermes.json");
        let missing = dir.join("nope-opencode.json");
        let err = resolve_with("opencode", &hermes, &missing).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("opencode auth store not found"), "{msg}");
        assert!(
            msg.contains(missing.display().to_string().as_str()),
            "{msg}"
        );
    }

    #[test]
    fn opencode_wrong_type_or_missing_key_is_unusable() {
        let (hermes, _opencode) = fixture_store();
        let dir = temp_dir();

        let oauth_path = dir.join("oc-oauth.json");
        std::fs::write(&oauth_path, r#"{"opencode-go":{"type":"oauth"}}"#).unwrap();
        let err = resolve_with("opencode", &hermes, &oauth_path).unwrap_err();
        assert!(
            err.to_string().contains("type 'oauth', not an api key"),
            "{}",
            err.to_string()
        );

        let nokey_path = dir.join("oc-nokey.json");
        std::fs::write(&nokey_path, r#"{"opencode-go":{"type":"api"}}"#).unwrap();
        let err = resolve_with("opencode", &hermes, &nokey_path).unwrap_err();
        assert!(
            err.to_string().contains("no key value"),
            "{}",
            err.to_string()
        );

        let other_path = dir.join("oc-other.json");
        std::fs::write(&other_path, r#"{"anthropic":{"type":"api","key":"x"}}"#).unwrap();
        let err = resolve_with("opencode", &hermes, &other_path).unwrap_err();
        assert!(
            err.to_string().contains("no 'opencode-go' entry"),
            "should list present providers: {err}"
        );
        assert!(err.to_string().contains("anthropic"), "{err}");
    }

    #[test]
    fn unknown_prefix_names_supported_prefixes() {
        let (hermes, opencode) = fixture_store();
        for src in ["bitwarden:x", "", "   ", "hermes", "opencode:go"] {
            let err = resolve_with(src, &hermes, &opencode).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("hermes:<label>") && msg.contains("'opencode'"),
                "'{src}' should name supported prefixes: {msg}"
            );
        }
    }

    #[test]
    fn source_prefix_supported_syntax_check() {
        assert!(source_prefix_supported("hermes:lks00"));
        assert!(source_prefix_supported(" Hermes:lks00 "));
        assert!(source_prefix_supported("opencode"));
        assert!(source_prefix_supported("OPENCODE"));
        assert!(!source_prefix_supported("bitwarden:x"));
        assert!(!source_prefix_supported(""));
    }

    // ---- listing ----

    #[test]
    fn listing_reports_labels_and_usability_without_values() {
        let (hermes, opencode) = fixture_store();
        let entries = list_available(&hermes, &opencode);

        let by_ref: std::collections::HashMap<String, &KeySourceEntry> = entries
            .iter()
            .map(|e| (e.source_ref(), e))
            .collect::<std::collections::HashMap<_, _>>();

        let lks = by_ref["hermes:lks00"];
        assert!(lks.usable);
        assert_eq!(lks.store, "hermes");

        // Env-sourced entry: present, labelled, but not usable here.
        let env = by_ref["hermes:opencode-ckibashi"];
        assert!(!env.usable);
        let reason = env.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("env:OPENCODE_GO_API_KEY"), "{reason}");

        let oc = by_ref["opencode"];
        assert!(oc.usable);

        // Never any secret material in a serialized listing.
        let json = serde_json::to_string(
            &entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "store": e.store,
                        "label": e.label,
                        "usable": e.usable,
                        "reason": e.reason,
                        "status": e.status,
                        "source_ref": e.source_ref(),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!json.contains("sk-test-"), "listing leaked a key: {json}");
    }

    #[test]
    fn listing_marks_missing_stores_with_reason() {
        let dir = temp_dir();
        let missing = dir.join("nope.json");
        let entries = list_available(&missing, &missing);
        assert_eq!(entries.len(), 2, "one row per store");
        assert!(entries.iter().all(|e| !e.usable));
        assert!(entries.iter().all(|e| e
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("nope.json")));
    }
}
