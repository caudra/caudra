use caudra_config::sandbox::{Revision, SandboxName, SandboxOrigin};
use caudra_storage::sandbox_auth::SandboxApiKey;
use futures_lite::io::AsyncReadExt;
use isahc::{
    HttpClient, Request,
    config::{Configurable, RedirectPolicy, VersionNegotiation},
    error::ErrorKind,
    http::{HeaderValue, Method},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{net::IpAddr, time::Duration};
use url::{Url, form_urlencoded};
use uuid::Uuid;

use crate::{
    Error, Result, TransportDiagnostic,
    dto::{
        Create, Credentials, Discovery, Expected, FailureEnvelope, Instance, MAX_CATALOG_SIZE,
        MAX_PAGE_SIZE, Operation, Page, Policy, Template, identifier, timestamp,
    },
};

const API_PREFIX: &str = "/daemon/v1";
const MAX_BODY_BYTES: u64 = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEY_BYTES: usize = 32;
const HEX: &[u8; 16] = b"0123456789abcdef";
const UNAVAILABLE: &str =
    "read-only request unavailable; check provider daemon and proxy configuration";
const UNKNOWN_OUTCOME: &str = "outcome may be unknown; inspect instead of replaying; check provider daemon and proxy configuration";
const SETUP_UNAVAILABLE: &str =
    "client unavailable; no request sent; check local HTTP client configuration";
const BODY_FAILURE: &str = "response body read failed";

fn transport_failure(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ConnectionFailed => "connection failed (daemon, proxy, or TLS handshake)",
        ErrorKind::NameResolution => "name resolution failed",
        ErrorKind::BadClientCertificate => "TLS client certificate rejected",
        ErrorKind::BadServerCertificate => "TLS server certificate validation failed",
        ErrorKind::InvalidTlsConfiguration | ErrorKind::TlsEngine => {
            "TLS configuration or engine failed"
        }
        ErrorKind::Timeout => "request timed out",
        ErrorKind::Io => "request/response I/O failed",
        ErrorKind::ClientInitialization => "HTTP client initialization failed",
        ErrorKind::ProtocolViolation => "HTTP transport protocol failed",
        _ => "HTTP request failed",
    }
}

fn transport_operation(method: &Method, path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or_default();
    if method == Method::GET {
        return match path {
            "/discover" => "discovery",
            "/templates" => "template listing",
            "/instances" => "instance listing",
            _ if path.starts_with("/templates/") => "template inspection",
            _ if path.starts_with("/operations/") => "operation inspection",
            _ => "instance inspection",
        };
    }
    if method == Method::DELETE {
        return "delete";
    }
    match path.rsplit('/').next() {
        Some("instances") => "create",
        Some("resume") => "resume",
        Some("pause") => "pause",
        Some("renew") => "lease renewal",
        Some("policy") => "network policy update",
        Some("credentials") => "credential acquisition",
        Some("cancel") => "create cancellation",
        _ => "lifecycle mutation",
    }
}

pub struct LifecycleClient {
    origin: SandboxOrigin,
    key: SandboxApiKey,
    http: HttpClient,
}

impl LifecycleClient {
    pub fn new(origin: SandboxOrigin, key: SandboxApiKey) -> Result<Self> {
        Self::with_timeout(origin, key, REQUEST_TIMEOUT)
    }

