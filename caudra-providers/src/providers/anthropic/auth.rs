use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use caudra_storage::StateDir;
use caudra_storage::auth::{
    OAuthTokens, delete_provider_credentials, delete_tokens, lock_provider_auth, now_millis,
    save_tokens, try_load_tokens,
};
use isahc::ReadResponseExt;
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::AgentError;
use crate::providers::oauth::{self, RefreshReason};
use crate::providers::{ResolvedAuth, urlenc};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const DEFAULT_EXPIRES_SECS: u64 = 3600;
const REDIRECT_HOST: &str = "127.0.0.1";
const REDIRECT_URI_HOST: &str = "localhost";
const REDIRECT_PATH: &str = "/callback";

pub(crate) const PROVIDER: &str = "anthropic";
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const API_ORIGIN: &str = "https://api.anthropic.com";
pub(crate) const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.251";

const CALLBACK_TIMEOUT_MESSAGE: &str = "timed out waiting for Anthropic OAuth callback";
const STATE_MISMATCH: &str = "Anthropic authorization failed: state mismatch";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
}

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'static str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
}

#[derive(Debug)]
struct CallbackResult {
    code: Option<String>,
    error: Option<String>,
}

fn http_client() -> Result<isahc::HttpClient, AgentError> {
    isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOKEN_TIMEOUT)
        .redirect_policy(RedirectPolicy::None)
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .map_err(|error| AgentError::Config {
            message: format!("Anthropic OAuth HTTP client: {error}"),
        })
}

fn post_token(body: &impl Serialize) -> Result<TokenResponse, AgentError> {
    let request = isahc::Request::builder()
        .method("POST")
        .uri(TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header(
            "user-agent",
            format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)"),
        )
        .body(serde_json::to_vec(body)?)?;
    let mut response = http_client()?.send(request)?;
    let status = response.status().as_u16();
    let text = response.text().unwrap_or_default();
    if status != 200 {
        return Err(AgentError::Api {
            status: oauth_error_status(status, &text),
            message: oauth_error(status, &text),
        });
    }
    serde_json::from_str(&text).map_err(AgentError::from)
}

