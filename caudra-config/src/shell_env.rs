//! `[agent.shell_env]`: which host variables local shell commands receive beyond the fixed
//! allowlist, and the writer that edits it in the global `caudra.toml`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};
use workcell_tool_contract::shell_env::{
    EnvPattern, EnvPatternError, is_shell_startup_variable, is_valid_variable_name,
};

use crate::global_file::{GlobalDocument, GlobalFileError};
use crate::toml_document::set_value;
use crate::{ConfigError, ConfigField, ConfigValue, shell_visible_environment};

const AGENT_KEY: &str = "agent";
const SHELL_ENV_KEY: &str = "shell_env";
const PASS_KEY: &str = "pass";
const SET_KEY: &str = "set";
const PASS_PATH: &str = "agent.shell_env.pass";
const SET_PATH: &str = "agent.shell_env.set";
const UNEXPECTED_LAYOUT: &str =
    "[agent.shell_env] must hold `pass` as an array and `set` as a table";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShellEnvError {
    #[error(transparent)]
    Pattern(#[from] EnvPatternError),
    #[error(
        "a bare * would forward every host variable, provider keys included; list names or NAME_* prefixes"
    )]
    Everything,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ShellEnvFileConfig {
    pub pass: Option<Vec<String>>,
    pub set: Option<BTreeMap<String, String>>,
}

impl ShellEnvFileConfig {
    pub(crate) fn merge(&mut self, overlay: Self) {
        if overlay.pass.is_some() {
            self.pass = overlay.pass;
        }
        if overlay.set.is_some() {
            self.set = overlay.set;
        }
    }
}

/// What local shell commands receive beyond the fixed allowlist. Only the global config sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ShellEnvConfig {
    pub pass: Vec<EnvPattern>,
    pub set: BTreeMap<String, String>,
}

