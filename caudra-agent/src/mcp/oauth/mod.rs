pub mod callback;
pub mod discovery;
pub mod manual;
pub mod pkce;
pub mod registration;
pub mod token;

use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use caudra_storage::StateDir;
use caudra_storage::auth::{McpAuthData, load_mcp_auth, save_mcp_auth};
use futures_lite::future;
use isahc::HttpClient;
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use isahc::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use isahc::http::{Request, StatusCode};
use isahc::net::dns::ResolveMap;
use tracing::{debug, info, warn};
use url::{Host, Url};

use self::callback::{CallbackResult, CallbackServer};
use self::discovery::{issuer_matches, parse_www_authenticate};
use super::config::{OauthClientConfig, resolve_url_addresses, risky_ip};
use super::error::McpError;
use super::http::{ACCEPT_VALUE, CT_JSON};
use super::protocol::{INITIALIZE_METHOD, JsonRpcRequest, initialize_params};

/// Caudra's OAuth Client ID Metadata Document. Authorization servers that
/// advertise `client_id_metadata_document_supported` fetch it to learn the
/// redirect URIs, so this URL is Caudra's client ID everywhere and must stay.
const CLIENT_METADATA_URL: &str = "https://caudra.ai/oauth/client-metadata.json";
const OAUTH_DOCS_URL: &str = "https://caudra.ai/docs/mcp/#oauth";
const PKCE_METHOD: &str = "S256";
const PROBE_REQUEST_ID: u64 = 0;
const BEARER_PREFIX: &str = "Bearer ";
/// Why a server that still answers 401 to a fresh token is not asked to sign
/// in again: the account signed in, and the server turned it away.
pub const REFUSED_ACCESS: &str = "the account signed in, but the server refused it. On Home Assistant, /api/mcp may need an administrator account, and /api/mcp/assist does not";
const AUTH_TIMEOUT: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// In-band refresh blocks requests waiting on the transport's auth lock, so it
/// gets a much tighter budget than the interactive flow.
const SILENT_REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("network error: {0}")]
    Network(String),
    #[error("server rejected request: HTTP {status} {body}")]
    ServerRejected { status: u16, body: String },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Copy)]
pub enum Interaction {
    Cli,
    Background,
}

