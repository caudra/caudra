use caudra_storage::sandbox_auth::SandboxCredentialRef;
use caudra_workspace::WorkspacePath;
use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU32;
use thiserror::Error;
use url::Url;

pub mod persistence;

pub const SANDBOX_FILE: &str = "sandboxes.toml";
pub const SANDBOX_VERSION: u32 = 1;
pub const MAX_SANDBOX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_SANDBOX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_SANDBOX_RECORDS: usize = 256;
pub const MAX_SANDBOX_NAME_BYTES: usize = 64;
pub const MAX_NETWORK_RULES: usize = 256;
pub const MAX_TRANSFER_EXCLUDES: usize = 128;
const MAX_ORIGIN_BYTES: usize = 2048;
const MAX_DOMAIN_BYTES: usize = 253;
const MAX_DOMAIN_LABEL_BYTES: usize = 63;
const MAX_FILTER_BYTES: usize = 512;
const MAX_ROOT_BYTES: usize = 4096;
const SHA256_PREFIX: &str = "sha256:";
const SHA256_HEX_BYTES: usize = 64;
const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/.env*",
    "**/target/**",
    "**/node_modules/**",
    "**/.venv/**",
    "**/.ssh/**",
    "**/.aws/**",
    "**/.caudra/**",
    "**/*.pem",
    "**/*.key",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxError {
    #[error("invalid sandbox field {field}: {requirement}")]
    Field {
        field: &'static str,
        requirement: &'static str,
    },
    #[error("sandbox configuration exceeds its file, record, or rule limit")]
    Limit,
    #[error("invalid sandbox document (values and parser diagnostics withheld)")]
    Document,
    #[error(
        "invalid sandbox document at line {line}, column {column}; check field names and value types"
    )]
    Parse { line: usize, column: usize },
    #[error("unsupported sandbox document version")]
    Version,
    #[error("sandbox {kind:?} record {name} already exists")]
    Exists { kind: RecordKind, name: SandboxName },
    #[error("sandbox {kind:?} record {name} does not exist")]
    Missing { kind: RecordKind, name: SandboxName },
    #[error("profile {profile} references missing {kind:?} record {name}")]
    Dangling {
        profile: SandboxName,
        kind: RecordKind,
        name: SandboxName,
    },
    #[error("sandbox launch capability mismatch: {0}")]
    Capability(&'static str),
}

fn field(field: &'static str, requirement: &'static str) -> SandboxError {
    SandboxError::Field { field, requirement }
}

macro_rules! text_type {
    ($name:ident) => {
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = SandboxError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SandboxName(String);
text_type!(SandboxName);

impl SandboxName {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        if value.is_empty()
            || value.len() > MAX_SANDBOX_NAME_BYTES
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(field(
                "name",
                "use 1-64 ASCII letters, digits, '.', '-' or '_', starting with a letter or digit",
            ));
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for SandboxName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SandboxOrigin(String);
text_type!(SandboxOrigin);

impl SandboxOrigin {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        let invalid = || {
            field(
                "endpoint",
                "use an HTTPS origin or numeric loopback HTTP origin, without credentials, path, query or fragment",
            )
        };
        if value.is_empty()
            || value.len() > MAX_ORIGIN_BYTES
            || value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
        {
            return Err(invalid());
        }
        let (scheme, rest) = value.split_once("://").ok_or_else(invalid)?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '@', '?', '#', '%']) {
            return Err(invalid());
        }
        let url = Url::parse(value).map_err(|_| invalid())?;
        if url.host_str().is_none() || url.port() == Some(0) {
            return Err(invalid());
        }
        match scheme {
            "https" => {}
            "http" => {
                let literal = if let Some(ipv6) = authority.strip_prefix('[') {
                    ipv6.split_once(']').map(|(ip, _)| ip).ok_or_else(invalid)?
                } else {
                    authority.split(':').next().ok_or_else(invalid)?
                };
                if !literal.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        Ok(Self(url.origin().ascii_serialization()))
    }
}

impl fmt::Debug for SandboxOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SandboxOrigin(<redacted>)")
    }
}

/// Content identity, not a mutable catalog alias or an edit counter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(String);
text_type!(Revision);

