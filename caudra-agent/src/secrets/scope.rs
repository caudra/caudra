//! Moves a secret between this session and every session. The secret card and the `/env` editor
//! both call these, so each move runs in one place. A remembered secret is a file under the
//! state directory plus `[agent.shell_env] set NAME = "{file:<path>}"` in the global
//! `caudra.toml`, and the two change in an order that never leaves the reference pointing at a
//! missing file, which would fail the config load.

use std::collections::BTreeMap;
use std::path::Path;

use caudra_config::interpolation::FILE_OPEN;
use caudra_config::shell_env::{GlobalShellEnvFile, ShellEnvEdit};
use caudra_storage::StateDir;
use caudra_storage::secrets::{
    delete_remembered_secret, load_remembered_secret, remembered_path, save_remembered_secret,
};

use super::{SecretError, SecretScope, SecretValue, SessionSecrets, check_name};

const REFERENCE_CLOSE: char = '}';
const REFERENCE_FORBIDDEN: &[char] = &['{', '}'];

/// What [`remember`] does with a `set` entry under the same name that is not its own reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignEntry {
    Refuse,
    Replace,
}

/// `{file:<path>}` for the remembered file of `name`.
pub fn remembered_reference(state_dir: &StateDir, name: &str) -> Result<String, SecretError> {
    let path = remembered_path(state_dir, name)?;
    match path.to_str() {
        Some(text) if !text.contains(REFERENCE_FORBIDDEN) && text.trim() == text => {
            Ok(format!("{FILE_OPEN}{text}{REFERENCE_CLOSE}"))
        }
        _ => Err(SecretError::UnreferenceablePath(path)),
    }
}

/// The names `set` holds as remembered secrets: their value is exactly their own reference.
pub fn remembered_names(state_dir: &StateDir, set: &BTreeMap<String, String>) -> Vec<String> {
    set.keys()
        .filter(|name| is_remembered(state_dir, set, name))
        .cloned()
        .collect()
}

fn is_remembered(state_dir: &StateDir, set: &BTreeMap<String, String>, name: &str) -> bool {
    remembered_reference(state_dir, name)
        .is_ok_and(|reference| set.get(name).is_some_and(|value| *value == reference))
}

/// Moves the session's `name` to every session: writes the remembered file, points `set` at it,
/// then drops the session copy, whose shell commands read the file until the config reloads.
/// Refuses with [`SecretError::ForeignEntry`] when `set` already holds another value for `name`,
/// unless `foreign` is [`ForeignEntry::Replace`].
pub fn remember(
    secrets: &SessionSecrets,
    config: &Path,
    name: &str,
    foreign: ForeignEntry,
) -> Result<(), SecretError> {
    check_name(name)?;
    let value = secrets
        .value(name)
        .ok_or_else(|| SecretError::NotFound(name.to_owned()))?;
    let state_dir = secrets.state_dir();
    let reference = remembered_reference(state_dir, name)?;
    let file = GlobalShellEnvFile::load(config)?;
    let current = file.set().get(name);
    let owned = current == Some(&reference);
    if current.is_some() && !owned && foreign == ForeignEntry::Refuse {
        return Err(SecretError::ForeignEntry(name.to_owned()));
    }
    save_remembered_secret(state_dir, name, value.expose())?;
    if !owned
        && let Err(error) = file.apply(ShellEnvEdit::SetValue {
            name: name.to_owned(),
            value: reference,
        })
    {
        delete_remembered_secret(state_dir, name)?;
        return Err(error.into());
    }
    secrets.note_remembered(name);
    secrets.remove(name)?;
    Ok(())
}

/// Moves the remembered `name` back to this session alone, with a fresh lifetime: the vault gets
/// the value, then the `set` entry goes, then the file.
pub fn forget_to_session(
    secrets: &SessionSecrets,
    config: &Path,
    name: &str,
) -> Result<(), SecretError> {
    let state_dir = secrets.state_dir();
    let value = load_remembered_secret(state_dir, name)?
        .ok_or_else(|| SecretError::NotFound(name.to_owned()))?;
    secrets.set(name, SecretValue::new(value))?;
    let file = GlobalShellEnvFile::load(config)?;
    if is_remembered(state_dir, file.set(), name) {
        file.apply(ShellEnvEdit::RemoveValue(name.to_owned()))?;
    }
    delete_remembered_secret(state_dir, name)?;
    Ok(())
}

