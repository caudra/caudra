use std::borrow::Cow;
use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use caudra_storage::accounts::{AccountIdentity, StoredSecret};
use caudra_storage::auth::ProviderCredentials;
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use isahc::{Body, HttpClient, Request, Response};
use keyring::Entry;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use serde_yaml::Value as YamlValue;
use thiserror::Error;
use tracing::{debug, warn};
use url::{Host, Url};

use super::{
    ENTERPRISE_DISPLAY_NAME, ENTERPRISE_SLUG, ENTERPRISE_TOKEN_ENV, PUBLIC_DISPLAY_NAME,
    PUBLIC_SLUG, PUBLIC_TOKEN_ENV,
};
use crate::AgentError;
use crate::accounts::{AccountCredential, CredentialSource, LoginCredential};

const PUBLIC_TOKEN_ENV_VARS: &[&str] = &[PUBLIC_TOKEN_ENV, "COPILOT_GITHUB_TOKEN"];
const ENTERPRISE_TOKEN_ENV_VARS: &[&str] = &[ENTERPRISE_TOKEN_ENV];
pub const ENTERPRISE_HOST_ENV: &str = "GH_COPILOT_ENTERPRISE_HOST";
const PUBLIC_HOST: &str = "github.com";
const PUBLIC_SUBDOMAIN_SUFFIX: &str = ".github.com";
pub(crate) const PUBLIC_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const PUBLIC_USER_URL: &str = "https://api.github.com/user";
const ENTERPRISE_REST_PREFIX: &str = "https://api.";
const USER_PATH: &str = "/user";
const GITHUB_JSON_CONTENT_TYPE: &str = "application/vnd.github+json";
const ENTERPRISE_API_PREFIX: &str = "https://copilot-api.";
const HTTPS_SCHEME: &str = "https";
const SCHEME_SEPARATOR: &str = "://";
const ROOT_PATH: &str = "/";
const KEYRING_SERVICE_PREFIX: &str = "gh:";
const GO_KEYRING_B64_PREFIX: &str = "go-keyring-base64:";
const KEYRING_LOCKED_MESSAGE: &str = "is unreadable; is the store locked?";
const OAUTH_TOKEN_FIELD: &str = "oauth_token";
const APP_KEY_SEPARATOR: char = ':';

const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const OAUTH_SCOPE: &str = "read:user";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEVICE_CODE_PATH: &str = "/login/device/code";
const ACCESS_TOKEN_PATH: &str = "/login/oauth/access_token";
const JSON_CONTENT_TYPE: &str = "application/json";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const POLL_MARGIN: Duration = Duration::from_secs(3);
const DEFAULT_EXPIRY: Duration = Duration::from_secs(900);
const MAX_LOGIN_DURATION: Duration = Duration::from_secs(900);
const MAX_DEVICE_CODE_LEN: usize = 256;
const MAX_USER_CODE_LEN: usize = 32;
const MAX_VERIFICATION_URI_LEN: usize = 512;
const MAX_ACCESS_TOKEN_LEN: usize = 4096;
const MAX_ERROR_CODE_LEN: usize = 64;
const UNKNOWN_ERROR_CODE: &str = "unknown_error";
const HTTP_OK: u16 = 200;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;
const HTTP_SERVER_ERROR: u16 = 500;

const AUTHORIZATION_PENDING: &str = "authorization_pending";
const SLOW_DOWN: &str = "slow_down";
const ACCESS_DENIED: &str = "access_denied";
const EXPIRED_TOKEN: &str = "expired_token";

const DEVICE_DENIED: &str = "GitHub device authorization was denied";
const DEVICE_EXPIRED: &str = "GitHub device authorization expired; run the login again";
const DEVICE_MALFORMED: &str = "GitHub returned a malformed device authorization response";
const TOKEN_MALFORMED: &str = "GitHub returned a malformed access token response";
const RESPONSE_TOO_LARGE: &str = "GitHub returned an oversized OAuth response";

/// Which credential slot a Copilot provider reads. The two never share a
/// token, so a personal and an enterprise account can stay signed in together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotIdentity {
    Public,
    Enterprise,
}

impl CopilotIdentity {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Public => PUBLIC_SLUG,
            Self::Enterprise => ENTERPRISE_SLUG,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Public => PUBLIC_DISPLAY_NAME,
            Self::Enterprise => ENTERPRISE_DISPLAY_NAME,
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        [Self::Public, Self::Enterprise]
            .into_iter()
            .find(|identity| identity.slug() == slug)
    }

    pub(crate) const fn token_env_vars(self) -> &'static [&'static str] {
        match self {
            Self::Public => PUBLIC_TOKEN_ENV_VARS,
            Self::Enterprise => ENTERPRISE_TOKEN_ENV_VARS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnterpriseHostError {
    #[error("enter a GitHub Enterprise hostname such as company.ghe.com")]
    Empty,
    #[error("not a valid hostname or https:// URL")]
    Malformed,
    #[error("only https:// URLs are supported")]
    InsecureScheme,
    #[error("the URL must not contain credentials")]
    Credentials,
    #[error("the URL must not name a port")]
    Port,
    #[error("the URL must name only the host, without a path, query, or fragment")]
    Path,
    #[error("an IP address is not a GitHub Enterprise hostname")]
    IpAddress,
    #[error("github.com is public GitHub; use the copilot provider")]
    PublicGithub,
}

/// A validated GitHub Enterprise hostname, lowercase and without scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnterpriseHost(String);

impl EnterpriseHost {
    pub fn parse(input: &str) -> Result<Self, EnterpriseHostError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(EnterpriseHostError::Empty);
        }
        let candidate = if input.contains(SCHEME_SEPARATOR) {
            Cow::Borrowed(input)
        } else {
            Cow::Owned(format!("{HTTPS_SCHEME}{SCHEME_SEPARATOR}{input}"))
        };
        let url = Url::parse(&candidate).map_err(|_| EnterpriseHostError::Malformed)?;
        if url.scheme() != HTTPS_SCHEME {
            return Err(EnterpriseHostError::InsecureScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EnterpriseHostError::Credentials);
        }
        if url.port().is_some() {
            return Err(EnterpriseHostError::Port);
        }
        if url.path() != ROOT_PATH || url.query().is_some() || url.fragment().is_some() {
            return Err(EnterpriseHostError::Path);
        }
        let host = match url.host() {
            Some(Host::Domain(domain)) => domain.to_owned(),
            Some(Host::Ipv4(_) | Host::Ipv6(_)) => return Err(EnterpriseHostError::IpAddress),
            None => return Err(EnterpriseHostError::Malformed),
        };
        if host == PUBLIC_HOST || host.ends_with(PUBLIC_SUBDOMAIN_SUFFIX) {
            return Err(EnterpriseHostError::PublicGithub);
        }
        Ok(Self(host))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn api_endpoint(&self) -> String {
        format!("{ENTERPRISE_API_PREFIX}{}", self.0)
    }
}

/// The account a login writes: public GitHub, or one enterprise host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotAccount {
    Public,
    Enterprise(EnterpriseHost),
}

