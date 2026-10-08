//! What each tab says, built from plain values so a tab can be read without
//! drawing the modal.

use caudra_agent::decisions::{DecisionFeature, endpoint_kind};
use caudra_config::ClockFormat;
use caudra_config::decisions::{DecisionThresholds, DecisionsConfig, FeatureMode};
use caudra_storage::decision_log::{DecisionEffect, DecisionStats, EndpointKind, LoggedDecision};
use caudra_storage::sessions::PermissionMode;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::{DecisionsModalContext, DecisionsScope, Logged};
use crate::clock::hms;
use crate::components::session_picker::age;
use crate::components::{escape_terminal_controls, format_integer, json_text};
use crate::theme::Theme;

const INDENT: &str = "  ";
const LABEL_COLS: usize = 14;
const COLUMN_GAP: &str = "  ";
const LIST_GAP: &str = " \u{b7} ";
const DASH: &str = "\u{2014}";
const NONE: &str = "none";
const AGE_COLS: usize = 4;
const EFFECT_COLS: usize = 9;
const LATENCY_COLS: usize = 7;
pub(super) const ERROR_MARK: &str = " \u{2717}";
pub(super) const LABEL_MARK: &str = " \u{2713}";
const ABSENT_MARK: &str = "  ";
const PERCENT: f64 = 100.0;
const MILLIS_PER_SECOND: u64 = 1_000;
const DATE_FORMAT: &str = "%Y-%m-%d ";
const ENGINE_HEADING: &str = "Engine";
const HEALTH_HEADING: &str = "Health";
const LOGGING_HEADING: &str = "Logging";
const SESSION_HEADING: &str = "Session";
const THRESHOLDS_HEADING: &str = "Thresholds";
const FEATURES_HEADING: &str = "Features";
const ACTIVITY_HEADING: &str = "Activity";
const CONFIGURE_HEADING: &str = "Configure";
const FEATURES_POINTER: &str = " (2 Features lists each)";
const FEATURES_TABLE: &str = "[decisions.features]";
const GLOBAL_CONFIG: &str = "in the global caudra.toml";
const THIS_SESSION: &str = " (this session)";
const CURRENT_MODE: &str = " (current)";
const TOTAL_LABEL: &str = "Total";
const THRESHOLD_OFF: &str = "off";
const ACTIVITY_HEADERS: [&str; 10] = [
    "Feature", "Mode", "Calls", "Errors", "p50", "p95", "Acted", "Labelled", "Agree", "Last",
];
pub(super) const REACHABLE: &str = "reachable (last attempt)";
pub(super) const OFFLINE: &str = "offline (last attempt)";
pub(super) const NOT_CHECKED: &str = "not checked yet";
pub(super) const ENGINE_CONFIGURED: &str = "configured";
pub(super) const ENGINE_OFF: &str = "off: set base_url under [decisions] in the global caudra.toml";
pub(super) const CONTENT_FLAGGED: &str = "flagged";
pub(super) const LOGGING_HINT: &str =
    "Logging is off: set log = true under [decisions] in the global caudra.toml";
pub(super) const READING: &str = "Reading decision log\u{2026}";
pub(super) const NO_DECISIONS: &str = "No decisions logged yet";
pub(super) const NO_SESSION_DECISIONS: &str = "No decisions in this session yet";
pub(super) const NO_PROJECT_DECISIONS: &str = "No decisions in this project yet";
pub(super) const SHADOW_UNLOGGED: &str =
    "Shadow mode only records answers, so with logging off it shows nothing";
pub(super) const WORKFLOW_DETAIL: &str = "Always on with a base URL; used by decide()";
/// Short lines, because the table body never wraps and a cut sentence would
/// hide its end behind the pan bar.
pub(super) const ACTIVITY_FOOTNOTE: [&str; 2] = [
    "Agreement compares raw answers with partial labels.",
    "Effects are a partial record.",
];
pub(super) const JSON_SECTIONS: [&str; 5] = ["State", "Questions", "Answers", "Meta", "Label"];

