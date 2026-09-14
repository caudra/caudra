use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::str::FromStr;

use caudra_storage::auth::{WorkcellCredentialRef, WorkcellCredentialRefError};
use caudra_storage::paths;
use caudra_workspace::{WorkspacePath, WorkspacePathError};
use serde::Deserialize;
use thiserror::Error;
use url::{Host, Url};

const WORKCELL_PROFILE_FILE: &str = "workcell.toml";
const WORKCELL_PROFILE_VERSION: u32 = 1;
const MAX_PROFILE_FILE_BYTES: u64 = 256 * 1024;
const MAX_PROFILE_NAME_BYTES: usize = 64;
const MAX_ENDPOINT_BYTES: usize = 2048;
const MAX_EXPECTED_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkcellProfileNameError {
    #[error("Workcell profile name must not be empty")]
    Empty,
    #[error("Workcell profile name exceeds {MAX_PROFILE_NAME_BYTES} bytes")]
    TooLong,
    #[error("Workcell profile name must start with an ASCII letter or digit")]
    InvalidStart,
    #[error("Workcell profile name may contain only ASCII letters, digits, '.', '-', and '_'")]
    InvalidCharacter,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkcellProfileName(String);

impl WorkcellProfileName {
    pub fn new(name: impl Into<String>) -> Result<Self, WorkcellProfileNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(WorkcellProfileNameError::Empty);
        }
        if name.len() > MAX_PROFILE_NAME_BYTES {
            return Err(WorkcellProfileNameError::TooLong);
        }
        if !name.as_bytes()[0].is_ascii_alphanumeric() {
            return Err(WorkcellProfileNameError::InvalidStart);
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(WorkcellProfileNameError::InvalidCharacter);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkcellProfileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkcellProfileName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for WorkcellProfileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for WorkcellProfileName {
    type Err = WorkcellProfileNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

#[derive(Clone, PartialEq, Eq)]
/// Validated remote transport endpoint.
///
/// HTTPS accepts DNS names and IP literals. HTTP accepts only numeric IPv4 or IPv6 loopback
/// literals because hostname resolution is not pinned to the validated address.
pub struct WorkcellEndpoint(Url);

impl WorkcellEndpoint {
    pub fn parse(endpoint: &str) -> Result<Self, WorkcellEndpointError> {
        if endpoint.is_empty() {
            return Err(WorkcellEndpointError::Empty);
        }
        if endpoint.len() > MAX_ENDPOINT_BYTES {
            return Err(WorkcellEndpointError::TooLong);
        }
        if endpoint_userinfo(endpoint) {
            return Err(WorkcellEndpointError::Userinfo);
        }
        let url = Url::parse(endpoint).map_err(|_| WorkcellEndpointError::Invalid)?;
        if url.host().is_none() || url.cannot_be_a_base() {
            return Err(WorkcellEndpointError::Invalid);
        }
        if url.query().is_some() {
            return Err(WorkcellEndpointError::Query);
        }
        if url.fragment().is_some() {
            return Err(WorkcellEndpointError::Fragment);
        }
        match url.scheme() {
            "https" => {}
            "http" if host_is_loopback(&url) => {}
            "http" => return Err(WorkcellEndpointError::InsecureRemote),
            _ => return Err(WorkcellEndpointError::Scheme),
        }
        Ok(Self(url))
    }

    pub fn as_url(&self) -> &Url {
        &self.0
    }

    pub fn is_loopback(&self) -> bool {
        host_is_loopback(&self.0)
    }
}

impl fmt::Debug for WorkcellEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkcellEndpoint")
            .field(&"<redacted>")
            .finish()
    }
}

impl FromStr for WorkcellEndpoint {
    type Err = WorkcellEndpointError;

    fn from_str(endpoint: &str) -> Result<Self, Self::Err> {
        Self::parse(endpoint)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkcellEndpointError {
    #[error("Workcell endpoint must not be empty")]
    Empty,
    #[error("Workcell endpoint exceeds {MAX_ENDPOINT_BYTES} bytes")]
    TooLong,
    #[error("Workcell endpoint is not a valid absolute URL")]
    Invalid,
    #[error("Workcell endpoint must not contain userinfo")]
    Userinfo,
    #[error("Workcell endpoint must not contain a query")]
    Query,
    #[error("Workcell endpoint must not contain a fragment")]
    Fragment,
    #[error("Workcell endpoint must use HTTPS, except for numeric IPv4 or IPv6 loopback HTTP")]
    InsecureRemote,
    #[error("Workcell endpoint must use HTTP or HTTPS")]
    Scheme,
}

fn endpoint_userinfo(endpoint: &str) -> bool {
    endpoint
        .split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn host_is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(_)) => false,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExpectedWorkcellId(String);

impl ExpectedWorkcellId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExpectedWorkcellIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ExpectedWorkcellIdError::Empty);
        }
        if value.len() > MAX_EXPECTED_ID_BYTES {
            return Err(ExpectedWorkcellIdError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(ExpectedWorkcellIdError::ControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ExpectedWorkcellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ExpectedWorkcellId")
            .field(&"<opaque>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExpectedWorkcellIdError {
    #[error("expected Workcell identifier must not be empty")]
    Empty,
    #[error("expected Workcell identifier exceeds {MAX_EXPECTED_ID_BYTES} bytes")]
    TooLong,
    #[error("expected Workcell identifier contains a control character")]
    ControlCharacter,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WorkcellProfile {
    pub endpoint: WorkcellEndpoint,
    pub cwd: WorkspacePath,
    pub credential_ref: WorkcellCredentialRef,
    pub expected_server_id: Option<ExpectedWorkcellId>,
    pub expected_workspace_id: Option<ExpectedWorkcellId>,
}

impl fmt::Debug for WorkcellProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkcellProfile")
            .field("endpoint", &self.endpoint)
            .field("cwd", &self.cwd)
            .field("credential_ref", &self.credential_ref)
            .field("expected_server_id", &self.expected_server_id)
            .field("expected_workspace_id", &self.expected_workspace_id)
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkcellProfiles(BTreeMap<WorkcellProfileName, WorkcellProfile>);

impl WorkcellProfiles {
    pub fn get(&self, name: &WorkcellProfileName) -> Option<&WorkcellProfile> {
        self.0.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkcellSourceRef {
    Direct,
    Profile(WorkcellProfileName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkcellSelection {
    Embedded,
    Remote(Box<RemoteWorkcellSelection>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWorkcellSelection {
    pub source: WorkcellSourceRef,
    pub endpoint: WorkcellEndpoint,
    pub cwd: WorkspacePath,
    pub credential_ref: Option<WorkcellCredentialRef>,
    pub expected_server_id: Option<ExpectedWorkcellId>,
    pub expected_workspace_id: Option<ExpectedWorkcellId>,
}

#[derive(Debug, Error)]
pub enum WorkcellProfileError {
    #[error("cannot resolve the local Workcell profile directory: {0}")]
    ConfigDirectory(#[source] io::Error),
    #[error("cannot inspect the local Workcell profile file: {0}")]
    Inspect(#[source] io::Error),
    #[error("local Workcell profile path must be a regular non-symlink file")]
    NotRegularFile,
    #[error("local Workcell profile file is not owned by the current user")]
    WrongOwner,
    #[error("local Workcell profile file must not be group or world writable")]
    WritableByOthers,
    #[error("local Workcell profile file exceeds {MAX_PROFILE_FILE_BYTES} bytes")]
    FileTooLarge,
    #[error("cannot read the local Workcell profile file: {0}")]
    Read(#[source] io::Error),
    #[error("local Workcell profile file is not valid strict v1 TOML")]
    Parse,
    #[error("unsupported local Workcell profile version {0}; expected version 1")]
    Version(u32),
    #[error(transparent)]
    ProfileName(#[from] WorkcellProfileNameError),
    #[error("invalid endpoint in Workcell profile '{profile}': {source}")]
    Endpoint {
        profile: WorkcellProfileName,
        #[source]
        source: WorkcellEndpointError,
    },
    #[error("invalid cwd in Workcell profile '{profile}': {source}")]
    Cwd {
        profile: WorkcellProfileName,
        #[source]
        source: WorkspacePathError,
    },
    #[error("invalid credential_ref in Workcell profile '{profile}': {source}")]
    CredentialRef {
        profile: WorkcellProfileName,
        #[source]
        source: WorkcellCredentialRefError,
    },
    #[error("invalid expected identifier in Workcell profile '{profile}': {source}")]
    ExpectedId {
        profile: WorkcellProfileName,
        #[source]
        source: ExpectedWorkcellIdError,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorkcellSelectionError {
    #[error("--workcell-profile conflicts with direct Workcell flags")]
    ConflictingSources,
    #[error("direct Workcell selection requires both --workcell-endpoint and --workcell-cwd")]
    IncompleteDirect,
    #[error(transparent)]
    ProfileName(#[from] WorkcellProfileNameError),
    #[error("Workcell profile '{0}' does not exist in the local workcell.toml")]
    UnknownProfile(WorkcellProfileName),
    #[error(transparent)]
    Endpoint(#[from] WorkcellEndpointError),
    #[error(transparent)]
    Cwd(#[from] WorkspacePathError),
    #[error(transparent)]
    CredentialRef(#[from] WorkcellCredentialRefError),
    #[error("a non-loopback Workcell endpoint requires --workcell-credential-ref")]
    MissingRemoteCredential,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkcellProfileFile {
    version: u32,
    workcell: RawWorkcellProfiles,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkcellProfiles {
    profiles: BTreeMap<String, RawWorkcellProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkcellProfile {
    endpoint: String,
    cwd: String,
    credential_ref: String,
    expected_server_id: Option<String>,
    expected_workspace_id: Option<String>,
}

pub fn load_workcell_profiles() -> Result<WorkcellProfiles, WorkcellProfileError> {
    let config_dir = paths::config_dir().map_err(WorkcellProfileError::ConfigDirectory)?;
    load_workcell_profiles_from(&config_dir)
}

pub fn load_workcell_profiles_from(
    config_dir: &Path,
) -> Result<WorkcellProfiles, WorkcellProfileError> {
    let path = config_dir.join(WORKCELL_PROFILE_FILE);
    let Some(file) = open_profile_file(&path)? else {
        return Ok(WorkcellProfiles::default());
    };
    let metadata = file.metadata().map_err(WorkcellProfileError::Inspect)?;
    validate_profile_metadata(&metadata)?;
    if metadata.len() > MAX_PROFILE_FILE_BYTES {
        return Err(WorkcellProfileError::FileTooLarge);
    }
    let mut content = String::new();
    file.take(MAX_PROFILE_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(WorkcellProfileError::Read)?;
    if content.len() as u64 > MAX_PROFILE_FILE_BYTES {
        return Err(WorkcellProfileError::FileTooLarge);
    }
    let raw: WorkcellProfileFile =
        toml::from_str(&content).map_err(|_| WorkcellProfileError::Parse)?;
    WorkcellProfiles::try_from(raw)
}

fn open_profile_file(path: &Path) -> Result<Option<File>, WorkcellProfileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(WorkcellProfileError::NotRegularFile);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WorkcellProfileError::Inspect(error)),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    options
        .open(path)
        .map(Some)
        .map_err(WorkcellProfileError::Read)
}

fn validate_profile_metadata(metadata: &fs::Metadata) -> Result<(), WorkcellProfileError> {
    if !metadata.is_file() {
        return Err(WorkcellProfileError::NotRegularFile);
    }
    #[cfg(unix)]
    {
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(WorkcellProfileError::WrongOwner);
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(WorkcellProfileError::WritableByOthers);
        }
    }
    Ok(())
}

impl TryFrom<WorkcellProfileFile> for WorkcellProfiles {
    type Error = WorkcellProfileError;

    fn try_from(file: WorkcellProfileFile) -> Result<Self, Self::Error> {
        if file.version != WORKCELL_PROFILE_VERSION {
            return Err(WorkcellProfileError::Version(file.version));
        }
        let mut profiles = BTreeMap::new();
        for (name, raw) in file.workcell.profiles {
            let name = WorkcellProfileName::new(name)?;
            let endpoint = WorkcellEndpoint::parse(&raw.endpoint).map_err(|source| {
                WorkcellProfileError::Endpoint {
                    profile: name.clone(),
                    source,
                }
            })?;
            let cwd = WorkspacePath::new(raw.cwd).map_err(|source| WorkcellProfileError::Cwd {
                profile: name.clone(),
                source,
            })?;
            let credential_ref =
                WorkcellCredentialRef::from_str(&raw.credential_ref).map_err(|source| {
                    WorkcellProfileError::CredentialRef {
                        profile: name.clone(),
                        source,
                    }
                })?;
            let expected_server_id = expected_id(raw.expected_server_id, &name)?;
            let expected_workspace_id = expected_id(raw.expected_workspace_id, &name)?;
            profiles.insert(
                name,
                WorkcellProfile {
                    endpoint,
                    cwd,
                    credential_ref,
                    expected_server_id,
                    expected_workspace_id,
                },
            );
        }
        Ok(Self(profiles))
    }
}

fn expected_id(
    value: Option<String>,
    profile: &WorkcellProfileName,
) -> Result<Option<ExpectedWorkcellId>, WorkcellProfileError> {
    value
        .map(ExpectedWorkcellId::new)
        .transpose()
        .map_err(|source| WorkcellProfileError::ExpectedId {
            profile: profile.clone(),
            source,
        })
}

pub fn select_workcell(
    profiles: &WorkcellProfiles,
    profile: Option<&str>,
    endpoint: Option<&str>,
    cwd: Option<&str>,
    credential_ref: Option<&str>,
) -> Result<WorkcellSelection, WorkcellSelectionError> {
    let has_direct = endpoint.is_some() || cwd.is_some() || credential_ref.is_some();
    if profile.is_some() && has_direct {
        return Err(WorkcellSelectionError::ConflictingSources);
    }
    if let Some(profile) = profile {
        let name = WorkcellProfileName::new(profile)?;
        let selected = profiles
            .get(&name)
            .ok_or_else(|| WorkcellSelectionError::UnknownProfile(name.clone()))?;
        return Ok(WorkcellSelection::Remote(Box::new(
            RemoteWorkcellSelection {
                source: WorkcellSourceRef::Profile(name),
                endpoint: selected.endpoint.clone(),
                cwd: selected.cwd.clone(),
                credential_ref: Some(selected.credential_ref.clone()),
                expected_server_id: selected.expected_server_id.clone(),
                expected_workspace_id: selected.expected_workspace_id.clone(),
            },
        )));
    }
    if !has_direct {
        return Ok(WorkcellSelection::Embedded);
    }
    let (Some(endpoint), Some(cwd)) = (endpoint, cwd) else {
        return Err(WorkcellSelectionError::IncompleteDirect);
    };
    let endpoint = WorkcellEndpoint::parse(endpoint)?;
    let cwd = WorkspacePath::new(cwd)?;
    let credential_ref = credential_ref
        .map(WorkcellCredentialRef::from_str)
        .transpose()?;
    if !endpoint.is_loopback() && credential_ref.is_none() {
        return Err(WorkcellSelectionError::MissingRemoteCredential);
    }
    Ok(WorkcellSelection::Remote(Box::new(
        RemoteWorkcellSelection {
            source: WorkcellSourceRef::Direct,
            endpoint,
            cwd,
            credential_ref,
            expected_server_id: None,
            expected_workspace_id: None,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::TempDir;
    use test_case::test_case;

    const PROFILE_NAME: &str = "production";
    const CREDENTIAL_REF: &str = "credential:production";
    const SECRET_ENDPOINT: &str = "https://workcell.example/private/tenant";
    const SERVER_ID: &str = "server-private-id";
    const VALID_PROFILE: &str = r#"
version = 1

[workcell.profiles.production]
endpoint = "https://workcell.example/api"
cwd = "projects/caudra"
credential_ref = "credential:production"
expected_server_id = "server-1"
expected_workspace_id = "workspace-1"
"#;

    fn write_profiles(directory: &Path, content: &str) {
        fs::write(directory.join(WORKCELL_PROFILE_FILE), content).expect("write profiles");
        #[cfg(unix)]
        fs::set_permissions(
            directory.join(WORKCELL_PROFILE_FILE),
            fs::Permissions::from_mode(0o600),
        )
        .expect("set profile permissions");
    }

    fn load_content(content: &str) -> Result<WorkcellProfiles, WorkcellProfileError> {
        let directory = TempDir::new().expect("tempdir");
        write_profiles(directory.path(), content);
        load_workcell_profiles_from(directory.path())
    }

    #[test]
    fn missing_file_means_no_profiles() {
        let directory = TempDir::new().expect("tempdir");

        let profiles = load_workcell_profiles_from(directory.path()).expect("missing is valid");

        assert!(profiles.is_empty());
    }

    #[test]
    fn strict_v1_profile_loads_typed_values() {
        let profiles = load_content(VALID_PROFILE).expect("valid profiles");
        let name = WorkcellProfileName::new(PROFILE_NAME).expect("valid name");
        let profile = profiles.get(&name).expect("production profile");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profile.endpoint.as_url().scheme(), "https");
        assert_eq!(profile.cwd.as_str(), "projects/caudra");
        assert_eq!(profile.credential_ref.to_string(), CREDENTIAL_REF);
        assert_eq!(
            profile
                .expected_server_id
                .as_ref()
                .map(ExpectedWorkcellId::as_str),
            Some("server-1")
        );
    }

    #[test_case(
        r#"version = 1
unexpected = true
[workcell.profiles.production]
endpoint = "https://workcell.example"
cwd = "project"
credential_ref = "credential:production"
"#;
        "unknown_top_level_field"
    )]
    #[test_case(
        r#"version = 1
[workcell.profiles.production]
endpoint = "https://workcell.example"
cwd = "project"
credential_ref = "credential:production"
unexpected = true
"#;
        "unknown_profile_field"
    )]
    #[test_case(
        r#"version = 1
[workcell.profiles.production]
endpoint = "https://workcell.example"
cwd = "project"
credential_ref = "credential:production"
[workcell.profiles.production]
endpoint = "https://other.example"
cwd = "other"
credential_ref = "credential:other"
"#;
        "duplicate_profile"
    )]
    fn strict_serde_rejects_ambiguous_input(content: &str) {
        assert!(matches!(
            load_content(content),
            Err(WorkcellProfileError::Parse)
        ));
    }

    #[test_case("[workcell.profiles.", "[profiles."; "legacy_top_level_profiles")]
    #[test_case("version = 1", ""; "missing_version")]
    #[test_case("[workcell.profiles.production]", "[workcell]\nunknown = true\n[workcell.profiles.production]"; "unknown_workcell_field")]
    fn finalized_profile_shape_is_required(from: &str, to: &str) {
        assert!(matches!(
            load_content(&VALID_PROFILE.replace(from, to)),
            Err(WorkcellProfileError::Parse)
        ));
    }

    #[test_case("https://workcell.example/api"; "https")]
    #[test_case("http://127.0.0.1:8080/api"; "ipv4_loopback_http")]
    #[test_case("http://[::1]:8080/api"; "ipv6_loopback_http")]
    fn endpoint_accepts_tls_and_explicit_loopback_http(endpoint: &str) {
        assert!(WorkcellEndpoint::parse(endpoint).is_ok());
    }

    #[test_case("http://workcell.example", WorkcellEndpointError::InsecureRemote; "remote_http")]
    #[test_case("http://localhost:8080", WorkcellEndpointError::InsecureRemote; "localhost_http")]
    #[test_case("https://user@workcell.example", WorkcellEndpointError::Userinfo; "username")]
    #[test_case("https://:token@workcell.example", WorkcellEndpointError::Userinfo; "password")]
    #[test_case("https://workcell.example?token=secret", WorkcellEndpointError::Query; "query")]
    #[test_case("https://workcell.example#secret", WorkcellEndpointError::Fragment; "fragment")]
    #[test_case("file:///tmp/workcell", WorkcellEndpointError::Invalid; "no_network_host")]
    fn endpoint_rejects_unsafe_urls(endpoint: &str, expected: WorkcellEndpointError) {
        assert_eq!(WorkcellEndpoint::parse(endpoint), Err(expected));
    }

    #[test]
    fn values_and_names_are_bounded() {
        assert_eq!(
            WorkcellProfileName::new("x".repeat(MAX_PROFILE_NAME_BYTES + 1)),
            Err(WorkcellProfileNameError::TooLong)
        );
        assert_eq!(
            WorkcellEndpoint::parse(&format!(
                "https://workcell.example/{}",
                "x".repeat(MAX_ENDPOINT_BYTES)
            )),
            Err(WorkcellEndpointError::TooLong)
        );
        assert_eq!(
            ExpectedWorkcellId::new("x".repeat(MAX_EXPECTED_ID_BYTES + 1)),
            Err(ExpectedWorkcellIdError::TooLong)
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_profile_file_is_rejected() {
        let directory = TempDir::new().expect("tempdir");
        write_profiles(directory.path(), VALID_PROFILE);
        let path = directory.path().join(WORKCELL_PROFILE_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o620))
            .expect("set insecure permissions");

        assert!(matches!(
            load_workcell_profiles_from(directory.path()),
            Err(WorkcellProfileError::WritableByOthers)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_profile_file_is_rejected() {
        let directory = TempDir::new().expect("tempdir");
        let target = directory.path().join("target.toml");
        fs::write(&target, VALID_PROFILE).expect("write target");
        symlink(&target, directory.path().join(WORKCELL_PROFILE_FILE)).expect("create symlink");

        assert!(matches!(
            load_workcell_profiles_from(directory.path()),
            Err(WorkcellProfileError::NotRegularFile)
        ));
    }

    #[test]
    fn debug_and_errors_redact_complete_endpoint_and_opaque_ids() {
        let content = format!(
            r#"version = 1
[workcell.profiles.production]
endpoint = "{SECRET_ENDPOINT}"
cwd = "project"
credential_ref = "{CREDENTIAL_REF}"
expected_server_id = "{SERVER_ID}"
"#
        );
        let profiles = load_content(&content).expect("valid profiles");
        let debug = format!("{profiles:?}");
        assert!(!debug.contains(SECRET_ENDPOINT));
        assert!(!debug.contains(SERVER_ID));

        let invalid = content.replace(SECRET_ENDPOINT, &format!("{SECRET_ENDPOINT}?bad=true"));
        let error = load_content(&invalid)
            .expect_err("query must fail")
            .to_string();
        assert!(!error.contains(SECRET_ENDPOINT));
        assert!(!error.contains("bad=true"));
        assert!(error.contains("must not contain a query"));
    }

    #[test]
    fn no_selector_preserves_embedded_default() {
        assert_eq!(
            select_workcell(&WorkcellProfiles::default(), None, None, None, None),
            Ok(WorkcellSelection::Embedded)
        );
    }

    #[test]
    fn selection_enforces_precedence_completeness_and_authentication() {
        let profiles = WorkcellProfiles::default();
        assert_eq!(
            select_workcell(
                &profiles,
                Some(PROFILE_NAME),
                Some("https://workcell.example"),
                Some("project"),
                Some(CREDENTIAL_REF)
            ),
            Err(WorkcellSelectionError::ConflictingSources)
        );
        assert_eq!(
            select_workcell(&profiles, None, Some("http://127.0.0.1:8080"), None, None),
            Err(WorkcellSelectionError::IncompleteDirect)
        );
        assert_eq!(
            select_workcell(
                &profiles,
                None,
                Some("https://workcell.example"),
                Some("project"),
                None
            ),
            Err(WorkcellSelectionError::MissingRemoteCredential)
        );
        assert!(matches!(
            select_workcell(
                &profiles,
                None,
                Some("http://127.0.0.1:8080"),
                Some("project"),
                None
            ),
            Ok(WorkcellSelection::Remote(selection)) if matches!(selection.as_ref(), RemoteWorkcellSelection {
                source: WorkcellSourceRef::Direct,
                credential_ref: None,
                ..
            })
        ));
    }

    #[test]
    fn named_selection_uses_only_the_supplied_local_profiles() {
        let profiles = load_content(VALID_PROFILE).expect("valid profiles");
        assert!(matches!(
            select_workcell(&profiles, Some(PROFILE_NAME), None, None, None),
            Ok(WorkcellSelection::Remote(selection)) if matches!(selection.as_ref(), RemoteWorkcellSelection {
                source: WorkcellSourceRef::Profile(_),
                credential_ref: Some(_),
                ..
            })
        ));
        assert!(matches!(
            select_workcell(
                &WorkcellProfiles::default(),
                Some(PROFILE_NAME),
                None,
                None,
                None
            ),
            Err(WorkcellSelectionError::UnknownProfile(_))
        ));
    }
}