impl CopilotAccount {
    pub fn identity(&self) -> CopilotIdentity {
        match self {
            Self::Public => CopilotIdentity::Public,
            Self::Enterprise(_) => CopilotIdentity::Enterprise,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Self::Public => PUBLIC_HOST,
            Self::Enterprise(host) => host.as_str(),
        }
    }

    fn stored_host(&self) -> Option<String> {
        match self {
            Self::Public => None,
            Self::Enterprise(host) => Some(host.as_str().to_owned()),
        }
    }

    fn user_url(&self) -> String {
        match self {
            Self::Public => PUBLIC_USER_URL.to_owned(),
            Self::Enterprise(host) => {
                format!("{ENTERPRISE_REST_PREFIX}{}{USER_PATH}", host.as_str())
            }
        }
    }
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

/// A token and the enterprise host it belongs to; `None` is public GitHub.
pub(crate) struct CopilotCredentials {
    pub(crate) token: String,
    pub(crate) host: Option<EnterpriseHost>,
}

fn config_error(message: impl Into<String>) -> AgentError {
    AgentError::Config {
        message: message.into(),
    }
}

/// The token and host `account` signs in with. A token from the environment
/// or `providers.toml` carries no host, so an enterprise one takes it from
/// the environment.
pub(crate) fn account_credentials(
    identity: CopilotIdentity,
    account: &AccountCredential,
) -> Result<CopilotCredentials, AgentError> {
    credentials_from(identity, account, |name| env::var(name).ok())
}

fn credentials_from(
    identity: CopilotIdentity,
    account: &AccountCredential,
    read_env: impl Fn(&str) -> Option<String>,
) -> Result<CopilotCredentials, AgentError> {
    let token = account
        .optional_api_key()
        .ok_or_else(|| config_error(not_authenticated(identity)))?;
    let host = match (account.host(), identity, &account.source) {
        (
            None,
            CopilotIdentity::Enterprise,
            CredentialSource::Env(_) | CredentialSource::Config,
        ) => Some(env_enterprise_host(&read_env)?),
        (host, _, _) => saved_host(identity, host)?,
    };
    debug!(account = %account.id, "using Copilot credentials");
    Ok(CopilotCredentials {
        token: token.to_owned(),
        host,
    })
}

fn env_token(
    identity: CopilotIdentity,
    read_env: &impl Fn(&str) -> Option<String>,
) -> Option<(&'static str, String)> {
    identity.token_env_vars().iter().find_map(|name| {
        read_env(name)
            .filter(|token| !token.trim().is_empty())
            .map(|token| (*name, token))
    })
}

/// The environment variable whose token wins over the saved one, if any.
pub fn env_override(identity: CopilotIdentity) -> Option<&'static str> {
    env_token(identity, &|name| env::var(name).ok()).map(|(name, _)| name)
}

fn env_enterprise_host(
    read_env: &impl Fn(&str) -> Option<String>,
) -> Result<EnterpriseHost, AgentError> {
    let raw = read_env(ENTERPRISE_HOST_ENV).ok_or_else(|| {
        config_error(format!(
            "{ENTERPRISE_TOKEN_ENV} needs {ENTERPRISE_HOST_ENV}, the GitHub Enterprise hostname"
        ))
    })?;
    EnterpriseHost::parse(&raw)
        .map_err(|error| config_error(format!("{ENTERPRISE_HOST_ENV} is invalid: {error}")))
}

fn saved_host(
    identity: CopilotIdentity,
    host: Option<&str>,
) -> Result<Option<EnterpriseHost>, AgentError> {
    let relogin = || format!("run `caudra auth login {}`", identity.slug());
    match host.filter(|host| *host != PUBLIC_HOST) {
        Some(host) => EnterpriseHost::parse(host).map(Some).map_err(|error| {
            config_error(format!(
                "saved {} credentials name an invalid host ({error}); {}",
                identity.display_name(),
                relogin()
            ))
        }),
        None if identity == CopilotIdentity::Enterprise => Err(config_error(format!(
            "saved {} credentials have no host; {}",
            identity.display_name(),
            relogin()
        ))),
        None => Ok(None),
    }
}

fn not_authenticated(identity: CopilotIdentity) -> String {
    match identity {
        CopilotIdentity::Public => format!(
            "not authenticated, run `caudra auth login {PUBLIC_SLUG}` or set {PUBLIC_TOKEN_ENV}"
        ),
        CopilotIdentity::Enterprise => format!(
            "not authenticated, run `caudra auth login {ENTERPRISE_SLUG}` or set \
             {ENTERPRISE_TOKEN_ENV} and {ENTERPRISE_HOST_ENV}"
        ),
    }
}

pub fn login(account: &CopilotAccount) -> Result<LoginCredential, AgentError> {
    let token = login_with(&NetworkIo::new()?, account, present_challenge)?;
    Ok(signed_in(account, token))
}

fn login_with(
    io: &impl DeviceFlowIo,
    account: &CopilotAccount,
    present: impl FnOnce(&DeviceChallenge),
) -> Result<String, AgentError> {
    let host = account.host();
    let token = device_login(io, host, present).inspect_err(|error| {
        warn!(%error, host, "Copilot device authorization failed");
    })?;
    println!(
        "Authenticated with {} ({host}).",
        account.identity().display_name()
    );
    Ok(token)
}