pub(super) fn overview(ctx: &DecisionsModalContext, theme: &Theme) -> Vec<Line<'static>> {
    let config = ctx.config;
    let status = ctx.status;
    let mut lines = vec![heading(ENGINE_HEADING, theme)];
    match &config.base_url {
        Some(url) => {
            lines.push(field("Status", ENGINE_CONFIGURED, theme));
            lines.push(field(
                "Endpoint",
                format!(
                    "{} ({})",
                    url.origin().ascii_serialization(),
                    kind_label(&endpoint_kind(config))
                ),
                theme,
            ));
        }
        None => lines.push(field("Status", ENGINE_OFF, theme)),
    }
    lines.push(field("Protocol", config.protocol.as_str(), theme));
    lines.push(field(
        "Model",
        escape_terminal_controls(&config.model),
        theme,
    ));
    lines.push(field("Timeout", format!("{} ms", config.timeout_ms), theme));
    lines.push(field("allow_remote", yes_no(config.allow_remote), theme));
    lines.push(field("allow_http", yes_no(config.allow_http), theme));
    lines.push(field(
        "API key",
        format!(
            "{} {}",
            escape_terminal_controls(&config.api_key_env),
            match ctx.api_key_set {
                true => "is set",
                false => "is not set",
            }
        ),
        theme,
    ));

    lines.push(Line::default());
    lines.push(heading(HEALTH_HEADING, theme));
    let reachability = match status.reachable {
        Some(true) => REACHABLE,
        Some(false) => OFFLINE,
        None => NOT_CHECKED,
    };
    lines.push(field("Reachability", reachability, theme));
    lines.push(match status.last_error {
        Some(error) => field_styled("Last error", error, theme.tool_error, theme),
        None => field("Last error", NONE, theme),
    });
    lines.push(match status.log_failed {
        true => field_styled("Log writes", "failing", theme.tool_error, theme),
        false => field("Log writes", "ok", theme),
    });

    lines.push(Line::default());
    lines.push(heading(LOGGING_HEADING, theme));
    lines.push(field("Status", on_off(config.log), theme));
    lines.push(field(
        "Retention",
        format!("{} days", config.log_retention_days),
        theme,
    ));
    lines.push(field(
        "Database",
        escape_terminal_controls(&ctx.log_path.display().to_string()),
        theme,
    ));
    if !config.log {
        lines.push(hint(LOGGING_HINT, theme));
    }

    lines.push(Line::default());
    lines.push(heading(SESSION_HEADING, theme));
    lines.push(field(
        "Content",
        match ctx.tainted {
            true => CONTENT_FLAGGED,
            false => "not flagged",
        },
        theme,
    ));
    lines.push(field("Permissions", mode_name(ctx.mode), theme));

    lines.push(Line::default());
    lines.push(heading(THRESHOLDS_HEADING, theme));
    let thresholds = threshold_rows(&config.thresholds);
    let name_cols = thresholds
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or_default();
    for (name, value) in thresholds {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{INDENT}{name:<name_cols$}{COLUMN_GAP}"),
                theme.tool_dim,
            ),
            Span::raw(
                value.map_or_else(|| THRESHOLD_OFF.to_owned(), |value| format!("{value:.2}")),
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(heading(FEATURES_HEADING, theme));
    lines.push(feature_summary(config, theme));
    lines
}

