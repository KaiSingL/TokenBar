pub mod grok;
pub mod opencodego;
pub mod zai;

use std::time::Duration;

use crate::error::AppError;
use crate::keysource::ResolvedKey;
use crate::model::{Account, ProviderKind, SessionEntry, UsageSnapshot};

pub struct FetchResult {
    pub snapshot: UsageSnapshot,
    /// When set, replace the stored session for this account (e.g. refreshed Grok tokens).
    pub session: Option<SessionEntry>,
}

/// Fetch usage for an account using provider-specific credentials.
pub async fn fetch_for_account(
    account: &Account,
    session: Option<&SessionEntry>,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<FetchResult, AppError> {
    match account.provider {
        ProviderKind::OpenCodeGo => {
            // Key when resolvable (explicit api_key wins, then api_key_source),
            // cookie otherwise. A *broken* configured source surfaces as its
            // own Config error instead of silently falling back to a stored
            // cookie — the user explicitly asked for that store.
            match resolve_opencode_api_key_checked(account)? {
                Some(key) => {
                    let snapshot = opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                        .fetch_usage_via_api(&account.name, &key.key)
                        .await?;
                    Ok(FetchResult {
                        snapshot,
                        session: None,
                    })
                }
                None => {
                    let entry = session.ok_or(AppError::InvalidCredentials)?;
                    let snapshot = opencodego::OpenCodeGoProvider::new(client.clone(), timeout)
                        .fetch_usage(&account.name, &entry.cookie, entry.workspace_id.as_deref())
                        .await?;
                    Ok(FetchResult {
                        snapshot,
                        session: None,
                    })
                }
            }
        }
        ProviderKind::Zai => {
            let api_key = resolve_zai_api_key(account).ok_or(AppError::InvalidCredentials)?;
            let snapshot = zai::ZaiProvider::new(client.clone(), timeout)
                .fetch_usage(&account.name, &api_key)
                .await?;
            Ok(FetchResult {
                snapshot,
                session: None,
            })
        }
        ProviderKind::Grok => {
            let entry = session.ok_or(AppError::InvalidCredentials)?;
            let (snapshot, session) = grok::GrokProvider::new(client.clone(), timeout)
                .fetch_usage(&account.name, entry)
                .await?;
            Ok(FetchResult { snapshot, session })
        }
    }
}

pub fn has_credentials(account: &Account, session: Option<&SessionEntry>) -> bool {
    match account.provider {
        ProviderKind::OpenCodeGo => {
            let has_api_key = non_blank(account.api_key.as_deref()).is_some();
            // Presence is enough here: resolution quality is the fetcher's
            // job, and a broken source must be visible as an account error.
            let has_source = non_blank(account.api_key_source.as_deref()).is_some();
            has_api_key
                || has_source
                || session
                    .map(|s| !s.cookie.trim().is_empty())
                    .unwrap_or(false)
        }
        ProviderKind::Zai => resolve_zai_api_key(account).is_some(),
        ProviderKind::Grok => session.map(|s| s.has_grok_session()).unwrap_or(false),
    }
}

/// The OpenCode Go API key for this account, when one is resolvable:
///
/// 1. an explicit `account.api_key` always wins;
/// 2. else `account.api_key_source` is resolved from its store;
/// 3. else `None` (scraped-cookie path as today).
///
/// Returns `None` both for "no key configured" and for a source that cannot
/// be resolved here (e.g. a Hermes env credential with no stored token) —
/// use [`resolve_opencode_api_key_checked`] when the failure reason matters.
pub fn resolve_opencode_api_key(account: &Account) -> Option<ResolvedKey> {
    resolve_opencode_api_key_checked(account).ok().flatten()
}

/// [`resolve_opencode_api_key`] that keeps the reason a configured source
/// could not be resolved: `Err` means the source is broken (bad label,
/// missing store file) and must surface on the account rather than silently
/// falling back to the cookie path.
pub fn resolve_opencode_api_key_checked(
    account: &Account,
) -> Result<Option<ResolvedKey>, AppError> {
    if let Some(key) = non_blank(account.api_key.as_deref()) {
        return Ok(Some(ResolvedKey::explicit(key)));
    }
    if let Some(source) = non_blank(account.api_key_source.as_deref()) {
        return crate::keysource::resolve(source).map(Some);
    }
    Ok(None)
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

pub fn resolve_zai_api_key(account: &Account) -> Option<String> {
    if let Some(key) = account
        .api_key
        .as_ref()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return Some(key);
    }
    std::env::var("Z_AI_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go_account(api_key: Option<&str>, api_key_source: Option<&str>) -> Account {
        Account {
            name: "go".into(),
            provider: ProviderKind::OpenCodeGo,
            api_key: api_key.map(str::to_string),
            api_key_source: api_key_source.map(str::to_string),
        }
    }

    fn session_with_cookie(cookie: &str) -> SessionEntry {
        SessionEntry {
            cookie: cookie.into(),
            workspace_id: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            email: None,
            user_id: None,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn precedence_explicit_key_beats_source_without_touching_stores() {
        // The explicit branch returns before any store is read, so this is
        // deterministic even on a machine with real stores configured.
        let account = go_account(Some("sk-explicit"), Some("bitwarden:x"));
        let rk = resolve_opencode_api_key_checked(&account)
            .expect("explicit key must never fail resolution")
            .expect("some key");
        assert_eq!(rk.key, "sk-explicit");
        assert!(rk.source.is_none(), "explicit key has no store provenance");
        assert_eq!(rk.store, "auth.toml");
    }

    #[test]
    fn precedence_source_resolves_from_fixture_store() {
        // Wire the fixture store in through keysource::resolve_with directly,
        // mirroring what resolve_opencode_api_key does against real paths.
        let dir =
            std::env::temp_dir().join(format!("tokenbar-api-precedence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hermes = dir.join("hermes-auth.json");
        let opencode = dir.join("opencode-auth.json");
        std::fs::write(
            &hermes,
            include_str!("../../tests/fixtures/keystores/hermes-auth.json"),
        )
        .unwrap();
        std::fs::write(
            &opencode,
            include_str!("../../tests/fixtures/keystores/opencode-auth.json"),
        )
        .unwrap();

        let rk = crate::keysource::resolve_with("hermes:lks00", &hermes, &opencode).unwrap();
        assert_eq!(rk.key, "sk-test-hermes-lks00");

        let rk = crate::keysource::resolve_with("opencode", &hermes, &opencode).unwrap();
        assert_eq!(rk.key, "sk-test-opencode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_key_and_no_source_is_none_for_cookie_path() {
        let account = go_account(None, None);
        assert!(resolve_opencode_api_key_checked(&account)
            .expect("no error")
            .is_none());
        assert!(resolve_opencode_api_key(&account).is_none());
        // Blank strings count as absent.
        let account = go_account(Some("   "), Some("  "));
        assert!(resolve_opencode_api_key(&account).is_none());
    }

    #[test]
    fn has_credentials_accepts_key_source_or_cookie() {
        assert!(has_credentials(&go_account(Some("k"), None), None));
        assert!(has_credentials(
            &go_account(None, Some("hermes:lks00")),
            None
        ));
        assert!(has_credentials(
            &go_account(None, None),
            Some(&session_with_cookie("auth=x"))
        ));
        assert!(!has_credentials(&go_account(None, None), None));
        assert!(!has_credentials(
            &go_account(Some("  "), Some(" ")),
            Some(&session_with_cookie(""))
        ));
    }

    #[tokio::test]
    async fn broken_source_surfaces_config_error_instead_of_cookie_fallback() {
        // Unknown store prefix: fails before any network I/O, even though a
        // usable cookie session exists — the user asked for a specific store.
        let account = go_account(None, Some("bitwarden:lks00"));
        let session = session_with_cookie("auth=Fe26.2**valid-looking-session");

        let client = reqwest::Client::builder().build().unwrap();
        let err = fetch_for_account(&account, Some(&session), &client, Duration::from_secs(5))
            .await
            .err()
            .expect("broken source must surface");
        assert!(
            matches!(err, AppError::Config(ref msg) if msg.contains("supported")),
            "expected a Config error naming supported prefixes, got: {err}"
        );
    }

    #[tokio::test]
    async fn no_credentials_at_all_is_invalid_credentials() {
        let account = go_account(None, None);
        let client = reqwest::Client::builder().build().unwrap();
        let err = fetch_for_account(&account, None, &client, Duration::from_secs(5))
            .await
            .err()
            .expect("must fail");
        assert!(matches!(err, AppError::InvalidCredentials));
    }
}