pub async fn authenticate(
    server_name: &str,
    server_url: &str,
    www_authenticate: Option<&str>,
    storage: &StateDir,
    interaction: Interaction,
    static_client: Option<OauthClientConfig>,
    server_addresses: Option<&[IpAddr]>,
) -> Result<McpAuthData, McpError> {
    let wrap = |e: OAuthError| McpError::OAuthFailed {
        server: server_name.into(),
        reason: e.to_string(),
    };
    if let Some(existing) = load_mcp_auth(storage, server_name, server_url)
        && let Some(ref tokens) = existing.tokens
        && !tokens.is_expired()
    {
        return Ok(existing);
    }

    match silent_refresh(storage, server_name, server_url, server_addresses).await {
        Ok(Some(data)) => return Ok(data),
        Ok(None) => {}
        Err(e) => {
            warn!(server = server_name, error = %e, "token refresh failed, starting full flow");
        }
    }

    let probed = match www_authenticate {
        Some(_) => None,
        None => probe_challenge(server_url, server_addresses).await,
    };
    let www_auth = www_authenticate
        .or(probed.as_deref())
        .and_then(parse_www_authenticate);

    let resource_meta =
        discovery::discover_resource_metadata(server_url, www_auth.as_ref(), server_addresses)
            .await
            .map_err(wrap)?;

    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| discovery::server_origin(server_url));

    let auth_server =
        discovery::discover_auth_server(&auth_server_url, server_url, server_addresses)
            .await
            .map_err(wrap)?;
    require_pkce(&auth_server).map_err(wrap)?;
    let issuer = auth_server
        .issuer
        .clone()
        .unwrap_or_else(|| auth_server_url.clone());

    build_http_client(
        &auth_server.authorization_endpoint,
        server_url,
        server_addresses,
        HTTP_TIMEOUT,
    )
    .map_err(wrap)?;

    let callback = CallbackServer::bind(
        static_client.as_ref().and_then(|c| c.callback_port),
        static_client
            .as_ref()
            .and_then(|c| c.callback_path.as_deref()),
        static_client
            .as_ref()
            .and_then(|c| c.callback_hostname.as_deref()),
    )
    .await
    .map_err(|e| wrap(OAuthError::Other(e)))?;
    let redirect_uri = callback.redirect_uri();

    let stored = load_mcp_auth(storage, server_name, server_url);
    let choice =
        choose_client(static_client, stored, &redirect_uri, &issuer, &auth_server).map_err(wrap)?;
    let reg = match choice {
        ClientChoice::Known(reg) => reg,
        ClientChoice::Register(endpoint) => {
            let client = build_http_client(&endpoint, server_url, server_addresses, HTTP_TIMEOUT)
                .map_err(wrap)?;
            registration::register_client(&client, &endpoint, &redirect_uri)
                .await
                .map_err(wrap)?
        }
    };
    info!(server = server_name, %issuer, client_id = %reg.client_id, "using OAuth client");

    let pkce = pkce::generate().map_err(wrap)?;

    let mut state_buf = [0u8; 16];
    getrandom::fill(&mut state_buf)
        .map_err(|e| wrap(OAuthError::Other(format!("CSPRNG unavailable: {e}"))))?;
    let state = URL_SAFE_NO_PAD.encode(state_buf);

    let scope = www_auth
        .as_ref()
        .and_then(|w| w.scope.clone())
        .or_else(|| resource_meta.scopes_supported.as_ref().map(|s| s.join(" ")));

    let auth_url = build_authorization_url(
        &auth_server.authorization_endpoint,
        &reg.client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        scope.as_deref(),
        server_url,
    )
    .map_err(wrap)?;

    info!(server = server_name, endpoint = %auth_server.authorization_endpoint, "starting OAuth authorization");
    let result = match interaction {
        Interaction::Cli => {
            eprintln!("\nOpen this URL in your browser:\n\n  {auth_url}\n");

            if is_headless() {
                info!(
                    server = server_name,
                    "no display detected, skipping browser open"
                );
            } else if let Err(e) = open::that(&auth_url) {
                warn!(server = server_name, error = %e, "failed to open browser");
            }

            eprintln!("Waiting for callback on {redirect_uri}...");
            eprintln!("If this machine has no browser, log in on another device and paste");
            eprintln!("the full redirect URL ({redirect_uri}?...) here:");

            let callback_or_paste = future::race(
                callback.wait_for_callback(&state),
                manual::wait_for_paste(&state),
            );

            future::race(callback_or_paste, auth_timeout()).await
        }
        Interaction::Background => {
            let cause = if is_headless() {
                Some("no display to open a browser".to_string())
            } else {
                open::that(&auth_url)
                    .err()
                    .map(|e| format!("failed to open browser: {e}"))
            };
            match cause {
                Some(cause) => Err(format!("{cause}; run 'caudra mcp auth {server_name}'")),
                None => future::race(callback.wait_for_callback(&state), auth_timeout()).await,
            }
        }
    }
    .map_err(|e| wrap(OAuthError::Other(e)))?;
    check_response_issuer(
        &issuer,
        auth_server.authorization_response_iss_parameter_supported,
        result.issuer.as_deref(),
    )
    .map_err(wrap)?;

    let token_client = build_http_client(
        &auth_server.token_endpoint,
        server_url,
        server_addresses,
        HTTP_TIMEOUT,
    )
    .map_err(wrap)?;
    let tokens = token::exchange_code(
        &token_client,
        &auth_server.token_endpoint,
        &result.code,
        &redirect_uri,
        &pkce.verifier,
        &reg.client_id,
        reg.client_secret.as_deref(),
        server_url,
    )
    .await
    .map_err(wrap)?;

    let data = McpAuthData {
        server_url: server_url.to_string(),
        tokens: Some(tokens),
        client_id: reg.client_id,
        client_secret: reg.client_secret,
        client_secret_expires_at: reg.client_secret_expires_at,
        redirect_uri: Some(redirect_uri),
        token_endpoint: Some(auth_server.token_endpoint.clone()),
        issuer: Some(issuer),
    };

    save_mcp_auth(storage, server_name, &data)
        .map_err(|e| wrap(OAuthError::Other(e.to_string())))?;
    info!(server = server_name, "OAuth authentication complete");
    Ok(data)
}

