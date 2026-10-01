use crate::config_file::{CONFIG_FILE, CONFIG_VERSION};
use crate::decisions::{DecisionFeatures, DecisionThresholds, DecisionsConfig, FeatureMode};
use crate::experimental::{Feature, FeatureFlags};
use crate::files::{self, CONFIG_FILES};
use crate::steering::{SteeringConfig, SteeringRule, SteeringRulesConfig};
use crate::{
    AgentConfig, ConfigValue, DEFAULT_BUILTINS, MIN_TOOL_OUTPUT_LINES, MessagingConfig,
    NATIVE_PLUGIN_OPTIONS, ProviderConfig, RetentionConfig, SnapshotsConfig, StorageConfig,
    TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig, WorktreesConfig,
};

use super::{Document, Entry, Header, Table, code_list, global_location, preamble};

const BOOL: &str = "bool";
const STRING: &str = "string";
const LINE_COUNT: &str = "usize";
const USAGE: &str = "To change a setting, copy its line into your caudra.toml under the same \
     [table] header, remove the \"#\", and set your value. A value in angle brackets, such as \
     <string>, marks a setting that has no default.";
const PROJECT_SCOPE: &str = "A project .caudra/caudra.toml takes the same settings, except \
     [experimental] and the ones marked global-only.";
const EXPERIMENTAL_ABOUT: &str = "Experimental features stay off until you turn them on, and each \
     switch is independent. Only the global caudra.toml may hold this table. Caudra reads it once \
     at startup, so a change needs a restart.";
const TOOL_OUTPUT_LINES_ABOUT: &str = "Rows of output an open tool card shows before it holds the \
     rest back. `other` covers every tool that no other key names.";
const STEERING_ABOUT: &str = "Automatic steering repairs unusable model output and can add \
     bounded guidance about repeated behavior. Each rule has its own table below.";
const RETENTION_ABOUT: &str = "`trim` and `forget` take keep policies in `restic forget` terms, \
     such as `{ keep_last = 50, keep_within = \"90d\" }`. A session is kept when any rule matches.";
const SNAPSHOTS_ABOUT: &str = "Each tool call that changes files leaves a change record, which \
     file revert undoes. A record over a limit is refused and its call runs unrecorded.";
const TELEMETRY_ABOUT: &str = "Each setting that names an environment variable gives way to it.";
const DECISIONS_ABOUT: &str = "The typed decision engine. It needs `decision_engine = true` under \
     [experimental]. Connection settings and thresholds are global-only.";
const DECISION_FEATURES_ABOUT: &str = "`off` turns a feature off, `shadow` collects predictions \
     without applying them, `advise` adds caution or suggestions, and `enforce` applies the \
     feature's own behavior. Each feature lists the modes it takes.";
const THRESHOLDS_ABOUT: &str = "Probabilities between 0 and 1. Flags trigger at or above their \
     threshold, and goal prescreening skips at or below its own.";

/// Every `caudra.toml` setting with its default, type, bounds, and
/// environment variable.
pub fn document() -> Document {
    let file = &files::CAUDRA;
    Document {
        preamble: preamble(
            file,
            [
                format!("{USAGE} {} {PROJECT_SCOPE}", global_location(file)),
                siblings(),
            ],
        ),
        version: CONFIG_VERSION,
        tables: tables(),
    }
}

fn siblings() -> String {
    let others: Vec<String> = CONFIG_FILES
        .iter()
        .filter(|file| file.name != CONFIG_FILE)
        .map(|file| format!("{} ({})", file.name, file.holds))
        .collect();
    format!(
        "caudra.toml holds the settings only you write. Other files hold what Caudra writes \
         itself, or what needs rules of its own: {}. `caudra config files` shows where each one \
         lives, and `caudra config example FILE` prints the reference of a TOML file.",
        others.join(", ")
    )
}

fn tables() -> Vec<Table> {
    let mut tables = vec![
        Table::of(Header::Root, TOP_LEVEL_FIELDS),
        Table::new(Header::Fixed("experimental".into()), experiments()).about(EXPERIMENTAL_ABOUT),
        Table::of(Header::Fixed("ui".into()), UiConfig::FIELDS),
        Table::new(
            Header::Fixed("ui.tool_output_lines".into()),
            tool_output_lines(),
        )
        .about(TOOL_OUTPUT_LINES_ABOUT),
        Table::of(Header::Fixed("agent".into()), AgentConfig::FIELDS),
        Table::of(
            Header::Fixed("agent.messaging".into()),
            MessagingConfig::FIELDS,
        ),
        Table::of(
            Header::Fixed("agent.steering".into()),
            SteeringConfig::FIELDS,
        )
        .about(STEERING_ABOUT),
    ];
    tables.extend(SteeringRulesConfig::RULES.iter().map(|rule| {
        Table::of(
            Header::Fixed(format!("agent.steering.rules.{}", rule.name)),
            SteeringRule::COMMON_FIELDS.iter().chain(rule.fields),
        )
        .about(rule.description)
    }));
    tables.extend([
        Table::of(Header::Fixed("provider".into()), ProviderConfig::FIELDS),
        Table::of(Header::Fixed("storage".into()), StorageConfig::FIELDS),
        Table::of(
            Header::Fixed("storage.retention".into()),
            RetentionConfig::FIELDS,
        )
        .about(RETENTION_ABOUT),
        Table::of(
            Header::Fixed("storage.snapshots".into()),
            SnapshotsConfig::FIELDS,
        )
        .about(SNAPSHOTS_ABOUT),
        Table::of(Header::Fixed("telemetry".into()), TelemetryConfig::FIELDS)
            .about(TELEMETRY_ABOUT),
        Table::of(Header::Fixed("worktrees".into()), WorktreesConfig::FIELDS),
        Table::of(Header::Fixed("decisions".into()), DecisionsConfig::FIELDS)
            .about(DECISIONS_ABOUT),
        Table::new(
            Header::Fixed("decisions.features".into()),
            decision_features(),
        )
        .about(DECISION_FEATURES_ABOUT),
        Table::of(
            Header::Fixed("decisions.thresholds".into()),
            DecisionThresholds::FIELDS,
        )
        .about(THRESHOLDS_ABOUT),
        Table::new(Header::Fixed("plugins".into()), Vec::new()).about(plugins_about()),
    ]);
    tables.extend(
        NATIVE_PLUGIN_OPTIONS
            .iter()
            .map(|(plugin, fields)| Table::of(Header::Fixed(format!("plugins.{plugin}")), *fields)),
    );
    tables
}