impl Revision {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        let Some(hex) = value.strip_prefix(SHA256_PREFIX) else {
            return Err(field(
                "revision",
                "expected sha256 followed by 64 hexadecimal digits",
            ));
        };
        if hex.len() != SHA256_HEX_BYTES || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(field(
                "revision",
                "expected sha256 followed by 64 hexadecimal digits",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    fn of(value: &impl Serialize) -> Result<Self, SandboxError> {
        let bytes =
            serde_json::to_vec(&(SANDBOX_VERSION, value)).map_err(|_| SandboxError::Document)?;
        let hex: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self(format!("{SHA256_PREFIX}{hex}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DomainRule(String);
text_type!(DomainRule);

impl DomainRule {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        let host = value.strip_prefix("*.").unwrap_or(value);
        if value.len() > MAX_DOMAIN_BYTES
            || host.parse::<IpAddr>().is_ok()
            || !host.contains('.')
            || !host.bytes().any(|byte| byte.is_ascii_alphabetic())
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > MAX_DOMAIN_LABEL_BYTES
                    || !label.as_bytes()[0].is_ascii_alphanumeric()
                    || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(field(
                "domains",
                "expected DNS names or leading *. subdomains, not URLs, ports or methods",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CidrRule(String);
text_type!(CidrRule);

impl CidrRule {
    pub fn parse(value: &str) -> Result<Self, SandboxError> {
        let invalid = || {
            field(
                "cidrs",
                "expected an IPv4 or IPv6 network with a valid prefix length",
            )
        };
        let (address, prefix) = value.split_once('/').ok_or_else(invalid)?;
        if !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let prefix = prefix.parse::<u32>().map_err(|_| invalid())?;
        let ip = address.parse::<IpAddr>().map_err(|_| invalid())?;
        let network = match ip {
            IpAddr::V4(ip) if prefix <= u32::BITS => IpAddr::V4(Ipv4Addr::from(
                u32::from(ip) & u32::MAX.checked_shl(u32::BITS - prefix).unwrap_or(0),
            )),
            IpAddr::V6(ip) if prefix <= u128::BITS => IpAddr::V6(Ipv6Addr::from(
                u128::from(ip) & u128::MAX.checked_shl(u128::BITS - prefix).unwrap_or(0),
            )),
            _ => return Err(invalid()),
        };
        Ok(Self(format!("{network}/{prefix}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    E2bLibvirt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProvider {
    pub kind: ProviderKind,
    pub api_endpoint: SandboxOrigin,
    pub proxy_endpoint: SandboxOrigin,
    pub credential_ref: SandboxCredentialRef,
}

impl SandboxProvider {
    pub fn revision(&self) -> Result<Revision, SandboxError> {
        Revision::of(self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Enforcement {
    #[default]
    Required,
    Off,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    #[default]
    SniOnly,
    Mitm,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    pub enforcement: Enforcement,
    #[serde(default)]
    pub tls_mode: TlsMode,
    #[serde(default)]
    pub domains: Vec<DomainRule>,
    #[serde(default)]
    pub cidrs: Vec<CidrRule>,
}

impl NetworkPolicy {
    fn normalize(&mut self) -> Result<(), SandboxError> {
        if self.domains.len() + self.cidrs.len() > MAX_NETWORK_RULES {
            return Err(SandboxError::Limit);
        }
        if self.enforcement == Enforcement::Off
            && (!self.domains.is_empty()
                || !self.cidrs.is_empty()
                || self.tls_mode != TlsMode::SniOnly)
        {
            return Err(field(
                "enforcement",
                "off cannot carry ignored rules or MITM settings",
            ));
        }
        self.domains.sort();
        self.domains.dedup();
        self.cidrs.sort();
        self.cidrs.dedup();
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitialSeed {
    #[default]
    Ask,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TransferPolicy {
    pub respect_gitignore: bool,
    pub initial_seed: InitialSeed,
    pub delete_extraneous: bool,
    pub exclude: Vec<String>,
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            initial_seed: InitialSeed::Ask,
            delete_extraneous: false,
            exclude: DEFAULT_EXCLUDES
                .iter()
                .map(|value| (*value).into())
                .collect(),
        }
    }
}

impl TransferPolicy {
    fn normalize(&mut self) -> Result<(), SandboxError> {
        if self.exclude.len() > MAX_TRANSFER_EXCLUDES {
            return Err(SandboxError::Limit);
        }
        if self.delete_extraneous {
            return Err(field(
                "delete_extraneous",
                "automatic deletion is unsupported",
            ));
        }
        for pattern in &self.exclude {
            if pattern.is_empty()
                || pattern.len() > MAX_FILTER_BYTES
                || pattern.starts_with(['/', '!'])
                || pattern.contains(['\\', ':'])
                || pattern.chars().any(char::is_control)
                || pattern
                    .split('/')
                    .any(|part| matches!(part, ".." | "." | ""))
                || GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .is_err()
            {
                return Err(field(
                    "exclude",
                    "expected a bounded relative exclusion glob without traversal or negation",
                ));
            }
        }
        self.exclude.sort();
        self.exclude.dedup();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpus: NonZeroU32,
    pub memory_mib: NonZeroU32,
    pub disk_gib: NonZeroU32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnExit {
    #[default]
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    pub provider: SandboxName,
    pub template: SandboxName,
    pub template_revision: Revision,
    pub cpus: NonZeroU32,
    pub memory_mib: NonZeroU32,
    pub disk_gib: NonZeroU32,
    pub cwd: WorkspacePath,
    pub network: SandboxName,
    pub transfer: SandboxName,
    #[serde(default = "persistent_default")]
    pub persistent: bool,
    pub running_ttl_seconds: NonZeroU32,
    #[serde(default)]
    pub on_exit: OnExit,
}

fn persistent_default() -> bool {
    true
}

impl SandboxProfile {
    pub fn resources(&self) -> Resources {
        Resources {
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            disk_gib: self.disk_gib,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordKind {
    Provider,
    Network,
    Transfer,
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxRecord {
    Provider(SandboxProvider),
    Network(NetworkPolicy),
    Transfer(TransferPolicy),
    Profile(SandboxProfile),
}

impl SandboxRecord {
    pub fn kind(&self) -> RecordKind {
        match self {
            Self::Provider(_) => RecordKind::Provider,
            Self::Network(_) => RecordKind::Network,
            Self::Transfer(_) => RecordKind::Transfer,
            Self::Profile(_) => RecordKind::Profile,
        }
    }
}

/// Editing is deliberately inert. Only a validated save produces launchable saved configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDraft {
    #[serde(default)]
    pub providers: BTreeMap<SandboxName, SandboxProvider>,
    #[serde(default)]
    pub networks: BTreeMap<SandboxName, NetworkPolicy>,
    #[serde(default)]
    pub transfers: BTreeMap<SandboxName, TransferPolicy>,
    #[serde(default)]
    pub profiles: BTreeMap<SandboxName, SandboxProfile>,
}

impl SandboxDraft {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, kind: RecordKind, name: &SandboxName) -> Result<SandboxRecord, SandboxError> {
        match kind {
            RecordKind::Provider => self
                .providers
                .get(name)
                .cloned()
                .map(SandboxRecord::Provider),
            RecordKind::Network => self.networks.get(name).cloned().map(SandboxRecord::Network),
            RecordKind::Transfer => self
                .transfers
                .get(name)
                .cloned()
                .map(SandboxRecord::Transfer),
            RecordKind::Profile => self.profiles.get(name).cloned().map(SandboxRecord::Profile),
        }
        .ok_or_else(|| SandboxError::Missing {
            kind,
            name: name.clone(),
        })
    }

    pub fn create(&mut self, name: SandboxName, record: SandboxRecord) -> Result<(), SandboxError> {
        if self.get(record.kind(), &name).is_ok() {
            return Err(SandboxError::Exists {
                kind: record.kind(),
                name,
            });
        }
        self.put(name, record);
        Ok(())
    }

    pub fn update(&mut self, name: SandboxName, record: SandboxRecord) -> Result<(), SandboxError> {
        self.get(record.kind(), &name)?;
        self.put(name, record);
        Ok(())
    }

    fn put(&mut self, name: SandboxName, record: SandboxRecord) {
        match record {
            SandboxRecord::Provider(value) => {
                self.providers.insert(name, value);
            }
            SandboxRecord::Network(value) => {
                self.networks.insert(name, value);
            }
            SandboxRecord::Transfer(value) => {
                self.transfers.insert(name, value);
            }
            SandboxRecord::Profile(value) => {
                self.profiles.insert(name, value);
            }
        }
    }

    pub fn duplicate(
        &mut self,
        kind: RecordKind,
        source: &SandboxName,
        name: SandboxName,
    ) -> Result<(), SandboxError> {
        self.create(name, self.get(kind, source)?)
    }

    pub fn remove(
        &mut self,
        kind: RecordKind,
        name: &SandboxName,
    ) -> Result<SandboxRecord, SandboxError> {
        let record = self.get(kind.clone(), name)?;
        match kind {
            RecordKind::Provider => {
                self.providers.remove(name);
            }
            RecordKind::Network => {
                self.networks.remove(name);
            }
            RecordKind::Transfer => {
                self.transfers.remove(name);
            }
            RecordKind::Profile => {
                self.profiles.remove(name);
            }
        }
        Ok(record)
    }

    pub fn affected_profiles(&self, kind: RecordKind, name: &SandboxName) -> Vec<&SandboxName> {
        self.profiles
            .iter()
            .filter(|(profile_name, profile)| match kind {
                RecordKind::Provider => &profile.provider == name,
                RecordKind::Network => &profile.network == name,
                RecordKind::Transfer => &profile.transfer == name,
                RecordKind::Profile => *profile_name == name,
            })
            .map(|(name, _)| name)
            .collect()
    }

    pub fn validate(&self) -> Result<(), SandboxError> {
        self.normalized().map(|_| ())
    }

    fn normalized(&self) -> Result<Self, SandboxError> {
        if self.providers.len() + self.networks.len() + self.transfers.len() + self.profiles.len()
            > MAX_SANDBOX_RECORDS
        {
            return Err(SandboxError::Limit);
        }
        check_records(&self.providers)?;
        check_records(&self.networks)?;
        check_records(&self.transfers)?;
        check_records(&self.profiles)?;
        let mut draft = self.clone();
        for policy in draft.networks.values_mut() {
            policy.normalize()?;
        }
        for policy in draft.transfers.values_mut() {
            policy.normalize()?;
        }
        for (name, profile) in &draft.profiles {
            for (kind, reference, exists) in [
                (
                    RecordKind::Provider,
                    &profile.provider,
                    draft.providers.contains_key(&profile.provider),
                ),
                (
                    RecordKind::Network,
                    &profile.network,
                    draft.networks.contains_key(&profile.network),
                ),
                (
                    RecordKind::Transfer,
                    &profile.transfer,
                    draft.transfers.contains_key(&profile.transfer),
                ),
            ] {
                if !exists {
                    return Err(SandboxError::Dangling {
                        profile: name.clone(),
                        kind,
                        name: reference.clone(),
                    });
                }
            }
        }
        if serde_json::to_vec(&draft)
            .map_err(|_| SandboxError::Document)?
            .len()
            > MAX_SANDBOX_FILE_BYTES
        {
            return Err(SandboxError::Limit);
        }
        Ok(draft)
    }

    pub fn import(document: &str) -> Result<Self, SandboxError> {
        if document.len() > MAX_SANDBOX_FILE_BYTES {
            return Err(SandboxError::Limit);
        }
        let file: SandboxDocument =
            toml::from_str(document).map_err(|error: toml::de::Error| {
                let offset = error.span().map_or(0, |span| span.start);
                let before = document.get(..offset).unwrap_or_default();
                SandboxError::Parse {
                    line: before.bytes().filter(|byte| *byte == b'\n').count() + 1,
                    column: before
                        .rsplit('\n')
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .count()
                        + 1,
                }
            })?;
        if file.version != SANDBOX_VERSION {
            return Err(SandboxError::Version);
        }
        file.sandbox.normalized()
    }

    /// Exports only configuration and purpose-scoped references, never secrets or instance IDs.
    pub fn export(&self) -> Result<String, SandboxError> {
        let document = SandboxDocument {
            version: SANDBOX_VERSION,
            sandbox: self.normalized()?,
        };
        let text = toml::to_string_pretty(&document).map_err(|_| SandboxError::Document)?;
        if text.len() > MAX_SANDBOX_FILE_BYTES {
            return Err(SandboxError::Limit);
        }
        Ok(text)
    }
}

fn check_records<T: Serialize>(records: &BTreeMap<SandboxName, T>) -> Result<(), SandboxError> {
    for value in records.values() {
        if serde_json::to_vec(value)
            .map_err(|_| SandboxError::Document)?
            .len()
            > MAX_SANDBOX_RECORD_BYTES
        {
            return Err(SandboxError::Limit);
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxDocument {
    version: u32,
    sandbox: SandboxDraft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavedRecord<T> {
    revision: Revision,
    value: T,
}

impl<'de, T: Deserialize<'de> + Serialize> Deserialize<'de> for SavedRecord<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire<T> {
            revision: Revision,
            value: T,
        }
        let wire = Wire::<T>::deserialize(deserializer)?;
        let saved = Self::new(wire.value).map_err(serde::de::Error::custom)?;
        if saved.revision != wire.revision {
            return Err(serde::de::Error::custom("saved sandbox revision mismatch"));
        }
        Ok(saved)
    }
}

impl<T: Serialize> SavedRecord<T> {
    fn new(value: T) -> Result<Self, SandboxError> {
        Ok(Self {
            revision: Revision::of(&value)?,
            value,
        })
    }
    pub fn revision(&self) -> &Revision {
        &self.revision
    }
    pub fn value(&self) -> &T {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSandboxes(SavedRecord<SandboxDraft>);

impl SavedSandboxes {
    fn new(draft: SandboxDraft) -> Result<Self, SandboxError> {
        SavedRecord::new(draft.normalized()?).map(Self)
    }
    pub fn configuration(&self) -> &SandboxDraft {
        self.0.value()
    }
    pub fn draft(&self) -> SandboxDraft {
        self.configuration().clone()
    }
    pub fn revision(&self) -> &Revision {
        self.0.revision()
    }
    pub fn record(
        &self,
        kind: RecordKind,
        name: &SandboxName,
    ) -> Result<SavedRecord<SandboxRecord>, SandboxError> {
        let value = self.configuration().get(kind, name)?;
        let revision = match &value {
            SandboxRecord::Provider(value) => Revision::of(value)?,
            SandboxRecord::Network(value) => Revision::of(value)?,
            SandboxRecord::Transfer(value) => Revision::of(value)?,
            SandboxRecord::Profile(value) => Revision::of(value)?,
        };
        Ok(SavedRecord { revision, value })
    }

    pub fn resolve_launch(
        &self,
        name: &SandboxName,
        capabilities: &ProviderCapabilities,
        catalog: &TemplateCatalog,
    ) -> Result<ResolvedLaunch, SandboxError> {
        let configuration = self.configuration();
        let profile = configuration
            .profiles
            .get(name)
            .ok_or_else(|| SandboxError::Missing {
                kind: RecordKind::Profile,
                name: name.clone(),
            })?;
        let provider = SavedRecord::new(configuration.providers[&profile.provider].clone())?;
        if provider.revision() != &capabilities.provider_revision {
            return Err(SandboxError::Capability("provider revision"));
        }
        let template = catalog
            .get(&profile.template, &profile.template_revision)
            .ok_or(SandboxError::Capability(
                "immutable template revision is absent from catalog",
            ))?;
        let network = &configuration.networks[&profile.network];
        capabilities.validate(profile, network, template)?;
        let effective = LaunchConfiguration {
            profile_name: name.clone(),
            profile: SavedRecord::new(profile.clone())?,
            provider,
            network: SavedRecord::new(network.clone())?,
            transfer: SavedRecord::new(configuration.transfers[&profile.transfer].clone())?,
            template: template.clone(),
        };
        Ok(ResolvedLaunch(SavedRecord::new(effective)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Architecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateEntry {
    pub id: SandboxName,
    pub revision: Revision,
    pub architecture: Architecture,
    pub minimum_resources: Resources,
    pub workspace_root: String,
    pub snapshot_root: String,
    pub workcell_compatible: bool,
    pub network_modes: Vec<Enforcement>,
    pub guest_ca: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateCatalog(BTreeMap<(SandboxName, Revision), TemplateEntry>);

impl TemplateCatalog {
    pub fn new(entries: Vec<TemplateEntry>) -> Result<Self, SandboxError> {
        Self::build(entries, false)
    }

    /// Older remote catalogs omit guest paths. Reviewed layout is optional metadata;
    /// live Workcell discovery must still establish the services before use.
    pub fn without_guest_layout(entries: Vec<TemplateEntry>) -> Result<Self, SandboxError> {
        Self::build(entries, true)
    }

    fn build(entries: Vec<TemplateEntry>, undisclosed_layout: bool) -> Result<Self, SandboxError> {
        if entries.len() > MAX_SANDBOX_RECORDS {
            return Err(SandboxError::Limit);
        }
        let mut catalog = BTreeMap::new();
        for entry in entries {
            if !undisclosed_layout
                || (entry.workcell_compatible
                    && (!entry.workspace_root.is_empty() || !entry.snapshot_root.is_empty()))
            {
                validate_root(&entry.workspace_root)?;
                validate_root(&entry.snapshot_root)?;
                if overlaps(&entry.workspace_root, &entry.snapshot_root)
                    || overlaps(&entry.snapshot_root, &entry.workspace_root)
                {
                    return Err(field(
                        "snapshot_root",
                        "workspace and snapshot roots must not overlap",
                    ));
                }
            }
            if entry.network_modes.is_empty() || entry.network_modes.len() > 2 {
                return Err(field(
                    "network_modes",
                    "declare supported enforced/unrestricted topology",
                ));
            }
            let key = (entry.id.clone(), entry.revision.clone());
            if catalog.insert(key, entry).is_some() {
                return Err(SandboxError::Capability(
                    "duplicate immutable template revision",
                ));
            }
        }
        Ok(Self(catalog))
    }

    pub fn get(&self, id: &SandboxName, revision: &Revision) -> Option<&TemplateEntry> {
        self.0.get(&(id.clone(), revision.clone()))
    }
    pub fn entries(&self) -> impl Iterator<Item = &TemplateEntry> {
        self.0.values()
    }
}

fn validate_root(root: &str) -> Result<(), SandboxError> {
    if root.len() > MAX_ROOT_BYTES
        || !root.starts_with('/')
        || root == "/"
        || root.contains('\\')
        || root.chars().any(char::is_control)
        || root[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(field(
            "guest_root",
            "expected a bounded absolute guest directory without traversal",
        ));
    }
    Ok(())
}

fn overlaps(root: &str, other: &str) -> bool {
    root == other
        || other
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A fixed resource is represented by min == max; unsupported overrides are not rounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRange {
    pub min: NonZeroU32,
    pub max: NonZeroU32,
    pub step: NonZeroU32,
}

impl ResourceRange {
    fn accepts(&self, value: NonZeroU32) -> bool {
        self.min <= self.max
            && value >= self.min
            && value <= self.max
            && (value.get() - self.min.get()).is_multiple_of(self.step.get())
    }
}

/// Caller-supplied, authenticated provider metadata. Offline validation is not a daemon probe
/// or permission to launch; provider_revision binds these claims to the saved trust anchors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub provider_revision: Revision,
    pub architecture: Architecture,
    pub cpus: ResourceRange,
    pub memory_mib: ResourceRange,
    pub disk_gib: ResourceRange,
    pub disk_growth: bool,
    pub persistent: bool,
    pub max_ttl_seconds: NonZeroU32,
    pub network_modes: Vec<Enforcement>,
    pub tls_modes: Vec<TlsMode>,
}

impl ProviderCapabilities {
    fn validate(
        &self,
        profile: &SandboxProfile,
        network: &NetworkPolicy,
        template: &TemplateEntry,
    ) -> Result<(), SandboxError> {
        if !template.workcell_compatible {
            return Err(SandboxError::Capability("template Workcell compatibility"));
        }
        if self.architecture != template.architecture {
            return Err(SandboxError::Capability("template architecture"));
        }
        for (request, minimum, range, label) in [
            (
                profile.cpus,
                template.minimum_resources.cpus,
                &self.cpus,
                "cpus",
            ),
            (
                profile.memory_mib,
                template.minimum_resources.memory_mib,
                &self.memory_mib,
                "memory_mib",
            ),
            (
                profile.disk_gib,
                template.minimum_resources.disk_gib,
                &self.disk_gib,
                "disk_gib",
            ),
        ] {
            if request < minimum || !range.accepts(request) {
                return Err(SandboxError::Capability(label));
            }
        }
        if !self.disk_growth && profile.disk_gib != template.minimum_resources.disk_gib {
            return Err(SandboxError::Capability("disk growth"));
        }
        if profile.persistent && !self.persistent {
            return Err(SandboxError::Capability("persistent disks"));
        }
        if profile.running_ttl_seconds > self.max_ttl_seconds {
            return Err(SandboxError::Capability("running TTL"));
        }
        if self.network_modes.len() > 2
            || self.tls_modes.len() > 2
            || !self.network_modes.contains(&network.enforcement)
            || !template.network_modes.contains(&network.enforcement)
        {
            return Err(SandboxError::Capability("network enforcement/topology"));
        }
        if network.enforcement == Enforcement::Required
            && (!self.tls_modes.contains(&network.tls_mode)
                || (network.tls_mode == TlsMode::Mitm && !template.guest_ca))
        {
            return Err(SandboxError::Capability("TLS mode or guest CA"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConfiguration {
    pub profile_name: SandboxName,
    pub profile: SavedRecord<SandboxProfile>,
    pub provider: SavedRecord<SandboxProvider>,
    pub network: SavedRecord<NetworkPolicy>,
    pub transfer: SavedRecord<TransferPolicy>,
    pub template: TemplateEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLaunch(SavedRecord<LaunchConfiguration>);

impl ResolvedLaunch {
    pub fn configuration(&self) -> &LaunchConfiguration {
        self.0.value()
    }
    pub fn revision(&self) -> &Revision {
        self.0.revision()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Architecture, CidrRule, DomainRule, Enforcement, InitialSeed, MAX_NETWORK_RULES,
        MAX_SANDBOX_FILE_BYTES, MAX_SANDBOX_NAME_BYTES, MAX_SANDBOX_RECORDS, MAX_TRANSFER_EXCLUDES,
        NetworkPolicy, OnExit, ProviderCapabilities, RecordKind, ResourceRange, Revision,
        SandboxDraft, SandboxError, SandboxName, SandboxOrigin, SandboxRecord, SavedSandboxes,
        TemplateCatalog, TemplateEntry, TlsMode, TransferPolicy,
    };
    use std::num::NonZeroU32;
    use test_case::test_case;

    pub(super) const SAMPLE: &str = r#"# Client-owned launch defaults
version = 1

[sandbox.providers.local]
kind = "e2b-libvirt"
api_endpoint = "http://127.0.0.1:3000"
proxy_endpoint = "http://127.0.0.1:49983"
credential_ref = "sandbox-api:local"

# Shared policy
[sandbox.networks.build]
enforcement = "required"
tls_mode = "sni-only"
domains = ["registry.example", "*.example.com"] # reviewed hosts
cidrs = ["192.0.2.0/24"]

[sandbox.transfers.source]
initial_seed = "ask"

[sandbox.profiles.dev]
provider = "local"
template = "rust"
template_revision = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
cpus = 4 # virtual CPUs
memory_mib = 4096
disk_gib = 20
cwd = "."
network = "build"
transfer = "source"
running_ttl_seconds = 3600
"#;
    const CANARY: &str = "secret-canary-do-not-display";
    const PROFILE: &str = "dev";
    const PROVIDER: &str = "local";
    const NETWORK: &str = "build";
    const TRANSFER: &str = "source";
    const CPU_CAPABILITY: &str = "cpus";
    const MEMORY_CAPABILITY: &str = "memory_mib";
    const DISK_CAPABILITY: &str = "disk_gib";
    const NETWORK_CAPABILITY: &str = "network enforcement/topology";
    const TLS_CAPABILITY: &str = "TLS mode or guest CA";
    const DISK_GROWTH_CAPABILITY: &str = "disk growth";
    const TTL_CAPABILITY: &str = "running TTL";
    const PERSISTENT_CAPABILITY: &str = "persistent disks";
    const ARCHITECTURE_CAPABILITY: &str = "template architecture";
    const WORKCELL_CAPABILITY: &str = "template Workcell compatibility";
    const PROVIDER_CAPABILITY: &str = "provider revision";
    const TEMPLATE_CAPABILITY: &str = "immutable template revision is absent from catalog";

    pub(super) fn name(value: &str) -> SandboxName {
        SandboxName::parse(value).unwrap()
    }
    fn positive(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }
    pub(super) fn draft() -> SandboxDraft {
        SandboxDraft::import(SAMPLE).unwrap()
    }
    fn saved() -> SavedSandboxes {
        SavedSandboxes::new(draft()).unwrap()
    }

    fn template() -> TemplateEntry {
        let profile = draft().profiles.remove(&name(PROFILE)).unwrap();
        let mut minimum_resources = profile.resources();
        minimum_resources.disk_gib = positive(8);
        TemplateEntry {
            id: profile.template,
            revision: profile.template_revision,
            architecture: Architecture::X86_64,
            minimum_resources,
            workspace_root: "/workspace".into(),
            snapshot_root: "/snapshots".into(),
            workcell_compatible: true,
            network_modes: vec![Enforcement::Required],
            guest_ca: false,
        }
    }

    fn catalog() -> TemplateCatalog {
        TemplateCatalog::new(vec![template()]).unwrap()
    }

    #[test]
    fn resolved_launch_roundtrip_validates_every_content_revision() {
        let launch = saved()
            .resolve_launch(&name(PROFILE), &capabilities(), &catalog())
            .unwrap();
        let value = serde_json::to_value(&launch).unwrap();
        assert_eq!(
            serde_json::from_value::<super::ResolvedLaunch>(value.clone()).unwrap(),
            launch
        );
        let mut changed = value;
        changed["value"]["profile"]["value"]["cpus"] = serde_json::json!(7);
        assert!(serde_json::from_value::<super::ResolvedLaunch>(changed).is_err());
    }

    fn capabilities() -> ProviderCapabilities {
        ProviderCapabilities {
            provider_revision: saved()
                .record(RecordKind::Provider, &name(PROVIDER))
                .unwrap()
                .revision()
                .clone(),
            architecture: Architecture::X86_64,
            cpus: ResourceRange {
                min: positive(1),
                max: positive(8),
                step: positive(1),
            },
            memory_mib: ResourceRange {
                min: positive(1024),
                max: positive(8192),
                step: positive(1024),
            },
            disk_gib: ResourceRange {
                min: positive(8),
                max: positive(64),
                step: positive(1),
            },
            disk_growth: true,
            persistent: true,
            max_ttl_seconds: positive(7200),
            network_modes: vec![Enforcement::Required],
            tls_modes: vec![TlsMode::SniOnly, TlsMode::Mitm],
        }
    }

    #[test_case("a"; "letter")]
    #[test_case("0-local.prod_2"; "allowed_punctuation")]
    fn accepts_names(value: &str) {
        assert_eq!(SandboxName::parse(value).unwrap().as_str(), value);
    }

    #[test_case(""; "empty")]
    #[test_case("../secret"; "traversal")]
    #[test_case(".hidden"; "invalid_start")]
    #[test_case("/absolute"; "absolute")]
    #[test_case("a/b"; "slash")]
    #[test_case("a b"; "space")]
    #[test_case("a\nb"; "newline")]
    #[test_case("é"; "unicode")]
    fn rejects_names(value: &str) {
        assert!(matches!(
            SandboxName::parse(value),
            Err(SandboxError::Field { field: "name", .. })
        ));
        assert!(
            serde_json::from_str::<SandboxName>(&serde_json::to_string(value).unwrap()).is_err()
        );
    }

    #[test]
    fn bounds_names() {
        assert!(SandboxName::parse(&"a".repeat(MAX_SANDBOX_NAME_BYTES)).is_ok());
        assert!(SandboxName::parse(&"a".repeat(MAX_SANDBOX_NAME_BYTES + 1)).is_err());
    }

    #[test_case("http://127.0.0.1:3000", "http://127.0.0.1:3000")]
    #[test_case("http://[::1]:3000/", "http://[::1]:3000")]
    #[test_case("https://API.EXAMPLE:443/", "https://api.example")]
    #[test_case("https://192.0.2.1:8443", "https://192.0.2.1:8443")]
    fn accepts_origins(value: &str, expected: &str) {
        let origin = SandboxOrigin::parse(value).unwrap();
        assert_eq!(origin.as_str(), expected);
        assert!(!format!("{origin:?}").contains(value));
    }

    #[test_case("http://localhost:3000")]
    #[test_case("http://example.com")]
    #[test_case("http://192.0.2.1")]
    #[test_case("http://[::ffff:127.0.0.1]")]
    #[test_case("http://127.1")]
    #[test_case("http://2130706433")]
    #[test_case("http://0x7f000001")]
    #[test_case("https://api.example/path")]
    #[test_case("https://api.example/../"; "traversal_path")]
    #[test_case("https://api.example//"; "double_slash_path")]
    #[test_case("https://user:secret@api.example")]
    #[test_case("https://@api.example"; "empty_userinfo")]
    #[test_case("https://api.example?token=secret")]
    #[test_case("https://api.example#secret")]
    #[test_case("https://api.example:0")]
    #[test_case("https://api.example\\evil")]
    #[test_case("https://api.example\n"; "control_character")]
    #[test_case(" https://api.example"; "whitespace")]
    #[test_case("https://%61pi.example")]
    #[test_case("file:///tmp/config")]
    fn rejects_origins(value: &str) {
        assert!(SandboxOrigin::parse(value).is_err());
    }

    #[test_case("*.EXAMPLE.com", "*.example.com")]
    #[test_case("registry.example", "registry.example")]
    fn normalizes_domains(value: &str, expected: &str) {
        assert_eq!(DomainRule::parse(value).unwrap().as_str(), expected);
    }

    #[test_case("*")]
    #[test_case("*example.com"; "invalid_wildcard")]
    #[test_case("a.*.example.com")]
    #[test_case("example.com:443")]
    #[test_case("GET example.com")]
    #[test_case("https://example.com")]
    #[test_case("example.com/path")]
    #[test_case("example.com."; "trailing_dot")]
    #[test_case("a..com"; "empty_label")]
    #[test_case("-a.com"; "leading_hyphen")]
    #[test_case("a-.com"; "trailing_hyphen")]
    #[test_case("192.0.2.1")]
    fn rejects_fake_domain_rules(value: &str) {
        assert!(DomainRule::parse(value).is_err());
    }

    #[test_case("192.0.2.23/24", "192.0.2.0/24")]
    #[test_case("192.0.2.23/0", "0.0.0.0/0")]
    #[test_case("192.0.2.23/32", "192.0.2.23/32")]
    #[test_case("2001:db8::123/32", "2001:db8::/32")]
    #[test_case("2001:db8::1/128", "2001:db8::1/128")]
    #[test_case("2001:db8::1/0", "::/0")]
    fn normalizes_cidrs(value: &str, expected: &str) {
        assert_eq!(CidrRule::parse(value).unwrap().as_str(), expected);
    }

    #[test_case("192.0.2.0/33")]
    #[test_case("::/129")]
    #[test_case("192.0.2.1:443/32")]
    #[test_case("example.com/24")]
    #[test_case("192.0.2.1")]
    #[test_case("::/-1"; "negative_prefix")]
    #[test_case("::/+1"; "positive_sign")]
    fn rejects_invalid_cidrs(value: &str) {
        assert!(CidrRule::parse(value).is_err());
    }

    #[test_case("version = 1", "version = 1\nunknown = 'secret-canary-do-not-display'"; "document")]
    #[test_case("[sandbox.providers.local]", "[sandbox]\nunknown = 'secret-canary-do-not-display'\n[sandbox.providers.local]"; "sandbox")]
    #[test_case("[sandbox.providers.local]", "[sandbox.providers.local]\napi_key = 'secret-canary-do-not-display'"; "provider")]
    #[test_case("[sandbox.networks.build]", "[sandbox.networks.build]\nports = [443]"; "ports")]
    #[test_case("[sandbox.networks.build]", "[sandbox.networks.build]\nmethods = ['GET']"; "methods")]
    #[test_case("[sandbox.transfers.source]", "[sandbox.transfers.source]\nlocal_root = '/tmp/secret'"; "transfer")]
    #[test_case("[sandbox.profiles.dev]", "[sandbox.profiles.dev]\ninstance_id = 'secret-canary-do-not-display'"; "profile")]
    #[test_case("credential_ref = \"sandbox-api:local\"", "credential_ref = 'env:SECRET'"; "env_ref")]
    #[test_case("credential_ref = \"sandbox-api:local\"", "credential_ref = 'credential:local'"; "workcell_ref")]
    #[test_case("cpus = 4", "cpus = 0"; "zero_cpu")]
    #[test_case("memory_mib = 4096", "memory_mib = -1"; "negative_memory")]
    #[test_case("disk_gib = 20", "disk_gib = 0"; "zero_disk")]
    #[test_case("running_ttl_seconds = 3600", "running_ttl_seconds = 0"; "zero_ttl")]
    #[test_case("running_ttl_seconds = 3600", ""; "missing_ttl")]
    #[test_case("enforcement = \"required\"", ""; "explicit_enforcement")]
    #[test_case("cwd = \".\"", "cwd = '../escape'"; "cwd_escape")]
    #[test_case("cwd = \".\"", "cwd = '/workspace'"; "cwd_absolute")]
    #[test_case("initial_seed = \"ask\"", "initial_seed = 'always'"; "no_auto_seed")]
    #[test_case("running_ttl_seconds = 3600", "running_ttl_seconds = 3600\non_exit = 'delete'"; "no_exit_delete")]
    fn strict_document_rejection_is_redacted(before: &str, after: &str) {
        let error = SandboxDraft::import(&SAMPLE.replace(before, after)).unwrap_err();
        assert!(matches!(error, SandboxError::Parse { .. }));
        assert!(!format!("{error:?}: {error}").contains(CANARY));
    }

    #[test]
    fn safe_defaults_and_import_export() {
        let draft = draft();
        let profile = &draft.profiles[&name(PROFILE)];
        assert!(profile.persistent);
        assert_eq!(profile.on_exit, OnExit::Detach);
        let transfer = &draft.transfers[&name(TRANSFER)];
        assert!(transfer.respect_gitignore);
        assert_eq!(transfer.initial_seed, InitialSeed::Ask);
        assert!(!transfer.delete_extraneous);
        assert!(transfer.exclude.contains(&"**/.env*".into()));
        assert_eq!(
            SandboxDraft::import(&draft.export().unwrap()).unwrap(),
            draft
        );
        assert_eq!(NetworkPolicy::default().enforcement, Enforcement::Required);
        assert!(NetworkPolicy::default().domains.is_empty());
        assert_eq!(
            SandboxDraft::import(&SAMPLE.replace("version = 1", "version = 2")),
            Err(SandboxError::Version)
        );
    }

    #[test_case("/etc/**")]
    #[test_case("../**"; "traversal")]
    #[test_case("x/../**")]
    #[test_case("!secret")]
    #[test_case("C:/secret")]
    #[test_case("a\\b")]
    #[test_case("["; "invalid_glob")]
    #[test_case("line\nbreak")]
    fn invalid_transfer_filters(value: &str) {
        let mut draft = draft();
        draft.transfers.get_mut(&name(TRANSFER)).unwrap().exclude = vec![value.into()];
        assert!(matches!(
            draft.validate(),
            Err(SandboxError::Field {
                field: "exclude",
                ..
            })
        ));
    }

    #[test]
    fn rule_and_record_limits_are_checked_before_deduplication() {
        let mut draft = draft();
        draft.networks.get_mut(&name(NETWORK)).unwrap().domains =
            vec![DomainRule::parse("a.example").unwrap(); MAX_NETWORK_RULES + 1];
        assert_eq!(draft.validate(), Err(SandboxError::Limit));
        let longest_domain = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61),
        ]
        .join(".");
        let network = draft.networks.get_mut(&name(NETWORK)).unwrap();
        network.domains = vec![DomainRule::parse(&longest_domain).unwrap(); MAX_NETWORK_RULES];
        network.cidrs.clear();
        assert_eq!(draft.validate(), Err(SandboxError::Limit));
        draft.networks.clear();
        draft.profiles.clear();
        draft.transfers.get_mut(&name(TRANSFER)).unwrap().exclude =
            vec!["cache/**".into(); MAX_TRANSFER_EXCLUDES + 1];
        assert_eq!(draft.validate(), Err(SandboxError::Limit));
        assert_eq!(
            SandboxDraft::import(&" ".repeat(MAX_SANDBOX_FILE_BYTES + 1)),
            Err(SandboxError::Limit)
        );
        let mut draft = SandboxDraft::new();
        for index in 0..=MAX_SANDBOX_RECORDS {
            draft
                .networks
                .insert(name(&format!("policy-{index}")), NetworkPolicy::default());
        }
        assert_eq!(draft.validate(), Err(SandboxError::Limit));
    }

    #[test]
    fn explicit_off_cannot_claim_enforcement_and_deletion_is_not_a_default_override() {
        let mut draft = draft();
        draft.networks.get_mut(&name(NETWORK)).unwrap().enforcement = Enforcement::Off;
        assert!(matches!(
            draft.validate(),
            Err(SandboxError::Field {
                field: "enforcement",
                ..
            })
        ));
        draft.networks.insert(
            name(NETWORK),
            NetworkPolicy {
                enforcement: Enforcement::Off,
                ..NetworkPolicy::default()
            },
        );
        draft.validate().unwrap();
        draft
            .transfers
            .get_mut(&name(TRANSFER))
            .unwrap()
            .delete_extraneous = true;
        assert!(matches!(
            draft.validate(),
            Err(SandboxError::Field {
                field: "delete_extraneous",
                ..
            })
        ));
    }

    #[test_case(RecordKind::Provider, PROVIDER)]
    #[test_case(RecordKind::Network, NETWORK)]
    #[test_case(RecordKind::Transfer, TRANSFER)]
    fn dangling_references_fail_validation(kind: RecordKind, reference: &str) {
        let mut draft = draft();
        draft.remove(kind.clone(), &name(reference)).unwrap();
        assert_eq!(
            draft.validate(),
            Err(SandboxError::Dangling {
                profile: name(PROFILE),
                kind,
                name: name(reference)
            })
        );
    }

    #[test_case(RecordKind::Provider, PROVIDER)]
    #[test_case(RecordKind::Network, NETWORK)]
    #[test_case(RecordKind::Transfer, TRANSFER)]
    #[test_case(RecordKind::Profile, PROFILE)]
    fn draft_crud_does_not_mutate_saved_configuration(kind: RecordKind, source: &str) {
        let saved = saved();
        let mut draft = saved.draft();
        let copy = name("copy");
        draft
            .duplicate(kind.clone(), &name(source), copy.clone())
            .unwrap();
        assert!(matches!(
            draft.duplicate(kind.clone(), &name(source), copy.clone()),
            Err(SandboxError::Exists { .. })
        ));
        let record = draft.get(kind.clone(), &copy).unwrap();
        draft.update(copy.clone(), record.clone()).unwrap();
        assert_eq!(draft.remove(kind.clone(), &copy).unwrap(), record);
        assert!(matches!(
            draft.update(copy, record),
            Err(SandboxError::Missing { .. })
        ));
        assert_eq!(
            draft.affected_profiles(kind, &name(source)),
            vec![&name(PROFILE)]
        );
        assert_eq!(saved.configuration(), &draft);
    }

    #[test]
    fn revisions_are_semantic_and_effective_snapshots_pin_shared_policies() {
        let saved = saved();
        let launch = saved
            .resolve_launch(&name(PROFILE), &capabilities(), &catalog())
            .unwrap();
        let original_snapshot = serde_json::to_string(&launch).unwrap();
        let reordered = SAMPLE
            .replace(
                "[\"registry.example\", \"*.example.com\"]",
                "[\"*.EXAMPLE.com\", \"registry.example\", \"registry.example\"]",
            )
            .replace("192.0.2.0/24", "192.0.2.99/24");
        let same = SavedSandboxes::new(
            SandboxDraft::import(&format!("# edited comment\n{reordered}")).unwrap(),
        )
        .unwrap();
        assert_eq!(same.revision(), saved.revision());
        let mut draft = saved.draft();
        draft
            .networks
            .get_mut(&name(NETWORK))
            .unwrap()
            .domains
            .push(DomainRule::parse("new.example").unwrap());
        draft
            .transfers
            .get_mut(&name(TRANSFER))
            .unwrap()
            .initial_seed = InitialSeed::None;
        assert_eq!(
            saved
                .resolve_launch(&name(PROFILE), &capabilities(), &catalog())
                .unwrap(),
            launch
        );
        let changed = SavedSandboxes::new(draft).unwrap();
        let future = changed
            .resolve_launch(&name(PROFILE), &capabilities(), &catalog())
            .unwrap();
        assert_ne!(launch.revision(), future.revision());
        assert_eq!(
            launch.configuration().profile.revision(),
            future.configuration().profile.revision()
        );
        assert_ne!(
            launch.configuration().network.revision(),
            future.configuration().network.revision()
        );
        assert_ne!(
            launch.configuration().transfer.revision(),
            future.configuration().transfer.revision()
        );
        assert_eq!(serde_json::to_string(&launch).unwrap(), original_snapshot);
        let mut unrelated = changed.draft();
        unrelated
            .create(
                name("unused"),
                SandboxRecord::Transfer(TransferPolicy::default()),
            )
            .unwrap();
        let unrelated = SavedSandboxes::new(unrelated).unwrap();
        assert_ne!(unrelated.revision(), changed.revision());
        assert_eq!(
            unrelated
                .resolve_launch(&name(PROFILE), &capabilities(), &catalog())
                .unwrap(),
            future
        );
    }

    #[test_case(|caps| caps.cpus.max = positive(2), CPU_CAPABILITY; "cpu_max")]
    #[test_case(|caps| caps.cpus.step = positive(2), CPU_CAPABILITY; "cpu_step_no_rounding")]
    #[test_case(|caps| caps.cpus.min = positive(9), CPU_CAPABILITY; "invalid_range")]
    #[test_case(|caps| caps.memory_mib.max = positive(2048), MEMORY_CAPABILITY; "memory")]
    #[test_case(|caps| caps.disk_gib.max = positive(8), DISK_CAPABILITY; "disk")]
    #[test_case(|caps| caps.disk_growth = false, DISK_GROWTH_CAPABILITY; "unsupported_growth")]
    #[test_case(|caps| caps.persistent = false, PERSISTENT_CAPABILITY; "persistent")]
    #[test_case(|caps| caps.max_ttl_seconds = positive(60), TTL_CAPABILITY; "ttl")]
    #[test_case(|caps| caps.network_modes = vec![Enforcement::Off], NETWORK_CAPABILITY; "no_enforcement")]
    #[test_case(|caps| caps.tls_modes.clear(), TLS_CAPABILITY; "no_tls")]
    #[test_case(|caps| caps.architecture = Architecture::Aarch64, ARCHITECTURE_CAPABILITY; "architecture")]
    fn capability_failures_are_closed(mutate: fn(&mut ProviderCapabilities), reason: &'static str) {
        let mut capabilities = capabilities();
        mutate(&mut capabilities);
        assert_eq!(
            saved().resolve_launch(&name(PROFILE), &capabilities, &catalog()),
            Err(SandboxError::Capability(reason))
        );
    }

    #[test]
    fn validates_template_revision_minimums_compatibility_and_mitm() {
        let saved = saved();
        let mut entry = template();
        entry.minimum_resources.cpus = positive(8);
        let catalog = TemplateCatalog::new(vec![entry]).unwrap();
        assert_eq!(
            saved.resolve_launch(&name(PROFILE), &capabilities(), &catalog),
            Err(SandboxError::Capability(CPU_CAPABILITY))
        );
        let mut entry = template();
        entry.workcell_compatible = false;
        let catalog = TemplateCatalog::new(vec![entry]).unwrap();
        assert_eq!(
            saved.resolve_launch(&name(PROFILE), &capabilities(), &catalog),
            Err(SandboxError::Capability(WORKCELL_CAPABILITY))
        );
        let mut entry = template();
        entry.revision = Revision::parse(&format!("sha256:{}", "b".repeat(64))).unwrap();
        let catalog = TemplateCatalog::new(vec![entry]).unwrap();
        assert_eq!(
            saved.resolve_launch(&name(PROFILE), &capabilities(), &catalog),
            Err(SandboxError::Capability(TEMPLATE_CAPABILITY))
        );
        let mut draft = saved.draft();
        draft.networks.get_mut(&name(NETWORK)).unwrap().tls_mode = TlsMode::Mitm;
        let mitm = SavedSandboxes::new(draft).unwrap();
        let catalog = TemplateCatalog::new(vec![template()]).unwrap();
        assert_eq!(
            mitm.resolve_launch(&name(PROFILE), &capabilities(), &catalog),
            Err(SandboxError::Capability(TLS_CAPABILITY))
        );
        let mut entry = template();
        entry.guest_ca = true;
        let catalog = TemplateCatalog::new(vec![entry]).unwrap();
        mitm.resolve_launch(&name(PROFILE), &capabilities(), &catalog)
            .unwrap();
    }

    #[test]
    fn changed_provider_invalidates_capability_inputs() {
        let mut draft = draft();
        draft
            .providers
            .get_mut(&name(PROVIDER))
            .unwrap()
            .api_endpoint = SandboxOrigin::parse("http://127.0.0.1:3001").unwrap();
        let changed = SavedSandboxes::new(draft).unwrap();
        assert_eq!(
            changed.resolve_launch(&name(PROFILE), &capabilities(), &catalog()),
            Err(SandboxError::Capability(PROVIDER_CAPABILITY))
        );
    }

    #[test]
    fn unrestricted_networking_needs_explicit_profile_and_capabilities() {
        let mut draft = draft();
        draft.networks.insert(
            name(NETWORK),
            NetworkPolicy {
                enforcement: Enforcement::Off,
                ..NetworkPolicy::default()
            },
        );
        let saved = SavedSandboxes::new(draft).unwrap();
        assert_eq!(
            saved.resolve_launch(&name(PROFILE), &capabilities(), &catalog()),
            Err(SandboxError::Capability(NETWORK_CAPABILITY))
        );
        let mut capabilities = capabilities();
        capabilities.network_modes = vec![Enforcement::Off];
        capabilities.tls_modes.clear();
        assert_eq!(
            saved.resolve_launch(&name(PROFILE), &capabilities, &catalog()),
            Err(SandboxError::Capability(NETWORK_CAPABILITY))
        );
        let mut template = template();
        template.network_modes = vec![Enforcement::Off];
        let catalog = TemplateCatalog::new(vec![template]).unwrap();
        let launch = saved
            .resolve_launch(&name(PROFILE), &capabilities, &catalog)
            .unwrap();
        assert_eq!(
            launch.configuration().network.value().enforcement,
            Enforcement::Off
        );
    }

    #[test]
    fn capability_documents_reject_unknown_claims() {
        let mut claims = serde_json::to_value(capabilities()).unwrap();
        claims["port_rules"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ProviderCapabilities>(claims).is_err());
        let mut entry = serde_json::to_value(template()).unwrap();
        entry["host_image_path"] = serde_json::json!("/host/image.qcow2");
        assert!(serde_json::from_value::<TemplateEntry>(entry).is_err());
    }

    #[test_case("/workspace")]
    #[test_case("/workspace/snapshots")]
    #[test_case("/")]
    #[test_case("relative")]
    #[test_case("/safe/../workspace")]
    fn rejects_unsafe_catalog_roots(root: &str) {
        let mut entry = template();
        entry.snapshot_root = root.into();
        assert!(TemplateCatalog::new(vec![entry]).is_err());
    }

    #[test]
    fn catalog_requires_unambiguous_immutable_entries() {
        assert!(TemplateCatalog::new(vec![template(), template()]).is_err());
        for value in ["latest", "sha256:abc", "/host/image.qcow2"] {
            assert!(Revision::parse(value).is_err());
        }
        let mut new_revision = template();
        new_revision.revision = Revision::parse(&format!("sha256:{}", "b".repeat(64))).unwrap();
        assert_eq!(
            TemplateCatalog::new(vec![template(), new_revision])
                .unwrap()
                .entries()
                .count(),
            2
        );
    }
}
