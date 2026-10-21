//! The Web search tab: one row per backend, listed ones first in their order, and what the
//! environment decides while `[websearch]` leaves `backends` or `mode` unset. Rows and the
//! info pane name keys and variables, never a value.

use std::ops::Range;

use caudra_config::websearch::{WebsearchBackendName, WebsearchConfig, WebsearchMode};
use caudra_workcell::{BackendStatus, SettingSource};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::hover_style;
use crate::theme::Theme;

const LIST_SEPARATOR: char = ',';
const DISABLED: &str = "disabled";
const CHECKED: &str = "[x]";
const UNCHECKED: &str = "[ ]";
const UP_ARROW: &str = "▲";
const DOWN_ARROW: &str = "▼";
const NO_ARROW: &str = " ";
const GAP: &str = " ";
const LABEL_COLS: usize = 8;
const SOURCE_COLS: usize = 6;
const POSITION_COLS: usize = 2;
const UNLISTED: &str = "·";
const READY: &str = "ready";
const CONFIG_SOURCE: &str = "config";
const ENV_SOURCE: &str = "env";
const UNSET_SOURCE: &str = "unset";
const MODE_LABEL: &str = "Mode  ";
const CHIP_GAP: &str = "  ";
const CURRENT_MODE: &str = " (current)";
const POSITION_PREFIX: &str = " · number ";
const POSITION_SUFFIX: &str = " in the list";
const DEFAULT_SUFFIX: &str = ", from the environment";
const DESCRIPTION_SEPARATOR: &str = " · ";
const KEY_GAP: &str = "  ";
const REFERENCE_PLACEHOLDER: &str = "NAME";
const SET_IN_CONFIG: &str = "set in caudra.toml";
const READS_PREFIX: &str = "reads ";
const NO_SETTINGS: &str = "Needs no settings.";
const UNLISTED_NOTE: &str = "Not used. Space adds it to the end of the list.";
const NO_STATUS: &str = "Its status shows for a local Workcell only.";

/// Backends `WORKCELL_WEBSEARCH_BACKEND` lists, as Workcell reads it: `disabled` lists none, and
/// with nothing set `exa-mcp` answers. A name Workcell does not know is left out.
pub(crate) fn environment_backends(value: Option<&str>) -> Vec<WebsearchBackendName> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => vec![WebsearchBackendName::ExaMcp],
        Some(DISABLED) => Vec::new(),
        Some(list) => list
            .split(LIST_SEPARATOR)
            .filter_map(|name| WebsearchBackendName::parse(name.trim()))
            .fold(Vec::new(), |mut listed, backend| {
                if !listed.contains(&backend) {
                    listed.push(backend);
                }
                listed
            }),
    }
}

/// The mode `WORKCELL_WEBSEARCH_MODE` names, `fallback` when it names none.
pub(crate) fn environment_mode(value: Option<&str>) -> WebsearchMode {
    value
        .map(str::trim)
        .and_then(|value| {
            WebsearchMode::ALL
                .into_iter()
                .find(|mode| mode.as_str() == value)
        })
        .unwrap_or_default()
}

/// One backend as its row shows it.
pub(super) struct BackendRow<'a> {
    pub(super) backend: WebsearchBackendName,
    /// Its place in the list, counting from zero, `None` while it is not listed.
    pub(super) position: Option<usize>,
    pub(super) status: Option<&'a BackendStatus>,
}

/// The listed backends in order, then the rest.
pub(super) fn backend_rows<'a>(
    listed: &[WebsearchBackendName],
    statuses: &'a [BackendStatus],
) -> Vec<BackendRow<'a>> {
    let unlisted = WebsearchBackendName::ALL
        .into_iter()
        .filter(|backend| !listed.contains(backend));
    listed
        .iter()
        .copied()
        .chain(unlisted)
        .map(|backend| BackendRow {
            backend,
            position: listed.iter().position(|listed| *listed == backend),
            status: statuses.iter().find(|status| status.backend == backend),
        })
        .collect()
}

