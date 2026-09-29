use std::fmt::Write;

use crate::config_file::CONFIG_VERSION;
use crate::decisions::{DecisionFeatures, DecisionThresholds, DecisionsConfig, FeatureMode};
use crate::experimental::{Feature, FeatureFlags};
use crate::steering::{SteeringConfig, SteeringRule, SteeringRulesConfig};
use crate::{
    AgentConfig, ConfigField, ConfigValue, DEFAULT_BUILTINS, MIN_TOOL_OUTPUT_LINES,
    NATIVE_PLUGIN_OPTIONS, ProviderConfig, RetentionConfig, SnapshotsConfig, StorageConfig,
    TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig, WorktreesConfig,
};

const COMMENT: &str = "# ";
const PARAGRAPH_BREAK: &str = "#\n";
const CODE_SPAN: char = '`';
const WIDTH: usize = 79;
const BOOL: &str = "bool";
const STRING: &str = "string";
const LINE_COUNT: &str = "usize";
const PREAMBLE: [&str; 3] = [
    "Every caudra.toml setting with its default. Each one is commented out, so this file \
     changes nothing until you edit it. `caudra config example` prints it.",
    "To change a setting, copy its line into your caudra.toml under the same [table] header, \
     remove the \"#\", and set your value. A value in angle brackets, such as <string>, marks a setting \
     that has no default. The global file is ~/.config/caudra/caudra.toml, or \
     %APPDATA%\\caudra\\caudra.toml on Windows. A project .caudra/caudra.toml takes the same \
     settings, except [experimental] and the ones marked global-only.",
    "Full reference: https://caudra.ai/docs/configuration/",
];
const EXPERIMENTAL_ABOUT: &str = "Experimental features stay off until you turn them on, and each \
     switch is independent. Only the global caudra.toml may hold this table. Caudra reads it once \
     at startup, so a change needs a restart.";
const TOOL_OUTPUT_LINES_ABOUT: &str = "Rows of output an open tool card shows before it holds the \
     rest back. `other` covers every tool that no other key names.";
const STEERING_ABOUT: &str = "Automatic steering repairs unusable model output and can add \
     bounded guidance about repeated behavior. Each rule has its own table below.";
const RETENTION_ABOUT: &str = "`trim` and `forget` take keep policies in `restic forget` terms, \
     such as `{ keep_last = 50, keep_within = \"90d\" }`. A session is kept when any rule matches.";
const TELEMETRY_ABOUT: &str = "Each setting that names an environment variable gives way to it.";
const DECISIONS_ABOUT: &str = "The typed decision engine. It needs `decision_engine = true` under \
     [experimental]. Connection settings and thresholds are global-only.";
const DECISION_FEATURES_ABOUT: &str = "`off` turns a feature off, `shadow` collects predictions \
     without applying them, `advise` adds caution or suggestions, and `enforce` applies the \
     feature's own behavior. Each feature lists the modes it takes.";
const THRESHOLDS_ABOUT: &str = "Probabilities between 0 and 1. Flags trigger at or above their \
     threshold, and goal prescreening skips at or below its own.";

/// One `[table]` of the example, in the order the file lists them.
struct Table {
    path: String,
    about: Option<String>,
    entries: Vec<Entry>,
}

/// One key: a [`ConfigField`] whose description may be built at runtime.
struct Entry {
    name: &'static str,
    ty: &'static str,
    default: ConfigValue,
    min: Option<u64>,
    max: Option<u64>,
    env: Option<&'static str>,
    description: String,
}

impl Table {
    fn new(path: impl Into<String>, about: Option<&str>, entries: Vec<Entry>) -> Self {
        Self {
            path: path.into(),
            about: about.map(str::to_owned),
            entries,
        }
    }

    fn of(path: impl Into<String>, about: Option<&str>, fields: &[ConfigField]) -> Self {
        Self::new(path, about, fields.iter().map(Entry::from).collect())
    }
}

