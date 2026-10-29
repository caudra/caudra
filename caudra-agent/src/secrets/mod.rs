//! Secrets the user hands a session's local shell commands as environment variables. The value
//! never enters chat, a tool result or the transcript: the vault keeps it, persisted owner-only
//! under `sessions/<id>/secrets/` until [`SESSION_SECRET_TTL`] runs out, and every shell command
//! of the session tree receives it. One process-wide registry hands the agent and the TUI the same
//! vault for a session.

pub mod redact;
pub mod scope;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::{
    Arc, LazyLock, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudra_config::shell_env::{ShellEnvConfig, ShellEnvFileError};
use caudra_storage::auth::now_millis;
use caudra_storage::id::CaudraId;
use caudra_storage::secrets::{
    StoredSecret, delete_session_secret, is_remembered_secret, load_session_secrets,
    save_session_secret,
};
use caudra_storage::{StateDir, StorageError};
use thiserror::Error;
use tracing::warn;
use workcell_tool_contract::shell_env::{
    BASE_ALLOWLIST, FORCED_VARIABLES, MAX_SET_VALUE_BYTES, is_server_variable,
    is_shell_startup_variable, is_valid_variable_name,
};

use crate::mcp::elicitation::MASKED_ANSWER;
use redact::Redactor;

/// How long a session secret lives after it was last saved.
pub const SESSION_SECRET_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const SESSION_SECRET_TTL_MS: u64 = SESSION_SECRET_TTL.as_millis() as u64;
const NUL: char = '\0';

type VaultKey = (PathBuf, CaudraId);

static VAULTS: LazyLock<Mutex<HashMap<VaultKey, Weak<Vault>>>> = LazyLock::new(Mutex::default);

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("{0} is not a variable name. Use letters, digits and _, and start with a letter or _")]
    InvalidName(String),
    #[error("shell commands get {0} from Caudra itself, so a secret cannot set it")]
    ReservedName(String),
    #[error("a secret needs a value")]
    EmptyValue,
    #[error("a secret holds at most {} bytes", MAX_SET_VALUE_BYTES)]
    TooLong,
    #[error("a secret cannot hold a NUL character")]
    Nul,
    #[error("no secret named {0} is saved here")]
    NotFound(String),
    /// `remember` would replace a `[agent.shell_env] set` entry it does not own. The UI asks
    /// before calling again with [`scope::ForeignEntry::Replace`].
    #[error("[agent.shell_env] already sets {0}")]
    ForeignEntry(String),
    #[error("{} cannot be named in a {{file:…}} reference", .0.display())]
    UnreferenceablePath(PathBuf),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Config(#[from] ShellEnvFileError),
}

/// A secret's value. It has no `Display` and no `Serialize`, and `Debug` masks it, so it reaches
/// a log or a transcript only through [`Self::expose`].
#[derive(Clone)]
pub struct SecretValue(String);

impl SecretValue {
    /// Surrounding whitespace is dropped, as a `{file:…}` reference drops it.
    pub fn new(value: String) -> Self {
        match value.trim() {
            trimmed if trimmed.len() == value.len() => Self(value),
            trimmed => Self(trimmed.to_owned()),
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    fn check(&self) -> Result<(), SecretError> {
        if self.0.is_empty() {
            return Err(SecretError::EmptyValue);
        }
        if self.0.len() > MAX_SET_VALUE_BYTES {
            return Err(SecretError::TooLong);
        }
        if self.0.contains(NUL) {
            return Err(SecretError::Nul);
        }
        Ok(())
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(MASKED_ANSWER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScope {
    /// This session tree, until [`SESSION_SECRET_TTL`] runs out.
    Session,
    /// Every session, through `[agent.shell_env] set NAME = "{file:…}"`.
    Remembered,
}

/// A secret by name, never by value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretListing {
    pub name: String,
    pub scope: SecretScope,
    /// `None` for a remembered secret, which never expires.
    pub expires_at: Option<SystemTime>,
}

impl SecretListing {
    pub fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= SystemTime::now())
    }
}

/// Refuses a name a shell command cannot receive as set: one that is not a variable, one that
/// decides how Bash starts, and one Workcell already forwards or forces.
pub fn check_name(name: &str) -> Result<(), SecretError> {
    if !is_valid_variable_name(name) {
        return Err(SecretError::InvalidName(name.to_owned()));
    }
    let reserved = is_shell_startup_variable(name)
        || is_server_variable(name)
        || BASE_ALLOWLIST
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
        || FORCED_VARIABLES
            .iter()
            .any(|(forced, _)| forced.eq_ignore_ascii_case(name));
    if reserved {
        return Err(SecretError::ReservedName(name.to_owned()));
    }
    Ok(())
}

/// The registry of open vaults, keyed by state directory and root session.
pub struct SecretVaults;

impl SecretVaults {
    /// The vault of `root_session_id`, loaded from disk unless another holder has it open.
    pub fn for_session(state_dir: &StateDir, root_session_id: CaudraId) -> SessionSecrets {
        let key = (state_dir.path().to_path_buf(), root_session_id);
        let mut vaults = VAULTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(vault) = vaults.get(&key).and_then(Weak::upgrade) {
            return SessionSecrets(vault);
        }
        vaults.retain(|_, vault| vault.strong_count() > 0);
        let vault = Arc::new(Vault::load(state_dir.clone(), root_session_id));
        vaults.insert(key, Arc::downgrade(&vault));
        SessionSecrets(vault)
    }
}

/// One session tree's secrets. Clones share the vault.
#[derive(Clone)]
pub struct SessionSecrets(Arc<Vault>);

struct Vault {
    state_dir: StateDir,
    session_id: CaudraId,
    entries: RwLock<BTreeMap<String, Entry>>,
}

struct Entry {
    value: SecretValue,
    /// Unix milliseconds.
    saved_at: u64,
}

impl Entry {
    fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.saved_at) >= SESSION_SECRET_TTL_MS
    }