pub fn import(account: &CopilotAccount) -> Result<LoginCredential, AgentError> {
    let host = account.host();
    let mut hints = Vec::new();
    let Some(token) = discover_token(host, &mut hints) else {
        for hint in hints {
            eprintln!("{hint}");
        }
        return Err(config_error(format!(
            "no Copilot token for {host} found. Run `gh auth login --hostname {host}`, sign in \
             with the Copilot client, or run `caudra auth login {}` to sign in with GitHub.",
            account.identity().slug()
        )));
    };
    println!("Copilot token for {host} imported from gh CLI / Copilot client / system keyring.");
    Ok(signed_in(account, token))
}

fn signed_in(account: &CopilotAccount, token: String) -> LoginCredential {
    let identity = fetch_identity(account, &token).unwrap_or_else(|error| {
        warn!(%error, host = account.host(), "could not read which GitHub user the token belongs to");
        AccountIdentity::default()
    });
    login_credential(account, token, identity)
}

fn login_credential(
    account: &CopilotAccount,
    token: String,
    identity: AccountIdentity,
) -> LoginCredential {
    LoginCredential {
        secret: StoredSecret::ApiKey(ProviderCredentials {
            api_key: token,
            host: account.stored_host(),
        }),
        identity,
    }
}

fn fetch_identity(account: &CopilotAccount, token: &str) -> Result<AccountIdentity, AgentError> {
    let request = Request::get(account.user_url())
        .header("accept", GITHUB_JSON_CONTENT_TYPE)
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", crate::providers::user_agent())
        .body(())?;
    let mut response = NetworkIo::new()?.client.send(request)?;
    let status = response.status().as_u16();
    if status != HTTP_OK {
        return Err(http_failure(status));
    }
    Ok(github_identity(&read_bounded(&mut response)?)?)
}

/// GitHub names a user only by login, which is both who signed in and what
/// the account is called.
fn github_identity(body: &str) -> Result<AccountIdentity, serde_json::Error> {
    let user: GithubUser = serde_json::from_str(body)?;
    Ok(AccountIdentity {
        subject: Some(user.login.clone()),
        organization: Some(user.login),
        ..AccountIdentity::default()
    })
}

fn read_bounded(response: &mut Response<Body>) -> Result<String, AgentError> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(config_error(RESPONSE_TOO_LARGE));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct HttpReply {
    status: u16,
    body: String,
}

