use std::collections::{HashMap, HashSet};
#[cfg(not(test))]
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{LazyLock, Once};
use std::thread;
use std::time::Duration;

use caudra_config::atomic_file::{self, AtomicFileError};
use caudra_config::models::{ModelsConfig, ModelsConfigError};
use caudra_config::providers::all_builtins;
use caudra_models::sign::{Signature, SignatureError, verify};
use caudra_models::{
    BLOCKED_CATALOG_PROVIDERS, MAX_BYTES, ParseError, Snapshot, TRUSTED_KEYS, parse_bounded,
    validate,
};
use caudra_storage::{StorageError, paths, try_exclusive_state_lock};
use flate2::read::GzDecoder;
use isahc::config::{Configurable, RedirectPolicy};
use isahc::http::{self, Request, StatusCode};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;
use url::Url;

use super::{bundled, current, current::install_newer};
use crate::providers::{catalog::OPENCODE_FAMILY_SLUGS, user_agent};

const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(30);
const SMALL_FILE_LIMIT: usize = 16 * 1024;
const CACHE_PATH: &str = "models/v1";
const MODELS_FILE: &str = "models.json";
const SIGNATURE_FILE: &str = "models.json.sig";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "refresh.lock";
#[cfg(not(test))]
const LEGACY_FILE: &str = "models-dev-catalog.json";
const LOCK_MODE: u32 = 0o600;
const EXTRA_AUTH_ENV: &[&str] = &[
    "COPILOT_GITHUB_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
];
static REFRESH_STARTED: Once = Once::new();
static ROUTING: LazyLock<ProtectedRouting> = LazyLock::new(ProtectedRouting::bundled);

#[derive(Debug, Error)]
pub enum RefreshError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Atomic(#[from] AtomicFileError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Signature(#[from] SignatureError),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Config(#[from] ModelsConfigError),
    #[error("model update request failed: {0}")]
    Network(#[from] isahc::Error),
    #[error("invalid model update request: {0}")]
    Request(#[from] http::Error),
    #[error("model update server returned HTTP {0}; keeping the current models")]
    Status(StatusCode),
    #[error("model update exceeds the {0}-byte limit")]
    TooLarge(usize),
    #[error("unsupported model update content encoding {0:?}")]
    Encoding(String),
    #[error("model update validation failed: {0}")]
    Validation(String),
    #[error("model catalog provider {0:?} conflicts with built-in routing or credentials")]
    BuiltinCollision(String),
}

#[derive(Debug)]
pub struct RefreshOutcome {
    pub old_updated_at: Timestamp,
    pub new_updated_at: Timestamp,
    pub swapped: bool,
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    etag: Option<String>,
    last_modified: Option<String>,
    checked_at: Option<Timestamp>,
}

impl State {
    fn stale(&self, now: Timestamp) -> bool {
        self.checked_at.is_none_or(|checked| {
            now.as_second().saturating_sub(checked.as_second()) > REFRESH_INTERVAL.as_secs() as i64
        })
    }
}

struct Cache<'a> {
    directory: PathBuf,
    trusted: &'a [[u8; 32]],
}

impl Cache<'static> {
    fn production() -> Result<Self, RefreshError> {
        Ok(Self {
            directory: paths::cache_dir()?.join(CACHE_PATH),
            trusted: TRUSTED_KEYS,
        })
    }
}