    fn expires_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.saved_at.saturating_add(SESSION_SECRET_TTL_MS))
    }
}

impl Vault {
    fn load(state_dir: StateDir, session_id: CaudraId) -> Self {
        let entries = load_session_secrets(&state_dir, session_id, SESSION_SECRET_TTL)
            .unwrap_or_else(|error| {
                warn!(session = %session_id, %error, "session secrets did not load");
                BTreeMap::new()
            })
            .into_iter()
            .map(|(name, stored)| {
                let entry = Entry {
                    value: SecretValue(stored.value),
                    saved_at: stored.saved_at,
                };
                (name, entry)
            })
            .collect();
        Self {
            state_dir,
            session_id,
            entries: RwLock::new(entries),
        }
    }
}

impl SessionSecrets {
    pub fn state_dir(&self) -> &StateDir {
        &self.0.state_dir
    }

    /// Saves `value` under `name` with a fresh lifetime, replacing any earlier one.
    pub fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretError> {
        check_name(name)?;
        value.check()?;
        let stored = StoredSecret {
            value: value.expose().to_owned(),
            saved_at: now_millis(),
        };
        let mut entries = self.write();
        save_session_secret(&self.0.state_dir, self.0.session_id, name, &stored)?;
        entries.insert(
            name.to_owned(),
            Entry {
                value,
                saved_at: stored.saved_at,
            },
        );
        Ok(())
    }

    /// Whether `name` was there to remove.
    pub fn remove(&self, name: &str) -> Result<bool, SecretError> {
        let mut entries = self.write();
        let deleted = delete_session_secret(&self.0.state_dir, self.0.session_id, name)?;
        Ok(entries.remove(name).is_some() || deleted)
    }

    /// Whether `name` holds a live secret.
    pub fn contains(&self, name: &str) -> bool {
        let now = now_millis();
        self.read()
            .get(name)
            .is_some_and(|entry| !entry.expired(now))
    }

    /// What a local shell command receives, after `[agent.shell_env]` so a session secret wins.
    pub fn overlay(&self) -> Vec<(String, OsString)> {
        let now = now_millis();
        self.read()
            .iter()
            .filter(|(_, entry)| !entry.expired(now))
            .map(|(name, entry)| (name.clone(), OsString::from(entry.value.expose())))
            .collect()
    }

    /// The session's secrets by name, expired ones included so an editor can show them.
    pub fn listing(&self) -> Vec<SecretListing> {
        self.read()
            .iter()
            .map(|(name, entry)| SecretListing {
                name: name.clone(),
                scope: SecretScope::Session,
                expires_at: Some(entry.expires_at()),
            })
            .collect()
    }

    /// Hides this session's secrets and the remembered ones `shell_env` resolved, each shown as
    /// `${NAME}`.
    pub fn redactor(&self, shell_env: &ShellEnvConfig) -> Redactor {
        let mut redactor = Redactor::default();
        for (name, entry) in self.read().iter() {
            redactor.hide_variable(name, entry.value.expose());
        }
        for (name, value) in &shell_env.set {
            if is_remembered_secret(&self.0.state_dir, name) {
                redactor.hide_variable(name, value);
            }
        }
        redactor
    }

