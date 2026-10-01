use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use caudra_storage::paths;
use serde::Deserialize;
use serde::de::IntoDeserializer;
use thiserror::Error;
use toml::de::{DeTable, Deserializer as TomlDeserializer, Error as TomlError};

use crate::config_version::ConfigVersion;
use crate::experimental::FeatureFlags;
use crate::{PROJECT_DIR, RawConfig};

pub const CONFIG_FILE: &str = "caudra.toml";
pub const CONFIG_VERSION: u32 = 1;
pub const INIT_LUA_FILE: &str = "init.lua";
const VERSION_KEY: &str = "version";
const EXPERIMENTAL_KEY: &str = "experimental";

#[derive(Debug, Error)]
pub enum ConfigFileError {
    #[error("resolve the config directory: {0}")]
    ConfigDir(#[source] io::Error),
    #[error("read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: Box<TomlError>,
    },
    #[error(
        "invalid {}: [experimental] is global-only; move it to the global caudra.toml",
        path.display()
    )]
    ProjectExperimental { path: PathBuf },
}

/// The user-global `caudra.toml`: experimental opt-ins and ordinary settings.
#[derive(Debug, Default)]
pub struct GlobalConfigFile {
    pub features: FeatureFlags,
    pub settings: RawConfig,
}

/// The global config directory, resolved without creating it, so reading
/// defaults never writes anything.
pub fn resolve_config_dir() -> Result<PathBuf, ConfigFileError> {
    paths::config_dir_path().map_err(ConfigFileError::ConfigDir)
}

pub fn global_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(CONFIG_FILE)
}

pub fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_DIR).join(CONFIG_FILE)
}

pub fn global_init_lua_path(config_dir: &Path) -> PathBuf {
    config_dir.join(INIT_LUA_FILE)
}

pub fn project_init_lua_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_DIR).join(INIT_LUA_FILE)
}

impl GlobalConfigFile {
    pub fn parse(text: &str) -> Result<Self, TomlError> {
        let (features, settings) = parse(text)?;
        Ok(Self {
            features: features.unwrap_or_default(),
            settings,
        })
    }
}

/// A missing file means every default, including every experiment off.
pub fn load_global_config(path: &Path) -> Result<GlobalConfigFile, ConfigFileError> {
    match read(path)? {
        Some(text) => GlobalConfigFile::parse(&text).map_err(|source| invalid(path, source)),
        None => Ok(GlobalConfigFile::default()),
    }
}

/// A repository must never opt its users into an experiment, so the table is
/// refused outright rather than ignored.
pub fn load_project_config(path: &Path) -> Result<RawConfig, ConfigFileError> {
    let Some(text) = read(path)? else {
        return Ok(RawConfig::default());
    };
    match parse(&text).map_err(|source| invalid(path, source))? {
        (Some(_), _) => Err(ConfigFileError::ProjectExperimental {
            path: path.to_path_buf(),
        }),
        (None, settings) => Ok(settings),
    }
}

fn read(path: &Path) -> Result<Option<String>, ConfigFileError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigFileError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn invalid(path: &Path, source: TomlError) -> ConfigFileError {
    ConfigFileError::Invalid {
        path: path.to_path_buf(),
        source: Box::new(source),
    }
}

