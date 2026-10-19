use std::thread;
use std::time::Duration;

use caudra_storage::accounts::{AccountIdentity, StoredSecret};
use caudra_storage::auth::{OAuthTokens, now_millis};
use isahc::ReadResponseExt;
use isahc::config::{Configurable, VersionNegotiation};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, error};

use crate::AgentError;
use crate::accounts::{AccountSecret, LoginCredential};
use crate::providers::oauth::{AccountStore, RefreshReason, jwt_claims};
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
const PROFILE_EMAIL_CLAIM: &str = "/https:~1~1api.openai.com~1profile/email";
const EMAIL_CLAIM: &str = "/email";
const PLAN_CLAIM: &str = "/https:~1~1api.openai.com~1auth/chatgpt_plan_type";
const NO_CREDENTIAL: &str = "this OpenAI account has neither a login nor an API key";

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
    caudra_config::https_proxy_route()
        .apply(isahc::HttpClient::builder())
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
    let claims = jwt_claims(token)?;
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

/// Who a login signs in as, from the claims its tokens carry.
fn login_identity(resp: &TokenResponse) -> AccountIdentity {
    let claims: Vec<Value> = [resp.id_token.as_deref(), Some(resp.access_token.as_str())]
        .into_iter()
        .flatten()
        .filter_map(jwt_claims)
        .collect();
    let claim = |pointer: &str| {
        claims
            .iter()
            .find_map(|claims| claims.pointer(pointer)?.as_str().map(str::to_owned))
    };
    AccountIdentity {
        subject: extract_account_id_from_tokens(resp),
        email: claim(PROFILE_EMAIL_CLAIM).or_else(|| claim(EMAIL_CLAIM)),
        organization: None,
        plan: claim(PLAN_CLAIM),
    }
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
    store: &AccountStore,
    rejected_accesses: &[String],
) -> Result<OAuthTokens, AgentError> {
    store.refresh(
        RefreshReason::for_rejected(rejected_accesses),
        refresh_tokens,
    )
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

/// The platform auth `secret` signs in with, and its tokens when it is a
/// login rather than an API key.
pub(crate) fn account_auth(
    secret: &AccountSecret,
) -> Result<(ResolvedAuth, Option<OAuthTokens>), AgentError> {
    match secret {
        AccountSecret::OAuth(tokens) => Ok((build_oauth_resolved(tokens), Some(tokens.clone()))),
        AccountSecret::ApiKey { key, .. } => Ok((ResolvedAuth::bearer(key), None)),
        AccountSecret::Keyless => Err(AgentError::Config {
            message: NO_CREDENTIAL.into(),
        }),
    }
}

pub fn login() -> Result<LoginCredential, AgentError> {
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

    let identity = login_identity(&token_resp);
    let tokens = into_oauth_tokens(token_resp, None)?;
    println!("Authenticated successfully.");
    Ok(LoginCredential {
        secret: StoredSecret::OAuth(tokens),
        identity,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::providers::oauth::unsigned_jwt;

    const ACCOUNT_ID: &str = "acct_123";
    const EMAIL: &str = "jane@example.com";
    const PLAN: &str = "pro";

    #[test]
    fn extract_account_id_from_jwt() {
        let token = unsigned_jwt(&json!({"chatgpt_account_id": ACCOUNT_ID}));
        assert_eq!(extract_account_id(&token).as_deref(), Some(ACCOUNT_ID));

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
    fn login_identity_reads_the_account_email_and_plan() {
        let response = TokenResponse {
            access_token: unsigned_jwt(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": ACCOUNT_ID, "chatgpt_plan_type": PLAN},
            })),
            refresh_token: None,
            id_token: Some(unsigned_jwt(&json!({"email": EMAIL}))),
            expires_in: None,
        };

        assert_eq!(
            login_identity(&response),
            AccountIdentity {
                subject: Some(ACCOUNT_ID.into()),
                email: Some(EMAIL.into()),
                organization: None,
                plan: Some(PLAN.into()),
            }
        );
    }
}