/// The network and the clock of a device login, apart so tests can drive both.
trait DeviceFlowIo {
    fn post_json(&self, url: &str, body: &JsonValue) -> Result<HttpReply, AgentError>;
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

struct NetworkIo {
    client: HttpClient,
}

impl NetworkIo {
    fn new() -> Result<Self, AgentError> {
        let client = caudra_config::https_proxy_route()
            .apply(HttpClient::builder())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect_policy(RedirectPolicy::None)
            .version_negotiation(VersionNegotiation::http11())
            .build()?;
        Ok(Self { client })
    }
}

impl DeviceFlowIo for NetworkIo {
    fn post_json(&self, url: &str, body: &JsonValue) -> Result<HttpReply, AgentError> {
        let mut response = self.client.send(oauth_request(url, body)?)?;
        Ok(HttpReply {
            status: response.status().as_u16(),
            body: read_bounded(&mut response)?,
        })
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

fn oauth_request(url: &str, body: &JsonValue) -> Result<Request<Vec<u8>>, AgentError> {
    Ok(Request::post(url)
        .header("accept", JSON_CONTENT_TYPE)
        .header("content-type", JSON_CONTENT_TYPE)
        .header("user-agent", crate::providers::user_agent())
        .body(serde_json::to_vec(body)?)?)
}

fn oauth_url(host: &str, path: &str) -> String {
    format!("{HTTPS_SCHEME}{SCHEME_SEPARATOR}{host}{path}")
}

#[derive(Deserialize)]
struct DeviceCodeReply {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    error: Option<String>,
}

struct DeviceChallenge {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: Duration,
    interval: Duration,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

enum PollOutcome {
    Token(String),
    Pending,
    SlowDown(Option<u64>),
}

fn present_challenge(challenge: &DeviceChallenge) {
    println!(
        "Open this URL in your browser:\n\n  {}\n",
        challenge.verification_uri
    );
    println!("Enter code: {}\n", challenge.user_code);
    if let Err(error) = open::that(&challenge.verification_uri) {
        warn!(%error, "failed to open browser");
    }
    println!("Waiting for authorization...");
}

fn device_login(
    io: &impl DeviceFlowIo,
    host: &str,
    present: impl FnOnce(&DeviceChallenge),
) -> Result<String, AgentError> {
    let challenge = request_challenge(io, host)?;
    let started = io.now();
    present(&challenge);
    poll_for_token(io, host, &challenge, started)
}

fn request_challenge(io: &impl DeviceFlowIo, host: &str) -> Result<DeviceChallenge, AgentError> {
    let reply = io.post_json(
        &oauth_url(host, DEVICE_CODE_PATH),
        &json!({ "client_id": CLIENT_ID, "scope": OAUTH_SCOPE }),
    )?;
    let parsed: Option<DeviceCodeReply> = serde_json::from_str(&reply.body).ok();
    if let Some(code) = parsed.as_ref().and_then(|reply| reply.error.as_deref()) {
        return Err(oauth_failure(code));
    }
    if reply.status != HTTP_OK {
        return Err(http_failure(reply.status));
    }
    validate_challenge(parsed.ok_or_else(|| config_error(DEVICE_MALFORMED))?, host)
}

fn validate_challenge(reply: DeviceCodeReply, host: &str) -> Result<DeviceChallenge, AgentError> {
    let malformed = || config_error(DEVICE_MALFORMED);
    let device_code = reply
        .device_code
        .filter(|code| is_bounded_token(code, MAX_DEVICE_CODE_LEN))
        .ok_or_else(malformed)?;
    let user_code = reply
        .user_code
        .filter(|code| is_user_code(code))
        .ok_or_else(malformed)?;
    let verification_uri = reply
        .verification_uri
        .filter(|uri| is_verification_uri(uri, host, &device_code))
        .ok_or_else(malformed)?;
    let expires_in = match reply.expires_in {
        None => DEFAULT_EXPIRY,
        Some(0) => return Err(malformed()),
        Some(secs) => Duration::from_secs(secs),
    };
    let interval = reply
        .interval
        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
        .clamp(MIN_INTERVAL, MAX_INTERVAL);
    Ok(DeviceChallenge {
        device_code,
        user_code,
        verification_uri,
        expires_in,
        interval,
    })
}

fn is_bounded_token(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && value.bytes().all(|b| b.is_ascii_graphic())
}

fn is_user_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_USER_CODE_LEN
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_verification_uri(uri: &str, host: &str, device_code: &str) -> bool {
    uri.len() <= MAX_VERIFICATION_URI_LEN
        && !uri.contains(device_code)
        && Url::parse(uri).is_ok_and(|url| {
            url.scheme() == HTTPS_SCHEME
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.host() == Some(Host::Domain(host))
        })
}

fn poll_for_token(
    io: &impl DeviceFlowIo,
    host: &str,
    challenge: &DeviceChallenge,
    started: Instant,
) -> Result<String, AgentError> {
    let deadline = started + challenge.expires_in.min(MAX_LOGIN_DURATION);
    let url = oauth_url(host, ACCESS_TOKEN_PATH);
    let body = json!({
        "client_id": CLIENT_ID,
        "device_code": challenge.device_code,
        "grant_type": DEVICE_GRANT,
    });
    let mut interval = challenge.interval;
    loop {
        let remaining = deadline.saturating_duration_since(io.now());
        if remaining.is_zero() {
            return Err(config_error(DEVICE_EXPIRED));
        }
        io.sleep((interval + POLL_MARGIN).min(remaining));
        if io.now() >= deadline {
            return Err(config_error(DEVICE_EXPIRED));
        }
        match poll_outcome(&io.post_json(&url, &body)?)? {
            PollOutcome::Token(token) => return Ok(token),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown(server_secs) => interval = next_interval(interval, server_secs),
        }
    }
}

fn poll_outcome(reply: &HttpReply) -> Result<PollOutcome, AgentError> {
    if reply.status == HTTP_TOO_MANY_REQUESTS {
        return Ok(PollOutcome::SlowDown(None));
    }
    if reply.status >= HTTP_SERVER_ERROR {
        return Ok(PollOutcome::Pending);
    }
    match serde_json::from_str::<TokenReply>(&reply.body).ok() {
        Some(TokenReply {
            error: Some(code),
            interval,
            ..
        }) => match code.as_str() {
            AUTHORIZATION_PENDING => Ok(PollOutcome::Pending),
            SLOW_DOWN => Ok(PollOutcome::SlowDown(interval)),
            ACCESS_DENIED => Err(config_error(DEVICE_DENIED)),
            EXPIRED_TOKEN => Err(config_error(DEVICE_EXPIRED)),
            other => Err(oauth_failure(other)),
        },
        _ if reply.status != HTTP_OK => Err(http_failure(reply.status)),
        Some(TokenReply {
            access_token: Some(token),
            ..
        }) if is_bounded_token(&token, MAX_ACCESS_TOKEN_LEN) => Ok(PollOutcome::Token(token)),
        _ => Err(config_error(TOKEN_MALFORMED)),
    }
}

fn next_interval(current: Duration, server_secs: Option<u64>) -> Duration {
    let stepped = current + SLOW_DOWN_STEP;
    server_secs
        .map_or(stepped, |secs| stepped.max(Duration::from_secs(secs)))
        .min(MAX_INTERVAL)
}

fn oauth_failure(code: &str) -> AgentError {
    config_error(format!(
        "GitHub device authorization failed: {}",
        sanitized_error_code(code)
    ))
}

fn http_failure(status: u16) -> AgentError {
    config_error(format!(
        "GitHub device authorization request failed (HTTP {status})"
    ))
}

fn sanitized_error_code(code: &str) -> &str {
    let valid = !code.is_empty()
        && code.len() <= MAX_ERROR_CODE_LEN
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if valid { code } else { UNKNOWN_ERROR_CODE }
}

fn discover_token(host: &str, hints: &mut Vec<String>) -> Option<String> {
    let read = |paths: Vec<PathBuf>| {
        paths
            .into_iter()
            .filter_map(|path| fs::read_to_string(path).ok())
    };
    read(copilot_config_paths())
        .find_map(|contents| extract_oauth_token_json(&contents, host))
        .or_else(|| {
            read(gh_config_paths()).find_map(|contents| extract_oauth_token_yaml(&contents, host))
        })
        .or_else(|| discover_keyring_token(host, hints))
}

fn discover_keyring_token(host: &str, hints: &mut Vec<String>) -> Option<String> {
    let files = readable_config_files();
    for account in keyring_accounts(&files, host) {
        let Ok(entry) = Entry::new(&format!("{KEYRING_SERVICE_PREFIX}{host}"), &account) else {
            debug!(host, account, "gh keyring account rejected");
            continue;
        };
        match entry.get_password() {
            Ok(raw) => return Some(decode_go_keyring(&raw)),
            Err(keyring::Error::NoEntry) => {}
            Err(err) => {
                warn!(
                    error = %err, host, account,
                    "gh keyring entry {}", KEYRING_LOCKED_MESSAGE
                );
                hints.push(locked_keyring_hint(host, &account, err));
            }
        }
    }
    None
}

fn locked_keyring_hint(host: &str, account: &str, error: impl std::fmt::Display) -> String {
    format!("gh keyring entry for {host}/{account} {KEYRING_LOCKED_MESSAGE} ({error})")
}

fn keyring_accounts(files: &[String], host: &str) -> Vec<String> {
    let mut accounts = Vec::new();
    for contents in files {
        for user in config_usernames(contents, host) {
            push_unique(&mut accounts, &user);
        }
    }
    push_unique(&mut accounts, "");
    accounts
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|item| item == value) {
        list.push(value.to_owned());
    }
}

// YAML 1.2 is a superset of JSON, so one serde_yaml parse covers both the JSON copilot config and
// the YAML gh config.
fn config_usernames(contents: &str, host: &str) -> Vec<String> {
    let mut usernames = Vec::new();
    if let Ok(value) = serde_yaml::from_str::<YamlValue>(contents)
        && let Some(cfg) = value.get(host).and_then(YamlValue::as_mapping)
    {
        if let Some(user) = cfg.get("user").and_then(YamlValue::as_str) {
            push_unique(&mut usernames, user);
        }
        if let Some(users) = cfg.get("users").and_then(YamlValue::as_mapping) {
            for key in users.keys().filter_map(|key| key.as_str()) {
                push_unique(&mut usernames, key);
            }
        }
    }
    usernames
}

fn readable_config_files() -> Vec<String> {
    copilot_config_paths()
        .into_iter()
        .chain(gh_config_paths())
        .filter_map(|path| fs::read_to_string(path).ok())
        .collect()
}

/// Reverses the encoding written by the `go-keyring` library (used by the gh CLI,
/// which is written in Go): it base64-encodes the token bytes and prefixes them
/// with `go-keyring-base64:`. Plain (un-prefixed) tokens are returned as-is.
fn decode_go_keyring(raw: &str) -> String {
    raw.strip_prefix(GO_KEYRING_B64_PREFIX)
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| raw.to_owned())
}

/// Copilot clients key `apps.json` as `host:client-id`, `hosts.json` and gh as the bare host.
fn config_key_matches(key: &str, host: &str) -> bool {
    key.strip_prefix(host)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(APP_KEY_SEPARATOR))
}

fn copilot_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| config.join("github-copilot"))
        .map(|base| vec![base.join("hosts.json"), base.join("apps.json")])
        .unwrap_or_default()
}

