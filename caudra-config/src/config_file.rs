use std::io;
use std::path::{Path, PathBuf};

use caudra_storage::paths;
use caudra_storage::sessions::SOURCE_CODE_TOOL_RENAMES;
use serde::Deserialize;
use serde::de::IntoDeserializer;
use thiserror::Error;
use toml::de::{DeTable, Deserializer as TomlDeserializer, Error as TomlError};

use crate::config_version::{CONFIG_VERSION_KEY, ConfigVersion};
use crate::experimental::FeatureFlags;
use crate::interpolation::{self, ConfigTextError, InterpolationError, Lookup};
use crate::migration::{self, ArrayRename, ConfigFormat, MigrationError, ReadError, Step, Upgrade};
use crate::{CODE_TOOL_NAME, PROJECT_DIR, RawConfig};

pub const CONFIG_FILE: &str = "caudra.toml";
pub const CONFIG_VERSION: u32 = 2;
pub const CONFIG_FORMAT: ConfigFormat = ConfigFormat {
    file: CONFIG_FILE,
    latest: CONFIG_VERSION,
    steps: &[Step {
        from: 1,
        why: None,
        apply: upgrade_from_1,
    }],
    requires_version: false,
};
pub const INIT_LUA_FILE: &str = "init.lua";
const EXPERIMENTAL_KEY: &str = "experimental";
const CODE_LINES_WHY: &str = "the `code` tool now runs plain computation";
pub(crate) const SOURCE_CODE_WHY: &str =
    "the code-graph tools are now `source_code_context` and `source_code_refs`";
const DEFER_BUILTIN_TOOLS_WHY: &str =
    "the tools it deferred are now `code` modules (`image`, `host`, `workflow`, `automation`)";
const TOOL_SEARCH_WHY: &str = "no tool is loaded on demand any more";
const ALLOW_REMOTE_WHY: &str =
    "configuring a decision endpoint is now the consent to send decision context to it";
const DECISION_ENGINE_WHY: &str = "the decision engine is no longer experimental";
const CONTINUATION_TURNS_WHY: &str =
    "truncation is now a steering rule, limited by `agent.steering.rules.truncation.max_attempts`";
const CODE_REQUIRED_WHY: &str = "`code`, which replaced `python_execution` and `mcp`, cannot be disabled; turn off a module's tool group instead";
/// `code` and the tools it replaced, which could be disabled while they were
/// tools of their own.
const UNDISABLEABLE_TOOLS: [&str; 3] = ["python_execution", "mcp", CODE_TOOL_NAME];