/// Every threshold, in `[decisions.thresholds]` order. An unset `shell_writes`
/// is `None`, which is what keeps shell-effect warnings off.
pub(super) fn threshold_rows(thresholds: &DecisionThresholds) -> [(&'static str, Option<f64>); 10] {
    [
        ("permission_flag", Some(thresholds.permission_flag)),
        ("auto_flag", Some(thresholds.auto_flag)),
        ("content_injection", Some(thresholds.content_injection)),
        (
            "content_addressed_to_agent",
            Some(thresholds.content_addressed_to_agent),
        ),
        ("shell_endless", Some(thresholds.shell_endless)),
        ("shell_duration", Some(thresholds.shell_duration)),
        ("routing_confidence", Some(thresholds.routing_confidence)),
        ("goal_skip_below", Some(thresholds.goal_skip_below)),
        ("shell_writes", thresholds.shell_writes),
        ("question_tool_nudge", Some(thresholds.question_tool_nudge)),
    ]
}

fn feature_summary(config: &DecisionsConfig, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(INDENT)];
    for mode in [
        FeatureMode::Enforce,
        FeatureMode::Advise,
        FeatureMode::Shadow,
        FeatureMode::Off,
    ] {
        let count = DecisionFeature::ALL
            .iter()
            .filter(|feature| {
                feature.config_key().is_some() && *feature.mode(&config.features) == mode
            })
            .count();
        if count == 0 {
            continue;
        }
        if spans.len() > 1 {
            spans.push(Span::styled(LIST_GAP, theme.tool_dim));
        }
        spans.push(Span::styled(
            format!("{count} {}", mode.as_str()),
            mode_style(&mode, theme),
        ));
    }
    spans.push(Span::styled(FEATURES_POINTER, theme.tool_dim));
    Line::from(spans)
}

/// The name a reader configures the feature by. Workflow has no key, so it
/// goes by the name it is logged under.
pub(super) fn feature_label(feature: &DecisionFeature) -> &'static str {
    feature.config_key().unwrap_or(feature.name())
}

/// The widest [`feature_label`], so a column of them lines up.
pub(super) fn feature_cols() -> usize {
    DecisionFeature::ALL
        .iter()
        .map(|feature| feature_label(feature).width())
        .max()
        .unwrap_or_default()
}

/// Workflow runs whenever an engine is set, and with no engine it cannot run.
pub(super) fn feature_mode<'a>(
    feature: &DecisionFeature,
    config: &'a DecisionsConfig,
) -> &'a FeatureMode {
    match (feature, &config.base_url) {
        (DecisionFeature::Workflow, None) => &FeatureMode::Off,
        _ => feature.mode(&config.features),
    }
}

pub(super) fn mode_style(mode: &FeatureMode, theme: &Theme) -> Style {
    match mode {
        FeatureMode::Off => theme.tool_dim,
        FeatureMode::Shadow => theme.item_desc,
        FeatureMode::Advise => theme.accent,
        FeatureMode::Enforce => theme.tool_success,
    }
}

pub(super) fn feature_row(
    feature: &DecisionFeature,
    config: &DecisionsConfig,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mode = feature_mode(feature, config);
    let cols = feature_cols();
    vec![
        Span::styled(
            format!("{:<cols$}{COLUMN_GAP}", feature_label(feature)),
            theme.item,
        ),
        Span::styled(mode.as_str(), mode_style(mode, theme)),
    ]
}