/// Refresh stored tokens without any user interaction. `Ok(None)` means an
/// interactive flow is required (no stored auth or no refresh token).
pub async fn silent_refresh(
    storage: &StateDir,
    server_name: &str,
    server_url: &str,
    server_addresses: Option<&[IpAddr]>,
) -> Result<Option<McpAuthData>, OAuthError> {
    let Some(existing) = load_mcp_auth(storage, server_name, server_url) else {
        return Ok(None);
    };

    let Some(ref tokens) = existing.tokens else {
        return Ok(None);
    };

    if tokens.refresh.is_empty() {
        return Ok(None);
    }

    // Trust the endpoint pinned at interactive auth over fresh discovery: a
    // later-compromised server must not redirect the refresh token (and any
    // static client secret) elsewhere. Pre-pin records fall back to discovery.
    let token_endpoint = match existing.token_endpoint.clone() {
        Some(pinned) => pinned,
        None => {
            discover_auth_server_for(server_url, None, server_addresses)
                .await?
                .token_endpoint
        }
    };
    let client = build_http_client(
        &token_endpoint,
        server_url,
        server_addresses,
        SILENT_REFRESH_HTTP_TIMEOUT,
    )?;

    let new_tokens = token::refresh_token(
        &client,
        &token_endpoint,
        &tokens.refresh,
        &existing.client_id,
        existing.client_secret.as_deref(),
        server_url,
    )
    .await?;

    let data = McpAuthData {
        tokens: Some(new_tokens),
        ..existing
    };

    save_mcp_auth(storage, server_name, &data).map_err(|e| OAuthError::Other(e.to_string()))?;
    info!(server = server_name, "MCP OAuth tokens refreshed");

    Ok(Some(data))
}

async fn discover_auth_server_for(
    server_url: &str,
    www_auth: Option<&discovery::WwwAuthenticateInfo>,
    server_addresses: Option<&[IpAddr]>,
) -> Result<discovery::AuthServerMetadata, OAuthError> {
    let resource_meta =
        discovery::discover_resource_metadata(server_url, www_auth, server_addresses).await?;
    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| discovery::server_origin(server_url));
    discovery::discover_auth_server(&auth_server_url, server_url, server_addresses).await
}

enum ClientChoice {
    Known(registration::ClientRegistration),
    Register(String),
}

/// MCP client registration order: a configured client, then the client this
/// issuer already gave us, then Caudra's Client ID Metadata Document, then
/// dynamic registration. A stored client is bound to its issuer and redirect
/// URI, since client IDs are unique per authorization server.
fn choose_client(
    static_client: Option<OauthClientConfig>,
    stored: Option<McpAuthData>,
    redirect_uri: &str,
    issuer: &str,
    auth_server: &discovery::AuthServerMetadata,
) -> Result<ClientChoice, OAuthError> {
    if let Some(client) = static_client {
        return Ok(ClientChoice::Known(registration::ClientRegistration {
            client_id: client.client_id,
            client_secret: client.client_secret,
            client_secret_expires_at: None,
        }));
    }
    if let Some(existing) = stored
        && existing.redirect_uri.as_deref() == Some(redirect_uri)
        && existing
            .issuer
            .as_deref()
            .is_some_and(|stored_issuer| issuer_matches(issuer, stored_issuer))
    {
        return Ok(ClientChoice::Known(registration::ClientRegistration {
            client_id: existing.client_id,
            client_secret: existing.client_secret,
            client_secret_expires_at: existing.client_secret_expires_at,
        }));
    }
    if auth_server.client_id_metadata_document_supported {
        return Ok(ClientChoice::Known(registration::ClientRegistration {
            client_id: CLIENT_METADATA_URL.into(),
            client_secret: None,
            client_secret_expires_at: None,
        }));
    }
    match &auth_server.registration_endpoint {
        Some(endpoint) => Ok(ClientChoice::Register(endpoint.clone())),
        None => Err(OAuthError::Other(format!(
            "authorization server {issuer} offers neither client ID metadata documents nor dynamic client registration; register a client with it and set `oauth = {{ client_id = \"...\" }}` for this server (see {OAUTH_DOCS_URL})"
        ))),
    }
}