impl Entry {
    fn new(
        name: &'static str,
        ty: &'static str,
        default: ConfigValue,
        description: String,
    ) -> Self {
        Self {
            name,
            ty,
            default,
            min: None,
            max: None,
            env: None,
            description,
        }
    }
}

impl From<&ConfigField> for Entry {
    fn from(field: &ConfigField) -> Self {
        Self {
            min: field.min,
            max: field.max,
            env: field.env,
            ..Self::new(
                field.name,
                field.ty,
                field.default,
                full_stop(field.description),
            )
        }
    }
}

/// Every `caudra.toml` setting as TOML comments that state its default, type,
/// bounds, and environment variable. Only `version` and the table headers are
/// live, so the text parses as a global config and changes nothing.
pub fn example_toml() -> String {
    render(COMMENT)
}

/// `key_prefix` goes in front of each key that has a default, which lets the
/// tests turn every one of them on.
fn render(key_prefix: &str) -> String {
    let tables = tables();
    let mut out = String::new();
    for (index, paragraph) in PREAMBLE.into_iter().enumerate() {
        if index > 0 {
            out.push_str(PARAGRAPH_BREAK);
        }
        comment(&mut out, paragraph);
    }
    let _ = writeln!(out, "\nversion = {CONFIG_VERSION}");
    for table in &tables {
        if !table.path.is_empty() {
            let _ = writeln!(out, "\n[{}]", table.path);
        }
        if let Some(about) = &table.about {
            comment(&mut out, about);
        }
        for entry in &table.entries {
            if has_own_table(&tables, &table.path, entry.name) {
                continue;
            }
            out.push('\n');
            comment(&mut out, &entry.description);
            comment(&mut out, &facts(entry));
            let _ = match entry.default.toml() {
                Some(value) => writeln!(out, "{key_prefix}{} = {value}", entry.name),
                None => writeln!(out, "{COMMENT}{} = <{}>", entry.name, entry.ty),
            };
        }
    }
    out
}

fn tables() -> Vec<Table> {
    let mut tables = vec![
        Table::of("", None, TOP_LEVEL_FIELDS),
        Table::new("experimental", Some(EXPERIMENTAL_ABOUT), experiments()),
        Table::of("ui", None, UiConfig::FIELDS),
        Table::new(
            "ui.tool_output_lines",
            Some(TOOL_OUTPUT_LINES_ABOUT),
            tool_output_lines(),
        ),
        Table::of("agent", None, AgentConfig::FIELDS),
        Table::of(
            "agent.steering",
            Some(STEERING_ABOUT),
            SteeringConfig::FIELDS,
        ),
    ];
    tables.extend(SteeringRulesConfig::RULES.iter().map(|rule| {
        let fields = SteeringRule::COMMON_FIELDS.iter().chain(rule.fields);
        Table::new(
            format!("agent.steering.rules.{}", rule.name),
            Some(rule.description),
            fields.map(Entry::from).collect(),
        )
    }));
    tables.extend([
        Table::of("provider", None, ProviderConfig::FIELDS),
        Table::of("storage", None, StorageConfig::FIELDS),
        Table::of(
            "storage.retention",
            Some(RETENTION_ABOUT),
            RetentionConfig::FIELDS,
        ),
        Table::of("storage.snapshots", None, SnapshotsConfig::FIELDS),
        Table::of("telemetry", Some(TELEMETRY_ABOUT), TelemetryConfig::FIELDS),
        Table::of("worktrees", None, WorktreesConfig::FIELDS),
        Table::of("decisions", Some(DECISIONS_ABOUT), DecisionsConfig::FIELDS),
        Table::new(
            "decisions.features",
            Some(DECISION_FEATURES_ABOUT),
            decision_features(),
        ),
        Table::of(
            "decisions.thresholds",
            Some(THRESHOLDS_ABOUT),
            DecisionThresholds::FIELDS,
        ),
        Table::new("plugins", Some(plugins_about().as_str()), Vec::new()),
    ]);
    tables.extend(
        NATIVE_PLUGIN_OPTIONS
            .iter()
            .map(|(plugin, fields)| Table::of(format!("plugins.{plugin}"), None, fields)),
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

/// A table-valued setting written as tables of its own, like `rules` under
/// `[agent.steering]`, would clash with them as an inline `{}`.
fn has_own_table(tables: &[Table], parent: &str, name: &str) -> bool {
    let path = if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    };
    tables.iter().any(|table| {
        table
            .path
            .strip_prefix(&path)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    })
}

fn facts(entry: &Entry) -> String {
    let mut facts = format!("Type: {}", entry.ty);
    let _ = match (entry.min, entry.max) {
        (Some(min), Some(max)) => write!(facts, ", {min} to {max}"),
        (Some(min), None) => write!(facts, ", at least {min}"),
        (None, Some(max)) => write!(facts, ", at most {max}"),
        (None, None) => Ok(()),
    };
    if let ConfigValue::Unset | ConfigValue::Varies(_) = entry.default {
        let _ = write!(facts, ". Default: {}", entry.default.format_default());
    }
    if let Some(env) = entry.env {
        let _ = write!(facts, ". Env: {env}");
    }
    facts.push('.');
    facts
}

fn code_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    quoted.join(", ")
}