pub(super) fn feature_detail(
    feature: &DecisionFeature,
    config: &DecisionsConfig,
    logged: Logged<'_>,
    scope: DecisionsScope,
    now: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mode = feature_mode(feature, config);
    let setting = feature.setting();
    let mut lines = vec![
        Line::styled(feature_label(feature), theme.bold),
        Line::styled(
            setting.map_or(WORKFLOW_DETAIL, |setting| setting.description),
            theme.item_desc,
        ),
        Line::default(),
        field_styled("Mode", mode.as_str(), mode_style(mode, theme), theme),
    ];
    if let Some(setting) = setting {
        let mut spans = vec![label_span("Modes", theme)];
        for (index, allowed) in setting.modes.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(LIST_GAP, theme.tool_dim));
            }
            match allowed == mode {
                true => {
                    let style = mode_style(allowed, theme).add_modifier(Modifier::BOLD);
                    spans.push(Span::styled(allowed.as_str(), style));
                    spans.push(Span::styled(CURRENT_MODE, theme.tool_dim));
                }
                false => spans.push(Span::styled(allowed.as_str(), theme.tool_dim)),
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(field("Logged as", feature.name(), theme));
    if *mode == FeatureMode::Shadow && !config.log {
        lines.push(hint(SHADOW_UNLOGGED, theme));
    }

    lines.push(Line::default());
    lines.push(Line::styled(
        format!("{ACTIVITY_HEADING} ({})", scope.label()),
        theme.keybind_section,
    ));
    match logged {
        Logged::Ready { activity, .. } => {
            match activity
                .stats
                .iter()
                .find(|stats| stats.feature == feature.name())
            {
                Some(stats) => lines.extend(stats_fields(stats, now, theme)),
                None => lines.push(hint(scope.empty(), theme)),
            }
        }
        unread => lines.extend(state_line(unread, theme)),
    }

    if let Some(key) = feature.config_key() {
        lines.push(Line::default());
        lines.push(heading(CONFIGURE_HEADING, theme));
        lines.push(Line::styled(
            format!("{INDENT}{FEATURES_TABLE}"),
            theme.inline_code,
        ));
        lines.push(Line::styled(
            format!("{INDENT}{key} = \"{}\"", mode.as_str()),
            theme.inline_code,
        ));
        lines.push(hint(GLOBAL_CONFIG, theme));
    }
    lines
}

fn stats_fields(stats: &DecisionStats, now: u64, theme: &Theme) -> Vec<Line<'static>> {
    let agreement = match stats.agreement_rate {
        Some(rate) => format!(
            "{} of {} compared",
            percent(rate),
            format_integer(stats.compared_labels)
        ),
        None => DASH.to_owned(),
    };
    vec![
        field("Calls", format_integer(stats.count), theme),
        field_styled(
            "Errors",
            errors(stats),
            match stats.error_count {
                0 => Style::default(),
                _ => theme.tool_error,
            },
            theme,
        ),
        field(
            "Latency",
            format!(
                "p50 {}{LIST_GAP}p95 {}",
                latency(stats.latency_p50_ms),
                latency(stats.latency_p95_ms)
            ),
            theme,
        ),
        field("Acted", format_integer(stats.acted_count), theme),
        field("Labelled", format_integer(stats.labelled_count), theme),
        field("Agreement", agreement, theme),
        field(
            "Last seen",
            format!("{} ago", age(now.saturating_sub(stats.last_timestamp))),
            theme,
        ),
    ]
}