/// OAuth 2.1 and the MCP spec: without advertised S256, refuse rather than
/// send a code that an intercepting app could redeem.
fn require_pkce(auth_server: &discovery::AuthServerMetadata) -> Result<(), OAuthError> {
    if auth_server
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == PKCE_METHOD)
    {
        Ok(())
    } else {
        Err(OAuthError::Other(
            "authorization server does not advertise S256 PKCE (code_challenge_methods_supported)"
                .into(),
        ))
    }
}

/// RFC 9207: a code must come from the issuer the flow started with. `iss` is
/// mandatory only when the server advertises it.
fn check_response_issuer(
    expected: &str,
    advertised: bool,
    received: Option<&str>,
) -> Result<(), OAuthError> {
    match received {
        Some(found) if !issuer_matches(expected, found) => Err(OAuthError::Other(format!(
            "authorization response came from issuer {found}, expected {expected}"
        ))),
        None if advertised => Err(OAuthError::Other(
            "authorization response lacks the iss parameter its server advertises".into(),
        )),
        _ => Ok(()),
    }
}

/// An unauthenticated `initialize` draws the server's 401 challenge, which
/// names its resource metadata and the scope to request. Any other outcome
/// leaves discovery to the well-known URLs.
async fn probe_challenge(server_url: &str, server_addresses: Option<&[IpAddr]>) -> Option<String> {
    probe(server_url, None, server_addresses).await?.challenge
}

/// Whether the server still answers 401 to a request carrying `access_token`,
/// as Home Assistant's `/api/mcp` does for a user who is not an administrator.
/// A probe that gets no answer counts as accepted, so the call that follows
/// reports the real failure.
pub async fn token_refused(
    server_url: &str,
    access_token: &str,
    server_addresses: Option<&[IpAddr]>,
) -> bool {
    probe(server_url, Some(access_token), server_addresses)
        .await
        .is_some()
}

/// A 401 the server answered a probe with.
struct Unauthorized {
    challenge: Option<String>,
}

/// Sends `initialize` as the server's first request, with `bearer` when one is
/// given. `None` unless the server answered 401.
async fn probe(
    server_url: &str,
    bearer: Option<&str>,
    server_addresses: Option<&[IpAddr]>,
) -> Option<Unauthorized> {
    let client = build_http_client(server_url, server_url, server_addresses, HTTP_TIMEOUT).ok()?;
    let body = serde_json::to_vec(&JsonRpcRequest::new(
        PROBE_REQUEST_ID,
        INITIALIZE_METHOD,
        Some(initialize_params()),
    ))
    .ok()?;
    let mut request = Request::post(server_url)
        .header(CONTENT_TYPE, CT_JSON)
        .header(ACCEPT, ACCEPT_VALUE);
    if let Some(token) = bearer {
        request = request.header(AUTHORIZATION, format!("{BEARER_PREFIX}{token}"));
    }
    let request = request.body(body).ok()?;
    let response = smol::unblock(move || client.send(request))
        .await
        .inspect_err(|error| debug!(%error, "OAuth probe failed"))
        .ok()?;
    if response.status() != StatusCode::UNAUTHORIZED {
        debug!(status = %response.status(), "OAuth probe got no 401");
        return None;
    }
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    Some(Unauthorized { challenge })
}

async fn auth_timeout() -> Result<CallbackResult, String> {
    smol::Timer::after(AUTH_TIMEOUT).await;
    Err(format!(
        "OAuth flow timed out after {} minutes",
        AUTH_TIMEOUT.as_secs() / 60
    ))
}

fn is_headless() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

