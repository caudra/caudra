use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    StateDir, StorageError, atomic_write_permissions, exclusive_state_lock,
    try_exclusive_state_lock,
};

const AUTH_DIR: &str = "auth";
const AUTH_FILE_MODE: u32 = 0o600;
const REFRESH_BUFFER_SECS: u64 = 60;
const ACCESS_FIELD: &str = "access";
const API_KEY_FIELD: &str = "api_key";
const EXPIRES_FIELD: &str = "expires";
const REFRESH_FIELD: &str = "refresh";
const WORKCELL_AUTH_DIR: &str = "workcell";
const WORKCELL_CREDENTIAL_VERSION: u32 = 1;
const WORKCELL_CREDENTIAL_FILE_MAX_BYTES: usize = MAX_WORKCELL_BEARER_TOKEN_BYTES + 1024;

pub const MAX_WORKCELL_CREDENTIAL_NAME_BYTES: usize = 64;
pub const MAX_WORKCELL_BEARER_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkcellCredentialNameError {
    #[error("Workcell credential name must not be empty")]
    Empty,
    #[error("Workcell credential name exceeds {MAX_WORKCELL_CREDENTIAL_NAME_BYTES} bytes")]
    TooLong,
    #[error("Workcell credential name must start with an ASCII letter or digit")]
    InvalidStart,
    #[error("Workcell credential name may contain only ASCII letters, digits, '.', '-', and '_'")]
    InvalidCharacter,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkcellCredentialName(String);

impl WorkcellCredentialName {
    pub fn new(name: impl Into<String>) -> Result<Self, WorkcellCredentialNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(WorkcellCredentialNameError::Empty);
        }
        if name.len() > MAX_WORKCELL_CREDENTIAL_NAME_BYTES {
            return Err(WorkcellCredentialNameError::TooLong);
        }
        if !name.as_bytes()[0].is_ascii_alphanumeric() {
            return Err(WorkcellCredentialNameError::InvalidStart);
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(WorkcellCredentialNameError::InvalidCharacter);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkcellCredentialName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkcellCredentialName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for WorkcellCredentialName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for WorkcellCredentialName {
    type Err = WorkcellCredentialNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl TryFrom<String> for WorkcellCredentialName {
    type Error = WorkcellCredentialNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<WorkcellCredentialName> for String {
    fn from(name: WorkcellCredentialName) -> Self {
        name.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkcellCredentialRefError {
    #[error("Workcell credential reference must use credential:NAME")]
    InvalidPrefix,
    #[error(transparent)]
    InvalidName(#[from] WorkcellCredentialNameError),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkcellCredentialRef(WorkcellCredentialName);

impl WorkcellCredentialRef {
    pub fn new(name: WorkcellCredentialName) -> Self {
        Self(name)
    }

    pub fn name(&self) -> &WorkcellCredentialName {
        &self.0
    }
}

impl fmt::Debug for WorkcellCredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkcellCredentialRef")
            .field(&format_args!("credential:{}", self.0))
            .finish()
    }
}

impl fmt::Display for WorkcellCredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "credential:{}", self.0)
    }
}

impl FromStr for WorkcellCredentialRef {
    type Err = WorkcellCredentialRefError;

    fn from_str(reference: &str) -> Result<Self, Self::Err> {
        let name = reference
            .strip_prefix("credential:")
            .ok_or(WorkcellCredentialRefError::InvalidPrefix)?;
        Ok(Self(WorkcellCredentialName::new(name)?))
    }
}

impl TryFrom<String> for WorkcellCredentialRef {
    type Error = WorkcellCredentialRefError;

    fn try_from(reference: String) -> Result<Self, Self::Error> {
        Self::from_str(&reference)
    }
}

impl From<WorkcellCredentialRef> for String {
    fn from(reference: WorkcellCredentialRef) -> Self {
        reference.to_string()
    }
}

#[derive(PartialEq, Eq)]
pub struct WorkcellCredential {
    bearer_token: String,
}

impl WorkcellCredential {
    pub fn new(bearer_token: String) -> Result<Self, StorageError> {
        if bearer_token.is_empty() {
            return Err(StorageError::InvalidWorkcellCredential(
                "bearer token must not be empty".into(),
            ));
        }
        if bearer_token.len() > MAX_WORKCELL_BEARER_TOKEN_BYTES {
            return Err(StorageError::InvalidWorkcellCredential(format!(
                "bearer token exceeds {MAX_WORKCELL_BEARER_TOKEN_BYTES} bytes"
            )));
        }
        if bearer_token.chars().any(char::is_whitespace) {
            return Err(StorageError::InvalidWorkcellCredential(
                "bearer token must not contain whitespace".into(),
            ));
        }
        Ok(Self { bearer_token })
    }

    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }
}

impl fmt::Debug for WorkcellCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkcellCredential")
            .field("bearer_token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkcellCredentialMetadata {
    pub name: WorkcellCredentialName,
    pub updated_at_millis: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkcellCredentialRecord {
    version: u32,
    bearer_token: String,
}

#[derive(Serialize)]
struct WorkcellCredentialRecordRef<'a> {
    version: u32,
    bearer_token: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuthTokens {
    pub fn is_expired(&self) -> bool {
        now_millis() + REFRESH_BUFFER_SECS * 1000 >= self.expires
    }

    pub fn is_hard_expired(&self) -> bool {
        now_millis() >= self.expires
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct McpAuthData {
    pub server_url: String,
    pub tokens: Option<OAuthTokens>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_secret_expires_at: Option<u64>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Token endpoint pinned at interactive auth. Silent refresh reuses it
    /// instead of trusting fresh discovery, so a later-compromised server
    /// cannot redirect the refresh token and client secret elsewhere.
    #[serde(default)]
    pub token_endpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuth {
    OAuth(OAuthTokens),
    ApiKey(ProviderCredentials),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthKind {
    OAuth,
    ApiKey,
}

impl ProviderAuth {
    pub fn kind(&self) -> ProviderAuthKind {
        match self {
            Self::OAuth(_) => ProviderAuthKind::OAuth,
            Self::ApiKey(_) => ProviderAuthKind::ApiKey,
        }
    }
}

impl ProviderCredentials {
    pub fn masked_api_key(&self) -> String {
        if self.api_key.len() > 8 {
            format!(
                "{}...{}",
                &self.api_key[..4],
                &self.api_key[self.api_key.len() - 4..]
            )
        } else {
            "****".to_string()
        }
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn auth_path(dir: &StateDir, filename: &str) -> PathBuf {
    dir.persistent_path()
        .join(AUTH_DIR)
        .join(format!("{filename}.json"))
}

fn workcell_auth_path(dir: &StateDir, name: &WorkcellCredentialName) -> PathBuf {
    dir.persistent_path()
        .join(AUTH_DIR)
        .join(WORKCELL_AUTH_DIR)
        .join(format!("{name}.json"))
}

fn workcell_auth_lock_path(dir: &StateDir, name: &WorkcellCredentialName) -> PathBuf {
    dir.persistent_path()
        .join(AUTH_DIR)
        .join(WORKCELL_AUTH_DIR)
        .join(format!("{name}.lock"))
}

fn load_auth<T: DeserializeOwned>(path: &Path) -> Option<T> {
    try_load_auth(path).ok().flatten()
}

fn try_load_auth<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StorageError> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(serde_json::from_str(&data)?))
}

fn try_load_provider_auth_path(path: &Path) -> Result<Option<ProviderAuth>, StorageError> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = serde_json::from_str(&data)?;
    let object = value
        .as_object()
        .ok_or_else(|| StorageError::InvalidProviderAuth("expected a JSON object".into()))?;
    let has_api_key = object.contains_key(API_KEY_FIELD);
    let has_oauth = [ACCESS_FIELD, REFRESH_FIELD, EXPIRES_FIELD]
        .iter()
        .any(|field| object.contains_key(*field));
    match (has_api_key, has_oauth) {
        (true, false) => serde_json::from_value(value)
            .map(ProviderAuth::ApiKey)
            .map(Some)
            .map_err(Into::into),
        (false, true) => serde_json::from_value(value)
            .map(ProviderAuth::OAuth)
            .map(Some)
            .map_err(Into::into),
        (true, true) => Err(StorageError::InvalidProviderAuth(
            "contains both OAuth and API-key fields".into(),
        )),
        (false, false) => Err(StorageError::InvalidProviderAuth(
            "missing OAuth or API-key fields".into(),
        )),
    }
}

fn save_auth(path: &Path, data: &impl Serialize) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(data)?;
    atomic_write_permissions(path, json.as_bytes(), AUTH_FILE_MODE)?;
    debug!(path = %path.display(), "auth data saved");
    Ok(())
}

fn delete_auth(path: &Path) -> Result<bool, StorageError> {
    if path.exists() {
        fs::remove_file(path)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn load_workcell_credential(
    dir: &StateDir,
    name: &WorkcellCredentialName,
) -> Result<Option<WorkcellCredential>, StorageError> {
    let path = workcell_auth_path(dir, name);
    let Some(file) = open_workcell_credential(&path)? else {
        return Ok(None);
    };
    let mut data = String::new();
    file.take(WORKCELL_CREDENTIAL_FILE_MAX_BYTES as u64 + 1)
        .read_to_string(&mut data)?;
    if data.len() > WORKCELL_CREDENTIAL_FILE_MAX_BYTES {
        return Err(StorageError::InvalidWorkcellCredential(
            "credential record exceeds the size limit".into(),
        ));
    }
    let record: WorkcellCredentialRecord = serde_json::from_str(&data)?;
    if record.version != WORKCELL_CREDENTIAL_VERSION {
        return Err(StorageError::InvalidWorkcellCredential(format!(
            "unsupported credential record version {}",
            record.version
        )));
    }
    WorkcellCredential::new(record.bearer_token).map(Some)
}

fn open_workcell_credential(path: &Path) -> Result<Option<File>, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StorageError::InvalidWorkcellCredential(
                "credential path must be a regular non-symlink file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(StorageError::InvalidWorkcellCredential(
            "credential path must be a regular file".into(),
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(StorageError::InvalidWorkcellCredential(
            "credential file must be owner-only and owned by the current user".into(),
        ));
    }
    Ok(Some(file))
}

pub fn save_workcell_credential(
    dir: &StateDir,
    name: &WorkcellCredentialName,
    credential: &WorkcellCredential,
) -> Result<(), StorageError> {
    let _lock = exclusive_state_lock(&workcell_auth_lock_path(dir, name), AUTH_FILE_MODE)?;
    let record = WorkcellCredentialRecordRef {
        version: WORKCELL_CREDENTIAL_VERSION,
        bearer_token: credential.bearer_token(),
    };
    save_auth(&workcell_auth_path(dir, name), &record)
}

pub fn list_workcell_credentials(
    dir: &StateDir,
) -> Result<Vec<WorkcellCredentialMetadata>, StorageError> {
    let directory = dir.persistent_path().join(AUTH_DIR).join(WORKCELL_AUTH_DIR);
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut credentials = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(name) = WorkcellCredentialName::new(stem) else {
            continue;
        };
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let updated_at_millis = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis() as u64);
        credentials.push(WorkcellCredentialMetadata {
            name,
            updated_at_millis,
        });
    }
    credentials.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(credentials)
}

pub fn delete_workcell_credential(
    dir: &StateDir,
    name: &WorkcellCredentialName,
) -> Result<bool, StorageError> {
    let _lock = exclusive_state_lock(&workcell_auth_lock_path(dir, name), AUTH_FILE_MODE)?;
    delete_auth(&workcell_auth_path(dir, name))
}

pub fn load_tokens(dir: &StateDir, provider: &str) -> Option<OAuthTokens> {
    try_load_tokens(dir, provider).ok().flatten()
}

pub fn try_load_tokens(
    dir: &StateDir,
    provider: &str,
) -> Result<Option<OAuthTokens>, StorageError> {
    Ok(match try_load_provider_auth(dir, provider)? {
        Some(ProviderAuth::OAuth(tokens)) => Some(tokens),
        Some(ProviderAuth::ApiKey(_)) | None => None,
    })
}

pub fn try_load_provider_auth(
    dir: &StateDir,
    provider: &str,
) -> Result<Option<ProviderAuth>, StorageError> {
    try_load_provider_auth_path(&auth_path(dir, provider))
}

pub fn save_tokens(
    dir: &StateDir,
    provider: &str,
    tokens: &OAuthTokens,
) -> Result<(), StorageError> {
    save_auth(&auth_path(dir, provider), tokens)
}

pub fn delete_tokens(dir: &StateDir, provider: &str) -> Result<bool, StorageError> {
    let path = auth_path(dir, provider);
    match try_load_provider_auth_path(&path)? {
        Some(ProviderAuth::OAuth(_)) => delete_auth(&path),
        Some(ProviderAuth::ApiKey(_)) | None => Ok(false),
    }
}

pub fn delete_provider_auth(dir: &StateDir, provider: &str) -> Result<bool, StorageError> {
    let _lock = lock_provider_auth(dir, provider)?;
    delete_auth(&auth_path(dir, provider))
}

pub fn lock_provider_auth(dir: &StateDir, provider: &str) -> Result<File, StorageError> {
    exclusive_state_lock(
        &dir.persistent_path()
            .join(AUTH_DIR)
            .join(format!("{provider}.lock")),
        AUTH_FILE_MODE,
    )
}

pub fn try_lock_provider_auth(
    dir: &StateDir,
    provider: &str,
) -> Result<Option<File>, StorageError> {
    try_exclusive_state_lock(
        &dir.persistent_path()
            .join(AUTH_DIR)
            .join(format!("{provider}.lock")),
        AUTH_FILE_MODE,
    )
}

pub fn load_mcp_auth(dir: &StateDir, server_name: &str, expected_url: &str) -> Option<McpAuthData> {
    let data: McpAuthData = load_auth(&auth_path(dir, &format!("mcp-{server_name}")))?;
    if data.server_url != expected_url {
        return None;
    }
    if let Some(expires_at) = data.client_secret_expires_at
        && now_millis() / 1000 >= expires_at
    {
        return None;
    }
    Some(data)
}

pub fn save_mcp_auth(
    dir: &StateDir,
    server_name: &str,
    data: &McpAuthData,
) -> Result<(), StorageError> {
    save_auth(&auth_path(dir, &format!("mcp-{server_name}")), data)
}

pub fn delete_mcp_auth(dir: &StateDir, server_name: &str) -> Result<bool, StorageError> {
    delete_auth(&auth_path(dir, &format!("mcp-{server_name}")))
}

pub fn load_provider_credentials(dir: &StateDir, slug: &str) -> Option<ProviderCredentials> {
    match try_load_provider_auth(dir, slug).ok().flatten()? {
        ProviderAuth::ApiKey(credentials) => Some(credentials),
        ProviderAuth::OAuth(_) => None,
    }
}

pub fn save_provider_credentials(
    dir: &StateDir,
    slug: &str,
    creds: &ProviderCredentials,
) -> Result<(), StorageError> {
    let _lock = lock_provider_auth(dir, slug)?;
    save_auth(&auth_path(dir, slug), creds)
}

pub fn delete_provider_credentials(dir: &StateDir, slug: &str) -> Result<bool, StorageError> {
    let path = auth_path(dir, slug);
    match try_load_provider_auth_path(&path)? {
        Some(ProviderAuth::ApiKey(_)) => delete_auth(&path),
        Some(ProviderAuth::OAuth(_)) | None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use test_case::test_case;

    const TEST_PROVIDER: &str = "anthropic";
    const TEST_URL: &str = "https://mcp.example.com";

    fn test_tokens() -> OAuthTokens {
        OAuthTokens {
            access: "access_tok".into(),
            refresh: "refresh_tok".into(),
            expires: 9_999_999_999,
            account_id: None,
        }
    }

    fn test_mcp_data() -> McpAuthData {
        McpAuthData {
            server_url: TEST_URL.into(),
            tokens: None,
            client_id: "client-123".into(),
            client_secret: None,
            client_secret_expires_at: None,
            redirect_uri: None,
            token_endpoint: None,
        }
    }

    #[test_case(0,                              true  ; "epoch_is_expired")]
    #[test_case(now_millis() + 3_600_000,       false ; "future_is_valid")]
    fn token_expiry(expires: u64, expected: bool) {
        let tokens = OAuthTokens {
            access: "a".into(),
            refresh: "r".into(),
            expires,
            account_id: None,
        };
        assert_eq!(tokens.is_expired(), expected);
    }

    #[test]
    fn save_load_delete_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let tokens = test_tokens();
        save_tokens(&dir, TEST_PROVIDER, &tokens).unwrap();

        let loaded = load_tokens(&dir, TEST_PROVIDER).unwrap();
        assert_eq!(loaded.access, "access_tok");
        assert_eq!(loaded.refresh, "refresh_tok");
        assert_eq!(loaded.expires, 9_999_999_999);

        #[cfg(unix)]
        {
            let metadata = fs::metadata(auth_path(&dir, TEST_PROVIDER)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, AUTH_FILE_MODE);
        }

        assert!(delete_tokens(&dir, TEST_PROVIDER).unwrap());
        assert!(load_tokens(&dir, TEST_PROVIDER).is_none());
        assert!(!delete_tokens(&dir, TEST_PROVIDER).unwrap());
    }

    #[test]
    fn provider_auth_classifies_existing_file_shapes() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let tokens = test_tokens();
        save_tokens(&dir, TEST_PROVIDER, &tokens).unwrap();
        assert_eq!(
            try_load_provider_auth(&dir, TEST_PROVIDER).unwrap(),
            Some(ProviderAuth::OAuth(tokens))
        );

        let credentials = ProviderCredentials {
            api_key: "api-key".into(),
            host: None,
        };
        save_provider_credentials(&dir, TEST_PROVIDER, &credentials).unwrap();
        assert_eq!(
            try_load_provider_auth(&dir, TEST_PROVIDER).unwrap(),
            Some(ProviderAuth::ApiKey(credentials.clone()))
        );
        assert!(try_load_tokens(&dir, TEST_PROVIDER).unwrap().is_none());
        assert_eq!(
            load_provider_credentials(&dir, TEST_PROVIDER),
            Some(credentials)
        );
    }

    #[test]
    fn provider_auth_rejects_ambiguous_files() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let path = auth_path(&dir, TEST_PROVIDER);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            r#"{"api_key":"key","access":"a","refresh":"r","expires":1}"#,
        )
        .unwrap();

        assert!(matches!(
            try_load_provider_auth(&dir, TEST_PROVIDER),
            Err(StorageError::InvalidProviderAuth(_))
        ));
    }

    #[test]
    fn typed_delete_preserves_other_auth_method() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let credentials = ProviderCredentials {
            api_key: "api-key".into(),
            host: None,
        };
        save_provider_credentials(&dir, TEST_PROVIDER, &credentials).unwrap();
        assert!(!delete_tokens(&dir, TEST_PROVIDER).unwrap());
        assert_eq!(
            load_provider_credentials(&dir, TEST_PROVIDER),
            Some(credentials)
        );

        let tokens = test_tokens();
        save_tokens(&dir, TEST_PROVIDER, &tokens).unwrap();
        assert!(!delete_provider_credentials(&dir, TEST_PROVIDER).unwrap());
        assert_eq!(load_tokens(&dir, TEST_PROVIDER), Some(tokens));
    }

    #[test]
    fn ephemeral_access_uses_the_persistent_root() {
        let tmp = TempDir::new().unwrap();
        let persistent = tmp.path().join("persistent");
        let volatile = tmp.path().join("volatile");
        let dir = StateDir::split(volatile.clone(), persistent.clone());
        let tokens = OAuthTokens {
            access: "access_tok".into(),
            refresh: "refresh_tok".into(),
            expires: 9_999_999_999,
            account_id: None,
        };

        save_tokens(&dir, "anthropic", &tokens).unwrap();

        assert_eq!(load_tokens(&dir, "anthropic").unwrap().access, "access_tok");
        assert!(persistent.join(AUTH_DIR).join("anthropic.json").is_file());
        assert!(!volatile.join(AUTH_DIR).exists());
    }

    #[test]
    fn workcell_credential_crud_is_purpose_specific_and_owner_only() {
        const NAME: &str = "production";
        const TOKEN: &str = "bearer-private-value";
        const SESSION_DATABASE: &str = "caudra.sqlite";

        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let name = WorkcellCredentialName::new(NAME).unwrap();
        let credential = WorkcellCredential::new(TOKEN.to_owned()).unwrap();

        save_workcell_credential(&dir, &name, &credential).unwrap();

        let loaded = load_workcell_credential(&dir, &name).unwrap().unwrap();
        assert_eq!(loaded.bearer_token(), TOKEN);
        let listed = list_workcell_credentials(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, name);
        let metadata_json = serde_json::to_string(&listed).unwrap();
        assert!(!metadata_json.contains(TOKEN));
        assert!(!dir.path().join(SESSION_DATABASE).exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(workcell_auth_path(&dir, &name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            AUTH_FILE_MODE
        );

        assert!(delete_workcell_credential(&dir, &name).unwrap());
        assert!(load_workcell_credential(&dir, &name).unwrap().is_none());
        assert!(!delete_workcell_credential(&dir, &name).unwrap());
    }

    #[test]
    fn workcell_credential_debug_and_metadata_redact_bearer() {
        const TOKEN: &str = "bearer-private-value";

        let credential = WorkcellCredential::new(TOKEN.to_owned()).unwrap();
        let debug = format!("{credential:?}");

        assert!(!debug.contains(TOKEN));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn workcell_credential_names_refs_and_tokens_are_bounded() {
        assert_eq!(
            WorkcellCredentialName::new("x".repeat(MAX_WORKCELL_CREDENTIAL_NAME_BYTES + 1)),
            Err(WorkcellCredentialNameError::TooLong)
        );
        assert!(matches!(
            WorkcellCredentialRef::from_str("env:WORKCELL_TOKEN"),
            Err(WorkcellCredentialRefError::InvalidPrefix)
        ));
        let reference = WorkcellCredentialRef::from_str("credential:production").unwrap();
        let serialized = serde_json::to_string(&reference).unwrap();
        assert_eq!(serialized, r#""credential:production""#);
        assert!(matches!(
            WorkcellCredential::new("x".repeat(MAX_WORKCELL_BEARER_TOKEN_BYTES + 1)),
            Err(StorageError::InvalidWorkcellCredential(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn workcell_credential_loader_rejects_symlinks_and_exposed_files() {
        use std::os::unix::fs::symlink;

        const NAME: &str = "production";
        const TOKEN: &str = "bearer-private-value";

        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let name = WorkcellCredentialName::new(NAME).unwrap();
        let credential = WorkcellCredential::new(TOKEN.to_owned()).unwrap();
        save_workcell_credential(&dir, &name, &credential).unwrap();
        let path = workcell_auth_path(&dir, &name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            load_workcell_credential(&dir, &name),
            Err(StorageError::InvalidWorkcellCredential(_))
        ));

        fs::remove_file(&path).unwrap();
        let target = tmp.path().join("credential-target.json");
        fs::write(
            &target,
            format!(r#"{{"version":1,"bearer_token":"{TOKEN}"}}"#),
        )
        .unwrap();
        symlink(target, path).unwrap();
        assert!(matches!(
            load_workcell_credential(&dir, &name),
            Err(StorageError::InvalidWorkcellCredential(_))
        ));
    }

    #[test]
    fn mcp_auth_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let data = McpAuthData {
            tokens: Some(OAuthTokens {
                access: "acc".into(),
                refresh: "ref".into(),
                expires: 9999999999,
                account_id: None,
            }),
            ..test_mcp_data()
        };
        save_mcp_auth(&dir, "srv", &data).unwrap();
        let loaded = load_mcp_auth(&dir, "srv", TEST_URL).unwrap();
        assert_eq!(loaded.client_id, "client-123");
        assert_eq!(loaded.tokens.unwrap().access, "acc");
    }

    #[test_case(
        test_mcp_data(),
        "https://other.example.com"
        ; "url_mismatch"
    )]
    #[test_case(
        McpAuthData {
            client_secret: Some("s".into()),
            client_secret_expires_at: Some(1),
            ..test_mcp_data()
        },
        TEST_URL
        ; "expired_client_secret"
    )]
    fn mcp_auth_load_returns_none(data: McpAuthData, lookup_url: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        save_mcp_auth(&dir, "srv", &data).unwrap();
        assert!(load_mcp_auth(&dir, "srv", lookup_url).is_none());
    }
}