/// What stands in for rows the log has not handed over. Nothing once it has.
pub(super) fn state_line(logged: Logged<'_>, theme: &Theme) -> Option<Line<'static>> {
    match logged {
        Logged::Loading => Some(hint(READING, theme)),
        Logged::Missing => Some(hint(NO_DECISIONS, theme)),
        Logged::Failed(error) => Some(Line::styled(
            format!("{INDENT}{}", escape_terminal_controls(error)),
            theme.tool_error,
        )),
        Logged::Ready { .. } => None,
    }
}

/// The scope's activity per feature: every feature that is on, and every
/// feature with rows, so a feature switched off since still answers for what
/// it did.
pub(super) fn activity(
    config: &DecisionsConfig,
    logged: Logged<'_>,
    scope: DecisionsScope,
    now: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !config.log {
        lines.push(Line::styled(LOGGING_HINT, theme.tool_warning));
        lines.push(Line::default());
    }
    let Logged::Ready { activity, .. } = logged else {
        lines.extend(state_line(logged, theme));
        return lines;
    };
    let plain = Style::default();
    let mut rows: Vec<Vec<(String, Style)>> = vec![
        ACTIVITY_HEADERS
            .iter()
            .map(|header| ((*header).to_owned(), theme.keybind_section))
            .collect(),
    ];
    for feature in &DecisionFeature::ALL {
        let mode = feature_mode(feature, config);
        let stats = activity
            .stats
            .iter()
            .find(|stats| stats.feature == feature.name());
        if *mode != FeatureMode::Off || stats.is_some() {
            rows.push(stats_cells(
                feature_label(feature).to_owned(),
                Some(mode),
                stats,
                now,
                theme,
            ));
        }
    }
    for stats in activity.stats.iter().filter(|stats| {
        !DecisionFeature::ALL
            .iter()
            .any(|feature| feature.name() == stats.feature)
    }) {
        rows.push(stats_cells(
            escape_terminal_controls(&stats.feature),
            None,
            Some(stats),
            now,
            theme,
        ));
    }
    if rows.len() == 1 {
        lines.push(hint(scope.empty(), theme));
        return lines;
    }
    let sum = |count: fn(&DecisionStats) -> u64| {
        format_integer(activity.stats.iter().map(count).sum::<u64>())
    };
    let blank = || (String::new(), plain);
    rows.push(vec![
        (TOTAL_LABEL.to_owned(), theme.bold),
        blank(),
        (sum(|stats| stats.count), theme.bold),
        (sum(|stats| stats.error_count), theme.bold),
        blank(),
        blank(),
        (sum(|stats| stats.acted_count), theme.bold),
        (sum(|stats| stats.labelled_count), theme.bold),
        blank(),
        blank(),
    ]);
    lines.extend(table(&rows));
    if activity.stats.is_empty() {
        lines.push(Line::default());
        lines.push(hint(scope.empty(), theme));
    }
    lines.push(Line::default());
    lines.extend(
        ACTIVITY_FOOTNOTE
            .into_iter()
            .map(|note| Line::styled(note, theme.tool_dim)),
    );
    lines
}

fn stats_cells(
    label: String,
    mode: Option<&FeatureMode>,
    stats: Option<&DecisionStats>,
    now: u64,
    theme: &Theme,
) -> Vec<(String, Style)> {
    let plain = Style::default();
    let mode = mode.map_or((DASH.to_owned(), theme.tool_dim), |mode| {
        (mode.as_str().to_owned(), mode_style(mode, theme))
    });
    let Some(stats) = stats.filter(|stats| stats.count > 0) else {
        let dash = || (DASH.to_owned(), theme.tool_dim);
        let zero = || ("0".to_owned(), theme.tool_dim);
        return vec![
            (label, theme.item),
            mode,
            zero(),
            zero(),
            dash(),
            dash(),
            zero(),
            zero(),
            dash(),
            dash(),
        ];
    };
    vec![
        (label, theme.item),
        mode,
        (format_integer(stats.count), plain),
        (
            errors(stats),
            match stats.error_count {
                0 => plain,
                _ => theme.tool_error,
            },
        ),
        (latency(stats.latency_p50_ms), plain),
        (latency(stats.latency_p95_ms), plain),
        (format_integer(stats.acted_count), plain),
        (format_integer(stats.labelled_count), plain),
        (stats.agreement_rate.map_or(DASH.to_owned(), percent), plain),
        (
            age(now.saturating_sub(stats.last_timestamp)),
            theme.tool_dim,
        ),
    ]
}

/// Rows of cells in columns as wide as their widest cell: the first column
/// reads left to right, the numbers after it line up on the right.
fn table(rows: &[Vec<(String, Style)>]) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or_default();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|(text, _)| text.width())
                .max()
                .unwrap_or_default()
        })
        .collect();
    rows.iter()
        .map(|row| {
            let spans: Vec<Span<'static>> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(column, ((text, style), width))| {
                    let pad = " ".repeat(width.saturating_sub(text.width()));
                    let cell = match column {
                        0 => format!("{text}{pad}"),
                        _ => format!("{COLUMN_GAP}{pad}{text}"),
                    };
                    Span::styled(cell, *style)
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

pub(super) fn recent_row(decision: &LoggedDecision, now: u64, theme: &Theme) -> Vec<Span<'static>> {
    let record = &decision.record;
    let cols = feature_cols();
    vec![
        Span::styled(
            format!("{:>AGE_COLS$} ", age(now.saturating_sub(record.timestamp))),
            theme.tool_dim,
        ),
        Span::styled(
            format!("{:<cols$} ", logged_label(&record.feature)),
            theme.item,
        ),
        Span::styled(
            format!("{:<EFFECT_COLS$}", effect_label(&record.effect)),
            effect_style(&record.effect, theme),
        ),
        Span::styled(
            format!("{:>LATENCY_COLS$}", latency(record.latency_ms)),
            theme.tool_dim,
        ),
        mark(record.error.is_some(), ERROR_MARK, theme.tool_error),
        mark(decision.label.is_some(), LABEL_MARK, theme.tool_success),
    ]
}

/// An absent mark still takes its column, so a ✓ never stands where another
/// row's ✗ does and the list keeps its width from one scope to the next.
fn mark(present: bool, glyph: &'static str, style: Style) -> Span<'static> {
    match present {
        true => Span::styled(glyph, style),
        false => Span::raw(ABSENT_MARK),
    }
}

pub(super) fn recent_detail(
    decision: &LoggedDecision,
    session: &str,
    clock: ClockFormat,
    now: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let record = &decision.record;
    let session = match record.session.as_deref() {
        Some(logged) if logged == session => {
            format!("{}{THIS_SESSION}", escape_terminal_controls(logged))
        }
        Some(logged) => escape_terminal_controls(logged),
        None => NONE.to_owned(),
    };
    let label = decision.label.as_ref();
    let mut lines = vec![
        field("Id", format!("#{}", decision.id), theme),
        field(
            "Time",
            format!(
                "{} ({} ago)",
                local_time(record.timestamp, clock),
                age(now.saturating_sub(record.timestamp))
            ),
            theme,
        ),
        field("Session", session, theme),
        field(
            "Project",
            record
                .project
                .as_deref()
                .map_or_else(|| NONE.to_owned(), escape_terminal_controls),
            theme,
        ),
        field("Feature", logged_feature(&record.feature), theme),
        field("Mode", escape_terminal_controls(&record.mode), theme),
        field_styled(
            "Effect",
            effect_label(&record.effect),
            effect_style(&record.effect, theme),
            theme,
        ),
        field(
            "Endpoint",
            format!(
                "{}{LIST_GAP}{}",
                kind_label(&record.endpoint_kind),
                escape_terminal_controls(&record.model)
            ),
            theme,
        ),
        field(
            "Question set",
            format!(
                "{}@{}",
                escape_terminal_controls(&record.question_set_id),
                escape_terminal_controls(&record.question_set_version)
            ),
            theme,
        ),
        field("Latency", latency(record.latency_ms), theme),
        match &record.error {
            Some(error) => field_styled(
                "Error",
                escape_terminal_controls(error),
                theme.tool_error,
                theme,
            ),
            None => field("Error", NONE, theme),
        },
        field(
            "Label",
            label.map_or_else(
                || NONE.to_owned(),
                |label| {
                    format!(
                        "{} at {}",
                        escape_terminal_controls(&label.source),
                        local_time(label.timestamp, clock)
                    )
                },
            ),
            theme,
        ),
    ];
    let label = label.and_then(|label| serde_json::to_value(label).ok());
    let sections = [
        Some(&record.state),
        Some(&record.questions),
        record.answers.as_ref(),
        Some(&record.meta),
        label.as_ref(),
    ];
    for (title, value) in JSON_SECTIONS.into_iter().zip(sections) {
        let Some(value) = value else {
            continue;
        };
        lines.push(Line::default());
        lines.push(heading(title, theme));
        lines.extend(json_lines(value));
    }
    lines
}

/// Pretty JSON, painted line by line. Serializing escapes C0 controls in
/// strings but not DEL or C1, so each line is escaped before it is painted.
fn json_lines(value: &Value) -> Vec<Line<'static>> {
    serde_json::to_string_pretty(value)
        .map(|text| {
            text.lines()
                .map(|line| json_text::themed_line(&escape_terminal_controls(line)))
                .collect()
        })
        .unwrap_or_default()
}

/// A logged feature by the name it is configured under, or verbatim when no
/// feature of this build logs under it.
fn logged_label(name: &str) -> String {
    DecisionFeature::ALL
        .iter()
        .find(|feature| feature.name() == name)
        .map_or_else(
            || escape_terminal_controls(name),
            |feature| feature_label(feature).to_owned(),
        )
}

fn logged_feature(name: &str) -> String {
    let label = logged_label(name);
    match label == name {
        true => label,
        false => format!("{label} (logged as {})", escape_terminal_controls(name)),
    }
}

fn local_time(timestamp: u64, clock: ClockFormat) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
        .map(|at| {
            at.to_zoned(TimeZone::system())
                .strftime(&format!("{DATE_FORMAT}{}", hms(clock)))
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

fn errors(stats: &DecisionStats) -> String {
    match stats.error_count {
        0 => format_integer(0),
        count => format!("{} ({})", format_integer(count), percent(stats.error_rate)),
    }
}

fn percent(rate: f64) -> String {
    format!("{:.0}%", rate * PERCENT)
}

fn latency(millis: u64) -> String {
    match millis < MILLIS_PER_SECOND {
        true => format!("{millis}ms"),
        false => format!("{:.1}s", millis as f64 / MILLIS_PER_SECOND as f64),
    }
}

fn effect_label(effect: &DecisionEffect) -> &'static str {
    match effect {
        DecisionEffect::None => "none",
        DecisionEffect::Advised => "advised",
        DecisionEffect::Escalated => "escalated",
        DecisionEffect::Rerouted => "rerouted",
        DecisionEffect::Skipped => "skipped",
    }
}

fn effect_style(effect: &DecisionEffect, theme: &Theme) -> Style {
    match effect {
        DecisionEffect::None => theme.tool_dim,
        DecisionEffect::Advised | DecisionEffect::Rerouted => theme.accent,
        DecisionEffect::Escalated => theme.tool_warning,
        DecisionEffect::Skipped => theme.item_desc,
    }
}

fn kind_label(kind: &EndpointKind) -> &'static str {
    match kind {
        EndpointKind::Local => "local",
        EndpointKind::Remote => "remote",
    }
}

fn mode_name(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "yolo",
    }
}