    pub(crate) fn value(&self, name: &str) -> Option<SecretValue> {
        let now = now_millis();
        self.read()
            .get(name)
            .filter(|entry| !entry.expired(now))
            .map(|entry| entry.value.clone())
    }

    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Entry>> {
        self.0
            .entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Entry>> {
        self.0
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::secrets::{save_remembered_secret, session_secrets_dir};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const OTHER_NAME: &str = "HCLOUD_TOKEN";
    const VALUE: &str = "secret-canary-4711";
    const REPLACED: &str = "secret-canary-0815";
    const REMEMBERED_NAME: &str = "GH_TOKEN";
    const REMEMBERED_VALUE: &str = "remembered-canary-2342";
    const CONFIGURED_NAME: &str = "PLAIN_SETTING";
    const CONFIGURED_VALUE: &str = "configured-not-secret";

    fn state() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        (temp, state)
    }

    fn secret(value: &str) -> SecretValue {
        SecretValue::new(value.to_owned())
    }

    fn overlay_values(secrets: &SessionSecrets) -> Vec<(String, String)> {
        secrets
            .overlay()
            .into_iter()
            .map(|(name, value)| (name, value.into_string().unwrap()))
            .collect()
    }

    #[test]
    fn debug_masks_the_value() {
        assert_eq!(format!("{:?}", secret(VALUE)), MASKED_ANSWER);
    }

    #[test]
    fn a_value_loses_surrounding_whitespace() {
        assert_eq!(secret(&format!(" {VALUE}\n")).expose(), VALUE);
    }

    #[test]
    fn the_agent_and_the_ui_share_one_vault_which_outlives_neither() {
        let (_temp, state) = state();
        let session = CaudraId::generate();
        let agent = SecretVaults::for_session(&state, session);
        let ui = SecretVaults::for_session(&state, session);

        agent.set(NAME, secret(VALUE)).unwrap();

        assert!(Arc::ptr_eq(&agent.0, &ui.0));
        assert_eq!(overlay_values(&ui), [(NAME.to_owned(), VALUE.to_owned())]);
        drop((agent, ui));
        let resumed = SecretVaults::for_session(&state, session);
        assert_eq!(
            overlay_values(&resumed),
            [(NAME.to_owned(), VALUE.to_owned())]
        );
    }

    #[test]
    fn set_replaces_and_remove_deletes_the_file() {
        let (_temp, state) = state();
        let secrets = SecretVaults::for_session(&state, CaudraId::generate());
        secrets.set(NAME, secret(VALUE)).unwrap();
        secrets.set(NAME, secret(REPLACED)).unwrap();
        secrets.set(OTHER_NAME, secret(VALUE)).unwrap();

        assert_eq!(
            overlay_values(&secrets),
            [
                (NAME.to_owned(), REPLACED.to_owned()),
                (OTHER_NAME.to_owned(), VALUE.to_owned()),
            ]
        );
        assert!(secrets.remove(NAME).unwrap());
        assert!(!secrets.remove(NAME).unwrap());
        let directory = session_secrets_dir(&state, secrets.0.session_id);
        assert!(!directory.join(NAME).exists());
        assert!(directory.join(OTHER_NAME).exists());
    }

    #[test]
    fn an_expired_secret_is_listed_but_never_injected() {
        let (_temp, state) = state();
        let secrets = SecretVaults::for_session(&state, CaudraId::generate());
        secrets.set(NAME, secret(VALUE)).unwrap();
        secrets.write().get_mut(NAME).unwrap().saved_at -= SESSION_SECRET_TTL_MS;

        assert!(secrets.overlay().is_empty());
        assert!(!secrets.contains(NAME));
        let [listing] = secrets.listing().try_into().unwrap();
        assert_eq!(listing.name, NAME);
        assert_eq!(listing.scope, SecretScope::Session);
        assert!(listing.expired());
    }

    #[test_case("1TOKEN", SecretError::InvalidName("1TOKEN".into()); "not_a_variable")]
    #[test_case("BASH_ENV", SecretError::ReservedName("BASH_ENV".into()); "bash_startup")]
    #[test_case("PATH", SecretError::ReservedName("PATH".into()); "allowlisted")]
    #[test_case("NO_COLOR", SecretError::ReservedName("NO_COLOR".into()); "forced")]
    #[test_case("WORKCELL_TOKEN", SecretError::ReservedName("WORKCELL_TOKEN".into()); "server")]
    fn a_name_a_shell_cannot_receive_is_refused(name: &str, expected: SecretError) {
        let (_temp, state) = state();
        let secrets = SecretVaults::for_session(&state, CaudraId::generate());

        let refused = secrets.set(name, secret(VALUE)).unwrap_err();

        assert_eq!(refused.to_string(), expected.to_string());
        assert!(secrets.listing().is_empty());
    }

    #[test]
    fn the_redactor_hides_session_and_remembered_secrets_only() {
        let (_temp, state) = state();
        let secrets = SecretVaults::for_session(&state, CaudraId::generate());
        secrets.set(NAME, secret(VALUE)).unwrap();
        save_remembered_secret(&state, REMEMBERED_NAME, REMEMBERED_VALUE).unwrap();
        let shell_env = ShellEnvConfig {
            set: BTreeMap::from([
                (REMEMBERED_NAME.to_owned(), REMEMBERED_VALUE.to_owned()),
                (CONFIGURED_NAME.to_owned(), CONFIGURED_VALUE.to_owned()),
            ]),
            ..ShellEnvConfig::default()
        };
        let text = format!("{VALUE} {REMEMBERED_VALUE} {CONFIGURED_VALUE}");

        let shown = secrets.redactor(&shell_env).text(&text).into_owned();

        assert_eq!(
            shown,
            format!(
                "{} {} {CONFIGURED_VALUE}",
                redact::shown_variable(NAME),
                redact::shown_variable(REMEMBERED_NAME)
            )
        );
    }
}