fn full_stop(text: &str) -> String {
    if text.is_empty() || text.ends_with(['.', '!', '?']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

fn comment(out: &mut String, text: &str) {
    let mut line = String::new();
    for word in unbroken_words(text) {
        if !line.is_empty() && COMMENT.len() + line.len() + 1 + word.len() > WIDTH {
            let _ = writeln!(out, "{COMMENT}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    let _ = writeln!(out, "{COMMENT}{line}");
}

/// Words split on whitespace, except that a code span stays whole, so a
/// wrapped comment never breaks `caudra storage` across two lines.
fn unbroken_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut open_span: Option<String> = None;
    for word in text.split_whitespace() {
        let word = match open_span.take() {
            Some(span) => format!("{span} {word}"),
            None => word.to_owned(),
        };
        if word.matches(CODE_SPAN).count() % 2 == 1 {
            open_span = Some(word);
        } else {
            words.push(word);
        }
    }
    words.extend(open_span);
    words
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use toml::Table as TomlTable;

    use super::{example_toml, render, tables, unbroken_words};
    use crate::config_file::GlobalConfigFile;
    use crate::experimental::FeatureFlags;
    use crate::{Config, RawConfig};

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
    fn the_example_is_a_global_config_that_changes_nothing() {
        let example = GlobalConfigFile::parse(&example_toml()).unwrap();
        assert_eq!(example.features, FeatureFlags::default());
        let config = example.settings.into_config(false).unwrap();
        assert_eq!(settings(&config), settings(&defaults()));
    }

    #[test]
    fn every_stated_default_is_the_built_in_one() {
        let example = GlobalConfigFile::parse(&render("")).unwrap();
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
    fn every_entry_is_described_and_sets_its_own_key() {
        let root: TomlTable = render("").parse().unwrap();
        for table in tables() {
            let values = table
                .path
                .split('.')
                .filter(|key| !key.is_empty())
                .fold(&root, |parent, key| {
                    parent.get(key).and_then(|value| value.as_table()).unwrap()
                });
            for entry in &table.entries {
                assert!(
                    !entry.description.is_empty(),
                    "{}.{}",
                    table.path,
                    entry.name
                );
                if entry.default.toml().is_some() {
                    assert!(
                        values.contains_key(entry.name),
                        "{}.{}",
                        table.path,
                        entry.name
                    );
                }
            }
        }
    }

    #[test_case("run `caudra storage` now", &["run", "`caudra storage`", "now"] ; "span_with_spaces")]
    #[test_case("`0` disables it", &["`0`", "disables", "it"] ; "one_word_span")]
    #[test_case("an `unclosed span", &["an", "`unclosed span"] ; "unclosed_span")]
    fn a_code_span_wraps_as_one_word(text: &str, expected: &[&str]) {
        assert_eq!(unbroken_words(text), expected);
    }
}