/// Where a row's controls start, counted from the row's first column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RowColumns {
    pub(super) check: Range<u16>,
    pub(super) up: Range<u16>,
    pub(super) down: Range<u16>,
}

/// The row's spans and where its checkbox and arrows landed. `listed` counts the listed
/// backends, so the last of them shows no way further down.
pub(super) fn backend_line(
    row: &BackendRow,
    listed: usize,
    theme: &Theme,
) -> (Vec<Span<'static>>, RowColumns) {
    let mut spans = Vec::new();
    let mut span = |text: String, style| {
        let start = width(&spans);
        spans.push(Span::styled(text, style));
        start..width(&spans)
    };
    let check = span(
        match row.position {
            Some(_) => CHECKED,
            None => UNCHECKED,
        }
        .to_owned(),
        theme.accent,
    );
    span(GAP.to_owned(), theme.tool_dim);
    let position = row.position.map_or_else(
        || UNLISTED.to_owned(),
        |position| (position + 1).to_string(),
    );
    span(format!("{position:>POSITION_COLS$}"), theme.tool_dim);
    span(GAP.to_owned(), theme.tool_dim);
    let arrow = |shown: bool, glyph: &str| match shown {
        true => glyph.to_owned(),
        false => NO_ARROW.to_owned(),
    };
    let up = span(
        arrow(row.position.is_some_and(|at| at > 0), UP_ARROW),
        theme.keybind_key,
    );
    span(GAP.to_owned(), theme.tool_dim);
    let down = span(
        arrow(row.position.is_some_and(|at| at + 1 < listed), DOWN_ARROW),
        theme.keybind_key,
    );
    span(GAP.to_owned(), theme.tool_dim);
    span(
        format!("{:<LABEL_COLS$}", row.backend.label()),
        Style::default(),
    );
    span(GAP.to_owned(), theme.tool_dim);
    let source = match row.status.map(|status| status.source) {
        Some(SettingSource::Config) => CONFIG_SOURCE,
        Some(SettingSource::Environment) => ENV_SOURCE,
        Some(SettingSource::Unset) | None => UNSET_SOURCE,
    };
    span(format!("{source:<SOURCE_COLS$}"), theme.tool_dim);
    span(GAP.to_owned(), theme.tool_dim);
    if let Some(status) = row.status {
        match status.issue {
            None => span(READY.to_owned(), theme.tool_success),
            Some(issue) => span(issue.as_str().to_owned(), theme.tool_warning),
        };
    }
    (spans, RowColumns { check, up, down })
}

/// The mode line with `current` marked and the mode under the pointer hovered, and the
/// columns each mode spans, so a click picks one.
pub(super) fn mode_line(
    current: WebsearchMode,
    hovered: Option<WebsearchMode>,
    theme: &Theme,
) -> (Line<'static>, Vec<(Range<u16>, WebsearchMode)>) {
    let mut spans = vec![Span::styled(MODE_LABEL, theme.tool_dim)];
    let mut chips = Vec::with_capacity(WebsearchMode::ALL.len());
    for (index, mode) in WebsearchMode::ALL.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(CHIP_GAP));
        }
        let style = match mode == current {
            true => theme.accent.add_modifier(Modifier::BOLD),
            false => theme.tool_dim,
        };
        let start = width(&spans);
        spans.push(Span::styled(
            mode.as_str(),
            hover_style(style, hovered == Some(mode)),
        ));
        chips.push((start..width(&spans), mode));
        if mode == current {
            spans.push(Span::styled(CURRENT_MODE, theme.tool_dim));
        }
    }
    (Line::from(spans), chips)
}