/// Gives `name` a new value in `scope`. A remembered secret keeps its reference; only its file
/// changes, which running sessions read at their next config load.
pub fn replace(
    secrets: &SessionSecrets,
    config: &Path,
    name: &str,
    scope: SecretScope,
    value: SecretValue,
) -> Result<(), SecretError> {
    match scope {
        SecretScope::Session => secrets.set(name, value),
        SecretScope::Remembered => {
            check_name(name)?;
            value.check()?;
            let state_dir = secrets.state_dir();
            if !is_remembered(state_dir, GlobalShellEnvFile::load(config)?.set(), name) {
                return Err(SecretError::NotFound(name.to_owned()));
            }
            save_remembered_secret(state_dir, name, value.expose())?;
            Ok(())
        }
    }
}

/// Deletes `name` from `scope`. A remembered secret loses its `set` entry before its file.
pub fn delete(
    secrets: &SessionSecrets,
    config: &Path,
    name: &str,
    scope: SecretScope,
) -> Result<(), SecretError> {
    let found = match scope {
        SecretScope::Session => secrets.remove(name)?,
        SecretScope::Remembered => {
            let state_dir = secrets.state_dir();
            let file = GlobalShellEnvFile::load(config)?;
            let referenced = is_remembered(state_dir, file.set(), name);
            if referenced {
                file.apply(ShellEnvEdit::RemoveValue(name.to_owned()))?;
            }
            delete_remembered_secret(state_dir, name)? || referenced
        }
    };
    if found {
        Ok(())
    } else {
        Err(SecretError::NotFound(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use caudra_config::config_file::{CONFIG_FILE, CONFIG_VERSION, load_global_config};
    use caudra_storage::id::CaudraId;
    use caudra_storage::secrets::is_remembered_secret;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::secrets::SecretVaults;

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const VALUE: &str = "secret-canary-4711";
    const REPLACED: &str = "secret-canary-0815";
    const FOREIGN: &str = "configured-elsewhere";

    struct Fixture {
        _temp: TempDir,
        config: PathBuf,
        secrets: SessionSecrets,
    }

    impl Fixture {
        fn new(set: Option<&str>) -> Self {
            let temp = TempDir::new().unwrap();
            let config = temp.path().join(CONFIG_FILE);
            let mut text = format!("version = {CONFIG_VERSION}\n");
            if let Some(value) = set {
                text.push_str(&format!(
                    "\n[agent.shell_env]\nset = {{ {NAME} = \"{value}\" }}\n"
                ));
            }
            fs::write(&config, text).unwrap();
            let state = StateDir::from_path(temp.path().join("state"));
            let secrets = SecretVaults::for_session(&state, CaudraId::generate());
            Self {
                _temp: temp,
                config,
                secrets,
            }
        }

        fn saved(set: Option<&str>) -> Self {
            let fixture = Self::new(set);
            fixture
                .secrets
                .set(NAME, SecretValue::new(VALUE.to_owned()))
                .unwrap();
            fixture
        }

        fn state_dir(&self) -> &StateDir {
            self.secrets.state_dir()
        }

        fn set_entry(&self) -> Option<String> {
            GlobalShellEnvFile::load(&self.config)
                .unwrap()
                .set()
                .get(NAME)
                .cloned()
        }

        /// What a fresh config load hands shell commands under `NAME`.
        fn loaded(&self) -> Option<String> {
            load_global_config(&self.config)
                .unwrap()
                .settings
                .into_config(false)
                .unwrap()
                .agent
                .shell_env
                .set
                .get(NAME)
                .cloned()
        }

        fn session_value(&self) -> Option<String> {
            self.secrets
                .value(NAME)
                .map(|value| value.expose().to_owned())
        }

        /// What this session's shell commands receive under `NAME`, besides the config.
        fn injected(&self) -> Option<String> {
            self.secrets
                .overlay()
                .into_iter()
                .find(|(name, _)| name == NAME)
                .map(|(_, value)| value.into_string().unwrap())
        }
    }

    #[test]
    fn a_remembered_secret_reaches_this_sessions_shells_before_the_config_reloads() {
        let fixture = Fixture::saved(None);
        remember(
            &fixture.secrets,
            &fixture.config,
            NAME,
            ForeignEntry::Refuse,
        )
        .unwrap();
        assert_eq!(fixture.injected().as_deref(), Some(VALUE));

        replace(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Remembered,
            SecretValue::new(REPLACED.to_owned()),
        )
        .unwrap();
        assert_eq!(fixture.injected().as_deref(), Some(REPLACED));

        delete(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Remembered,
        )
        .unwrap();
        assert_eq!(fixture.injected(), None);
    }

    #[test]
    fn remember_then_forget_round_trips_the_value() {
        let fixture = Fixture::saved(None);

        remember(
            &fixture.secrets,
            &fixture.config,
            NAME,
            ForeignEntry::Refuse,
        )
        .unwrap();

        assert_eq!(fixture.session_value(), None);
        assert_eq!(
            fixture.set_entry(),
            Some(remembered_reference(fixture.state_dir(), NAME).unwrap())
        );
        assert_eq!(fixture.loaded().as_deref(), Some(VALUE));
        assert_eq!(
            remembered_names(
                fixture.state_dir(),
                GlobalShellEnvFile::load(&fixture.config).unwrap().set()
            ),
            [NAME]
        );

        forget_to_session(&fixture.secrets, &fixture.config, NAME).unwrap();

        assert_eq!(fixture.session_value().as_deref(), Some(VALUE));
        assert_eq!(fixture.set_entry(), None);
        assert!(!is_remembered_secret(fixture.state_dir(), NAME));
        assert_eq!(fixture.loaded(), None);
    }

    #[test_case(ForeignEntry::Refuse; "refused")]
    #[test_case(ForeignEntry::Replace; "replaced_when_confirmed")]
    fn remember_over_a_foreign_set_entry_asks_first(foreign: ForeignEntry) {
        let fixture = Fixture::saved(Some(FOREIGN));

        let remembered = remember(&fixture.secrets, &fixture.config, NAME, foreign);

        match foreign {
            ForeignEntry::Refuse => {
                assert!(matches!(remembered, Err(SecretError::ForeignEntry(name)) if name == NAME));
                assert_eq!(fixture.set_entry().as_deref(), Some(FOREIGN));
                assert_eq!(fixture.session_value().as_deref(), Some(VALUE));
                assert!(!is_remembered_secret(fixture.state_dir(), NAME));
            }
            ForeignEntry::Replace => {
                remembered.unwrap();
                assert_eq!(fixture.loaded().as_deref(), Some(VALUE));
            }
        }
    }

    #[test]
    fn replace_on_a_remembered_secret_changes_the_file_and_keeps_the_reference() {
        let fixture = Fixture::saved(None);
        remember(
            &fixture.secrets,
            &fixture.config,
            NAME,
            ForeignEntry::Refuse,
        )
        .unwrap();
        let reference = fixture.set_entry();

        replace(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Remembered,
            SecretValue::new(REPLACED.to_owned()),
        )
        .unwrap();

        assert_eq!(fixture.set_entry(), reference);
        assert_eq!(fixture.loaded().as_deref(), Some(REPLACED));
        assert_eq!(fixture.session_value(), None);
    }

    #[test]
    fn delete_on_a_remembered_secret_leaves_a_config_that_loads() {
        let fixture = Fixture::saved(None);
        remember(
            &fixture.secrets,
            &fixture.config,
            NAME,
            ForeignEntry::Refuse,
        )
        .unwrap();

        delete(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Remembered,
        )
        .unwrap();

        assert_eq!(fixture.set_entry(), None);
        assert!(!is_remembered_secret(fixture.state_dir(), NAME));
        assert_eq!(fixture.loaded(), None);
    }

    #[test]
    fn delete_on_a_session_secret_removes_it_and_a_second_delete_finds_nothing() {
        let fixture = Fixture::saved(None);

        delete(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Session,
        )
        .unwrap();

        assert_eq!(fixture.session_value(), None);
        assert!(matches!(
            delete(&fixture.secrets, &fixture.config, NAME, SecretScope::Session),
            Err(SecretError::NotFound(name)) if name == NAME
        ));
    }

    #[test]
    fn replace_on_a_name_that_is_not_remembered_writes_nothing() {
        let fixture = Fixture::new(Some(FOREIGN));

        let replaced = replace(
            &fixture.secrets,
            &fixture.config,
            NAME,
            SecretScope::Remembered,
            SecretValue::new(REPLACED.to_owned()),
        );

        assert!(matches!(replaced, Err(SecretError::NotFound(name)) if name == NAME));
        assert!(!is_remembered_secret(fixture.state_dir(), NAME));
    }
}
