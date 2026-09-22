use crate::StateDir;
use crate::auth::{MAX_WORKCELL_CREDENTIAL_NAME_BYTES, WorkcellCredentialName};
use crate::private_file::{PrivateFile, PrivateFileError};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const CREDENTIAL_PREFIX: &str = "sandbox-api:";
const CREDENTIAL_VERSION: u32 = 1;
const CREDENTIAL_RECORD_OVERHEAD: usize = 1024;
pub const MAX_SANDBOX_API_KEY_BYTES: usize = 16 * 1024;
pub const MAX_SANDBOX_CREDENTIAL_NAME_BYTES: usize = MAX_WORKCELL_CREDENTIAL_NAME_BYTES;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxCredentialError {
    #[error("sandbox API credential reference must use sandbox-api:NAME with a valid bounded name")]
    Reference,
    #[error("sandbox API key must be nonempty bounded visible ASCII without whitespace")]
    Key,
    #[error("invalid sandbox API credential record")]
    Record,
    #[error("unsupported sandbox API credential record version")]
    Version,
    #[error(transparent)]
    File(#[from] PrivateFileError),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SandboxCredentialRef(WorkcellCredentialName);

impl SandboxCredentialRef {
    pub fn new(name: impl Into<String>) -> Result<Self, SandboxCredentialError> {
        WorkcellCredentialName::new(name)
            .map(Self)
            .map_err(|_| SandboxCredentialError::Reference)
    }

    pub fn name(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SandboxCredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{CREDENTIAL_PREFIX}{}", self.0)
    }
}

impl FromStr for SandboxCredentialRef {
    type Err = SandboxCredentialError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(
            value
                .strip_prefix(CREDENTIAL_PREFIX)
                .ok_or(SandboxCredentialError::Reference)?,
        )
    }
}

impl TryFrom<String> for SandboxCredentialRef {
    type Error = SandboxCredentialError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SandboxCredentialRef> for String {
    fn from(value: SandboxCredentialRef) -> Self {
        value.to_string()
    }
}

/// Lifecycle API secret, intentionally neither serializable nor a Workcell bearer credential.
pub struct SandboxApiKey(String);

impl SandboxApiKey {
    pub fn new(value: String) -> Result<Self, SandboxCredentialError> {
        if value.is_empty()
            || value.len() > MAX_SANDBOX_API_KEY_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(SandboxCredentialError::Key);
        }
        Ok(Self(value))
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SandboxApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SandboxApiKey(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialRecord {
    version: u32,
    api_key: String,
}

#[derive(Serialize)]
struct CredentialRecordRef<'a> {
    version: u32,
    api_key: &'a str,
}

fn credential_file(
    dir: &StateDir,
    reference: &SandboxCredentialRef,
) -> Result<PrivateFile, SandboxCredentialError> {
    Ok(PrivateFile::new(
        dir.persistent_path()
            .join("auth")
            .join("sandbox-api")
            .join(format!("{}.json", reference.name())),
        MAX_SANDBOX_API_KEY_BYTES * 2 + CREDENTIAL_RECORD_OVERHEAD,
    )?)
}

pub fn load_sandbox_api_key(
    dir: &StateDir,
    reference: &SandboxCredentialRef,
) -> Result<Option<SandboxApiKey>, SandboxCredentialError> {
    let Some(data) = credential_file(dir, reference)?.load()?.data else {
        return Ok(None);
    };
    let record: CredentialRecord =
        serde_json::from_slice(&data).map_err(|_| SandboxCredentialError::Record)?;
    if record.version != CREDENTIAL_VERSION {
        return Err(SandboxCredentialError::Version);
    }
    SandboxApiKey::new(record.api_key).map(Some)
}

pub fn save_sandbox_api_key(
    dir: &StateDir,
    reference: &SandboxCredentialRef,
    key: &SandboxApiKey,
) -> Result<(), SandboxCredentialError> {
    let file = credential_file(dir, reference)?;
    let loaded = file.load()?;
    let record = CredentialRecordRef {
        version: CREDENTIAL_VERSION,
        api_key: key.expose_secret(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| SandboxCredentialError::Record)?;
    file.compare_exchange(&loaded.revision, Some(&bytes))?;
    Ok(())
}

pub fn delete_sandbox_api_key(
    dir: &StateDir,
    reference: &SandboxCredentialRef,
) -> Result<bool, SandboxCredentialError> {
    let file = credential_file(dir, reference)?;
    let loaded = file.load()?;
    if loaded.data.is_none() {
        return Ok(false);
    }
    file.compare_exchange(&loaded.revision, None)?;
    Ok(true)
}

pub fn list_sandbox_credentials(
    dir: &StateDir,
) -> Result<Vec<SandboxCredentialRef>, SandboxCredentialError> {
    let directory = dir.persistent_path().join("auth/sandbox-api");
    PrivateFile::new(directory.join(".catalog"), 0)?.load()?;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PrivateFileError::from(error).into()),
    };
    let mut references = Vec::new();
    for entry in entries {
        let entry = entry.map_err(PrivateFileError::from)?;
        if !entry.file_type().map_err(PrivateFileError::from)?.is_file() {
            continue;
        }
        if let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
        {
            references.push(SandboxCredentialRef::new(name)?);
        }
    }
    references.sort();
    Ok(references)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SANDBOX_API_KEY_BYTES, MAX_SANDBOX_CREDENTIAL_NAME_BYTES, SandboxApiKey,
        SandboxCredentialError, SandboxCredentialRef,
    };
    use test_case::test_case;

    const CANARY: &str = "sandbox-secret-canary";

    #[test_case("credential:local")]
    #[test_case("env:SECRET")]
    #[test_case("sandbox-api:")]
    #[test_case("sandbox-api:../local"; "traversal")]
    #[test_case("sandbox-api:/local"; "absolute")]
    #[test_case("sandbox-api:a/b"; "slash")]
    #[test_case("sandbox-api:a b"; "space")]
    fn rejects_unsafe_or_wrong_purpose_references(value: &str) {
        assert_eq!(
            value.parse::<SandboxCredentialRef>(),
            Err(SandboxCredentialError::Reference)
        );
    }

    #[test_case("")]
    #[test_case("line\nbreak")]
    #[test_case("a b")]
    #[test_case("a\0b")]
    #[test_case("a\u{7f}b")]
    #[test_case("é")]
    fn rejects_invalid_keys_without_echo(value: &str) {
        let error = SandboxApiKey::new(value.into()).unwrap_err();
        assert_eq!(error, SandboxCredentialError::Key);
    }

    #[test]
    fn key_and_reference_bounds_and_redaction() {
        let key = SandboxApiKey::new(CANARY.into()).unwrap();
        assert!(!format!("{key:?}").contains(CANARY));
        assert_eq!(key.expose_secret(), CANARY);
        assert_eq!(
            SandboxApiKey::new("a".repeat(MAX_SANDBOX_API_KEY_BYTES + 1)).unwrap_err(),
            SandboxCredentialError::Key
        );
        assert_eq!(
            SandboxCredentialRef::new("a".repeat(MAX_SANDBOX_CREDENTIAL_NAME_BYTES + 1)),
            Err(SandboxCredentialError::Reference)
        );
        let reference = SandboxCredentialRef::new("local").unwrap();
        let json = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            serde_json::from_str::<SandboxCredentialRef>(&json).unwrap(),
            reference
        );
    }

    #[cfg(unix)]
    mod persistence {
        use super::CANARY;
        use crate::StateDir;
        use crate::auth::{
            WorkcellCredential, WorkcellCredentialName, load_workcell_credential,
            save_workcell_credential,
        };
        use crate::private_file::{FileRevision, PrivateFileError};
        use crate::sandbox_auth::{
            SandboxApiKey, SandboxCredentialError, SandboxCredentialRef, credential_file,
            delete_sandbox_api_key, load_sandbox_api_key, save_sandbox_api_key,
        };
        use std::fs::{self, Permissions};
        use std::io;
        use std::os::unix::fs::{PermissionsExt, symlink};
        use tempfile::{Builder, TempDir};
        use test_case::test_case;

        const WORKCELL_TOKEN: &str = "workcell-only-secret";
        const NAME: &str = "local";
        const OWNER_MODE: u32 = 0o600;
        const DIRECTORY_MODE: u32 = 0o700;
        const GROUP_READABLE_MODE: u32 = 0o644;

        fn tempdir() -> io::Result<TempDir> {
            Builder::new()
                .permissions(Permissions::from_mode(DIRECTORY_MODE))
                .tempdir()
        }

        #[test]
        fn credential_crud_is_persistent_and_separate_from_workcell() {
            let temp = tempdir().unwrap();
            let persistent = temp.path().join("persistent");
            let volatile = temp.path().join("volatile");
            let dir = StateDir::split(volatile.clone(), persistent.clone());
            let reference = SandboxCredentialRef::new(NAME).unwrap();
            assert!(load_sandbox_api_key(&dir, &reference).unwrap().is_none());
            assert!(!delete_sandbox_api_key(&dir, &reference).unwrap());
            assert!(!persistent.exists());
            let key = SandboxApiKey::new(CANARY.into()).unwrap();
            save_sandbox_api_key(&dir, &reference, &key).unwrap();
            assert!(!volatile.exists());
            let path = credential_file(&dir, &reference).unwrap();
            assert_eq!(
                fs::metadata(path.path()).unwrap().permissions().mode() & 0o777,
                OWNER_MODE
            );
            assert_eq!(
                load_sandbox_api_key(&dir, &reference)
                    .unwrap()
                    .unwrap()
                    .expose_secret(),
                CANARY
            );
            let workcell_name = WorkcellCredentialName::new(NAME).unwrap();
            assert!(
                load_workcell_credential(&dir, &workcell_name)
                    .unwrap()
                    .is_none()
            );
            save_workcell_credential(
                &dir,
                &workcell_name,
                &WorkcellCredential::new(WORKCELL_TOKEN.into()).unwrap(),
            )
            .unwrap();
            save_sandbox_api_key(
                &dir,
                &reference,
                &SandboxApiKey::new(format!("{CANARY}-rotated")).unwrap(),
            )
            .unwrap();
            assert!(delete_sandbox_api_key(&dir, &reference).unwrap());
            assert!(!delete_sandbox_api_key(&dir, &reference).unwrap());
            assert_eq!(
                load_workcell_credential(&dir, &workcell_name)
                    .unwrap()
                    .unwrap()
                    .bearer_token(),
                WORKCELL_TOKEN
            );
        }

        #[test_case("{\"version\":1,\"api_key\":\"sandbox-secret-canary\",\"token\":\"sandbox-secret-canary\"}", SandboxCredentialError::Record; "unknown_field")]
        #[test_case("{\"version\":2,\"api_key\":\"sandbox-secret-canary\"}", SandboxCredentialError::Version; "version")]
        #[test_case("{\"version\":1,\"api_key\":\"sandbox-secret-canary\\n\"}", SandboxCredentialError::Key; "invalid_key")]
        #[test_case("sandbox-secret-canary", SandboxCredentialError::Record; "invalid_json")]
        fn malformed_records_never_echo_secrets(record: &str, expected: SandboxCredentialError) {
            let temp = tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().into());
            let reference = SandboxCredentialRef::new(NAME).unwrap();
            let file = credential_file(&dir, &reference).unwrap();
            file.compare_exchange(&FileRevision::Missing, Some(record.as_bytes()))
                .unwrap();
            let error = load_sandbox_api_key(&dir, &reference).unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?}: {error}").contains(CANARY));
        }

        #[test]
        fn credential_permissions_and_symlinks_fail_closed() {
            let temp = tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().into());
            let reference = SandboxCredentialRef::new(NAME).unwrap();
            let key = SandboxApiKey::new(CANARY.into()).unwrap();
            save_sandbox_api_key(&dir, &reference, &key).unwrap();
            let file = credential_file(&dir, &reference).unwrap();
            fs::set_permissions(file.path(), Permissions::from_mode(GROUP_READABLE_MODE)).unwrap();
            let refused = || {
                SandboxCredentialError::File(PrivateFileError::Permissions {
                    path: file.path().to_path_buf(),
                    mode: GROUP_READABLE_MODE,
                })
            };
            assert_eq!(
                load_sandbox_api_key(&dir, &reference).err(),
                Some(refused())
            );
            assert_eq!(save_sandbox_api_key(&dir, &reference, &key), Err(refused()));
            fs::remove_file(file.path()).unwrap();
            symlink(temp.path().join("absent"), file.path()).unwrap();
            assert_eq!(
                delete_sandbox_api_key(&dir, &reference),
                Err(SandboxCredentialError::File(PrivateFileError::UnsafePath))
            );
            assert_eq!(
                save_sandbox_api_key(&dir, &reference, &key),
                Err(SandboxCredentialError::File(PrivateFileError::UnsafePath))
            );
        }
    }
}