/// What the info pane says about `row`: whether it is ready and why not, and where each of its
/// settings comes from.
pub(super) fn backend_info(
    row: &BackendRow,
    written: &WebsearchConfig,
    defaulted: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let backend = row.backend;
    let mut heading = vec![Span::styled(
        format!("{} ({})", backend.label(), backend.as_str()),
        theme.bold,
    )];
    if let Some(position) = row.position {
        heading.push(Span::styled(
            format!(
                "{POSITION_PREFIX}{}{POSITION_SUFFIX}{}",
                position + 1,
                if defaulted { DEFAULT_SUFFIX } else { "" }
            ),
            theme.tool_dim,
        ));
    }
    let mut lines = vec![Line::from(heading)];
    lines.push(match row.status {
        None => Line::styled(NO_STATUS, theme.tool_dim),
        Some(status) => match status.issue {
            None => Line::styled(READY, theme.tool_success),
            Some(issue) => Line::styled(issue.message(), theme.tool_warning),
        },
    });
    if row.position.is_none() {
        lines.push(Line::styled(UNLISTED_NOTE, theme.tool_dim));
    }
    let fields = backend.fields();
    if fields.is_empty() {
        lines.push(Line::styled(NO_SETTINGS, theme.tool_dim));
    }
    let key_cols = fields
        .iter()
        .map(|field| field.name.width())
        .max()
        .unwrap_or_default();
    for field in fields {
        let source = match (written.value(backend, field.name), field.env) {
            (Some(_), _) => SET_IN_CONFIG.to_owned(),
            (None, Some(env)) => format!("{READS_PREFIX}{env}"),
            (None, None) => UNSET_SOURCE.to_owned(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<key_cols$}{KEY_GAP}", field.name),
                theme.tool_dim,
            ),
            Span::styled(source, theme.accent),
            Span::styled(
                format!("{DESCRIPTION_SEPARATOR}{}", field.description),
                theme.item_desc,
            ),
        ]));
    }
    lines
}

/// What saving a credential as plain text risks, and the reference to write instead.
pub(super) fn literal_warning(key: &str, env: Option<&str>) -> String {
    let reference = env.unwrap_or(REFERENCE_PLACEHOLDER);
    format!(
        "{key} would sit in caudra.toml in plain text. Put it in the global .env and set {{env:{reference}}} instead, or press Enter again to save it."
    )
}

fn width(spans: &[Span]) -> u16 {
    u16::try_from(spans.iter().map(Span::width).sum::<usize>()).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(None, &[WebsearchBackendName::ExaMcp]; "unset")]
    #[test_case(Some("  "), &[WebsearchBackendName::ExaMcp]; "blank")]
    #[test_case(Some("disabled"), &[]; "disabled")]
    #[test_case(Some("searxng, brave,altavista,brave"), &[WebsearchBackendName::Searxng, WebsearchBackendName::Brave]; "list")]
    fn the_environment_list_reads_as_workcell_reads_it(
        value: Option<&str>,
        expected: &[WebsearchBackendName],
    ) {
        assert_eq!(environment_backends(value), expected);
    }

    #[test_case(None, WebsearchMode::Fallback; "unset")]
    #[test_case(Some("all"), WebsearchMode::All; "all")]
    #[test_case(Some("random"), WebsearchMode::Fallback; "unknown")]
    fn the_environment_mode_defaults_to_fallback(value: Option<&str>, expected: WebsearchMode) {
        assert_eq!(environment_mode(value), expected);
    }

    #[test]
    fn listed_backends_come_first_in_their_order() {
        let listed = [WebsearchBackendName::Kagi, WebsearchBackendName::Searxng];
        let order: Vec<_> = backend_rows(&listed, &[])
            .iter()
            .map(|row| (row.backend, row.position))
            .collect();
        assert_eq!(
            &order[..3],
            [
                (WebsearchBackendName::Kagi, Some(0)),
                (WebsearchBackendName::Searxng, Some(1)),
                (WebsearchBackendName::ExaMcp, None),
            ]
        );
        assert_eq!(order.len(), WebsearchBackendName::ALL.len());
    }
}