fn experiments() -> Vec<Entry> {
    Feature::ALL
        .into_iter()
        .map(|feature| {
            Entry::new(
                feature.key(),
                BOOL,
                ConfigValue::Bool(FeatureFlags::default().enabled(feature)),
                format!("Turn on {}.", feature.subject()),
            )
        })
        .collect()
}

fn tool_output_lines() -> Vec<Entry> {
    ToolOutputLines::FIELD_DEFAULTS
        .iter()
        .map(|&(name, default)| {
            let tools = ToolOutputLines::FIELD_TOOLS
                .iter()
                .find(|(field, _)| *field == name)
                .map_or(&[][..], |(_, tools)| *tools);
            Entry {
                min: Some(MIN_TOOL_OUTPUT_LINES as u64),
                ..Entry::new(
                    name,
                    LINE_COUNT,
                    ConfigValue::U64(default as u64),
                    format!("Rows for {}.", code_list(tools)),
                )
            }
        })
        .collect()
}

fn decision_features() -> Vec<Entry> {
    DecisionFeatures::ALL
        .iter()
        .map(|feature| {
            let modes: Vec<&str> = feature.modes.iter().map(FeatureMode::as_str).collect();
            Entry::new(
                feature.name,
                STRING,
                ConfigValue::Str(FeatureMode::default().as_str()),
                format!("{} Modes: {}.", feature.description, modes.join(", ")),
            )
        })
        .collect()
}

fn plugins_about() -> String {
    format!(
        "Bundled tools are on by default. Turn one off with `enabled = false` in its own table, \
         such as [plugins.websearch]. Names: {}.",
        DEFAULT_BUILTINS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::document;
    use crate::config_file::GlobalConfigFile;
    use crate::example::Render;
    use crate::experimental::FeatureFlags;
    use crate::{Config, RawConfig};
    use toml::Value;

    const MODEL: &str = "provider/model";

    fn defaults() -> Config {
        RawConfig::default().into_config(false).unwrap()
    }

    fn settings(config: &Config) -> String {
        format!(
            "{:?}",
            (
                config.always_yolo,
                config.always_auto,
                config.always_fast,
                &config.always_thinking,
                &config.ui,
                &config.agent,
                &config.provider,
                &config.storage,
                &config.telemetry,
                &config.worktrees,
                &config.decisions,
                &config.plugins,
            )
        )
    }

    #[test]
    fn the_reference_is_a_global_config_that_changes_nothing() {
        let reference = GlobalConfigFile::parse(&document().render(Render::Reference)).unwrap();
        assert_eq!(reference.features, FeatureFlags::default());
        let config = reference.settings.into_config(false).unwrap();
        assert_eq!(settings(&config), settings(&defaults()));
    }

    #[test]
    fn every_stated_default_is_the_built_in_one() {
        let live = document().render(Render::Live { defaults: true });
        let example = GlobalConfigFile::parse(&live).unwrap();
        assert_eq!(example.features, FeatureFlags::default());
        let mut config = example.settings.into_config(false).unwrap();
        let defaults = defaults();
        assert_eq!(
            serde_json::to_value(config.agent.steering.resolve(MODEL)).unwrap(),
            serde_json::to_value(defaults.agent.steering.resolve(MODEL)).unwrap()
        );
        config.agent.steering = defaults.agent.steering.clone();
        let stated = (
            &config.ui,
            &config.agent,
            &config.provider,
            &config.storage,
            &config.decisions,
        );
        let built_in = (
            &defaults.ui,
            &defaults.agent,
            &defaults.provider,
            &defaults.storage,
            &defaults.decisions,
        );
        assert_eq!(format!("{stated:?}"), format!("{built_in:?}"));
    }

    #[test]
    fn messaging_reference_lists_opt_in_and_inbound_defaults() {
        let live = document().render(Render::Live { defaults: true });
        let example: Value = toml::from_str(&live).unwrap();
        assert_eq!(
            example["experimental"]["cross_session_messaging"].as_bool(),
            Some(false)
        );
        assert_eq!(
            example["agent"]["messaging"]["inbound"].as_str(),
            Some("auto")
        );
        assert!(
            example["agent"]["messaging"]
                .get("project_inbound")
                .is_none()
        );
        assert!(
            !document()
                .render(Render::Reference)
                .contains("project_inbound")
        );
    }
}