fn oauth_error_code(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
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

fn oauth_error(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let code = parsed.as_ref().and_then(|value| {
        value
            .get("error")
            .and_then(|error| error.as_str().or_else(|| error.get("type")?.as_str()))
    });
    let description = parsed.as_ref().and_then(|value| {
        value
            .get("error_description")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
    });
    match (code, description) {
        (Some(code), Some(description)) => {
            format!("Anthropic OAuth request failed ({status}): {code}: {description}")
        }
        (Some(code), None) => format!("Anthropic OAuth request failed ({status}): {code}"),
        _ => format!("Anthropic OAuth request failed ({status})"),
    }
}

fn into_oauth_tokens(
    response: TokenResponse,
    fallback_refresh: Option<String>,
) -> Result<OAuthTokens, AgentError> {
    if response.access_token.is_empty() {
        return Err(AgentError::Config {
            message: "Anthropic token response did not include an access token".into(),
        });
    }
    let refresh = response
        .refresh_token
        .filter(|token| !token.is_empty())
        .or(fallback_refresh)
        .ok_or_else(|| AgentError::Config {
            message: "Anthropic token response did not include a refresh token".into(),
        })?;
    let now = now_millis();
    let expires = response
        .expires_at
        .map(|expiry| {
            if expiry > 10_000_000_000 {
                expiry
            } else {
                expiry.saturating_mul(1000)
            }
        })
        .filter(|expiry| *expiry > now)
        .unwrap_or_else(|| {
            now.saturating_add(
                response
                    .expires_in
                    .unwrap_or(DEFAULT_EXPIRES_SECS)
                    .saturating_mul(1000),
            )
        });
    Ok(OAuthTokens {
        access: response.access_token,
        refresh,
        expires,
        account_id: None,
    })
}

pub(crate) fn build_oauth_resolved(tokens: &OAuthTokens) -> ResolvedAuth {
    ResolvedAuth {
        base_url: Some(API_ORIGIN.into()),
        headers: vec![("authorization".into(), format!("Bearer {}", tokens.access))],
    }
}

pub(crate) fn load_oauth_tokens(storage: &StateDir) -> Result<Option<OAuthTokens>, AgentError> {
    Ok(try_load_tokens(storage, PROVIDER)?)
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

pub(crate) fn refresh_tokens(tokens: &OAuthTokens) -> Result<OAuthTokens, AgentError> {
    if tokens.refresh.is_empty() {
        return Err(AgentError::Api {
            status: 401,
            message: "Anthropic credentials do not include a refresh token".into(),
        });
    }
    debug!(
        expired = tokens.is_expired(),
        "refreshing Anthropic OAuth token"
    );
    let response = post_token(&RefreshRequest {
        grant_type: "refresh_token",
        refresh_token: &tokens.refresh,
        client_id: CLIENT_ID,
    })?;
    into_oauth_tokens(response, Some(tokens.refresh.clone()))
}

pub fn login(storage: &StateDir) -> Result<(), AgentError> {
    login_inner(storage, true)
}

pub fn login_browser_callback(storage: &StateDir) -> Result<(), AgentError> {
    login_inner(storage, false)
}

fn login_inner(storage: &StateDir, allow_paste: bool) -> Result<(), AgentError> {
    println!(
        "Anthropic subscription OAuth uses Claude Code's public client registration.\nThis may conflict with Anthropic's terms for Claude subscriptions."
    );
    if !confirm("Continue? [y/N] ")? {
        return Err(AgentError::Cancelled);
    }
    let tokens = browser_login(allow_paste)?;
    let _lock = lock_provider_auth(storage, PROVIDER)?;
    save_tokens(storage, PROVIDER, &tokens)?;
    println!("Authenticated with Anthropic successfully.");
    Ok(())
}

pub fn logout(storage: &StateDir) -> Result<(), AgentError> {
    let _lock = lock_provider_auth(storage, PROVIDER)?;
    if delete_tokens(storage, PROVIDER)? {
        println!("Logged out of Anthropic OAuth.");
    } else if delete_provider_credentials(storage, PROVIDER)? {
        println!("Removed saved Anthropic API key.");
    } else {
        println!("Not currently logged in to Anthropic.");
    }
    Ok(())
}

fn confirm(message: &str) -> Result<bool, AgentError> {
    print!("{message}");
    io::stdout().flush().map_err(|error| AgentError::Config {
        message: format!("prompt: {error}"),
    })?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|error| AgentError::Config {
            message: format!("prompt: {error}"),
        })?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn browser_login(allow_paste: bool) -> Result<OAuthTokens, AgentError> {
    let (verifier, challenge) = pkce_pair()?;
    let state = random_token()?;
    let listener = bind_callback()?;
    let redirect_uri = format!(
        "http://{REDIRECT_URI_HOST}:{}{REDIRECT_PATH}",
        listener.local_addr()?.port()
    );
    let authorize_url = build_authorize_url(&redirect_uri, &challenge, &state);

    println!("Open this URL in your browser:\n\n  {authorize_url}\n");
    if let Err(error) = open::that(&authorize_url) {
        warn!(%error, "failed to open browser");
    }
    println!("Waiting for Anthropic OAuth callback on {redirect_uri}...");
    if allow_paste {
        println!(
            "If the redirect cannot reach this process, paste the redirect URL or CODE#STATE."
        );
    }

    let callback = wait_for_callback(listener, &state, allow_paste)?;
    if let Some(error) = callback.error {
        return Err(AgentError::Config {
            message: format!("Anthropic authorization failed: {error}"),
        });
    }
    let code = callback.code.ok_or_else(|| AgentError::Config {
        message: "Anthropic authorization returned no code".into(),
    })?;
    let response = post_token(&ExchangeRequest {
        grant_type: "authorization_code",
        code: &code,
        redirect_uri: &redirect_uri,
        client_id: CLIENT_ID,
        code_verifier: &verifier,
        state: &state,
    })?;
    into_oauth_tokens(response, None)
}

fn build_authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        urlenc(CLIENT_ID),
        urlenc(redirect_uri),
        urlenc(SCOPES),
        urlenc(challenge),
        urlenc(state),
    )
}

fn bind_callback() -> Result<TcpListener, AgentError> {
    let listener = TcpListener::bind((REDIRECT_HOST, 0)).map_err(|error| AgentError::Config {
        message: format!("Anthropic OAuth callback server: {error}"),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| AgentError::Config {
            message: format!("Anthropic OAuth callback server: {error}"),
        })?;
    Ok(listener)
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    allow_paste: bool,
) -> Result<CallbackResult, AgentError> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    let paste_rx = allow_paste.then(spawn_paste_reader);
    loop {
        if Instant::now() >= deadline {
            return Err(AgentError::Config {
                message: CALLBACK_TIMEOUT_MESSAGE.into(),
            });
        }
        if let Some(paste_rx) = &paste_rx
            && let Ok(pasted) = paste_rx.try_recv()
        {
            match parse_callback_input(&pasted, expected_state) {
                Ok(result) => return Ok(result),
                Err(error) => eprintln!("Ignored OAuth callback: {error}"),
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0u8; 4096];
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let read = stream.read(&mut buffer).unwrap_or_default();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let Some(target) = request.split_whitespace().nth(1) else {
                    continue;
                };
                if target.split('?').next() != Some(REDIRECT_PATH) {
                    let _ = write_http(&mut stream, 404, "Not found");
                    continue;
                }
                match parse_callback_target(target, expected_state) {
                    Ok(result) => {
                        let _ = write_http(
                            &mut stream,
                            200,
                            "Anthropic authorization received. You can close this tab.",
                        );
                        return Ok(result);
                    }
                    Err(_) => {
                        let _ =
                            write_http(&mut stream, 400, "Anthropic authorization state mismatch.");
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(error) => {
                return Err(AgentError::Config {
                    message: format!("Anthropic OAuth callback: {error}"),
                });
            }
        }
    }
}

fn spawn_paste_reader() -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let pasted = line.trim().to_string();
                    if !pasted.is_empty() && sender.send(pasted).is_err() {
                        return;
                    }
                }
            }
        }
    });
    receiver
}

