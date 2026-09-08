use std::time::Duration;
use std::{env, thread};

use caudra_storage::StateDir;
use caudra_storage::auth::{
    OAuthTokens, delete_tokens, lock_provider_auth, now_millis, save_tokens, try_load_tokens,
};
use isahc::ReadResponseExt;
use isahc::config::{Configurable, VersionNegotiation};
use serde::Deserialize;
use tracing::{debug, error};

use crate::AgentError;
use crate::providers::oauth::{self, RefreshReason};
use crate::providers::{ResolvedAuth, urlenc};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const PROVIDER: &str = "openai";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    expires_in: Option<u64>,
}

fn http_client(timeout: Duration) -> Result<isahc::HttpClient, AgentError> {
    isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("http client: {e}"),
        })
}

fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    claims
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|org| org.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
}

fn extract_account_id_from_tokens(resp: &TokenResponse) -> Option<String> {
    if let Some(id_token) = &resp.id_token
        && let Some(id) = extract_account_id(id_token)
    {
        return Some(id);
    }
    extract_account_id(&resp.access_token)
}

fn request_device_code() -> Result<DeviceCodeResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let body = serde_json::json!({"client_id": CLIENT_ID});
    let json_body = serde_json::to_vec(&body)?;

    let request = isahc::Request::builder()
        .method("POST")
        .uri(DEVICE_CODE_URL)
        .header("content-type", "application/json")
        .body(json_body)?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("device code request: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!("device code request failed: {body_text}"),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(Into::into)
}

fn poll_device_token(device: &DeviceCodeResponse) -> Result<DeviceTokenResponse, AgentError> {
    let client = http_client(POLL_TIMEOUT)?;
    let interval_secs = device.interval.parse::<u64>().unwrap_or(5).max(1);
    let poll_interval = Duration::from_secs(interval_secs) + POLL_SAFETY_MARGIN;
    let deadline = std::time::Instant::now() + POLL_TIMEOUT;

    let body = serde_json::json!({
        "device_auth_id": device.device_auth_id,
        "user_code": device.user_code,
    });
    let json_body = serde_json::to_vec(&body)?;

    loop {
        if std::time::Instant::now() > deadline {
            return Err(AgentError::Config {
                message: "device authorization timed out".into(),
            });
        }

        thread::sleep(poll_interval);

        let request = isahc::Request::builder()
            .method("POST")
            .uri(DEVICE_TOKEN_URL)
            .header("content-type", "application/json")
            .body(json_body.clone())?;

        let mut resp = client.send(request).map_err(|e| AgentError::Config {
            message: format!("device token poll: {e}"),
        })?;

        if resp.status().as_u16() == 200 {
            let body_text = resp.text()?;
            return serde_json::from_str(&body_text).map_err(Into::into);
        }

        let status = resp.status().as_u16();
        if status != 403 && status != 404 {
            let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
            return Err(AgentError::Config {
                message: format!("device token poll failed ({status}): {body_text}"),
            });
        }
    }
}

fn exchange_device_token(device_token: &DeviceTokenResponse) -> Result<TokenResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;

    let form_body = format!(
        "grant_type=authorization_code\
         &code={}\
         &redirect_uri={}\
         &client_id={}\
         &code_verifier={}",
        urlenc(&device_token.authorization_code),
        urlenc(REDIRECT_URI),
        urlenc(CLIENT_ID),
        urlenc(&device_token.code_verifier),
    );

    let request = isahc::Request::builder()
        .method("POST")
        .uri(OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body.into_bytes())?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("token exchange: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!("token exchange failed: {body_text}"),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(Into::into)
}

fn into_oauth_tokens(
    resp: TokenResponse,
    fallback: Option<&OAuthTokens>,
) -> Result<OAuthTokens, AgentError> {
    if resp.access_token.is_empty() {
        return Err(AgentError::Config {
            message: "OpenAI token response did not include an access token".into(),
        });
    }
    let account_id = extract_account_id_from_tokens(&resp)
        .or_else(|| fallback.and_then(|tokens| tokens.account_id.clone()));
    let refresh = resp
        .refresh_token
        .filter(|token| !token.is_empty())
        .or_else(|| fallback.map(|tokens| tokens.refresh.clone()))
        .ok_or_else(|| AgentError::Config {
            message: "OpenAI token response did not include a refresh token".into(),
        })?;
    let expires = now_millis() + resp.expires_in.unwrap_or(3600) * 1000;
    Ok(OAuthTokens {
        access: resp.access_token,
        refresh,
        expires,
        account_id,
    })
}

pub(crate) fn refresh_tokens(tokens: &OAuthTokens) -> Result<OAuthTokens, AgentError> {
    if tokens.refresh.is_empty() {
        return Err(AgentError::api(
            401,
            "OpenAI credentials do not include a refresh token",
        ));
    }
    let expired = tokens.is_expired();
    debug!(expired, "refreshing OpenAI OAuth tokens");

    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let form_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlenc(&tokens.refresh),
        urlenc(CLIENT_ID),
    );

    let request = isahc::Request::builder()
        .method("POST")
        .uri(OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body.into_bytes())?;

    let mut resp = client.send(request)?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::api(
            oauth_error_status(status, &body_text),
            format!("OpenAI token refresh failed ({status}): {body_text}"),
        ));
    }

    let body_text = resp.text()?;
    let token_resp: TokenResponse = serde_json::from_str(&body_text)?;
    into_oauth_tokens(token_resp, Some(tokens))
}

fn oauth_error_code(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .get("error")
        .and_then(|error| error.as_str().or_else(|| error.get("type")?.as_str()))
        .map(ToOwned::to_owned)
}