fn yes_no(value: bool) -> &'static str {
    match value {
        true => "yes",
        false => "no",
    }
}

fn on_off(value: bool) -> &'static str {
    match value {
        true => "on",
        false => "off",
    }
}

fn heading(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::styled(title, theme.keybind_section)
}

pub(super) fn hint(text: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::raw(INDENT), Span::styled(text, theme.tool_dim)])
}

fn label_span(label: &str, theme: &Theme) -> Span<'static> {
    Span::styled(format!("{INDENT}{label:<LABEL_COLS$}"), theme.tool_dim)
}

fn field(label: &str, value: impl Into<String>, theme: &Theme) -> Line<'static> {
    field_styled(label, value, Style::default(), theme)
}

fn field_styled(
    label: &str,
    value: impl Into<String>,
    style: Style,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        label_span(label, theme),
        Span::styled(value.into(), style),
    ])
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::DecisionThresholds;

    use super::threshold_rows;

    #[test]
    fn every_threshold_is_listed_in_config_order() {
        let listed: Vec<&str> = threshold_rows(&DecisionThresholds::default())
            .iter()
            .map(|(name, _)| *name)
            .collect();
        let configured: Vec<&str> = DecisionThresholds::FIELDS
            .iter()
            .map(|field| field.name)
            .collect();

        assert_eq!(listed, configured);
    }
}