impl Cache<'_> {
    fn load(&self) -> Result<Option<Snapshot>, RefreshError> {
        let bytes = match File::open(self.directory.join(MODELS_FILE)) {
            Ok(file) => read_bounded(file, MAX_BYTES)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let signature = read_bounded(
            File::open(self.directory.join(SIGNATURE_FILE))?,
            SMALL_FILE_LIMIT,
        )?;
        verified_snapshot(&bytes, &signature, self.trusted).map(Some)
    }

    fn load_or_warn(&self) -> Option<Snapshot> {
        match self.load() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(path = %self.directory.display(), %error, "ignoring invalid model cache");
                None
            }
        }
    }

    fn state(&self) -> State {
        let result = File::open(self.directory.join(STATE_FILE))
            .map_err(RefreshError::from)
            .and_then(|file| read_bounded(file, SMALL_FILE_LIMIT))
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(RefreshError::from));
        match result {
            Ok(state) => state,
            Err(RefreshError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                State::default()
            }
            Err(error) => {
                warn!(path = %self.directory.display(), %error, "ignoring invalid model cache state");
                State::default()
            }
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), RefreshError> {
        let path = self.directory.join(name);
        let previous = match File::open(&path) {
            Ok(file) => Some(read_bounded(file, MAX_BYTES)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        atomic_file::commit(&path, &atomic_file::revision_of(previous.as_deref()), bytes)?;
        Ok(())
    }

    fn refresh(
        &self,
        config: &ModelsConfig,
        force: bool,
        now: Timestamp,
    ) -> Result<RefreshOutcome, RefreshError> {
        let (old_revision, old_updated_at) = {
            let old = current();
            (old.revision, old.snapshot_updated_at)
        };
        let outcome = || {
            let new = current();
            RefreshOutcome {
                old_updated_at,
                new_updated_at: new.snapshot_updated_at,
                swapped: new.revision != old_revision,
            }
        };
        if !force && !config.refresh {
            return Ok(outcome());
        }
        config.validate_url()?;
        let Some(_lease) = try_exclusive_state_lock(&self.directory.join(LOCK_FILE), LOCK_MODE)?
        else {
            return Ok(outcome());
        };
        let mut state = self.state();
        if let Some(snapshot) = self.load_or_warn() {
            install_newer(snapshot);
        } else {
            state.etag = None;
            state.last_modified = None;
        }
        if !force && !state.stale(now) {
            return Ok(outcome());
        }
        state.checked_at = Some(now);
        self.write(STATE_FILE, &serde_json::to_vec(&state)?)?;
        let result = self.fetch(config, &mut state);
        if let Err(error) = &result {
            warn!(%error, checked_at = %now, "model refresh failed; keeping the current models");
        }
        result?;
        self.write(STATE_FILE, &serde_json::to_vec(&state)?)?;
        Ok(outcome())
    }

    fn fetch(&self, config: &ModelsConfig, state: &mut State) -> Result<(), RefreshError> {
        let response = get(&config.url, Some(state))?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(());
        }
        if response.status() != StatusCode::OK {
            return Err(RefreshError::Status(response.status()));
        }
        let etag = header(&response, "etag");
        let last_modified = header(&response, "last-modified");
        let bytes = response_bytes(response, MAX_BYTES)?;
        let signature_response = get(&format!("{}.sig", config.url), None)?;
        if signature_response.status() != StatusCode::OK {
            return Err(RefreshError::Status(signature_response.status()));
        }
        let signature = response_bytes(signature_response, SMALL_FILE_LIMIT)?;
        let snapshot = verified_snapshot(&bytes, &signature, self.trusted)?;
        let incumbent = current();
        if snapshot.updated_at > incumbent.snapshot_updated_at
            && snapshot.content_sha256 != incumbent.content_sha256
        {
            self.write(MODELS_FILE, &bytes)?;
            self.write(SIGNATURE_FILE, &signature)?;
            install_newer(snapshot);
        }
        state.etag = etag;
        state.last_modified = last_modified;
        Ok(())
    }
}

