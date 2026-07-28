//! SpaceXAI OAuth2 device-code flow — same idea as `grok login --device-auth`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::debug;

use crate::error::AppError;

pub const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const USERINFO_URL: &str = "https://auth.x.ai/oauth2/userinfo";
/// Public Grok CLI OIDC client id.
pub const DEFAULT_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const DEFAULT_SCOPES: &str =
    "openid profile email offline_access api:access grok-cli:access";

const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Prefer this for the browser when present (often includes the user code).
    pub verification_uri_complete: Option<String>,
    pub interval_secs: u64,
    pub expires_in: u64,
}

pub fn client_id() -> String {
    std::env::var("TOKENBAR_GROK_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

pub fn browser_url(auth: &DeviceAuthorization) -> &str {
    auth.verification_uri_complete
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(auth.verification_uri.as_str())
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub fn request_device_code(
    client: &reqwest::blocking::Client,
    client_id: &str,
) -> Result<DeviceAuthorization, AppError> {
    let body = format!(
        "client_id={}&scope={}",
        urlencoding::encode(client_id),
        urlencoding::encode(DEFAULT_SCOPES),
    );
    let resp = client
        .post(DEVICE_CODE_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .map_err(|e| AppError::Login(format!("Device code request failed: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| AppError::Login(format!("Device code response read failed: {e}")))?;
    if !status.is_success() {
        return Err(AppError::Login(format!(
            "Device code HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        )));
    }

    let raw: DeviceCodeResponse = serde_json::from_str(&text)
        .map_err(|e| AppError::Login(format!("Device code JSON: {e}")))?;

    Ok(DeviceAuthorization {
        device_code: raw.device_code,
        user_code: raw.user_code,
        verification_uri: raw.verification_uri,
        verification_uri_complete: raw.verification_uri_complete,
        interval_secs: raw.interval.unwrap_or(5).max(1),
        expires_in: raw.expires_in.unwrap_or(600).max(60),
    })
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug)]
enum PollOutcome {
    Tokens(OAuthTokens),
    Pending { slow_down: bool },
}

/// Poll until authorized, denied, expired, cancelled, or timeout.
pub fn poll_device_token(
    client: &reqwest::blocking::Client,
    client_id: &str,
    device_code: &str,
    mut interval_secs: u64,
    expires_in: u64,
    cancel: &mpsc::Receiver<()>,
) -> Result<OAuthTokens, AppError> {
    let deadline = Instant::now() + Duration::from_secs(expires_in.min(900));
    let mut interval = Duration::from_secs(interval_secs.max(1));

    loop {
        if cancel.try_recv().is_ok() {
            return Err(AppError::Login("Login cancelled".into()));
        }
        if Instant::now() >= deadline {
            return Err(AppError::Login(
                "Device authorization timed out. Run login again.".into(),
            ));
        }

        match poll_once(client, client_id, device_code)? {
            PollOutcome::Tokens(t) => return Ok(t),
            PollOutcome::Pending { slow_down } => {
                if slow_down {
                    interval_secs = interval_secs.saturating_add(5).max(interval_secs + 1);
                    interval = Duration::from_secs(interval_secs);
                    debug!("device poll slow_down → interval={interval_secs}s");
                }
            }
        }

        // Sleep in small slices so cancel is responsive.
        let slice = Duration::from_millis(200);
        let mut slept = Duration::ZERO;
        while slept < interval {
            if cancel.try_recv().is_ok() {
                return Err(AppError::Login("Login cancelled".into()));
            }
            std::thread::sleep(slice);
            slept += slice;
        }
    }
}

fn poll_once(
    client: &reqwest::blocking::Client,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome, AppError> {
    let body = format!(
        "grant_type={}&device_code={}&client_id={}",
        urlencoding::encode(GRANT_DEVICE_CODE),
        urlencoding::encode(device_code),
        urlencoding::encode(client_id),
    );
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .map_err(|e| AppError::Login(format!("Token poll failed: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| AppError::Login(format!("Token poll read failed: {e}")))?;

    let tok: TokenResponse = serde_json::from_str(&text).unwrap_or(TokenResponse {
        access_token: None,
        refresh_token: None,
        expires_in: None,
        id_token: None,
        error: None,
        error_description: Some(text.chars().take(200).collect()),
    });

    if let Some(err) = tok.error.as_deref() {
        return match err {
            "authorization_pending" => Ok(PollOutcome::Pending { slow_down: false }),
            "slow_down" => Ok(PollOutcome::Pending { slow_down: true }),
            "expired_token" | "expired_token_code" => Err(AppError::Login(
                "Device code expired. Run login again.".into(),
            )),
            "access_denied" => Err(AppError::Login("OAuth access denied by user.".into())),
            other => {
                let desc = tok.error_description.unwrap_or_default();
                Err(AppError::Login(format!("OAuth error: {other} {desc}")))
            }
        };
    }

    if !status.is_success() {
        return Err(AppError::Login(format!(
            "Token poll HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        )));
    }

    let access_token = tok
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Login("Token response missing access_token".into()))?;

    let expires_at = tok
        .expires_in
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs.max(0)));

    let mut email = None;
    let mut user_id = None;
    if let Ok(ui) = fetch_userinfo(client, &access_token) {
        email = ui.email;
        user_id = ui.sub;
    }
    if user_id.is_none() {
        if let Some(id_tok) = tok.id_token.as_deref() {
            if let Some((sub, em)) = parse_id_token_claims(id_tok) {
                user_id = sub;
                if email.is_none() {
                    email = em;
                }
            }
        }
    }

    Ok(PollOutcome::Tokens(OAuthTokens {
        access_token,
        refresh_token: tok.refresh_token,
        expires_at,
        email,
        user_id,
    }))
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

fn fetch_userinfo(
    client: &reqwest::blocking::Client,
    access_token: &str,
) -> Result<UserInfo, AppError> {
    let resp = client
        .get(USERINFO_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| AppError::Login(format!("userinfo failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Login(format!(
            "userinfo HTTP {}",
            resp.status()
        )));
    }
    resp.json()
        .map_err(|e| AppError::Login(format!("userinfo JSON: {e}")))
}

fn parse_id_token_claims(id_token: &str) -> Option<(Option<String>, Option<String>)> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let mut parts = id_token.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let sub = v.get("sub").and_then(|x| x.as_str()).map(str::to_string);
    let email = v.get("email").and_then(|x| x.as_str()).map(str::to_string);
    Some((sub, email))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_url_prefers_complete() {
        let a = DeviceAuthorization {
            device_code: "d".into(),
            user_code: "U".into(),
            verification_uri: "https://auth.x.ai/device".into(),
            verification_uri_complete: Some("https://auth.x.ai/device?user_code=U".into()),
            interval_secs: 5,
            expires_in: 600,
        };
        assert!(browser_url(&a).contains("user_code=U"));
    }

    #[test]
    fn parse_pending_error_shapes() {
        // Ensure serde accepts minimal pending-style token error bodies.
        let j = r#"{"error":"authorization_pending"}"#;
        let t: TokenResponse = serde_json::from_str(j).unwrap();
        assert_eq!(t.error.as_deref(), Some("authorization_pending"));
    }
}