#[derive(Debug, Error)]
pub enum ConfigFileError {
    #[error("resolve the config directory: {0}")]
    ConfigDir(#[source] io::Error),
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error("invalid {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: Box<TomlError>,
    },
    #[error("invalid {}: {source}", path.display())]
    Reference {
        path: PathBuf,
        #[source]
        source: InterpolationError,
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
    pub fn parse(text: &str, lookup: Lookup<'_>) -> Result<Self, ConfigTextError> {
        let (features, settings) = parse(text, lookup)?;
        Ok(Self {
            features: features.unwrap_or_default(),
            settings,
        })
    }
}

/// A missing file means every default, including every experiment off.
pub fn load_global_config(path: &Path) -> Result<GlobalConfigFile, ConfigFileError> {
    match migration::read(path, &CONFIG_FORMAT)? {
        Some(loaded) => GlobalConfigFile::parse(&loaded.text, Lookup::Global(path))
            .map_err(|source| invalid(path, source)),
        None => Ok(GlobalConfigFile::default()),
    }
}

/// Whether `text`, the global `caudra.toml` at `path` in the latest format,
/// loads into a valid config.
pub fn validate_global(path: &Path, text: &str) -> Result<(), String> {
    GlobalConfigFile::parse(text, Lookup::Global(path))
        .map_err(|error| error.to_string())?
        .settings
        .into_config(false)
        .map(drop)
        .map_err(|error| error.to_string())
}

/// A repository must never opt its users into an experiment or read their
/// environment, so both are refused outright rather than ignored.
pub fn load_project_config(path: &Path) -> Result<RawConfig, ConfigFileError> {
    match migration::read(path, &CONFIG_FORMAT)? {
        Some(loaded) => parse_project(path, &loaded.text),
        None => Ok(RawConfig::default()),
    }
}

/// Whether `text`, the project `caudra.toml` at `path` in the latest format,
/// loads into a valid config.
pub fn validate_project(path: &Path, text: &str) -> Result<(), String> {
    parse_project(path, text)
        .map_err(|error| error.to_string())?
        .into_config(false)
        .map(drop)
        .map_err(|error| error.to_string())
}

fn parse_project(path: &Path, text: &str) -> Result<RawConfig, ConfigFileError> {
    match parse(text, Lookup::Refused).map_err(|source| invalid(path, source))? {
        (Some(_), _) => Err(ConfigFileError::ProjectExperimental {
            path: path.to_path_buf(),
        }),
        (None, settings) => Ok(settings),
    }
}

fn invalid(path: &Path, error: ConfigTextError) -> ConfigFileError {
    let path = path.to_path_buf();
    match error {
        ConfigTextError::Toml(source) => ConfigFileError::Invalid {
            path,
            source: Box::new(source),
        },
        ConfigTextError::Reference(source) => ConfigFileError::Reference { path, source },
    }
}

/// The envelope keys come off a spanned table before the settings are read,
/// so `RawConfig` keeps rejecting unknown keys and every error keeps its line.
fn parse(
    text: &str,
    lookup: Lookup<'_>,
) -> Result<(Option<FeatureFlags>, RawConfig), ConfigTextError> {
    ConfigVersion::<CONFIG_VERSION>::check_document(text)?;
    let located = |mut error: TomlError| {
        error.set_input(Some(text));
        error
    };
    let mut root = DeTable::parse(text)?;
    root.get_mut().remove(CONFIG_VERSION_KEY);
    let features = root
        .get_mut()
        .remove(EXPERIMENTAL_KEY)
        .map(|table| FeatureFlags::deserialize(table.into_deserializer()))
        .transpose()
        .map_err(located)?;
    interpolation::expand(root.get_mut(), text, lookup)?;
    let settings = RawConfig::deserialize(TomlDeserializer::from(root)).map_err(located)?;
    Ok((features, settings))
}

fn upgrade_from_1(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
    upgrade.because(CODE_LINES_WHY).rename_key(
        &["ui", "tool_output_lines"],
        "python_execution",
        CODE_TOOL_NAME,
    )?;
    upgrade.because(SOURCE_CODE_WHY).rename_key(
        &["decisions", "features"],
        "code_context",
        "source_code_context",
    )?;
    upgrade.rename_in_array(
        &["agent"],
        "disabled_tools",
        &SOURCE_CODE_TOOL_RENAMES.map(|(old, new)| (old, ArrayRename::To(new))),
    );
    upgrade.because(CODE_REQUIRED_WHY).rename_in_array(
        &["agent"],
        "disabled_tools",
        &UNDISABLEABLE_TOOLS.map(|tool| (tool, ArrayRename::Retire)),
    );
    upgrade
        .because(DEFER_BUILTIN_TOOLS_WHY)
        .retire_key(&["agent"], "defer_builtin_tools");
    upgrade
        .because(CONTINUATION_TURNS_WHY)
        .retire_key(&["agent"], "max_continuation_turns");
    upgrade
        .because(TOOL_SEARCH_WHY)
        .retire_key(&["decisions", "features"], "tool_search");
    upgrade
        .because(ALLOW_REMOTE_WHY)
        .retire_key(&["decisions"], "allow_remote");
    upgrade
        .because(DECISION_ENGINE_WHY)
        .retire_key(&[EXPERIMENTAL_KEY], "decision_engine");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        CONFIG_FILE, CONFIG_FORMAT, ConfigFileError, load_global_config, load_project_config,
        project_config_path, validate_global, validate_project,
    };
    use crate::decisions::FeatureMode;
    use crate::experimental::{Feature, FeatureFlags};
    use crate::interpolation::{ENV_OPEN, FILE_OPEN};
    use crate::migration::{MigrationError, ReadError, migrate, test_stamp};
    use crate::{ENV_FILE, InboundPolicy};
    use test_case::test_case;

    const ENDPOINT_HOST: &str = "CAUDRA_TEST_CONFIG_FILE_ENDPOINT_HOST";
    /// Neither a variable nor a file has this name.
    const MISSING_THEME: &str = "CAUDRA_TEST_CONFIG_FILE_MISSING_THEME";
    const GLOBAL_SCOPE: &str = "global";
    const PROJECT_SCOPE: &str = "project";
    const LEGACY_GLOBAL: &str = "\
# My Caudra config

version = 1

[ui.tool_output_lines]
# keep code output short
python_execution = 40
bash = 30

[agent]
todo_reminder = false
defer_builtin_tools = true
# truncation retries
max_continuation_turns = 5
disabled_tools = [\"code_context\", \"python_execution\", \"webfetch\"]

[decisions]
allow_remote = true

[decisions.features]
code_context = \"shadow\" # try it first
tool_search = \"off\"

[experimental]
decision_engine = true
workflows = true
";
    const LEGACY_GLOBAL_UPGRADED: &str = "\
# My Caudra config

# Upgraded by Caudra 9.9.9 on 2026-10-20: version 1 → 2.
version = 2

[ui.tool_output_lines]
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `ui.tool_output_lines.python_execution` to `code`, because the `code` tool now runs plain computation.
#
# keep code output short
code = 40
bash = 30

[agent]
todo_reminder = false
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `agent.defer_builtin_tools`, because the tools it deferred are now `code` modules (`image`, `host`, `workflow`, `automation`).
# defer_builtin_tools = true
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `agent.max_continuation_turns`, because truncation is now a steering rule, limited by `agent.steering.rules.truncation.max_attempts`.
#
# truncation retries
# max_continuation_turns = 5
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `code_context` to `source_code_context` in `agent.disabled_tools`, because the code-graph tools are now `source_code_context` and `source_code_refs`.
# Upgraded by Caudra 9.9.9 on 2026-10-20: removed `python_execution` in `agent.disabled_tools`, because `code`, which replaced `python_execution` and `mcp`, cannot be disabled; turn off a module's tool group instead.
disabled_tools = [\"source_code_context\", \"webfetch\"]

[decisions]
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `decisions.allow_remote`, because configuring a decision endpoint is now the consent to send decision context to it.
# allow_remote = true

[decisions.features]
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `decisions.features.code_context` to `source_code_context`, because the code-graph tools are now `source_code_context` and `source_code_refs`.
source_code_context = \"shadow\" # try it first
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `decisions.features.tool_search`, because no tool is loaded on demand any more.
# tool_search = \"off\"

[experimental]
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `experimental.decision_engine`, because the decision engine is no longer experimental.
# decision_engine = true
workflows = true
";
    const LEGACY_PROJECT: &str = "\
[agent]
disabled_tools = [\"code_refs\", \"mcp\", \"code\"]
";
    const LEGACY_PROJECT_UPGRADED: &str = "\
# Added by Caudra 9.9.9 on 2026-10-20: the format version of this file, which Caudra now requires.
version = 2

[agent]
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `code_refs` to `source_code_refs` in `agent.disabled_tools`, because the code-graph tools are now `source_code_context` and `source_code_refs`.
# Upgraded by Caudra 9.9.9 on 2026-10-20: removed `mcp` and removed `code` in `agent.disabled_tools`, because `code`, which replaced `python_execution` and `mcp`, cannot be disabled; turn off a module's tool group instead.
disabled_tools = [\"source_code_refs\"]
";
    const EVERY_FLAG: &str = "[experimental]\nworkflows = true\nsandboxes = true\nremote_workcell = true\nlua_plugins = true\ncross_session_messaging = true\nautomations = true\nautopilot = true\n";

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
    #[test_case("version = 2\n[experimental]\n", FeatureFlags::NONE; "empty_table")]
    #[test_case("[experimental]\nworkflows = false\n", FeatureFlags::NONE; "explicit_false")]
    #[test_case("[experimental]\nlua_plugins = true\n", FeatureFlags::NONE.with(Feature::LuaPlugins); "one_flag")]
    #[test_case("[experimental]\ncross_session_messaging = false\n", FeatureFlags::NONE; "messaging_false")]
    #[test_case("[experimental]\ncross_session_messaging = true\n", FeatureFlags::NONE.with(Feature::CrossSessionMessaging); "messaging_only")]
    #[test_case("[experimental]\nworkflows = true\ncross_session_messaging = false\n", FeatureFlags::NONE.with(Feature::Workflows); "messaging_independent_of_workflows")]
    #[test_case("[experimental]\nautomations = false\n", FeatureFlags::NONE; "automations_false")]
    #[test_case("[experimental]\nautomations = true\n", FeatureFlags::NONE.with(Feature::Automations); "automations_alone_enables_no_messaging_or_workflows")]
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
            "version = 2\nalways_fast = true\n[experimental]\nworkflows = true\n[ui]\nscrollbar = false\n",
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
    #[test_case("version = 2\n[experimental]\nworkflows = 1\n", "line 3"; "non_bool_flag")]
    #[test_case("version = 2\nexperimental = true\n", "line 2"; "flag_table_not_a_table")]
    #[test_case("version = 2\n[ui]\nsho_thinking = true\n", "line 3"; "unknown_setting_keeps_its_line")]
    fn invalid_global_files_are_refused(contents: &str, expected: &str) {
        let (_dir, path) = write(contents);
        let error = load_global_config(&path).unwrap_err();
        assert!(matches!(error, ConfigFileError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case("version = 3\n", "newer"; "newer_version")]
    #[test_case("version = 0\n", "version"; "invalid_version")]
    #[test_case("[ui\n", "line 1"; "syntax_error")]
    fn unreadable_formats_are_refused_with_the_path(contents: &str, expected: &str) {
        let (_dir, path) = write(contents);
        let error = load_global_config(&path).unwrap_err();
        assert!(
            matches!(error, ConfigFileError::Read(ReadError::Invalid { .. })),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains(expected), "{message}");
    }

    #[test_case("[experimental]\n"; "empty_table")]
    #[test_case(EVERY_FLAG; "enabled_flags")]
    #[test_case("[experimental]\ncross_session_messaging = true\n"; "messaging_enabled")]
    #[test_case("[experimental]\ncross_session_messaging = false\n"; "messaging_disabled")]
    #[test_case("[experimental]\nautomations = true\n"; "automations_enabled")]
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
        fs::write(&path, "version = 2\n[agent]\ntodo_reminder = false\n").unwrap();
        assert_eq!(
            load_project_config(&path).unwrap().agent.todo_reminder,
            Some(false)
        );
    }

    #[test]
    fn global_settings_read_the_environment() {
        let (dir, path) = write(&format!(
            "[telemetry]\nendpoint = \"https://{{env:{ENDPOINT_HOST}}}/v1\"\n"
        ));
        fs::write(
            dir.path().join(ENV_FILE),
            format!("{ENDPOINT_HOST}=otel.test\n"),
        )
        .unwrap();
        let settings = load_global_config(&path).unwrap().settings;
        assert_eq!(
            settings.telemetry.endpoint.as_deref(),
            Some("https://otel.test/v1")
        );
    }

    #[test_case(GLOBAL_SCOPE, ENV_OPEN; "global_unset")]
    #[test_case(PROJECT_SCOPE, ENV_OPEN; "project_refused")]
    #[test_case(GLOBAL_SCOPE, FILE_OPEN; "global_missing_file")]
    #[test_case(PROJECT_SCOPE, FILE_OPEN; "project_file_refused")]
    fn reference_errors_name_the_file_and_line(scope: &str, open: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = project_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("version = 2\n[ui]\ntheme = \"{open}{MISSING_THEME}}}\"\n"),
        )
        .unwrap();
        let error = if scope == GLOBAL_SCOPE {
            load_global_config(&path).unwrap_err()
        } else {
            load_project_config(&path).unwrap_err()
        };
        assert!(
            matches!(error, ConfigFileError::Reference { .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains(MISSING_THEME), "{message}");
    }

    /// Every load and reload reads the file upgraded in memory; only the
    /// startup upgrade writes it.
    #[test_case(GLOBAL_SCOPE; "global")]
    #[test_case(PROJECT_SCOPE; "project")]
    fn a_load_never_writes_the_upgrade_back(scope: &str) {
        const UNSTAMPED: &str = "[agent]\ntodo_reminder = false\n";
        let dir = tempfile::tempdir().unwrap();
        let path = project_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, UNSTAMPED).unwrap();

        let settings = if scope == GLOBAL_SCOPE {
            load_global_config(&path).unwrap().settings
        } else {
            load_project_config(&path).unwrap()
        };

        assert_eq!(settings.agent.todo_reminder, Some(false));
        assert_eq!(fs::read_to_string(&path).unwrap(), UNSTAMPED);
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            load_global_config(&path),
            Err(ConfigFileError::Read(ReadError::Io { .. }))
        ));
    }

    #[test]
    fn a_legacy_global_file_upgrades_to_text_that_loads() {
        let (_dir, path) = write(LEGACY_GLOBAL);

        let migrated = migrate(LEGACY_GLOBAL, &CONFIG_FORMAT, &test_stamp()).unwrap();
        let text = migrated.text.unwrap();
        let loaded = load_global_config(&path).unwrap();
        let config = loaded.settings.into_config(false).unwrap();

        assert_eq!(text, LEGACY_GLOBAL_UPGRADED);
        assert_eq!(validate_global(&path, &text), Ok(()));
        assert_eq!(migrated.changes.len(), 9);
        assert_eq!(config.ui.tool_output_lines.get("code"), 40);
        assert_eq!(
            config.agent.disabled_tools,
            ["source_code_context", "webfetch"]
        );
        assert_eq!(
            config.decisions.features.source_code_context,
            FeatureMode::Shadow
        );
        assert_eq!(loaded.features, FeatureFlags::NONE.with(Feature::Workflows));
    }

    #[test]
    fn a_legacy_project_file_upgrades_to_text_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = project_config_path(dir.path());

        let text = migrate(LEGACY_PROJECT, &CONFIG_FORMAT, &test_stamp())
            .unwrap()
            .text
            .unwrap();

        assert_eq!(text, LEGACY_PROJECT_UPGRADED);
        assert_eq!(validate_project(&path, &text), Ok(()));
    }

    #[test]
    fn the_old_and_new_code_budget_together_are_a_conflict() {
        const BOTH: &str = "[ui.tool_output_lines]\npython_execution = 1\ncode = 2\n";

        assert_eq!(
            migrate(BOTH, &CONFIG_FORMAT, &test_stamp()),
            Err(MigrationError::Conflict {
                file: CONFIG_FILE,
                old: "ui.tool_output_lines.python_execution".to_owned(),
                new: "ui.tool_output_lines.code".to_owned(),
            })
        );
    }
}