#[cfg(not(test))]
pub(super) fn startup_snapshot(bundled: Snapshot) -> Snapshot {
    let cache = match Cache::production() {
        Ok(cache) => cache,
        Err(error) => {
            warn!(%error, "model cache unavailable; using bundled models");
            return bundled;
        }
    };
    if let Ok(root) = paths::cache_dir() {
        match fs::remove_file(root.join(LEGACY_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!(%error, "could not remove legacy model cache"),
        }
    }
    select(bundled, cache.load_or_warn())
}

fn select(bundled: Snapshot, cached: Option<Snapshot>) -> Snapshot {
    cached
        .filter(|cached| cached.updated_at > bundled.updated_at)
        .unwrap_or(bundled)
}

pub fn refresh(config: &ModelsConfig, force: bool) -> Result<RefreshOutcome, RefreshError> {
    Cache::production()?.refresh(config, force, Timestamp::now())
}

pub fn start_refresh(config: &ModelsConfig) {
    if !config.refresh {
        return;
    }
    REFRESH_STARTED.call_once(|| {
        let config = config.clone();
        if let Err(error) = thread::Builder::new()
            .name("model-refresh".to_owned())
            .spawn(move || {
                loop {
                    if let Err(error) = refresh(&config, false) {
                        warn!(%error, "background model refresh failed");
                    }
                    thread::sleep(REFRESH_INTERVAL + Duration::from_secs(1));
                }
            })
        {
            warn!(%error, "could not start model refresh");
        }
    });
}

fn get(url: &str, state: Option<&State>) -> Result<http::Response<isahc::Body>, RefreshError> {
    let parsed = Url::parse(url).map_err(|_| ModelsConfigError::Url)?;
    let mut request = caudra_config::proxy_route(&parsed)
        .apply(Request::get(url))
        .timeout(TIMEOUT)
        .redirect_policy(RedirectPolicy::None)
        .automatic_decompression(false)
        .header("user-agent", user_agent())
        .header("accept-encoding", "gzip");
    if let Some(state) = state {
        if let Some(etag) = &state.etag {
            request = request.header("if-none-match", etag);
        }
        if let Some(modified) = &state.last_modified {
            request = request.header("if-modified-since", modified);
        }
    }
    Ok(isahc::send(request.body(())?)?)
}

fn header(response: &http::Response<isahc::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn response_bytes(
    response: http::Response<isahc::Body>,
    limit: usize,
) -> Result<Vec<u8>, RefreshError> {
    if header(&response, "content-length")
        .and_then(|length| length.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
    {
        return Err(RefreshError::TooLarge(limit));
    }
    let encoding = header(&response, "content-encoding");
    let bytes = read_bounded(response.into_body(), limit)?;
    match encoding.as_deref() {
        None | Some("identity") => Ok(bytes),
        Some("gzip") => read_bounded(GzDecoder::new(bytes.as_slice()), limit),
        Some(other) => Err(RefreshError::Encoding(other.to_owned())),
    }
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, RefreshError> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(RefreshError::TooLarge(limit));
    }
    Ok(bytes)
}

fn verified_snapshot(
    bytes: &[u8],
    signature: &[u8],
    trusted: &[[u8; 32]],
) -> Result<Snapshot, RefreshError> {
    if bytes.len() > MAX_BYTES {
        return Err(RefreshError::TooLarge(MAX_BYTES));
    }
    let signature: Signature = serde_json::from_slice(signature)?;
    verify(bytes, &signature, trusted)?;
    let snapshot = parse_bounded(bytes)?;
    validate(&snapshot).map_err(|issues| {
        RefreshError::Validation(
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    ROUTING.validate(&snapshot)?;
    Ok(snapshot)
}

struct ProtectedRouting {
    catalog: HashMap<String, (Option<String>, Vec<String>)>,
    slugs: HashSet<String>,
    env: HashSet<String>,
    hosts: HashSet<String>,
}

impl ProtectedRouting {
    fn bundled() -> Self {
        let catalog = bundled::snapshot();
        let mut slugs: HashSet<String> = catalog
            .payload
            .providers
            .iter()
            .flat_map(|provider| [Some(provider.slug.clone()), provider.catalog_slug.clone()])
            .flatten()
            .collect();
        slugs.extend(OPENCODE_FAMILY_SLUGS.iter().map(|slug| (*slug).to_owned()));
        slugs.extend(
            BLOCKED_CATALOG_PROVIDERS
                .iter()
                .map(|slug| (*slug).to_owned()),
        );
        let mut env: HashSet<String> = EXTRA_AUTH_ENV.iter().map(|key| (*key).to_owned()).collect();
        let mut hosts = HashSet::new();
        for provider in all_builtins() {
            slugs.insert(provider.slug.to_owned());
            if !provider.default_api_key_env.is_empty() {
                env.insert(provider.default_api_key_env.to_owned());
            }
            if let Some(host) = url_host(provider.default_base_url) {
                hosts.insert(host);
            }
            for (_, plan) in provider.plans.unwrap_or_default() {
                if let Some(host) = url_host(plan.base_url) {
                    hosts.insert(host);
                }
            }
        }
        for slug in &slugs {
            if let Some(provider) = catalog.payload.catalog.get(slug) {
                env.extend(provider.env.iter().cloned());
                if let Some(host) = provider.api.as_deref().and_then(url_host) {
                    hosts.insert(host);
                }
            }
        }
        for (slug, provider) in &catalog.payload.catalog {
            if provider.env.iter().any(|key| env.contains(key))
                || provider
                    .api
                    .as_deref()
                    .and_then(url_host)
                    .is_some_and(|host| hosts.contains(&host))
            {
                slugs.insert(slug.clone());
            }
        }
        let catalog = catalog
            .payload
            .catalog
            .into_iter()
            .filter(|(slug, _)| slugs.contains(slug))
            .map(|(slug, provider)| (slug, (provider.api, provider.env)))
            .collect();
        Self {
            catalog,
            slugs,
            env,
            hosts,
        }
    }

    fn validate(&self, snapshot: &Snapshot) -> Result<(), RefreshError> {
        for (slug, provider) in &snapshot.payload.catalog {
            let collision = if self.slugs.contains(slug) {
                self.catalog
                    .get(slug)
                    .is_none_or(|(api, env)| &provider.api != api || &provider.env != env)
            } else {
                provider.env.iter().any(|key| self.env.contains(key))
                    || provider
                        .api
                        .as_deref()
                        .and_then(url_host)
                        .is_some_and(|host| self.hosts.contains(&host))
            };
            if collision {
                return Err(RefreshError::BuiltinCollision(slug.clone()));
            }
        }
        Ok(())
    }
}

fn url_host(url: &str) -> Option<String> {
    Url::parse(url).ok()?.host_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use caudra_models::sign::sign;
    use caudra_models::{Payload, Snapshot, to_pretty_bytes};
    use flate2::{Compression, write::GzEncoder};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Cache, LOCK_FILE, LOCK_MODE, MAX_BYTES, MODELS_FILE, ModelsConfig, REFRESH_INTERVAL,
        RefreshError, SIGNATURE_FILE, STATE_FILE, State, Timestamp, bundled, current, select,
        try_exclusive_state_lock, verified_snapshot,
    };
    use crate::Model;
    use crate::model_data::install;

    const NOW: &str = "2026-10-29T12:00:00Z";
    const NEWER: &str = "2026-10-30T12:00:00Z";
    const OLDER: &str = "2026-10-28T12:00:00Z";
    const EXPIRED: &str = "2020-01-01T00:00:00Z";
    const TEST_SIGNING_KEY_PKCS8_B64: &str = "MFMCAQEwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9goSMDIQDXWpgBgrEKt9VL/tPJZAc6DuFy89qmIyWvAhpo9wdRGg==";
    const TEST_PUBLIC_KEY_B64: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    const SPEC: &str = "refresh-test/model";
    const ETAG: &str = "\"test-etag\"";
    const MODIFIED: &str = "Thu, 29 Oct 2026 12:00:00 GMT";
    const NOTE: &str = "updated";
    const FUTURE: &str = "future-requirement";
    const BASE: &str = r#"{"providers":[{"slug":"refresh-test","display_name":"Refresh test","family":"generic","fallback_context_window":32000,"models":[{"prefixes":["model"],"family":"generic","context_window":32000,"pricing":{"input":1,"output":1,"cache_write":0,"cache_read":0}}]}]}"#;
    const CATALOG: &str = r#"{"test":{"name":"Test","env":["REFRESH_TEST_KEY"],"api":"https://refresh.example/v1","npm":"@ai-sdk/openai-compatible","models":{}}}"#;
    const SERVER_TIMEOUT: Duration = Duration::from_secs(5);
    const NEW_CONTEXT: u32 = 64000;

    struct Reply {
        status: u16,
        body: Vec<u8>,
        headers: String,
    }

    impl Reply {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                body,
                headers: String::new(),
            }
        }
        fn status(status: u16) -> Self {
            Self {
                status,
                body: Vec::new(),
                headers: String::new(),
            }
        }
    }

    struct Server {
        address: SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        stopped: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        fn new(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let log = Arc::clone(&requests);
            let stop = Arc::clone(&stopped);
            let thread = thread::spawn(move || {
                let mut replies: VecDeque<_> = replies.into();
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    stream.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
                    stream.set_write_timeout(Some(SERVER_TIMEOUT)).unwrap();
                    let mut request = String::new();
                    let mut reader = BufReader::new(&stream);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        request.push_str(&line);
                    }
                    log.lock().unwrap().push(request);
                    let reply = replies.pop_front().unwrap_or_else(|| Reply::status(500));
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {} Response\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                        reply.status,
                        reply.body.len(),
                        reply.headers
                    );
                    let _ = stream.write_all(&reply.body);
                }
            });
            Self {
                address,
                requests,
                stopped,
                thread: Some(thread),
            }
        }

        fn config(&self) -> ModelsConfig {
            ModelsConfig {
                url: format!("http://{}/models.json", self.address),
                refresh: true,
            }
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn key() -> [u8; 32] {
        STANDARD
            .decode(TEST_PUBLIC_KEY_B64)
            .unwrap()
            .try_into()
            .unwrap()
    }
    fn signed(bytes: &[u8]) -> Vec<u8> {
        serde_json::to_vec(
            &sign(bytes, &STANDARD.decode(TEST_SIGNING_KEY_PKCS8_B64).unwrap()).unwrap(),
        )
        .unwrap()
    }
    fn snapshot(at: &str, changed: bool) -> Snapshot {
        let mut payload: Payload = serde_json::from_str(BASE).unwrap();
        if changed {
            payload.providers[0].note = Some(NOTE.to_owned());
        }
        Snapshot::stamp(payload, None, at.parse().unwrap())
    }
    fn cache<'a>(dir: &TempDir, trusted: &'a [[u8; 32]]) -> Cache<'a> {
        Cache {
            directory: dir.path().to_owned(),
            trusted,
        }
    }
    fn publish(cache: &Cache<'_>, snapshot: &Snapshot) {
        let bytes = to_pretty_bytes(snapshot);
        cache.write(MODELS_FILE, &bytes).unwrap();
        cache.write(SIGNATURE_FILE, &signed(&bytes)).unwrap();
    }

    #[test_case(NEWER, true; "newer")]
    #[test_case(OLDER, false; "older")]
    #[test_case(NOW, false; "equal")]
    fn signed_refresh_selects_newest_wholesale(at: &str, swapped: bool) {
        install(snapshot(NOW, false));
        let previous = current();
        let candidate = snapshot(at, true);
        let bytes = to_pretty_bytes(&candidate);
        let server = Server::new(vec![Reply::ok(bytes.clone()), Reply::ok(signed(&bytes))]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        let outcome = cache
            .refresh(&server.config(), true, NOW.parse().unwrap())
            .unwrap();
        assert_eq!(outcome.swapped, swapped);
        assert_eq!(server.count(), 2);
        assert_eq!(current().revision, previous.revision + u64::from(swapped));
        assert_eq!(
            current().content_sha256,
            if swapped {
                candidate.content_sha256
            } else {
                previous.content_sha256.clone()
            }
        );
        if swapped {
            assert_eq!(std::fs::read(dir.path().join(MODELS_FILE)).unwrap(), bytes);
        }
    }

    #[test_case("signature")]
    #[test_case("untrusted")]
    #[test_case("oversized")]
    #[test_case("gzip_bomb")]
    #[test_case("format")]
    #[test_case("validation")]
    #[test_case("url")]
    #[test_case("env")]
    #[test_case("builtin_env")]
    #[test_case("builtin_api")]
    #[test_case("builtin_override")]
    fn rejected_update_keeps_incumbent(case: &str) {
        install(snapshot(NOW, false));
        let previous = current();
        let mut candidate = snapshot(NEWER, true);
        candidate.payload.catalog = serde_json::from_str(CATALOG).unwrap();
        match case {
            "format" => candidate.format += 1,
            "validation" => candidate.payload.providers[0].models[0].pricing.input = -1.0,
            "url" => {
                candidate.payload.catalog.get_mut("test").unwrap().api =
                    Some("http://public.example/v1".to_owned())
            }
            "env" => {
                candidate.payload.catalog.get_mut("test").unwrap().env = vec!["bad-name".to_owned()]
            }
            "builtin_env" => {
                candidate.payload.catalog.get_mut("test").unwrap().env =
                    vec!["OPENAI_API_KEY".to_owned()]
            }
            "builtin_api" => {
                candidate.payload.catalog.get_mut("test").unwrap().api =
                    Some("https://api.openai.com/v1".to_owned())
            }
            "builtin_override" => {
                let row = candidate.payload.catalog.remove("test").unwrap();
                candidate.payload.catalog.insert("openai".to_owned(), row);
            }
            _ => {}
        }
        let mut bytes = to_pretty_bytes(&candidate);
        let mut signature = signed(&bytes);
        if case == "signature" {
            bytes.push(b' ');
        }
        if case == "untrusted" {
            let mut value: serde_json::Value = serde_json::from_slice(&signature).unwrap();
            value["key_id"] = serde_json::json!("0000000000000000");
            signature = serde_json::to_vec(&value).unwrap();
        }
        let mut reply = Reply::ok(bytes);
        if case == "oversized" || case == "gzip_bomb" {
            reply.body = vec![b' '; MAX_BYTES + 1];
            if case == "gzip_bomb" {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
                encoder.write_all(&reply.body).unwrap();
                reply.body = encoder.finish().unwrap();
                reply.headers = "Content-Encoding: gzip\r\n".to_owned();
            }
        }
        let server = Server::new(vec![reply, Reply::ok(signature)]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        assert!(
            cache
                .refresh(&server.config(), true, NOW.parse().unwrap())
                .is_err()
        );
        assert!(Arc::ptr_eq(&previous, &current()));
        assert_eq!(cache.state().checked_at, Some(NOW.parse().unwrap()));
        assert!(!dir.path().join(MODELS_FILE).exists());
    }

    #[test_case(304; "not_modified")]
    #[test_case(429; "rate_limited")]
    #[test_case(503; "unavailable")]
    fn status_records_check_without_changing_models(status: u16) {
        install(snapshot(NOW, false));
        let previous = current();
        let server = Server::new(vec![Reply::status(status)]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        publish(&cache, &snapshot(NOW, false));
        let state = State {
            etag: Some(ETAG.to_owned()),
            last_modified: Some(MODIFIED.to_owned()),
            checked_at: Some(EXPIRED.parse().unwrap()),
        };
        cache
            .write(STATE_FILE, &serde_json::to_vec(&state).unwrap())
            .unwrap();
        let result = cache.refresh(&server.config(), false, NOW.parse().unwrap());
        assert_eq!(result.is_ok(), status == 304);
        assert!(Arc::ptr_eq(&previous, &current()));
        let state = cache.state();
        assert_eq!(state.checked_at, Some(NOW.parse().unwrap()));
        assert_eq!(state.etag.as_deref(), Some(ETAG));
        assert_eq!(state.last_modified.as_deref(), Some(MODIFIED));
        cache
            .refresh(&server.config(), false, NOW.parse().unwrap())
            .unwrap();
        assert_eq!(server.count(), 1);
        let request = server.requests.lock().unwrap()[0].to_lowercase();
        assert!(request.contains(&format!("if-none-match: {ETAG}")));
        assert!(request.contains(&format!("if-modified-since: {}", MODIFIED.to_lowercase())));
        assert!(request.contains("accept-encoding: gzip"));
        assert!(request.contains(&format!(
            "user-agent: {}",
            crate::providers::user_agent().to_lowercase()
        )));
    }

    #[test_case(false, false; "pair_missing")]
    #[test_case(true, false; "signature_missing")]
    #[test_case(true, true; "corrupt")]
    fn invalid_cache_is_restored_without_validators(model_present: bool, signature_present: bool) {
        let previous = snapshot(NOW, false);
        install(previous.clone());
        let candidate = snapshot(NEWER, true);
        let bytes = to_pretty_bytes(&candidate);
        let server = Server::new(vec![Reply::ok(bytes.clone()), Reply::ok(signed(&bytes))]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        if model_present {
            cache.write(MODELS_FILE, &bytes).unwrap();
        }
        if signature_present {
            cache
                .write(SIGNATURE_FILE, &signed(&to_pretty_bytes(&previous)))
                .unwrap();
        }
        cache
            .write(
                STATE_FILE,
                &serde_json::to_vec(&State {
                    etag: Some(ETAG.to_owned()),
                    last_modified: Some(MODIFIED.to_owned()),
                    checked_at: Some(EXPIRED.parse().unwrap()),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(
            cache
                .refresh(&server.config(), false, NOW.parse().unwrap())
                .unwrap()
                .swapped
        );
        assert_eq!(server.count(), 2);
        let request = server.requests.lock().unwrap()[0].to_lowercase();
        assert!(!request.contains("if-none-match:"));
        assert!(!request.contains("if-modified-since:"));
        assert_eq!(cache.load().unwrap(), Some(candidate));
    }

    #[test]
    fn opt_out_sends_no_request_but_force_fetches() {
        let server = Server::new(vec![Reply::status(304)]);
        let mut config = server.config();
        config.refresh = false;
        let dir = TempDir::new().unwrap();
        let cache = cache(&dir, &[]);
        cache.refresh(&config, false, NOW.parse().unwrap()).unwrap();
        assert_eq!(server.count(), 0);
        cache.refresh(&config, true, NOW.parse().unwrap()).unwrap();
        assert_eq!(server.count(), 1);
    }

    #[test_case(false; "expired_cache")]
    #[test_case(true; "corrupt_cache")]
    fn startup_uses_verified_cache_even_when_expired(corrupt: bool) {
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        let cached = snapshot(NEWER, true);
        publish(&cache, &cached);
        cache
            .write(
                STATE_FILE,
                &serde_json::to_vec(&State {
                    checked_at: Some(EXPIRED.parse().unwrap()),
                    ..Default::default()
                })
                .unwrap(),
            )
            .unwrap();
        if corrupt {
            cache.write(SIGNATURE_FILE, b"{}").unwrap();
        }
        let bundled = snapshot(NOW, false);
        let expected = if corrupt { &bundled } else { &cached };
        assert_eq!(select(bundled.clone(), cache.load_or_warn()), *expected);
    }

    #[test_case(NEWER, true; "newer")]
    #[test_case(NOW, false; "tie")]
    #[test_case(OLDER, false; "older")]
    fn startup_prefers_bundle_on_tie(at: &str, cached_wins: bool) {
        let bundled = snapshot(NOW, false);
        let cached = snapshot(at, true);
        let expected = if cached_wins {
            cached.clone()
        } else {
            bundled.clone()
        };
        assert_eq!(select(bundled, Some(cached)), expected);
    }

    #[test]
    fn bundled_pin_passes_runtime_routing_validation() {
        let bytes = to_pretty_bytes(&bundled::snapshot());
        verified_snapshot(&bytes, &signed(&bytes), &[key()]).unwrap();
    }

    #[test]
    fn unsupported_entry_is_installed_but_cannot_resolve() {
        install(snapshot(NOW, false));
        let mut candidate = snapshot(NEWER, true);
        candidate.payload.providers[0].models[0]
            .requires
            .push(FUTURE.to_owned());
        let bytes = to_pretty_bytes(&candidate);
        let server = Server::new(vec![Reply::ok(bytes.clone()), Reply::ok(signed(&bytes))]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        assert!(
            cache(&dir, &keys)
                .refresh(&server.config(), true, NOW.parse().unwrap())
                .unwrap()
                .swapped
        );
        assert!(matches!(
            Model::from_spec(SPEC),
            Err(crate::ModelError::NeedsNewerCaudra { .. })
        ));
    }

    #[test]
    fn concurrent_refreshes_leave_a_verified_pair() {
        install(snapshot(NOW, false));
        let candidate = snapshot(NEWER, true);
        let bytes = to_pretty_bytes(&candidate);
        let server = Server::new(vec![Reply::ok(bytes.clone()), Reply::ok(signed(&bytes))]);
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        let config = server.config();
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    cache.refresh(&config, false, NOW.parse().unwrap()).unwrap();
                });
            }
        });
        assert_eq!(cache.load().unwrap(), Some(candidate));
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn held_writer_lock_does_not_block_cache_reads_or_refresh() {
        let dir = TempDir::new().unwrap();
        let keys = [key()];
        let cache = cache(&dir, &keys);
        let candidate = snapshot(NEWER, true);
        publish(&cache, &candidate);
        let _lease = try_exclusive_state_lock(&dir.path().join(LOCK_FILE), LOCK_MODE)
            .unwrap()
            .unwrap();
        let server = Server::new(vec![]);
        cache
            .refresh(&server.config(), true, NOW.parse().unwrap())
            .unwrap();
        assert_eq!(server.count(), 0);
        assert_eq!(cache.load().unwrap(), Some(candidate));
    }

    #[test]
    fn network_failure_records_check() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/models.json", listener.local_addr().unwrap());
        drop(listener);
        let dir = TempDir::new().unwrap();
        let cache = cache(&dir, &[]);
        assert!(matches!(
            cache.refresh(
                &ModelsConfig { url, refresh: true },
                false,
                NOW.parse().unwrap()
            ),
            Err(RefreshError::Network(_))
        ));
        assert_eq!(cache.state().checked_at, Some(NOW.parse().unwrap()));
    }

    #[test]
    fn resolved_model_stays_fixed_until_resolved_again() {
        install(snapshot(NOW, false));
        let model = Model::from_spec(SPEC).unwrap();
        let old_context = model.context_window;
        let mut candidate = snapshot(NEWER, true);
        candidate.payload.providers[0].models[0].context_window = NEW_CONTEXT;
        candidate.payload.providers[0].models[0].vision = true;
        candidate.payload.providers[0].models[0].pricing.input = 2.0;
        let old_price = model.pricing.input;
        assert!(!model.supports_vision());
        install(candidate);
        assert_eq!(model.context_window, old_context);
        assert_eq!(model.pricing.input, old_price);
        assert!(!model.supports_vision());
        let next = model
            .refreshed(&caudra_config::ModelPolicy::new(&[], &[]).unwrap())
            .unwrap();
        assert_eq!(next.context_window, NEW_CONTEXT);
        assert!(next.supports_vision());
        assert_eq!(next.pricing.input, 2.0);
        assert!(next.model_revision > model.model_revision);
    }

    #[test_case(false; "removed")]
    #[test_case(true; "unsupported")]
    fn removed_or_unsupported_models_preserve_the_old_resolution(unsupported: bool) {
        install(snapshot(NOW, false));
        let model = Model::from_spec(SPEC).unwrap();
        let mut candidate = snapshot(NEWER, true);
        if unsupported {
            candidate.payload.providers[0].models[0]
                .requires
                .push(FUTURE.to_owned());
        } else {
            candidate.payload.providers[0].models.clear();
        }
        install(candidate);
        assert!(
            model
                .refreshed(&caudra_config::ModelPolicy::new(&[], &[]).unwrap())
                .is_err()
        );
        assert_eq!(model.spec(), SPEC);
    }

    #[test_case(0, false; "fresh")]
    #[test_case(86400, false; "exact_day")]
    #[test_case(86401, true; "expired")]
    fn daily_check_uses_elapsed_time(seconds: i64, stale: bool) {
        let now: Timestamp = NOW.parse().unwrap();
        let state = State {
            checked_at: Some(now),
            ..Default::default()
        };
        let later = Timestamp::from_second(now.as_second() + seconds).unwrap();
        assert_eq!(state.stale(later), stale);
        assert_eq!(REFRESH_INTERVAL, Duration::from_secs(86400));
    }
}