/// The envelope keys come off a spanned table before the settings are read,
/// so `RawConfig` keeps rejecting unknown keys and every error keeps its line.
fn parse(text: &str) -> Result<(Option<FeatureFlags>, RawConfig), TomlError> {
    ConfigVersion::<CONFIG_VERSION>::check_document(text)?;
    let located = |mut error: TomlError| {
        error.set_input(Some(text));
        error
    };
    let mut root = DeTable::parse(text)?;
    root.get_mut().remove(VERSION_KEY);
    let features = root
        .get_mut()
        .remove(EXPERIMENTAL_KEY)
        .map(|table| FeatureFlags::deserialize(table.into_deserializer()))
        .transpose()
        .map_err(located)?;
    let settings = RawConfig::deserialize(TomlDeserializer::from(root)).map_err(located)?;
    Ok((features, settings))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        CONFIG_FILE, ConfigFileError, load_global_config, load_project_config, project_config_path,
    };
    use crate::InboundPolicy;
    use crate::experimental::{Feature, FeatureFlags};
    use test_case::test_case;

    const EVERY_FLAG: &str = "[experimental]\nworkflows = true\nsandboxes = true\nremote_workcell = true\nlua_plugins = true\ndecision_engine = true\ncross_session_messaging = true\n";

    fn write(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn missing_file_is_all_defaults_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join(CONFIG_FILE);
        let loaded = load_global_config(&path).unwrap();
        assert_eq!(loaded.features, FeatureFlags::NONE);
        assert!(!path.parent().unwrap().exists());
        assert_eq!(
            load_project_config(&project_config_path(dir.path()))
                .unwrap()
                .always_fast,
            None
        );
    }

    #[test_case("", FeatureFlags::NONE; "empty_file")]
    #[test_case("version = 1\n[experimental]\n", FeatureFlags::NONE; "empty_table")]
    #[test_case("[experimental]\nworkflows = false\n", FeatureFlags::NONE; "explicit_false")]
    #[test_case("[experimental]\nlua_plugins = true\n", FeatureFlags::NONE.with(Feature::LuaPlugins); "one_flag")]
    #[test_case("[experimental]\ncross_session_messaging = false\n", FeatureFlags::NONE; "messaging_false")]
    #[test_case("[experimental]\ncross_session_messaging = true\n", FeatureFlags::NONE.with(Feature::CrossSessionMessaging); "messaging_only")]
    #[test_case("[experimental]\nworkflows = true\ncross_session_messaging = false\n", FeatureFlags::NONE.with(Feature::Workflows); "messaging_independent_of_workflows")]
    fn global_flags_resolve(contents: &str, expected: FeatureFlags) {
        let (_dir, path) = write(contents);
        assert_eq!(load_global_config(&path).unwrap().features, expected);
    }

    #[test]
    fn every_flag_parses() {
        let (_dir, path) = write(EVERY_FLAG);
        assert_eq!(
            load_global_config(&path).unwrap().features,
            FeatureFlags::all()
        );
    }

    #[test]
    fn settings_share_the_file_with_flags() {
        let (_dir, path) = write(
            "version = 1\nalways_fast = true\n[experimental]\nworkflows = true\n[ui]\nscrollbar = false\n",
        );
        let loaded = load_global_config(&path).unwrap();
        assert!(loaded.features.enabled(Feature::Workflows));
        assert_eq!(loaded.settings.always_fast, Some(true));
        assert_eq!(loaded.settings.ui.scrollbar, Some(false));
    }

    #[test_case(""; "missing_flag")]
    #[test_case("[experimental]\ncross_session_messaging = false\n"; "disabled_flag")]
    fn inbound_accept_does_not_enable_messaging(flags: &str) {
        let (_dir, path) = write(&format!("{flags}[agent.messaging]\ninbound = 'accept'\n"));
        let loaded = load_global_config(&path).unwrap();
        assert_eq!(loaded.features, FeatureFlags::NONE);
        assert_eq!(
            loaded
                .settings
                .into_config(false)
                .unwrap()
                .agent
                .messaging
                .inbound,
            InboundPolicy::Accept,
        );
    }

    #[test_case("[experimental]\nworkflow = true\n", "unknown field `workflow`"; "unknown_flag")]
    #[test_case("[experimental]\nworkflows = 1\n", "line 2"; "non_bool_flag")]
    #[test_case("experimental = true\n", "line 1"; "flag_table_not_a_table")]
    #[test_case("[ui]\nsho_thinking = true\n", "line 2"; "unknown_setting_keeps_its_line")]
    #[test_case("version = 2\n", "newer"; "newer_version")]
    #[test_case("version = 0\n", "version"; "invalid_version")]
    #[test_case("[ui\n", "line 1"; "syntax_error")]
    fn invalid_global_files_are_refused(contents: &str, expected: &str) {
        let (_dir, path) = write(contents);
        let error = load_global_config(&path).unwrap_err();
        assert!(matches!(error, ConfigFileError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case("[experimental]\n"; "empty_table")]
    #[test_case(EVERY_FLAG; "enabled_flags")]
    #[test_case("[experimental]\ncross_session_messaging = true\n"; "messaging_enabled")]
    #[test_case("[experimental]\ncross_session_messaging = false\n"; "messaging_disabled")]
    fn project_files_cannot_hold_experiments(contents: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = project_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        assert!(matches!(
            load_project_config(&path),
            Err(ConfigFileError::ProjectExperimental { .. })
        ));
    }

    #[test]
    fn project_settings_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = project_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "version = 1\n[agent]\ntodo_reminder = false\n").unwrap();
        assert_eq!(
            load_project_config(&path).unwrap().agent.todo_reminder,
            Some(false)
        );
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            load_global_config(&path),
            Err(ConfigFileError::Read { .. })
        ));
    }
}
