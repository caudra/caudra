use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
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