impl ShellEnvConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: PASS_KEY,
            ty: "string[]",
            default: ConfigValue::Toml("[]"),
            min: None,
            max: None,
            env: None,
            description: "Host variables forwarded to local shell commands: exact names or NAME_* prefixes, never a bare *. Values come from Caudra's environment and the global .env, never a project .env",
        },
        ConfigField {
            name: SET_KEY,
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            description: "Values set for local shell commands; {env:NAME} reads Caudra's environment and {file:PATH} a file. Wins over pass",
        },
    ];

    pub(crate) fn from_file(file: ShellEnvFileConfig) -> Result<Self, ConfigError> {
        let pass = file
            .pass
            .unwrap_or_default()
            .iter()
            .map(|entry| {
                forwarded_pattern(entry).map_err(|source| ConfigError::ShellEnv {
                    key: PASS_PATH.into(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?;
        let set = file.set.unwrap_or_default();
        for name in set.keys() {
            check_set_name(name).map_err(|source| ConfigError::ShellEnv {
                key: format!("{SET_PATH}.{name}"),
                source,
            })?;
        }
        Ok(Self { pass, set })
    }
}

fn forwarded_pattern(entry: &str) -> Result<EnvPattern, ShellEnvError> {
    let pattern = EnvPattern::parse(entry)?;
    if pattern == EnvPattern::All {
        return Err(ShellEnvError::Everything);
    }
    if let Some(name) = pattern.reserved_capture() {
        return Err(EnvPatternError::Reserved(name).into());
    }
    Ok(pattern)
}

fn check_set_name(name: &str) -> Result<(), ShellEnvError> {
    if !is_valid_variable_name(name) {
        return Err(EnvPatternError::Invalid(name.to_owned()).into());
    }
    if is_shell_startup_variable(name) {
        return Err(EnvPatternError::Reserved(name.to_owned()).into());
    }
    Ok(())
}

/// The variables a local shell command receives on top of its allowlist, in the order to apply
/// them: forwarded host variables, then `set`, so a set value wins.
pub fn shell_env_overlay(config: &ShellEnvConfig) -> Vec<(String, OsString)> {
    let mut overlay: Vec<(String, OsString)> = if config.pass.is_empty() {
        Vec::new()
    } else {
        shell_visible_environment()
            .into_iter()
            .filter(|(name, _)| {
                !is_shell_startup_variable(name)
                    && config.pass.iter().any(|pattern| pattern.matches(name))
            })
            .collect()
    };
    overlay.extend(
        config
            .set
            .iter()
            .map(|(name, value)| (name.clone(), OsString::from(value))),
    );
    overlay
}

pub type ShellEnvFileError = GlobalFileError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnvEdit {
    AddPass(EnvPattern),
    RemovePass(String),
    SetValue { name: String, value: String },
    RemoveValue(String),
}

/// `[agent.shell_env]` of the global `caudra.toml` as written, `{env:NAME}` and `{file:PATH}`
/// references unexpanded, with the revision an edit commits against.
#[derive(Debug, Clone)]
pub struct GlobalShellEnvFile {
    file: GlobalDocument,
    pass: Vec<String>,
    set: BTreeMap<String, String>,
}

impl GlobalShellEnvFile {
    /// A missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self, ShellEnvFileError> {
        GlobalDocument::load(path).map(Self::read)
    }

    pub fn path(&self) -> &Path {
        &self.file.path
    }

    pub fn pass(&self) -> &[String] {
        &self.pass
    }

    pub fn set(&self) -> &BTreeMap<String, String> {
        &self.set
    }

    /// Commits `edit` only while the file still holds what [`Self::load`] read, and only when
    /// the result loads as a valid global config.
    pub fn apply(&self, edit: ShellEnvEdit) -> Result<Self, ShellEnvFileError> {
        let mut document = self.file.document.clone();
        edit_document(&mut document, edit).map_err(|()| ShellEnvFileError::Invalid {
            path: self.file.path.clone(),
            message: UNEXPECTED_LAYOUT.into(),
        })?;
        self.file.commit(&document).map(Self::read)
    }

    fn read(file: GlobalDocument) -> Self {
        let section = written_section(&file.document);
        let pass = section
            .and_then(|section| section.get(PASS_KEY))
            .and_then(Item::as_array)
            .map(|pass| {
                pass.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let set = section
            .and_then(|section| section.get(SET_KEY))
            .and_then(Item::as_table_like)
            .map(|set| {
                set.iter()
                    .filter_map(|(name, value)| Some((name.to_owned(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Self { file, pass, set }
    }
}

fn written_section(document: &DocumentMut) -> Option<&dyn TableLike> {
    document
        .get(AGENT_KEY)?
        .as_table_like()?
        .get(SHELL_ENV_KEY)?
        .as_table_like()
}

fn written_section_mut(document: &mut DocumentMut) -> Option<&mut dyn TableLike> {
    document
        .get_mut(AGENT_KEY)?
        .as_table_like_mut()?
        .get_mut(SHELL_ENV_KEY)?
        .as_table_like_mut()
}

fn created_section(document: &mut DocumentMut) -> Result<&mut dyn TableLike, ()> {
    let mut agent = Table::new();
    agent.set_implicit(true);
    document
        .entry(AGENT_KEY)
        .or_insert(Item::Table(agent))
        .as_table_like_mut()
        .ok_or(())?
        .entry(SHELL_ENV_KEY)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or(())
}

/// `Err` only for a layout the config parser would not have accepted.
fn edit_document(document: &mut DocumentMut, edit: ShellEnvEdit) -> Result<(), ()> {
    match edit {
        ShellEnvEdit::AddPass(pattern) => {
            let entry = pattern.to_string();
            let pass = created_section(document)?
                .entry(PASS_KEY)
                .or_insert(Item::Value(Value::Array(Array::new())))
                .as_array_mut()
                .ok_or(())?;
            if !pass.iter().any(|value| value.as_str() == Some(&entry)) {
                pass.push(entry);
            }
        }
        ShellEnvEdit::RemovePass(entry) => {
            if let Some(section) = written_section_mut(document) {
                let emptied = section
                    .get_mut(PASS_KEY)
                    .and_then(Item::as_array_mut)
                    .is_some_and(|pass| {
                        pass.retain(|value| value.as_str() != Some(&entry));
                        pass.is_empty()
                    });
                if emptied {
                    section.remove(PASS_KEY);
                }
            }
            prune(document);
        }
        ShellEnvEdit::SetValue { name, value } => {
            let set = created_section(document)?
                .entry(SET_KEY)
                .or_insert(Item::Value(Value::InlineTable(InlineTable::new())))
                .as_table_like_mut()
                .ok_or(())?;
            set_value(set, &name, Value::from(value));
        }
        ShellEnvEdit::RemoveValue(name) => {
            if let Some(section) = written_section_mut(document) {
                let emptied = section
                    .get_mut(SET_KEY)
                    .and_then(Item::as_table_like_mut)
                    .is_some_and(|set| {
                        set.remove(&name);
                        set.is_empty()
                    });
                if emptied {
                    section.remove(SET_KEY);
                }
            }
            prune(document);
        }
    }
    Ok(())
}

/// Drops a section the last removal emptied, unless it has a header of its own.
fn prune(document: &mut DocumentMut) {
    if let Some(agent) = document
        .get_mut(AGENT_KEY)
        .and_then(Item::as_table_like_mut)
        && agent
            .get(SHELL_ENV_KEY)
            .is_some_and(is_empty_without_header)
    {
        agent.remove(SHELL_ENV_KEY);
    }
    if document.get(AGENT_KEY).is_some_and(is_empty_without_header) {
        document.remove(AGENT_KEY);
    }
}

fn is_empty_without_header(item: &Item) -> bool {
    let has_header = item
        .as_table()
        .is_some_and(|table| !table.is_implicit() && !table.is_dotted());
    !has_header && item.as_table_like().is_some_and(TableLike::is_empty)
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::config_file::{CONFIG_FILE, load_global_config};
    use crate::{ENV_FILE, PROJECT_DIR, RawConfig, load_env_files_scoped};

    const ORIGINAL: &str = "# my settings\nversion = 1\n\n[ui]\n# keep this\ntheme = \"{env:CAUDRA_TEST_SHELL_ENV_THEME}\" # trailing\n\n[agent]\ntodo_reminder = false\n";
    const THEME_VARIABLE: &str = "CAUDRA_TEST_SHELL_ENV_THEME";
    const DOCKER_HOST: &str = "DOCKER_HOST";
    const AWS_PREFIX: &str = "AWS_*";
    const PROFILE_NAME: &str = "AWS_PROFILE";
    const PROFILE_VALUE: &str = "readonly";
    const TOKEN_NAME: &str = "TOKEN";
    const TOKEN_REFERENCE: &str = "{env:CAUDRA_TEST_SHELL_ENV_TOKEN}";
    const TOKEN_VARIABLE: &str = "CAUDRA_TEST_SHELL_ENV_TOKEN";
    const UNSET_REFERENCE: &str = "{env:CAUDRA_TEST_SHELL_ENV_NEVER_SET}";
    const UNSET_VARIABLE: &str = "CAUDRA_TEST_SHELL_ENV_NEVER_SET";
    const EXTERNAL_EDIT: &str = "# edited elsewhere\n";
    #[cfg(unix)]
    const SHARED_MODE: u32 = 0o644;
    #[cfg(unix)]
    const OWNER_MODE: u32 = 0o600;
    #[cfg(unix)]
    const MODE_MASK: u32 = 0o777;

    fn written(contents: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, contents).unwrap();
        fs::write(
            dir.path().join(ENV_FILE),
            format!("{THEME_VARIABLE}=dark\n{TOKEN_VARIABLE}=token\n"),
        )
        .unwrap();
        (dir, path)
    }

    fn config(path: &Path) -> ShellEnvConfig {
        load_global_config(path)
            .unwrap()
            .settings
            .into_config(false)
            .unwrap()
            .agent
            .shell_env
    }

    fn pattern(text: &str) -> EnvPattern {
        EnvPattern::parse(text).unwrap()
    }

    fn shell_env_layer(source: &str) -> RawConfig {
        toml::from_str(&format!("[agent.shell_env]\n{source}")).unwrap()
    }

    #[test]
    fn edits_keep_everything_else_byte_identical() {
        let (_dir, path) = written(ORIGINAL);
        let loaded = GlobalShellEnvFile::load(&path).unwrap();
        let edited = loaded
            .apply(ShellEnvEdit::AddPass(pattern(DOCKER_HOST)))
            .unwrap()
            .apply(ShellEnvEdit::SetValue {
                name: TOKEN_NAME.into(),
                value: TOKEN_REFERENCE.into(),
            })
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(ORIGINAL), "{text}");
        assert_eq!(edited.pass(), [DOCKER_HOST]);
        assert_eq!(edited.set()[TOKEN_NAME], TOKEN_REFERENCE);
        let reloaded = GlobalShellEnvFile::load(&path).unwrap();
        assert_eq!(reloaded.set()[TOKEN_NAME], TOKEN_REFERENCE);
        assert_eq!(config(&path).set[TOKEN_NAME], "token");

        edited
            .apply(ShellEnvEdit::RemovePass(DOCKER_HOST.into()))
            .unwrap()
            .apply(ShellEnvEdit::RemoveValue(TOKEN_NAME.into()))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(ORIGINAL), "{text}");
        assert_eq!(config(&path), ShellEnvConfig::default());
    }

    #[test]
    fn adds_and_removes_pass_and_set_entries() {
        let (_dir, path) = written(ORIGINAL);
        let file = GlobalShellEnvFile::load(&path)
            .unwrap()
            .apply(ShellEnvEdit::AddPass(pattern(DOCKER_HOST)))
            .unwrap()
            .apply(ShellEnvEdit::AddPass(pattern(AWS_PREFIX)))
            .unwrap()
            .apply(ShellEnvEdit::AddPass(pattern(AWS_PREFIX)))
            .unwrap()
            .apply(ShellEnvEdit::SetValue {
                name: PROFILE_NAME.into(),
                value: "default".into(),
            })
            .unwrap()
            .apply(ShellEnvEdit::SetValue {
                name: PROFILE_NAME.into(),
                value: PROFILE_VALUE.into(),
            })
            .unwrap();
        assert_eq!(file.pass(), [DOCKER_HOST, AWS_PREFIX]);
        assert_eq!(
            config(&path),
            ShellEnvConfig {
                pass: vec![pattern(DOCKER_HOST), pattern(AWS_PREFIX)],
                set: BTreeMap::from([(PROFILE_NAME.into(), PROFILE_VALUE.into())]),
            }
        );
        let file = file
            .apply(ShellEnvEdit::RemovePass(DOCKER_HOST.into()))
            .unwrap();
        assert_eq!(file.pass(), [AWS_PREFIX]);
        assert_eq!(config(&path).pass, [pattern(AWS_PREFIX)]);
    }

    #[test_case("*", "a bare *"; "everything")]
    #[test_case("BASH_*", "decides how Bash starts"; "reserved_prefix")]
    #[test_case("IFS", "decides how Bash starts"; "reserved_name")]
    fn refuses_patterns_the_host_cannot_forward(text: &str, expected: &str) {
        let (_dir, path) = written(ORIGINAL);
        let error = GlobalShellEnvFile::load(&path)
            .unwrap()
            .apply(ShellEnvEdit::AddPass(pattern(text)))
            .unwrap_err();
        assert!(
            matches!(&error, ShellEnvFileError::Invalid { message, .. } if message.contains(expected)),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[test_case("BASH_ENV", PROFILE_VALUE, "BASH_ENV"; "startup_name")]
    #[test_case("1BAD", PROFILE_VALUE, "1BAD"; "invalid_name")]
    #[test_case(PROFILE_NAME, UNSET_REFERENCE, UNSET_VARIABLE; "unset_reference")]
    fn refuses_values_the_config_would_reject(name: &str, value: &str, expected: &str) {
        let (_dir, path) = written(ORIGINAL);
        let error = GlobalShellEnvFile::load(&path)
            .unwrap()
            .apply(ShellEnvEdit::SetValue {
                name: name.into(),
                value: value.into(),
            })
            .unwrap_err();
        let message = error.to_string();
        assert!(
            matches!(error, ShellEnvFileError::Invalid { .. }) && message.contains(expected),
            "{message}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[test]
    fn a_concurrent_external_write_conflicts() {
        let (_dir, path) = written(ORIGINAL);
        let loaded = GlobalShellEnvFile::load(&path).unwrap();
        let external = format!("{EXTERNAL_EDIT}{ORIGINAL}");
        fs::write(&path, &external).unwrap();
        assert!(matches!(
            loaded.apply(ShellEnvEdit::AddPass(pattern(DOCKER_HOST))),
            Err(ShellEnvFileError::Conflict { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), external);
    }

    #[test]
    fn a_broken_file_is_refused_and_left_alone() {
        let broken = "[agent\n";
        let (_dir, path) = written(broken);
        assert!(matches!(
            GlobalShellEnvFile::load(&path),
            Err(ShellEnvFileError::Unparseable { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn creates_a_missing_file_owner_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent").join(CONFIG_FILE);
        let loaded = GlobalShellEnvFile::load(&path).unwrap();
        assert!(loaded.pass().is_empty() && loaded.set().is_empty());
        assert!(
            loaded
                .apply(ShellEnvEdit::RemovePass(DOCKER_HOST.into()))
                .is_ok()
        );
        assert!(!path.exists());
        loaded
            .apply(ShellEnvEdit::AddPass(pattern(DOCKER_HOST)))
            .unwrap();
        assert_eq!(config(&path).pass, [pattern(DOCKER_HOST)]);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & MODE_MASK,
            OWNER_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_file_mode() {
        let (_dir, path) = written(ORIGINAL);
        fs::set_permissions(&path, Permissions::from_mode(SHARED_MODE)).unwrap();
        GlobalShellEnvFile::load(&path)
            .unwrap()
            .apply(ShellEnvEdit::AddPass(pattern(DOCKER_HOST)))
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & MODE_MASK,
            SHARED_MODE
        );
    }

    #[test_case("pass = ['*']", PASS_PATH; "everything")]
    #[test_case("pass = ['BASH_FUNC_*']", PASS_PATH; "reserved_prefix")]
    #[test_case("pass = ['A*B']", PASS_PATH; "invalid_pattern")]
    #[test_case("set = { PS4 = 'x' }", "agent.shell_env.set.PS4"; "startup_set")]
    #[test_case("set = { 'A-B' = 'x' }", "agent.shell_env.set.A-B"; "invalid_set_name")]
    fn invalid_shell_env_names_its_key(source: &str, key: &str) {
        let error = shell_env_layer(source)
            .into_config(false)
            .map(drop)
            .unwrap_err();
        assert!(
            matches!(&error, ConfigError::ShellEnv { key: named, .. } if named == key),
            "{error}"
        );
    }

    #[test_case(""; "empty_table")]
    #[test_case("pass = ['DOCKER_HOST']"; "pass")]
    fn project_layers_cannot_hold_shell_env(source: &str) {
        let mut raw = RawConfig::default();
        raw.merge(shell_env_layer(source));
        raw.merge_global(RawConfig::default());
        assert!(matches!(
            raw.into_config(false),
            Err(ConfigError::ProjectShellEnv)
        ));
    }

    #[test]
    fn global_layers_replace_each_field() {
        let mut raw = shell_env_layer("pass = ['DOCKER_HOST']\nset = { AWS_PROFILE = 'a' }");
        raw.merge_global(shell_env_layer("pass = ['AWS_*']"));
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().agent.shell_env,
            ShellEnvConfig {
                pass: vec![pattern(AWS_PREFIX)],
                set: BTreeMap::from([(PROFILE_NAME.into(), "a".into())]),
            }
        );
    }

    #[test]
    fn overlay_forwards_matches_then_set_values() {
        const PREFIX: &str = "CAUDRA_TEST_OVERLAY_F*";
        const FIRST: &str = "CAUDRA_TEST_OVERLAY_FIRST";
        const EXACT: &str = "CAUDRA_TEST_OVERLAY_EXACT_ONLY";
        const ABSENT: &str = "CAUDRA_TEST_OVERLAY_ABSENT_EXACT";
        unsafe {
            std::env::set_var(FIRST, "first");
            std::env::set_var(EXACT, "exact");
            std::env::remove_var(ABSENT);
        }
        let overlay = shell_env_overlay(&ShellEnvConfig {
            pass: vec![pattern(PREFIX), pattern(EXACT), pattern(ABSENT)],
            set: BTreeMap::from([(FIRST.into(), "set".into())]),
        });
        assert_eq!(
            overlay,
            [
                (EXACT.into(), "exact".into()),
                (FIRST.into(), "first".into()),
                (FIRST.into(), "set".into()),
            ]
        );
        unsafe {
            std::env::remove_var(FIRST);
            std::env::remove_var(EXACT);
        }
    }

    #[test]
    fn project_env_values_never_reach_the_overlay() {
        const PREFIX: &str = "CAUDRA_TEST_PROVENANCE_*";
        const SHADOWED: &str = "CAUDRA_TEST_PROVENANCE_SHADOWED";
        const PROJECT_ONLY: &str = "CAUDRA_TEST_PROVENANCE_PROJECT_ONLY";
        const GLOBAL_ONLY: &str = "CAUDRA_TEST_PROVENANCE_GLOBAL_ONLY";
        let dir = TempDir::new().unwrap();
        let global = dir.path().join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join(ENV_FILE),
            format!("{SHADOWED}=global\n{GLOBAL_ONLY}=global\n"),
        )
        .unwrap();
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join(ENV_FILE),
            format!("{SHADOWED}=project\n{PROJECT_ONLY}=project\n"),
        )
        .unwrap();
        unsafe {
            for name in [SHADOWED, PROJECT_ONLY, GLOBAL_ONLY] {
                std::env::remove_var(name);
            }
        }
        load_env_files_scoped(dir.path(), Some(&global), true);
        assert_eq!(std::env::var(SHADOWED).unwrap(), "project");
        assert_eq!(
            crate::env_source(SHADOWED),
            crate::EnvSource::ProjectEnvFile
        );
        assert_eq!(
            crate::env_source(GLOBAL_ONLY),
            crate::EnvSource::GlobalEnvFile
        );

        let overlay = shell_env_overlay(&ShellEnvConfig {
            pass: vec![pattern(PREFIX)],
            set: BTreeMap::new(),
        });
        assert_eq!(
            overlay,
            [
                (GLOBAL_ONLY.into(), "global".into()),
                (SHADOWED.into(), "global".into()),
            ]
        );
        unsafe {
            for name in [SHADOWED, PROJECT_ONLY, GLOBAL_ONLY] {
                std::env::remove_var(name);
            }
        }
    }
}