fn oauth_error_status(status: u16, body: &str) -> u16 {
    if oauth_error_code(body).as_deref() == Some("invalid_grant") {
        401
    } else {
        status
    }
}

pub(crate) fn refresh_from_storage(
    storage: &StateDir,
    rejected_accesses: &[String],
) -> Result<OAuthTokens, AgentError> {
    let reason = if rejected_accesses.is_empty() {
        RefreshReason::Proactive
    } else {
        RefreshReason::Rejected(rejected_accesses)
    };
    oauth::refresh_from_storage(storage, PROVIDER, reason, refresh_tokens)
}

pub(crate) const CODING_PLAN_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

pub(crate) fn build_oauth_resolved(tokens: &OAuthTokens) -> ResolvedAuth {
    ResolvedAuth {
        base_url: None,
        headers: vec![("authorization".into(), format!("Bearer {}", tokens.access))],
    }
}

pub(crate) fn build_coding_plan_resolved(tokens: &OAuthTokens) -> ResolvedAuth {
    let mut headers = vec![("authorization".into(), format!("Bearer {}", tokens.access))];
    if let Some(account_id) = &tokens.account_id {
        headers.push(("chatgpt-account-id".into(), account_id.clone()));
    }
    ResolvedAuth {
        base_url: Some(CODING_PLAN_BASE_URL.into()),
        headers,
    }
}

pub fn resolve(dir: &StateDir) -> Result<ResolvedAuth, AgentError> {
    resolve_with_tokens(dir).map(|(resolved, _)| resolved)
}

pub(crate) fn resolve_with_tokens(
    dir: &StateDir,
) -> Result<(ResolvedAuth, Option<OAuthTokens>), AgentError> {
    if let Some(tokens) = try_load_tokens(dir, PROVIDER)? {
        debug!(
            expired = tokens.is_expired(),
            "using OpenAI OAuth authentication"
        );
        return Ok((build_oauth_resolved(&tokens), Some(tokens)));
    }

    if let Ok(key) = env::var("OPENAI_API_KEY") {
        debug!("using OpenAI API key authentication");
        return Ok((
            ResolvedAuth {
                base_url: None,
                headers: vec![("authorization".into(), format!("Bearer {key}"))],
            },
            None,
        ));
    }

    if let Some(creds) = caudra_storage::auth::load_provider_credentials(dir, PROVIDER) {
        debug!("using OpenAI saved API key");
        return Ok((
            ResolvedAuth {
                base_url: None,
                headers: vec![("authorization".into(), format!("Bearer {}", creds.api_key))],
            },
            None,
        ));
    }

    Err(AgentError::Config {
        message: "not authenticated, run `caudra auth login openai` or set OPENAI_API_KEY".into(),
    })
}

pub fn login(dir: &StateDir) -> Result<(), AgentError> {
    let device = request_device_code()?;

    println!("Open this URL in your browser:\n\n  {DEVICE_AUTH_URL}\n");
    println!("Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");

    let device_token = poll_device_token(&device).map_err(|e| {
        error!(error = %e, "OpenAI device authorization failed");
        e
    })?;

    let token_resp = exchange_device_token(&device_token).map_err(|e| {
        error!(error = %e, "OpenAI token exchange failed");
        e
    })?;

    let tokens = into_oauth_tokens(token_resp, None)?;
    let _lock = lock_provider_auth(dir, PROVIDER)?;
    save_tokens(dir, PROVIDER, &tokens)?;
    println!("Authenticated successfully.");
    Ok(())
}

pub fn logout(dir: &StateDir) -> Result<(), AgentError> {
    let _lock = lock_provider_auth(dir, PROVIDER)?;
    if delete_tokens(dir, PROVIDER)? {
        println!("Logged out of OpenAI.");
    } else {
        println!("Not currently logged in to OpenAI.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_account_id_from_jwt() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"chatgpt_account_id": "acct_123"})
                .to_string()
                .as_bytes(),
        );
        let token = format!("{header}.{payload}.sig");
        assert_eq!(extract_account_id(&token).as_deref(), Some("acct_123"));

        assert_eq!(extract_account_id("not.a.jwt"), None);
        assert_eq!(extract_account_id("invalid"), None);
    }

    #[test]
    fn refresh_response_preserves_refresh_token_and_account_id() {
        let current = OAuthTokens {
            access: "old-access".into(),
            refresh: "old-refresh".into(),
            expires: 0,
            account_id: Some("account".into()),
        };
        let refreshed = into_oauth_tokens(
            TokenResponse {
                access_token: "new-access".into(),
                refresh_token: None,
                id_token: None,
                expires_in: Some(60),
            },
            Some(&current),
        )
        .unwrap();

        assert_eq!(refreshed.refresh, "old-refresh");
        assert_eq!(refreshed.account_id.as_deref(), Some("account"));
    }

    #[test]
    fn invalid_grant_is_an_authentication_error() {
        assert_eq!(
            oauth_error_status(
                400,
                r#"{"error":"invalid_grant","error_description":"invalid refresh"}"#,
            ),
            401
        );
    }

    #[test]
    fn expired_tokens_do_not_refresh_during_resolution() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        save_tokens(
            &storage,
            PROVIDER,
            &OAuthTokens {
                access: "expired-access".into(),
                refresh: "refresh".into(),
                expires: 0,
                account_id: None,
            },
        )
        .unwrap();

        let resolved = resolve(&storage).unwrap();

        assert_eq!(
            resolved.headers,
            vec![("authorization".into(), "Bearer expired-access".into())]
        );
    }
}