fn gh_config_paths() -> Vec<PathBuf> {
    config_dir()
        .map(|config| vec![config.join("gh").join("hosts.yml")])
        .unwrap_or_default()
}

fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| caudra_storage::paths::home().map(|home| home.join(".config")))
}

fn extract_oauth_token_json(contents: &str, host: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(contents).ok()?;
    value.as_object()?.iter().find_map(|(key, value)| {
        config_key_matches(key, host)
            .then(|| value[OAUTH_TOKEN_FIELD].as_str().map(str::to_owned))
            .flatten()
    })
}

fn extract_oauth_token_yaml(contents: &str, host: &str) -> Option<String> {
    let value: YamlValue = serde_yaml::from_str(contents).ok()?;
    value.as_mapping()?.iter().find_map(|(key, value)| {
        config_key_matches(key.as_str()?, host)
            .then(|| value[OAUTH_TOKEN_FIELD].as_str().map(str::to_owned))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, VecDeque};

    use caudra_storage::accounts::{AccountId, AccountName};
    use test_case::test_case;

    use super::*;
    use crate::accounts::AccountSecret;

    const HOST: &str = "myco.ghe.com";
    const DEVICE_CODE: &str = "device-secret-3584d83530557fdd";
    const USER_CODE: &str = "WDJB-MJHT";
    const ACCESS_TOKEN: &str = "gho_access_token";
    const ENTERPRISE_ENV_TOKEN: &str = "enterprise-env-token";
    const SAVED_TOKEN: &str = "saved-token";
    const ACCOUNT: &str = "work";
    const GITHUB_LOGIN: &str = "octocat";
    const ENTERPRISE_API: &str = "https://copilot-api.myco.ghe.com";

    fn enterprise_host() -> EnterpriseHost {
        EnterpriseHost::parse(HOST).unwrap()
    }

    fn message(error: AgentError) -> String {
        match error {
            AgentError::Config { message } => message,
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test_case("myco.ghe.com" ; "bare_host")]
    #[test_case("https://myco.ghe.com" ; "https_url")]
    #[test_case("https://myco.ghe.com/" ; "trailing_slash")]
    #[test_case("  MyCo.GHE.com  " ; "mixed_case_and_spaces")]
    #[test_case("https://myco.ghe.com:443" ; "default_port")]
    fn enterprise_host_normalizes(input: &str) {
        assert_eq!(EnterpriseHost::parse(input).unwrap().as_str(), HOST);
    }

    #[test_case("", EnterpriseHostError::Empty ; "empty")]
    #[test_case("   ", EnterpriseHostError::Empty ; "blank")]
    #[test_case("my host", EnterpriseHostError::Malformed ; "space")]
    #[test_case("http://myco.ghe.com", EnterpriseHostError::InsecureScheme ; "http")]
    #[test_case("ftp://myco.ghe.com", EnterpriseHostError::InsecureScheme ; "ftp")]
    #[test_case("https://user:pw@myco.ghe.com", EnterpriseHostError::Credentials ; "credentials")]
    #[test_case("myco.ghe.com:8443", EnterpriseHostError::Port ; "port")]
    #[test_case("myco.ghe.com/login", EnterpriseHostError::Path ; "path")]
    #[test_case("https://myco.ghe.com/?a=b", EnterpriseHostError::Path ; "query")]
    #[test_case("https://myco.ghe.com/#x", EnterpriseHostError::Path ; "fragment")]
    #[test_case("10.0.0.1", EnterpriseHostError::IpAddress ; "ipv4")]
    #[test_case("https://[::1]", EnterpriseHostError::IpAddress ; "ipv6")]
    #[test_case("github.com", EnterpriseHostError::PublicGithub ; "public")]
    #[test_case("https://api.github.com", EnterpriseHostError::PublicGithub ; "public_subdomain")]
    fn enterprise_host_rejects(input: &str, expected: EnterpriseHostError) {
        assert_eq!(EnterpriseHost::parse(input), Err(expected));
    }

    #[test]
    fn enterprise_api_endpoint_uses_copilot_api_subdomain() {
        assert_eq!(enterprise_host().api_endpoint(), ENTERPRISE_API);
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn account(
        identity: CopilotIdentity,
        secret: AccountSecret,
        source: CredentialSource,
    ) -> AccountCredential {
        AccountCredential {
            id: AccountId::new(identity.slug(), AccountName::new(ACCOUNT).unwrap()),
            secret,
            source,
            identity: AccountIdentity::default(),
        }
    }

    fn token(token: &str, host: Option<&str>) -> AccountSecret {
        AccountSecret::ApiKey {
            key: token.into(),
            host: host.map(str::to_owned),
        }
    }

    fn env_account(identity: CopilotIdentity) -> AccountCredential {
        account(
            identity,
            token(ENTERPRISE_ENV_TOKEN, None),
            CredentialSource::Env(ENTERPRISE_TOKEN_ENV.into()),
        )
    }

    #[test]
    fn enterprise_env_needs_its_host() {
        let error = credentials_from(
            CopilotIdentity::Enterprise,
            &env_account(CopilotIdentity::Enterprise),
            env_of(&[]),
        )
        .err()
        .unwrap();
        assert!(message(error).contains(ENTERPRISE_HOST_ENV));
    }

    #[test]
    fn enterprise_env_takes_its_host_from_the_environment() {
        let creds = credentials_from(
            CopilotIdentity::Enterprise,
            &env_account(CopilotIdentity::Enterprise),
            env_of(&[(ENTERPRISE_HOST_ENV, HOST)]),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(creds.token, ENTERPRISE_ENV_TOKEN);
        assert_eq!(creds.host, Some(enterprise_host()));
    }

    #[test]
    fn an_account_without_a_token_is_not_authenticated() {
        let keyless = account(
            CopilotIdentity::Enterprise,
            AccountSecret::Keyless,
            CredentialSource::Config,
        );
        let error = credentials_from(CopilotIdentity::Enterprise, &keyless, env_of(&[]))
            .err()
            .unwrap();
        assert!(message(error).contains(ENTERPRISE_SLUG));
    }

    #[test_case(CopilotIdentity::Enterprise, Some(HOST), Some(HOST) ; "enterprise_saved_host")]
    #[test_case(CopilotIdentity::Public, None, None ; "public_saved")]
    #[test_case(CopilotIdentity::Public, Some(PUBLIC_HOST), None ; "public_saved_github_host")]
    #[test_case(CopilotIdentity::Public, Some(HOST), Some(HOST) ; "legacy_public_enterprise_host")]
    fn saved_credentials_keep_their_host(
        identity: CopilotIdentity,
        stored: Option<&str>,
        expected: Option<&str>,
    ) {
        let saved = account(
            identity,
            token(SAVED_TOKEN, stored),
            CredentialSource::Stored,
        );
        let creds = credentials_from(identity, &saved, env_of(&[(ENTERPRISE_HOST_ENV, HOST)]))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(creds.token, SAVED_TOKEN);
        assert_eq!(creds.host.as_ref().map(EnterpriseHost::as_str), expected);
    }

    #[test_case(None ; "missing_host")]
    #[test_case(Some("http://bad host") ; "invalid_host")]
    fn enterprise_saved_without_valid_host_asks_for_relogin(stored: Option<&str>) {
        let saved = account(
            CopilotIdentity::Enterprise,
            token(SAVED_TOKEN, stored),
            CredentialSource::Stored,
        );
        let error = credentials_from(
            CopilotIdentity::Enterprise,
            &saved,
            env_of(&[(ENTERPRISE_HOST_ENV, HOST)]),
        )
        .err()
        .unwrap();
        assert!(message(error).contains(&format!("caudra auth login {ENTERPRISE_SLUG}")));
    }

    #[test_case(CopilotAccount::Public, PUBLIC_USER_URL ; "public")]
    #[test_case(CopilotAccount::Enterprise(enterprise_host()), "https://api.myco.ghe.com/user" ; "enterprise")]
    fn identity_is_read_from_the_accounts_own_api(account: CopilotAccount, expected: &str) {
        assert_eq!(account.user_url(), expected);
    }

    #[test]
    fn github_login_names_the_account() {
        let identity =
            github_identity(&json!({"login": GITHUB_LOGIN, "id": 1}).to_string()).unwrap();

        assert_eq!(identity.subject.as_deref(), Some(GITHUB_LOGIN));
        assert_eq!(
            identity
                .suggested_name()
                .map(|name| name.to_string())
                .as_deref(),
            Some(GITHUB_LOGIN)
        );
    }

    #[test_case(r#"{"github.com": {"oauth_token": "token-1"}}"#, PUBLIC_HOST => Some("token-1".to_owned()) ; "json_public")]
    #[test_case(r#"{"github.com": {"oauth_token": "token-1"}}"#, HOST => None ; "json_public_not_enterprise")]
    #[test_case(r#"{"myco.ghe.com": {"oauth_token": "ghe"}, "github.com": {"oauth_token": "pub"}}"#, HOST => Some("ghe".to_owned()) ; "json_exact_enterprise")]
    #[test_case(r#"{"github.com:Iv1.b507a08c87ecfe98": {"oauth_token": "app"}}"#, PUBLIC_HOST => Some("app".to_owned()) ; "json_app_key")]
    #[test_case(r#"{"myco.ghe.com.evil": {"oauth_token": "x"}}"#, HOST => None ; "json_host_prefix_only")]
    fn extract_json_token(contents: &str, host: &str) -> Option<String> {
        extract_oauth_token_json(contents, host)
    }

    #[test_case("github.com:\n  oauth_token: token-1\n  user: octocat\n", PUBLIC_HOST => Some("token-1".to_owned()) ; "yaml_public")]
    #[test_case("myco.ghe.com:\n  oauth_token: ghe\n", PUBLIC_HOST => None ; "yaml_enterprise_not_public")]
    #[test_case("github.com:\n  oauth_token: pub\nmyco.ghe.com:\n  oauth_token: ghe\n", HOST => Some("ghe".to_owned()) ; "yaml_exact_enterprise")]
    #[test_case("token: &token fake-token\ngithub.com:\n  oauth_token: *token\n", PUBLIC_HOST => Some("fake-token".to_owned()) ; "yaml_alias")]
    #[test_case("github.com:\n  oauth_token: true\n", PUBLIC_HOST => None ; "yaml_boolean_is_not_token")]
    #[test_case("github.com:\n  oauth_token: 'true'\n", PUBLIC_HOST => Some("true".to_owned()) ; "yaml_quoted_token")]
    #[test_case("github.com:\n  oauth_token: first\n  oauth_token: second\n", PUBLIC_HOST => None ; "yaml_duplicate_token")]
    #[test_case("github.com:\n  oauth_token: fake-token\n---\ngithub.com: {}", PUBLIC_HOST => None ; "yaml_multiple_documents")]
    #[test_case("42: {}\ngithub.com:\n  oauth_token: fake-token", PUBLIC_HOST => Some("fake-token".to_owned()) ; "yaml_nonstring_host_skipped")]
    #[test_case("github.com:\n  oauth_token: fake-token\n  user: [", PUBLIC_HOST => None ; "yaml_malformed_is_ignored")]
    fn extract_yaml_token(contents: &str, host: &str) -> Option<String> {
        extract_oauth_token_yaml(contents, host)
    }

    #[test]
    fn locked_keyring_hint_includes_host_account_and_error() {
        let hint = locked_keyring_hint("github.com", "janedoe", "access denied");
        assert!(hint.contains("github.com"));
        assert!(hint.contains("janedoe"));
        assert!(hint.contains(KEYRING_LOCKED_MESSAGE));
        assert!(hint.ends_with("(access denied)"));
    }

    #[test_case("go-keyring-base64:Z2hvX3Rva2Vu" => "gho_token".to_owned(); "go_keyring_prefixed")]
    #[test_case("gho_token" => "gho_token".to_owned(); "plain_token")]
    #[test_case("go-keyring-base64:!!!not-base64" => "go-keyring-base64:!!!not-base64".to_owned(); "invalid_base64_kept_raw")]
    fn test_decode_go_keyring(raw: &str) -> String {
        decode_go_keyring(raw)
    }

    #[test_case(
        r#"{"github.com": {"user": "janedoe", "oauth_token": "t"}}"#, "github.com" =>
        vec!["janedoe".to_owned()]; "json_user"
    )]
    #[test_case(
        "github.com:\n  user: janedoe\n  users:\n    janedoe:\n    janesmith_second:\n",
        "github.com" =>
        vec!["janedoe".to_owned(), "janesmith_second".to_owned()]; "yaml_user_and_users"
    )]
    #[test_case(
        "github.com:\n  user: janedoe\n", "myco.ghe.com" =>
        Vec::<String>::new(); "missing_host"
    )]
    #[test_case(
        "github.com:\n  user: true\n  users: {42: {}, octocat: {}}", PUBLIC_HOST =>
        vec!["octocat".to_owned()]; "yaml_nonstring_usernames_skipped"
    )]
    #[test_case(
        r#"{"github.com": {"user": "first", "user": "second"}}"#, PUBLIC_HOST =>
        Vec::<String>::new(); "json_duplicate_user_is_ignored"
    )]
    fn test_config_usernames(contents: &str, host: &str) -> Vec<String> {
        config_usernames(contents, host)
    }

    #[test_case(
        vec![r#"{"github.com": {"oauth_token": "t"}}"#.to_owned()], "github.com" =>
        vec!["".to_owned()]; "host_without_username_gets_empty_account")]
    #[test_case(
        vec!["github.com:\n  user: janedoe\n".to_owned()], "github.com" =>
        vec!["janedoe".to_owned(), "".to_owned()]; "usernames_then_empty_account_last")]
    fn test_keyring_accounts(files: Vec<String>, host: &str) -> Vec<String> {
        keyring_accounts(&files, host)
    }

    #[test]
    fn oauth_request_sends_json_headers() {
        let request = oauth_request(&oauth_url(HOST, DEVICE_CODE_PATH), &json!({})).unwrap();
        assert_eq!(
            request.uri().to_string(),
            format!("https://{HOST}{DEVICE_CODE_PATH}")
        );
        assert_eq!(request.headers()["accept"], JSON_CONTENT_TYPE);
        assert_eq!(request.headers()["content-type"], JSON_CONTENT_TYPE);
        assert_eq!(
            request.headers()["user-agent"],
            crate::providers::user_agent()
        );
    }

    /// Replays scripted replies on a virtual clock that only `sleep` advances.
    struct FakeIo {
        start: Instant,
        elapsed: Cell<Duration>,
        replies: RefCell<VecDeque<HttpReply>>,
        requests: RefCell<Vec<(String, JsonValue)>>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl FakeIo {
        fn new(replies: Vec<(u16, JsonValue)>) -> Self {
            Self {
                start: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
                replies: RefCell::new(
                    replies
                        .into_iter()
                        .map(|(status, body)| HttpReply {
                            status,
                            body: body.to_string(),
                        })
                        .collect(),
                ),
                requests: RefCell::default(),
                sleeps: RefCell::default(),
            }
        }

        fn sleep_secs(&self) -> Vec<u64> {
            self.sleeps.borrow().iter().map(Duration::as_secs).collect()
        }
    }

    impl DeviceFlowIo for FakeIo {
        fn post_json(&self, url: &str, body: &JsonValue) -> Result<HttpReply, AgentError> {
            self.requests
                .borrow_mut()
                .push((url.to_owned(), body.clone()));
            Ok(self
                .replies
                .borrow_mut()
                .pop_front()
                .expect("unexpected request"))
        }

        fn now(&self) -> Instant {
            self.start + self.elapsed.get()
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
            self.elapsed.set(self.elapsed.get() + duration);
        }
    }

    fn challenge(host: &str) -> JsonValue {
        json!({
            "device_code": DEVICE_CODE,
            "user_code": USER_CODE,
            "verification_uri": format!("https://{host}/login/device"),
            "expires_in": 900,
            "interval": 5,
        })
    }

    fn pending() -> (u16, JsonValue) {
        (HTTP_OK, json!({"error": AUTHORIZATION_PENDING}))
    }

    fn granted() -> (u16, JsonValue) {
        (
            HTTP_OK,
            json!({"access_token": ACCESS_TOKEN, "token_type": "bearer"}),
        )
    }

    fn run(io: &FakeIo, host: &str) -> Result<String, AgentError> {
        device_login(io, host, |_| {})
    }

    #[test]
    fn device_flow_polls_until_granted_and_keeps_slow_down() {
        let io = FakeIo::new(vec![
            (HTTP_OK, challenge(HOST)),
            pending(),
            (HTTP_OK, json!({"error": SLOW_DOWN, "interval": 10})),
            pending(),
            granted(),
        ]);

        assert_eq!(run(&io, HOST).unwrap(), ACCESS_TOKEN);

        assert_eq!(io.sleep_secs(), [8, 8, 13, 13]);
        let requests = io.requests.borrow();
        assert_eq!(requests[0].0, format!("https://{HOST}{DEVICE_CODE_PATH}"));
        assert_eq!(
            requests[0].1,
            json!({"client_id": CLIENT_ID, "scope": OAUTH_SCOPE})
        );
        assert_eq!(requests[1].0, format!("https://{HOST}{ACCESS_TOKEN_PATH}"));
        assert_eq!(
            requests[1].1,
            json!({"client_id": CLIENT_ID, "device_code": DEVICE_CODE, "grant_type": DEVICE_GRANT})
        );
    }

    #[test]
    fn rate_limits_and_server_errors_keep_polling() {
        let io = FakeIo::new(vec![
            (HTTP_OK, challenge(PUBLIC_HOST)),
            (HTTP_TOO_MANY_REQUESTS, json!({})),
            (502, json!({})),
            granted(),
        ]);

        assert_eq!(run(&io, PUBLIC_HOST).unwrap(), ACCESS_TOKEN);
        assert_eq!(io.sleep_secs(), [8, 13, 13]);
    }

    #[test_case(json!({"error": ACCESS_DENIED}), DEVICE_DENIED ; "denied")]
    #[test_case(json!({"error": EXPIRED_TOKEN}), DEVICE_EXPIRED ; "expired")]
    #[test_case(json!({"error": "device_flow_disabled"}), "device_flow_disabled" ; "unsupported")]
    #[test_case(json!({"error": "<script>alert(1)</script>"}), UNKNOWN_ERROR_CODE ; "unsafe_code")]
    #[test_case(json!({"token_type": "bearer"}), TOKEN_MALFORMED ; "missing_token")]
    #[test_case(json!({"access_token": "two words"}), TOKEN_MALFORMED ; "unsafe_token")]
    fn oauth_errors_on_http_200_end_the_login(reply: JsonValue, expected: &str) {
        let io = FakeIo::new(vec![(HTTP_OK, challenge(HOST)), (HTTP_OK, reply)]);

        let error = message(run(&io, HOST).err().unwrap());

        assert!(error.contains(expected), "{error}");
        assert!(!error.contains(DEVICE_CODE));
    }

    #[test_case(302 ; "redirect_not_followed")]
    #[test_case(404 ; "not_found")]
    fn unexpected_status_ends_the_login(status: u16) {
        let io = FakeIo::new(vec![(HTTP_OK, challenge(HOST)), (status, json!("<html>"))]);

        let error = message(run(&io, HOST).err().unwrap());

        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert_eq!(io.requests.borrow().len(), 2);
    }

    #[test]
    fn polling_stops_at_the_challenge_deadline() {
        let mut short = challenge(HOST);
        short["expires_in"] = json!(10);
        let io = FakeIo::new(vec![(HTTP_OK, short), pending()]);

        let error = message(run(&io, HOST).err().unwrap());

        assert_eq!(error, DEVICE_EXPIRED);
        assert_eq!(io.sleep_secs(), [8, 2]);
        assert_eq!(io.requests.borrow().len(), 2);
    }

    #[test]
    fn server_expiry_is_capped_by_the_local_limit() {
        let mut long = challenge(HOST);
        long["expires_in"] = json!(86_400);
        long["interval"] = json!(MAX_INTERVAL.as_secs());
        let polls =
            (MAX_LOGIN_DURATION.as_secs() / (MAX_INTERVAL + POLL_MARGIN).as_secs()) as usize;
        let mut replies = vec![(HTTP_OK, long)];
        replies.extend(std::iter::repeat_with(pending).take(polls));
        let io = FakeIo::new(replies);

        assert_eq!(message(run(&io, HOST).err().unwrap()), DEVICE_EXPIRED);
        assert!(io.elapsed.get() <= MAX_LOGIN_DURATION);
    }

    #[test_case(|c: &mut JsonValue| { c.as_object_mut().unwrap().remove("device_code"); } ; "missing_device_code")]
    #[test_case(|c: &mut JsonValue| c["user_code"] = json!("WDJB MJHT") ; "user_code_with_space")]
    #[test_case(|c: &mut JsonValue| c["verification_uri"] = json!("https://evil.example/login/device") ; "other_origin")]
    #[test_case(|c: &mut JsonValue| c["verification_uri"] = json!("http://myco.ghe.com/login/device") ; "insecure_uri")]
    #[test_case(|c: &mut JsonValue| c["verification_uri"] = json!(format!("https://myco.ghe.com/login/device?code={DEVICE_CODE}")) ; "uri_leaks_device_code")]
    #[test_case(|c: &mut JsonValue| c["expires_in"] = json!(0) ; "zero_expiry")]
    #[test_case(|c: &mut JsonValue| c["device_code"] = json!("x".repeat(MAX_DEVICE_CODE_LEN + 1)) ; "oversized_device_code")]
    fn malformed_challenges_are_rejected(edit: fn(&mut JsonValue)) {
        let mut reply = challenge(HOST);
        edit(&mut reply);
        let io = FakeIo::new(vec![(HTTP_OK, reply)]);

        let error = message(run(&io, HOST).err().unwrap());

        assert_eq!(error, DEVICE_MALFORMED);
        assert_eq!(io.requests.borrow().len(), 1);
    }

    #[test]
    fn challenge_interval_is_clamped() {
        let mut reply = challenge(HOST);
        reply["interval"] = json!(0);
        let io = FakeIo::new(vec![(HTTP_OK, reply), granted()]);

        run(&io, HOST).unwrap();

        assert_eq!(io.sleeps.borrow()[0], MIN_INTERVAL + POLL_MARGIN);
    }

    #[test]
    fn device_code_error_reports_only_the_code() {
        let io = FakeIo::new(vec![(
            HTTP_OK,
            json!({"error": "unauthorized_client", "error_description": DEVICE_CODE}),
        )]);

        let error = message(run(&io, HOST).err().unwrap());

        assert!(error.ends_with("unauthorized_client"), "{error}");
    }

    #[test_case(CopilotAccount::Public, PUBLIC_HOST, None ; "public")]
    #[test_case(CopilotAccount::Enterprise(enterprise_host()), HOST, Some(HOST) ; "enterprise")]
    fn login_returns_the_token_and_its_host(
        account: CopilotAccount,
        host: &str,
        stored_host: Option<&str>,
    ) {
        let io = FakeIo::new(vec![(HTTP_OK, challenge(host)), granted()]);

        let token = login_with(&io, &account, |_| {}).unwrap();
        let login = login_credential(&account, token, AccountIdentity::default());

        assert_eq!(
            login.secret,
            StoredSecret::ApiKey(ProviderCredentials {
                api_key: ACCESS_TOKEN.into(),
                host: stored_host.map(str::to_owned),
            })
        );
    }

    #[test]
    fn identities_round_trip_their_slugs() {
        for identity in [CopilotIdentity::Public, CopilotIdentity::Enterprise] {
            assert_eq!(CopilotIdentity::from_slug(identity.slug()), Some(identity));
        }
        assert_eq!(CopilotIdentity::from_slug("openai"), None);
    }
}