fn write_http(stream: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!("<html><body><h1>{message}</h1></body></html>");
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn parse_callback_target(target: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    parse_callback_query(
        target.split_once('?').map_or("", |(_, query)| query),
        expected_state,
    )
}

fn parse_callback_input(input: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let value = input.trim();
    if let Some((code, state)) = value.split_once('#')
        && !code.is_empty()
        && !state.is_empty()
    {
        if percent_decode(state) != expected_state {
            return Err(config_error(STATE_MISMATCH));
        }
        return Ok(CallbackResult {
            code: Some(percent_decode(code)),
            error: None,
        });
    }
    let query = if let Some(index) = value.find('?') {
        &value[index + 1..]
    } else if value.contains('=') {
        value
    } else {
        return Err(config_error(
            "paste the complete Anthropic redirect URL or CODE#STATE",
        ));
    };
    parse_callback_query(query, expected_state)
}

fn parse_callback_query(query: &str, expected_state: &str) -> Result<CallbackResult, AgentError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            "error" => error = Some(percent_decode(value)),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(config_error(STATE_MISMATCH));
    }
    Ok(CallbackResult { code, error })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            output.push(value);
            index += 3;
            continue;
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn pkce_pair() -> Result<(String, String), AgentError> {
    let verifier = random_token()?;
    Ok((verifier.clone(), pkce_challenge(&verifier)))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_token() -> Result<String, AgentError> {
    let mut buffer = [0u8; 32];
    getrandom::fill(&mut buffer).map_err(|error| AgentError::Config {
        message: format!("CSPRNG unavailable: {error}"),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

fn config_error(message: &str) -> AgentError {
    AgentError::Config {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn authorization_url_contains_required_fields() {
        let url = build_authorize_url("http://localhost:1234/callback", "challenge", "state");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("code=true"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1234%2Fcallback"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state"));
        assert!(url.contains("user%3Ainference"));
    }

    #[test]
    fn pkce_matches_rfc_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_are_independent_random_values() {
        let (verifier, challenge) = pkce_pair().unwrap();
        let state = random_token().unwrap();
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        assert_eq!(state.len(), 43);
        assert_ne!(verifier, state);
    }

    #[test]
    fn code_and_state_input_is_accepted() {
        let parsed = parse_callback_input("auth-code#expected", "expected").unwrap();
        assert_eq!(parsed.code.as_deref(), Some("auth-code"));
    }

    #[test]
    fn callback_url_requires_matching_state() {
        let error = parse_callback_input(
            "http://localhost:1234/callback?code=abc&state=wrong",
            "expected",
        )
        .unwrap_err();
        assert!(error.to_string().contains("state mismatch"));
    }

    #[test]
    fn refresh_response_preserves_rotating_token_fallback() {
        let tokens = into_oauth_tokens(
            TokenResponse {
                access_token: "new-access".into(),
                refresh_token: None,
                expires_in: Some(60),
                expires_at: None,
            },
            Some("old-refresh".into()),
        )
        .unwrap();
        assert_eq!(tokens.refresh, "old-refresh");
    }

    #[test]
    fn token_requests_use_expected_json_fields() {
        let exchange = serde_json::to_value(ExchangeRequest {
            grant_type: "authorization_code",
            code: "code",
            redirect_uri: "http://localhost:1234/callback",
            client_id: CLIENT_ID,
            code_verifier: "verifier",
            state: "state",
        })
        .unwrap();
        assert_eq!(exchange["client_id"], CLIENT_ID);
        assert_eq!(exchange["state"], "state");
        assert_eq!(exchange["code_verifier"], "verifier");

        let refresh = serde_json::to_value(RefreshRequest {
            grant_type: "refresh_token",
            refresh_token: "refresh",
            client_id: CLIENT_ID,
        })
        .unwrap();
        assert!(refresh.get("scope").is_none());
        assert_eq!(refresh["refresh_token"], "refresh");
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
        let tokens = OAuthTokens {
            access: "expired-access".into(),
            refresh: "refresh".into(),
            expires: 0,
            account_id: None,
        };
        save_tokens(&storage, PROVIDER, &tokens).unwrap();

        let resolved = build_oauth_resolved(&load_oauth_tokens(&storage).unwrap().unwrap());

        assert_eq!(
            resolved.headers,
            vec![("authorization".into(), "Bearer expired-access".into())]
        );
    }
}