fn build_http_client(
    endpoint: &str,
    server_url: &str,
    server_addresses: Option<&[IpAddr]>,
    timeout: Duration,
) -> Result<HttpClient, OAuthError> {
    let endpoint_url =
        Url::parse(endpoint).map_err(|error| OAuthError::Other(error.to_string()))?;
    if !endpoint_url.username().is_empty() || endpoint_url.password().is_some() {
        return Err(OAuthError::Other(
            "OAuth endpoint must not contain URL credentials".into(),
        ));
    }
    let same_server_origin = same_origin(&endpoint_url, server_url);
    if endpoint_url.scheme() != "https" && !same_server_origin {
        return Err(OAuthError::Other(
            "OAuth endpoint must use HTTPS or the reviewed MCP origin".into(),
        ));
    }
    let addresses = if same_server_origin {
        server_addresses
            .filter(|addresses| !addresses.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| resolve_url_addresses(endpoint).unwrap_or_default())
    } else {
        resolve_url_addresses(endpoint).map_err(OAuthError::Other)?
    };
    if addresses.is_empty() {
        return Err(OAuthError::Other(
            "OAuth endpoint resolved to no addresses".into(),
        ));
    }
    if !same_server_origin && addresses.iter().copied().any(risky_ip) {
        return Err(OAuthError::Other(
            "OAuth endpoint resolves to a private or reserved address".into(),
        ));
    }
    let mut builder = HttpClient::builder()
        .redirect_policy(RedirectPolicy::None)
        // Same pin as mcp::http, so oauth and data traffic match.
        .version_negotiation(VersionNegotiation::http11())
        .timeout(timeout);
    if let (Some(Host::Domain(host)), Some(port)) =
        (endpoint_url.host(), endpoint_url.port_or_known_default())
    {
        let mappings = addresses.iter().fold(ResolveMap::new(), |map, address| {
            map.add(host, port, *address)
        });
        builder = builder.dns_resolve(mappings);
    }
    builder
        .build()
        .map_err(|error| OAuthError::Other(error.to_string()))
}

fn same_origin(endpoint: &Url, server_url: &str) -> bool {
    let Ok(server) = Url::parse(server_url) else {
        return false;
    };
    endpoint.scheme() == server.scheme()
        && endpoint.host() == server.host()
        && endpoint.port_or_known_default() == server.port_or_known_default()
}

fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    scope: Option<&str>,
    resource: &str,
) -> Result<String, OAuthError> {
    let parsed = Url::parse(authorization_endpoint).map_err(|e| {
        OAuthError::InvalidResponse(format!(
            "invalid authorization endpoint {authorization_endpoint}: {e}"
        ))
    })?;
    let sep = if parsed.query().is_some() { '&' } else { '?' };
    let mut url = format!(
        "{authorization_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&state={state}&code_challenge={code_challenge}&code_challenge_method=S256&resource={}",
        token::url_encode(client_id),
        token::url_encode(redirect_uri),
        token::url_encode(resource),
    );
    if let Some(s) = scope {
        url.push_str("&scope=");
        url.push_str(&token::url_encode(s));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{IpAddr, Ipv4Addr, TcpListener};
    use std::thread;
    use std::time::Duration;

    use caudra_storage::auth::McpAuthData;
    use test_case::test_case;
    use url::Url;

    use super::discovery::AuthServerMetadata;
    use super::{
        CLIENT_METADATA_URL, ClientChoice, OAuthError, build_authorization_url, build_http_client,
        check_response_issuer, choose_client, require_pkce, token_refused,
    };
    use crate::mcp::config::OauthClientConfig;

    const CLIENT_ID: &str = "client-id";
    const REDIRECT_URI: &str = "http://127.0.0.1:8080/callback";
    const STATE: &str = "state-value";
    const CHALLENGE: &str = "challenge-value";
    const SCOPE: &str = "offline#access";
    const RESOURCE: &str = "https://example.com/resource a b";
    const ISSUER: &str = "https://auth.example.com";
    const OTHER_ISSUER: &str = "https://other.example.com";
    const REGISTRATION_ENDPOINT: &str = "https://auth.example.com/register";
    const STATIC_ID: &str = "static-client";
    const STORED_ID: &str = "stored-client";
    const REGISTER: &str = "register";
    const REFUSED_TOKEN: &str = "refused";
    const ACCEPTED_TOKEN: &str = "accepted";
    const UNAUTHORIZED_STATUS: &str = "401 Unauthorized";
    const OK_STATUS: &str = "200 OK";

    fn auth_server(metadata_documents: bool, registration: bool) -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: Some(ISSUER.into()),
            authorization_endpoint: format!("{ISSUER}/authorize"),
            token_endpoint: format!("{ISSUER}/token"),
            registration_endpoint: registration.then(|| REGISTRATION_ENDPOINT.into()),
            code_challenge_methods_supported: vec!["S256".into()],
            client_id_metadata_document_supported: metadata_documents,
            authorization_response_iss_parameter_supported: false,
        }
    }

    fn stored(redirect_uri: &str, issuer: Option<&str>) -> McpAuthData {
        McpAuthData {
            server_url: "https://mcp.example.com/mcp".into(),
            tokens: None,
            client_id: STORED_ID.into(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: Some(redirect_uri.into()),
            token_endpoint: None,
            issuer: issuer.map(String::from),
        }
    }

    fn static_client() -> OauthClientConfig {
        OauthClientConfig {
            client_id: STATIC_ID.into(),
            client_secret: None,
            callback_port: None,
            callback_path: None,
            callback_hostname: None,
        }
    }

    fn chosen(choice: ClientChoice) -> String {
        match choice {
            ClientChoice::Known(reg) => reg.client_id,
            ClientChoice::Register(endpoint) => {
                assert_eq!(endpoint, REGISTRATION_ENDPOINT);
                REGISTER.into()
            }
        }
    }

    #[test_case(true,  Some(stored(REDIRECT_URI, Some(ISSUER))),       true,  true,  STATIC_ID           ; "static_client_wins")]
    #[test_case(false, Some(stored(REDIRECT_URI, Some(ISSUER))),       true,  true,  STORED_ID           ; "stored_client_of_same_issuer")]
    #[test_case(false, Some(stored(REDIRECT_URI, Some(OTHER_ISSUER))), false, true,  REGISTER            ; "stored_client_of_other_issuer_dropped")]
    #[test_case(false, Some(stored(REDIRECT_URI, None)),               true,  false, CLIENT_METADATA_URL ; "stored_client_without_issuer_dropped")]
    #[test_case(false, Some(stored("http://127.0.0.1:1/cb", Some(ISSUER))), false, true, REGISTER        ; "stored_client_of_other_redirect_dropped")]
    #[test_case(false, None,                                           true,  true,  CLIENT_METADATA_URL ; "metadata_document_before_registration")]
    #[test_case(false, None,                                           false, true,  REGISTER            ; "registration_without_metadata_document")]
    fn chooses_client(
        has_static: bool,
        stored: Option<McpAuthData>,
        metadata_documents: bool,
        registration: bool,
        expected: &str,
    ) {
        let choice = choose_client(
            has_static.then(static_client),
            stored,
            REDIRECT_URI,
            ISSUER,
            &auth_server(metadata_documents, registration),
        )
        .unwrap_or_else(|e| panic!("expected a client, got {e}"));
        assert_eq!(chosen(choice), expected);
    }

    #[test]
    fn no_way_to_get_a_client_names_static_config() {
        let Err(error) =
            choose_client(None, None, REDIRECT_URI, ISSUER, &auth_server(false, false))
        else {
            panic!("expected an error");
        };
        let message = error.to_string();
        assert!(message.contains(ISSUER));
        assert!(message.contains("oauth = {"));
    }

    #[test_case(&[],                 false ; "absent_refused")]
    #[test_case(&["plain"],          false ; "plain_only_refused")]
    #[test_case(&["plain", "S256"],  true  ; "s256_accepted")]
    fn pkce_support(methods: &[&str], should_pass: bool) {
        let meta = AuthServerMetadata {
            code_challenge_methods_supported: methods.iter().map(|m| m.to_string()).collect(),
            ..auth_server(false, true)
        };
        assert_eq!(require_pkce(&meta).is_ok(), should_pass);
    }

    #[test_case(false, None,                         true  ; "not_advertised_absent")]
    #[test_case(true,  None,                         false ; "advertised_absent")]
    #[test_case(true,  Some(ISSUER),                 true  ; "advertised_matching")]
    #[test_case(false, Some("https://auth.example.com/"), true ; "trailing_slash")]
    #[test_case(false, Some(OTHER_ISSUER),           false ; "mismatch_refused_even_unadvertised")]
    fn response_issuer(advertised: bool, received: Option<&str>, should_pass: bool) {
        assert_eq!(
            check_response_issuer(ISSUER, advertised, received).is_ok(),
            should_pass
        );
    }

    fn query_pairs(url: &str) -> Vec<(String, String)> {
        let parsed = Url::parse(url).expect("built authorization URL should parse");
        parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn has(pairs: &[(String, String)], key: &str, value: &str) -> bool {
        pairs.iter().any(|(k, v)| k == key && v == value)
    }

    #[test_case("https://example.com/authorize", None; "plain_endpoint")]
    #[test_case("https://mcp-slack.example.com/dcr/authorize?provider=slack", Some("slack"); "slack_provider")]
    fn builds_authorization_url(endpoint: &str, provider: Option<&str>) {
        let url = build_authorization_url(
            endpoint,
            CLIENT_ID,
            REDIRECT_URI,
            STATE,
            CHALLENGE,
            Some(SCOPE),
            RESOURCE,
        )
        .expect("endpoint should parse");

        assert!(url.contains("resource=https%3A%2F%2Fexample.com%2Fresource%20a%20b"));
        assert!(url.contains("scope=offline%23access"));

        let pairs = query_pairs(&url);
        assert!(has(&pairs, "response_type", "code"));
        assert!(has(&pairs, "client_id", CLIENT_ID));
        assert!(has(&pairs, "resource", RESOURCE));
        assert!(has(&pairs, "scope", SCOPE));
        if let Some(expected) = provider {
            assert!(has(&pairs, "provider", expected));
        }
    }

    #[test_case("not a url"; "invalid_endpoint")]
    fn invalid_endpoint_returns_error(endpoint: &str) {
        assert!(matches!(
            build_authorization_url(
                endpoint,
                CLIENT_ID,
                REDIRECT_URI,
                STATE,
                CHALLENGE,
                None,
                RESOURCE
            ),
            Err(OAuthError::InvalidResponse(_))
        ));
    }

    #[test]
    fn oauth_http_client_rejects_unreviewed_private_origin() {
        assert!(
            build_http_client(
                "https://10.0.0.2/token",
                "https://8.8.8.8/mcp",
                Some(&[IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]),
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    #[test]
    fn oauth_http_client_allows_and_pins_reviewed_origin() {
        let reviewed = [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))];
        assert!(
            build_http_client(
                "https://private.example/token",
                "https://private.example/mcp",
                Some(&reviewed),
                Duration::from_secs(1),
            )
            .is_ok()
        );
    }

    /// A server that answers one request: 401 when it carries `refused` as
    /// its bearer token, 200 otherwise.
    fn serve_once() -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut refused = false;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                refused |= line.to_ascii_lowercase()
                    == format!("authorization: bearer {REFUSED_TOKEN}\r\n");
                line.clear();
            }
            let status = if refused {
                UNAUTHORIZED_STATUS
            } else {
                OK_STATUS
            };
            write!(
                &stream,
                "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            )
            .unwrap();
        });
        url
    }

    #[test_case(REFUSED_TOKEN, true ; "still_401")]
    #[test_case(ACCEPTED_TOKEN, false ; "accepted")]
    fn a_token_the_server_turns_away_is_refused(token: &str, refused: bool) {
        let url = serve_once();
        let addresses = [IpAddr::V4(Ipv4Addr::LOCALHOST)];

        assert_eq!(
            smol::block_on(token_refused(&url, token, Some(&addresses))),
            refused
        );
    }
}