    fn with_timeout(origin: SandboxOrigin, key: SandboxApiKey, timeout: Duration) -> Result<Self> {
        if key.expose_secret().len() < KEY_BYTES {
            return Err(Error::Credential);
        }
        let url = Url::parse(origin.as_str()).map_err(|_| Error::Protocol)?;
        let mut builder = HttpClient::builder()
            .redirect_policy(RedirectPolicy::None)
            .version_negotiation(VersionNegotiation::http11())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout);
        if url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        }) {
            builder = builder.proxy(None);
        }
        Ok(Self {
            origin,
            key,
            http: builder.build().map_err(|error| {
                Error::Transport(TransportDiagnostic {
                    operation: "client setup",
                    failure: transport_failure(error.kind()),
                    guidance: SETUP_UNAVAILABLE,
                })
            })?,
        })
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
        key: Option<&str>,
    ) -> Result<T> {
        let operation = transport_operation(&method, path);
        let guidance = if method == Method::GET {
            UNAVAILABLE
        } else {
            UNKNOWN_OUTCOME
        };
        let accepted_allowed = method == Method::POST
            && (path == "/instances"
                || path
                    .strip_prefix("/operations/")
                    .and_then(|path| path.strip_suffix("/cancel"))
                    .is_some_and(|key| validate_key(key).is_ok()));
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| Error::Protocol)?
            .unwrap_or_default();
        if body.len() as u64 > MAX_BODY_BYTES {
            return Err(Error::Protocol);
        }
        let mut auth =
            HeaderValue::from_str(self.key.expose_secret()).map_err(|_| Error::Credential)?;
        auth.set_sensitive(true);
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{API_PREFIX}{path}", self.origin.as_str()))
            .header("X-API-Key", auth)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let request = request.body(body).map_err(|_| Error::Protocol)?;
        let mut response = self.http.send_async(request).await.map_err(|error| {
            Error::Transport(TransportDiagnostic {
                operation,
                failure: transport_failure(error.kind()),
                guidance,
            })
        })?;
        let status = response.status().as_u16();
        if response.status().is_redirection() {
            return Err(Error::Protocol);
        }
        if status == 202 && !accepted_allowed {
            return Err(Error::Protocol);
        }
        if response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            != Some("no-store")
        {
            return Err(Error::Protocol);
        }
        if response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|len| len > MAX_BODY_BYTES)
        {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| {
                Error::Transport(TransportDiagnostic {
                    operation,
                    failure: BODY_FAILURE,
                    guidance,
                })
            })?;
        if bytes.len() as u64 > MAX_BODY_BYTES {
            return Err(Error::Protocol);
        }
        if !matches!(status, 200 | 202) {
            let failure: FailureEnvelope =
                serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            return Err(Error::Daemon {
                status,
                code: failure.error.code,
                retryable: failure.error.retryable,
                outcome_unknown: failure.error.outcome_unknown,
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request(Method::GET, path, None::<&()>, None).await
    }

    pub async fn discover(&self) -> Result<Discovery> {
        let discovery: Discovery = self.get("/discover").await?;
        identifier(&discovery.owner_id)?;
        timestamp(&discovery.server_time)?;
        let c = &discovery.capabilities;
        if discovery.api_version != "1"
            || discovery.authentication != "api_key_namespace"
            || discovery.idempotency_key != "uuidv7"
            || discovery.recovery != "query_operation_never_replay_unknown"
            || discovery.credential_scope != "sandbox_lifetime"
            || discovery.proxy_origin != "client_configured"
            || !c.idempotent_create
            || !c.operation_lookup
            || !c.conditional_mutations
            || !c.explicit_credentials
            || !c.template_catalog
            || !c.conditional_template_create
            || c.memory_pause
            || c.warm_start
            || discovery.tls_modes.len() > 2
            || discovery
                .tls_modes
                .iter()
                .enumerate()
                .any(|(index, mode)| discovery.tls_modes[..index].contains(mode))
            || (!c.egress_policy && (!discovery.tls_modes.is_empty() || c.live_tls_mode_change))
            || discovery.limits.max_lease_seconds == 0
            || discovery.limits.list_page_size as usize > MAX_PAGE_SIZE
        {
            return Err(Error::Protocol);
        }
        Ok(discovery)
    }

    /// Without a revision the daemon answers with the catalog head, the revision a create launches.
    pub async fn template(
        &self,
        id: &SandboxName,
        revision: Option<&Revision>,
    ) -> Result<Template> {
        let route = match revision {
            Some(revision) => format!("/templates/{id}?revision={}", revision.as_str()),
            None => format!("/templates/{id}"),
        };
        let template: Template = self.get(&route).await?;
        if &template.manifest.id != id
            || revision.is_some_and(|revision| &template.revision != revision)
        {
            return Err(Error::Identity);
        }
        Ok(template)
    }

    pub async fn templates(&self) -> Result<Vec<Template>> {
        self.pages("templates", |item: &Template| item.manifest.id.as_str())
            .await
    }

    pub async fn instances(&self, owner: &str) -> Result<Vec<Instance>> {
        let instances = self
            .pages("instances", |item: &Instance| item.sandbox_id.as_str())
            .await?;
        for instance in &instances {
            instance.validate(owner)?;
        }
        Ok(instances)
    }

    async fn pages<T: DeserializeOwned>(
        &self,
        route: &str,
        id: impl Fn(&T) -> &str,
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut after = String::new();
        loop {
            let path = if after.is_empty() {
                format!("/{route}")
            } else {
                let query = form_urlencoded::Serializer::new(String::new())
                    .append_pair("after", &after)
                    .finish();
                format!("/{route}?{query}")
            };
            let page: Page<T> = self.get(&path).await?;
            if page.items.len() > MAX_PAGE_SIZE || items.len() + page.items.len() > MAX_CATALOG_SIZE
            {
                return Err(Error::Protocol);
            }
            let mut previous = after.clone();
            for item in &page.items {
                identifier(id(item))?;
                if id(item) <= previous.as_str() {
                    return Err(Error::Protocol);
                }
                previous = id(item).to_owned();
            }
            if !page.next_after.is_empty() && (page.items.is_empty() || page.next_after != previous)
            {
                return Err(Error::Protocol);
            }
            items.extend(page.items);
            if page.next_after.is_empty() {
                return Ok(items);
            }
            after = page.next_after;
        }
    }

    pub async fn operation(&self, key: &str) -> Result<Operation> {
        validate_key(key)?;
        self.get(&format!("/operations/{key}")).await
    }

    pub async fn instance(&self, id: &str) -> Result<Instance> {
        identifier(id)?;
        self.get(&format!("/instances/{id}")).await
    }

    pub(crate) async fn create(&self, key: &str, create: &Create) -> Result<Operation> {
        validate_key(key)?;
        self.request(Method::POST, "/instances", Some(create), Some(key))
            .await
    }

    pub(crate) async fn control(
        &self,
        id: &str,
        action: &str,
        expected: &Expected,
        lease: Option<u32>,
    ) -> Result<Instance> {
        identifier(id)?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Control<'a> {
            #[serde(flatten)]
            expected: &'a Expected,
            #[serde(skip_serializing_if = "Option::is_none")]
            lease_seconds: Option<u32>,
        }
        let (method, path) = if action == "delete" {
            (Method::DELETE, format!("/instances/{id}"))
        } else {
            (Method::POST, format!("/instances/{id}/{action}"))
        };
        self.request(
            method,
            &path,
            Some(&Control {
                expected,
                lease_seconds: lease,
            }),
            None,
        )
        .await
    }

    pub(crate) async fn credentials(&self, instance: &Instance) -> Result<Credentials> {
        identifier(&instance.sandbox_id)?;
        let credentials: Credentials = self
            .request(
                Method::POST,
                &format!("/instances/{}/credentials", instance.sandbox_id),
                Some(&instance.expected()),
                None,
            )
            .await?;
        if credentials.instance != *instance
            || !credentials.valid_token()
            || credentials.mcp_path != format!("/sandboxes/{}/mcp", instance.sandbox_id)
            || credentials.files_path != "/files"
            || credentials.credential_scope != "sandbox_lifetime"
        {
            return Err(Error::Identity);
        }
        Ok(credentials)
    }

    pub(crate) async fn apply_policy(
        &self,
        id: &str,
        expected: &Expected,
        policy: &Policy,
    ) -> Result<Instance> {
        identifier(id)?;
        #[derive(Serialize)]
        struct Update<'a> {
            #[serde(flatten)]
            expected: &'a Expected,
            egress: &'a Policy,
        }
        self.request(
            Method::PUT,
            &format!("/instances/{id}/policy"),
            Some(&Update {
                expected,
                egress: policy,
            }),
            None,
        )
        .await
    }

    pub(crate) async fn cancel_create(&self, key: &str, execution: &str) -> Result<Operation> {
        validate_key(key)?;
        identifier(execution)?;
        #[derive(Serialize)]
        struct Cancel<'a> {
            #[serde(rename = "expectedExecutionID")]
            execution: &'a str,
        }
        self.request(
            Method::POST,
            &format!("/operations/{key}/cancel"),
            Some(&Cancel { execution }),
            None,
        )
        .await
    }
}

fn validate_key(key: &str) -> Result<()> {
    let parsed = Uuid::parse_str(key).map_err(|_| Error::Protocol)?;
    if parsed.get_version_num() != 7 || parsed.to_string() != key {
        return Err(Error::Protocol);
    }
    Ok(())
}

pub fn generate_api_key() -> Result<SandboxApiKey> {
    let mut bytes = [0; KEY_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| Error::Credential)?;
    let mut key = String::with_capacity(KEY_BYTES * 2);
    for byte in bytes {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(SandboxApiKey::new(key)?)
}

#[cfg(test)]
mod tests {
    use super::{
        BODY_FAILURE, LifecycleClient, MAX_BODY_BYTES, UNAVAILABLE, UNKNOWN_OUTCOME,
        generate_api_key, transport_failure, transport_operation,
    };
    use crate::{Error, TransportDiagnostic};
    use caudra_config::sandbox::SandboxOrigin;
    use isahc::{error::ErrorKind, http::Method};
    use serde_json::Value;
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::Duration,
    };
    use test_case::test_case;

    const SECRET: &str = "never-reflect-this-secret";
    const TEST_TIMEOUT: Duration = Duration::from_millis(500);
    const IO_TIMEOUT: Duration = Duration::from_secs(5);
    const CANCEL_PATH: &str = "/operations/0199650a-9e00-7000-8000-000000000001/cancel";
    const TIMEOUT_FAILURE: &str = "request timed out";
    const TLS_FAILURE: &str = "TLS server certificate validation failed";

    fn read_headers(stream: &TcpStream) {
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
    }

    #[test_case(Method::POST, "/instances", true; "create")]
    #[test_case(Method::POST, CANCEL_PATH, true; "cancel")]
    #[test_case(Method::GET, CANCEL_PATH, false; "cancel_read")]
    #[test_case(Method::POST, "/operations/not-a-uuid/cancel", false; "invalid_operation")]
    #[test_case(Method::POST, "/operations/nested/0199650a-9e00-7000-8000-000000000001/cancel", false; "nested_cancel")]
    #[test_case(Method::POST, "/instances/instance-test/pause", false; "pause")]
    #[test_case(Method::POST, "/instances/instance-test/renew", false; "renew")]
    #[test_case(Method::POST, "/instances/instance-test/credentials", false; "credentials")]
    #[test_case(Method::PUT, "/instances/instance-test/policy", false; "policy")]
    fn accepted_is_limited_to_exact_async_routes(method: Method, path: &str, allowed: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&stream);
            let _ = write!(
                stream,
                "HTTP/1.1 202 Accepted\r\nCache-Control: no-store\r\nContent-Length: 2\r\n\r\n{{}}"
            );
        });
        let client = LifecycleClient::new(origin, generate_api_key().unwrap()).unwrap();
        let result = smol::block_on(client.request::<Value>(method, path, None::<&()>, None));
        server.join().unwrap();
        assert_eq!(result.is_ok(), allowed, "{result:?}");
    }

    #[test_case(302, "{}"; "redirect")]
    #[test_case(202, "{}"; "unexpected_accepted_read")]
    #[test_case(200, "not json"; "invalid_json")]
    #[test_case(500, "{\"runtimeDiagnostics\":\"never-reflect-this-secret\"}"; "untyped_error")]
    #[test_case(500, "{\"error\":{\"code\":\"never-reflect-this-secret\",\"retryable\":false,\"outcomeUnknown\":true}}"; "unknown_code_redacted")]
    fn http_bounds_and_errors_do_not_reflect_bodies(status: u16, body: &'static str) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&stream);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} response\r\nCache-Control: no-store\r\nLocation: https://untrusted.test/\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        });
        let client = LifecycleClient::new(origin, generate_api_key().unwrap()).unwrap();
        let error = smol::block_on(client.get::<Value>("/discover")).unwrap_err();
        assert!(!format!("{error:?}: {error}").contains(SECRET));
        server.join().unwrap();
    }

    #[test]
    fn response_limit_prevents_unbounded_body_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{{",
                MAX_BODY_BYTES + 1
            )
            .unwrap();
            released.recv_timeout(IO_TIMEOUT).unwrap();
        });
        let client = LifecycleClient::new(origin, generate_api_key().unwrap()).unwrap();
        let result = smol::block_on(client.get::<Value>("/discover"));
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(matches!(result, Err(Error::Protocol)), "{result:?}");
    }

    #[test]
    fn stalled_response_times_out_without_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            released.recv_timeout(IO_TIMEOUT).unwrap();
        });
        let client =
            LifecycleClient::with_timeout(origin, generate_api_key().unwrap(), TEST_TIMEOUT)
                .unwrap();
        let error = smol::block_on(client.get::<Value>("/discover")).unwrap_err();
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(error.to_string().contains(TIMEOUT_FAILURE));
        assert!(error.to_string().contains(UNAVAILABLE));
        assert!(!error.to_string().contains(UNKNOWN_OUTCOME));
    }

    #[test]
    fn unreachable_discovery_is_unavailable_not_unknown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let client = LifecycleClient::new(origin, generate_api_key().unwrap()).unwrap();
        drop(listener);
        let error = smol::block_on(client.discover()).unwrap_err();
        let Error::Transport(diagnostic) = &error else {
            panic!("{error:?}")
        };
        assert_eq!(diagnostic.operation, "discovery");
        assert_eq!(
            diagnostic.failure,
            transport_failure(&ErrorKind::ConnectionFailed)
        );
        assert!(error.to_string().contains(UNAVAILABLE));
        assert!(!error.to_string().contains("unknown"));
        assert!(!error.to_string().contains("replaying"));
    }

    #[test_case(false; "lost_headers")]
    #[test_case(true; "lost_body")]
    fn dispatched_mutation_lost_response_remains_unknown(partial_body: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin =
            SandboxOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&stream);
            if partial_body {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nContent-Length: 100\r\n\r\n{{"
                )
                .unwrap();
            }
        });
        let key = generate_api_key().unwrap();
        let secret = key.expose_secret().to_owned();
        let client = LifecycleClient::new(origin, key).unwrap();
        let path = format!("/instances/{SECRET}/resume?token={SECRET}");
        let error = smol::block_on(client.request::<Value>(Method::POST, &path, None::<&()>, None))
            .unwrap_err();
        server.join().unwrap();
        let Error::Transport(diagnostic) = &error else {
            panic!("{error:?}")
        };
        assert_eq!(diagnostic.operation, "resume");
        if partial_body {
            assert_eq!(diagnostic.failure, BODY_FAILURE);
        }
        assert!(error.to_string().contains(UNKNOWN_OUTCOME));
        let rendered = format!("{error:?}: {error}");
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("http://"));
    }

    #[test_case(ErrorKind::Timeout, TIMEOUT_FAILURE; "timeout")]
    #[test_case(ErrorKind::BadServerCertificate, TLS_FAILURE; "tls")]
    fn transport_metadata_is_allowlisted(kind: ErrorKind, expected: &str) {
        let path = format!("/instances/{SECRET}/policy?token={SECRET}");
        let error = Error::Transport(TransportDiagnostic {
            operation: transport_operation(&Method::PUT, &path),
            failure: transport_failure(&kind),
            guidance: UNKNOWN_OUTCOME,
        });
        assert!(error.to_string().contains(expected));
        assert!(error.to_string().contains("network policy update"));
        assert!(!format!("{error:?}: {error}").contains(SECRET));
    }

    #[test_case("http://localhost:8080")]
    #[test_case("http://192.0.2.1")]
    #[test_case("https://user:secret@example.test")]
    #[test_case("https://example.test/path")]
    #[test_case("https://example.test/?token=secret")]
    fn origins_refuse_unsafe_auth_destinations(origin: &str) {
        assert!(SandboxOrigin::parse(origin).is_err());
    }

    #[test]
    fn generated_lifecycle_keys_are_256_bit_and_debug_redacted() {
        let key = generate_api_key().unwrap();
        assert_eq!(key.expose_secret().len(), 64);
        assert!(key.expose_secret().bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!format!("{key:?}").contains(key.expose_secret()));
    }
}
