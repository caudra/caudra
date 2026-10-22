//! The pstack tab: the model each kind of delegated work runs on, as `/settings` stores it in
//! Caudra's state rather than `caudra.toml`. Rows show what pstack's next page serves; an edit
//! reads the stored roles, changes them, and writes them back.

use std::collections::BTreeMap;

use caudra_agent::pstack::roles::{
    DEFAULT_EFFORT, Kind, ResolvedSeat, RoleRow, RoleTable, Seat, Source, StoredRoles,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// The levels Effort steps through, as pstack ranks them.
const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
const EFFORT_LABEL: &str = "Effort";
const ADD_SEAT: &str = "  + add seat";
const DEFAULT_SUFFIX: &str = " (default)";
const GAP: &str = "  ";
const SEPARATOR: &str = " · ";
const ARGUMENT_QUOTE: char = '`';
const LEAD: &str = "The model each kind of pstack's delegated work runs on, kept in Caudra's state";
const NO_MODEL_CHOICE_LEAD: &str =
    "agent.task_model_choice is off, so every kind runs as inherit; choices still save";
const EFFORT_INFO: &str = "The effort auto seats aim for, snapped down to a level each model takes. A seat whose thinking is \"effort\" follows it too.";
const THINKING_PREFIX: &str = "Thinking: ";
const FOLLOWS_EFFORT: &str = "follows Effort";
const INHERIT_INFO: &str = "Runs as inherit: the task omits model and thinking, so it runs on the profile's subagent_model, else the Subagent model.";
const PICK_KEYS: &str = "Enter picks a model";
const ADD_KEYS: &str = "Enter picks a model for a new seat";
const THINKING_KEYS: &str = "t steps thinking";
const REMOVE_KEYS: &str = "d removes this seat";
const AUTO_KEYS_PREFIX: &str = "R returns ";
const AUTO_KEYS_SUFFIX: &str = " to auto";
const EFFORT_KEYS: &str = "Enter or → steps it, wrapping";
const EFFORT_RESET_PREFIX: &str = "R returns it to ";
const APPLIES: &str = "Saved in Caudra's state, never caudra.toml; it applies next page load.";

/// What the tab shows: the table pstack serves, what the user stored, and the thinking levels
/// each model in either takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PstackRoles {
    pub(crate) table: RoleTable,
    pub(crate) stored: StoredRoles,
    levels: BTreeMap<String, Vec<String>>,
}

impl PstackRoles {
    pub(crate) fn new(
        table: RoleTable,
        stored: StoredRoles,
        levels_of: impl Fn(&str) -> Vec<String>,
    ) -> Self {
        let resolved = table
            .rows
            .iter()
            .flat_map(|row| &row.seats)
            .filter_map(|seat| seat.model.as_deref());
        let chosen = stored
            .kinds
            .values()
            .flatten()
            .filter_map(|seat| match seat {
                Seat::Model { model, .. } => Some(model.as_str()),
                Seat::Inherit => None,
            });
        let levels = resolved
            .chain(chosen)
            .map(|model| (model.to_owned(), levels_of(model)))
            .collect();
        Self {
            table,
            stored,
            levels,
        }
    }

    fn row(&self, kind: Kind) -> Option<&RoleRow> {
        self.table.row(kind)
    }

    /// The seat the user stored behind a row that shows it, `None` for an auto row.
    fn stored_seat(&self, kind: Kind, seat: usize) -> Option<&Seat> {
        self.row(kind)
            .filter(|row| row.source == Source::Set)
            .and_then(|_| self.stored.kinds.get(&kind))
            .and_then(|seats| seats.get(seat))
    }

    /// The model a seat runs on, else the one the user stored while `task` cannot choose one.
    pub(super) fn seat_model(&self, kind: Kind, seat: usize) -> Option<&str> {
        let resolved = self
            .row(kind)
            .and_then(|row| row.seats.get(seat))
            .and_then(|resolved| resolved.model.as_deref());
        resolved.or_else(|| match self.stored_seat(kind, seat) {
            Some(Seat::Model { model, .. }) => Some(model.as_str()),
            _ => None,
        })
    }

    /// The thinking the user pinned on a seat, `None` while it follows Effort.
    pub(super) fn stored_thinking(&self, kind: Kind, seat: usize) -> Option<&str> {
        match self.stored_seat(kind, seat) {
            Some(Seat::Model { thinking, .. }) => thinking.as_deref(),
            _ => None,
        }
    }

    pub(super) fn levels(&self, model: &str) -> &[String] {
        self.levels.get(model).map_or(&[], Vec::as_slice)
    }

    /// Whether the user stored a choice for `kind`, so returning it to auto changes anything.
    pub(super) fn stores(&self, kind: Kind) -> bool {
        self.stored.kinds.contains_key(&kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PstackRow {
    Effort,
    Seat {
        kind: Kind,
        seat: usize,
    },
    /// Adds a seat to a list kind.
    Add(Kind),
}

/// A change to the stored roles, made against the table the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoleEdit {
    /// `None` returns to the default.
    Effort(Option<String>),
    /// `seat: None` adds a seat to a list kind.
    Seat {
        kind: Kind,
        seat: Option<usize>,
        value: Seat,
    },
    /// `thinking: None` follows Effort.
    Thinking {
        kind: Kind,
        seat: usize,
        thinking: Option<String>,
    },
    Remove {
        kind: Kind,
        seat: usize,
    },
    /// Back to auto.
    Reset(Kind),
}

/// What the app does for the tab: write an edit, or open the model picker on a seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PstackAction {
    Edit(RoleEdit),
    /// `seat: None` picks for a new seat; `model` is the one the seat runs on now.
    Pick {
        kind: Kind,
        seat: Option<usize>,
        model: Option<String>,
    },
}

pub(super) fn rows(table: &RoleTable) -> Vec<PstackRow> {
    let mut rows = vec![PstackRow::Effort];
    for row in &table.rows {
        let kind = row.kind;
        rows.extend((0..row.seats.len()).map(|seat| PstackRow::Seat { kind, seat }));
        if kind.is_list() {
            rows.push(PstackRow::Add(kind));
        }
    }
    rows
}

pub(super) fn lead(roles: &PstackRoles) -> &'static str {
    match roles.table.task_model_choice {
        true => LEAD,
        false => NO_MODEL_CHOICE_LEAD,
    }
}

fn label(row: PstackRow) -> String {
    match row {
        PstackRow::Effort => EFFORT_LABEL.to_owned(),
        PstackRow::Seat { kind, seat } if kind.is_list() => {
            format!("{} {}", kind.label(), seat + 1)
        }
        PstackRow::Seat { kind, .. } => kind.label().to_owned(),
        PstackRow::Add(_) => ADD_SEAT.to_owned(),
    }
}

fn arguments(seat: &ResolvedSeat) -> String {
    seat.arguments().replace(ARGUMENT_QUOTE, "")
}

fn effort_value(roles: &PstackRoles) -> String {
    match &roles.stored.effort {
        Some(effort) => effort.clone(),
        None => format!("{DEFAULT_EFFORT}{DEFAULT_SUFFIX}"),
    }
}

/// Each row's spans: its label, the `task` arguments it resolves to, and where they come from.
pub(super) fn row_lines(
    rows: &[PstackRow],
    roles: &PstackRoles,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let label_cols = rows
        .iter()
        .map(|row| label(*row).width())
        .max()
        .unwrap_or_default();
    let value = |row: PstackRow| match row {
        PstackRow::Effort => effort_value(roles),
        PstackRow::Seat { kind, seat } => roles
            .row(kind)
            .and_then(|row| row.seats.get(seat))
            .map(arguments)
            .unwrap_or_default(),
        PstackRow::Add(_) => String::new(),
    };
    let value_cols = rows
        .iter()
        .map(|row| value(*row).width())
        .max()
        .unwrap_or_default();
    rows.iter()
        .map(|&row| {
            let label_style = match row {
                PstackRow::Add(_) => theme.tool_dim,
                _ => Style::default(),
            };
            let mut spans = vec![
                Span::styled(format!("{:<label_cols$}", label(row)), label_style),
                Span::raw(GAP),
                Span::styled(format!("{:<value_cols$}", value(row)), theme.accent),
            ];
            if let PstackRow::Seat { kind, .. } = row
                && let Some(row) = roles.row(kind)
            {
                spans.push(Span::raw(GAP));
                spans.push(Span::styled(row.source.describe(), theme.tool_dim));
            }
            spans
        })
        .collect()
}

/// What the info pane says about `row`: what the kind is for, its thinking, and its keys.
pub(super) fn row_info(row: PstackRow, roles: &PstackRoles, theme: &Theme) -> Vec<Line<'static>> {
    let keys = |parts: &[String]| Line::styled(parts.join(SEPARATOR), theme.item_desc);
    let applies = Line::styled(APPLIES, theme.tool_dim);
    match row {
        PstackRow::Effort => vec![
            Line::from(vec![
                Span::styled(EFFORT_LABEL, theme.bold),
                Span::styled(
                    format!("{SEPARATOR}{}", effort_value(roles)),
                    theme.tool_dim,
                ),
            ]),
            Line::styled(EFFORT_INFO, theme.tool_dim),
            keys(&[
                EFFORT_KEYS.to_owned(),
                format!("{EFFORT_RESET_PREFIX}{DEFAULT_EFFORT}"),
            ]),
            applies,
        ],
        PstackRow::Seat { kind, seat } => {
            let source = roles
                .row(kind)
                .map(|row| row.source.describe())
                .unwrap_or_default();
            let thinking = match roles.seat_model(kind, seat) {
                None => INHERIT_INFO.to_owned(),
                Some(_) => format!(
                    "{THINKING_PREFIX}{}",
                    roles.stored_thinking(kind, seat).unwrap_or(FOLLOWS_EFFORT)
                ),
            };
            let mut parts = vec![
                PICK_KEYS.to_owned(),
                THINKING_KEYS.to_owned(),
                auto_keys(kind),
            ];
            if kind.is_list() {
                parts.push(REMOVE_KEYS.to_owned());
            }
            vec![
                Line::from(vec![
                    Span::styled(label(row), theme.bold),
                    Span::styled(format!("{SEPARATOR}{source}"), theme.tool_dim),
                ]),
                Line::styled(kind.description(), theme.tool_dim),
                Line::styled(thinking, theme.tool_dim),
                keys(&parts),
                applies,
            ]
        }
        PstackRow::Add(kind) => vec![
            Line::styled(kind.label(), theme.bold),
            Line::styled(kind.description(), theme.tool_dim),
            keys(&[ADD_KEYS.to_owned(), auto_keys(kind)]),
            applies,
        ],
    }
}

fn auto_keys(kind: Kind) -> String {
    format!("{AUTO_KEYS_PREFIX}{}{AUTO_KEYS_SUFFIX}", kind.label())
}

/// The next rung up from `current`, or from the default while none is stored, wrapping.
pub(super) fn next_effort(current: Option<&str>) -> String {
    let current = current.unwrap_or(DEFAULT_EFFORT);
    let next = EFFORT_LADDER
        .iter()
        .position(|level| *level == current)
        .map_or(0, |at| (at + 1) % EFFORT_LADDER.len());
    EFFORT_LADDER[next].to_owned()
}

/// The level after `current` among `levels`, then `None` to follow Effort, then the first again.
pub(super) fn next_thinking(current: Option<&str>, levels: &[String]) -> Option<String> {
    let next = current
        .and_then(|current| levels.iter().position(|level| level == current))
        .map_or(0, |at| at + 1);
    levels.get(next).cloned()
}

/// Applies `edit` to what the user stored. A kind on auto first takes the seats `table` shows,
/// each following Effort, so editing one seat keeps the others as they were.
pub(crate) fn apply(
    roles: &mut StoredRoles,
    table: &RoleTable,
    edit: RoleEdit,
    levels_of: impl Fn(&str) -> Vec<String>,
) {
    match edit {
        RoleEdit::Effort(effort) => roles.effort = effort,
        RoleEdit::Reset(kind) => {
            roles.kinds.remove(&kind);
        }
        RoleEdit::Seat { kind, seat, value } => {
            let seats = pinned(roles, table, kind);
            let value = keep_thinking(seat.and_then(|at| seats.get(at)), value, levels_of);
            match (kind.is_list(), seat) {
                (false, _) => *seats = vec![value],
                (true, Some(at)) if at < seats.len() => seats[at] = value,
                (true, _) => seats.push(value),
            }
        }
        RoleEdit::Thinking {
            kind,
            seat,
            thinking,
        } => {
            if let Some(Seat::Model { thinking: kept, .. }) =
                pinned(roles, table, kind).get_mut(seat)
            {
                *kept = thinking;
            }
        }
        RoleEdit::Remove { kind, seat } => {
            let seats = pinned(roles, table, kind);
            if seat < seats.len() {
                seats.remove(seat);
            }
            if seats.is_empty() {
                roles.kinds.remove(&kind);
            }
        }
    }
}

fn pinned<'a>(roles: &'a mut StoredRoles, table: &RoleTable, kind: Kind) -> &'a mut Vec<Seat> {
    let row = table.row(kind);
    if !row.is_some_and(|row| row.source == Source::Set) {
        let seats = row
            .map(|row| row.seats.iter().map(pin).collect())
            .unwrap_or_default();
        roles.kinds.insert(kind, seats);
    }
    roles.kinds.entry(kind).or_default()
}

fn pin(seat: &ResolvedSeat) -> Seat {
    match &seat.model {
        Some(model) => Seat::Model {
            model: model.clone(),
            thinking: None,
        },
        None => Seat::Inherit,
    }
}

/// A new model keeps the seat's pinned thinking when it takes that level too.
fn keep_thinking(
    previous: Option<&Seat>,
    value: Seat,
    levels_of: impl Fn(&str) -> Vec<String>,
) -> Seat {
    match (previous, value) {
        (
            Some(Seat::Model {
                thinking: Some(thinking),
                ..
            }),
            Seat::Model {
                model,
                thinking: None,
            },
        ) => {
            let kept = levels_of(&model)
                .contains(thinking)
                .then(|| thinking.clone());
            Seat::Model {
                model,
                thinking: kept,
            }
        }
        (_, value) => value,
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::pstack::roles::AutoReason;
    use test_case::test_case;

    use super::*;

    const OPUS: &str = "anthropic/claude-opus-5-5";
    const SONNET: &str = "anthropic/claude-sonnet-5-5";
    const GROK: &str = "xai/grok-4.7";
    const LOW: &str = "low";
    const HIGH: &str = "high";
    const XHIGH: &str = "xhigh";
    const MAX: &str = "max";
    const MINIMAL: &str = "minimal";
    const MEDIUM: &str = "medium";
    const UNKNOWN_LEVEL: &str = "turbo";
    const OPUS_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    const GROK_LEVELS: &[&str] = &["low", "high"];

    fn levels_of(model: &str) -> Vec<String> {
        let levels = match model {
            OPUS | SONNET => OPUS_LEVELS,
            GROK => GROK_LEVELS,
            _ => &[],
        };
        owned(levels)
    }

    fn owned(levels: &[&str]) -> Vec<String> {
        levels.iter().map(|level| (*level).to_owned()).collect()
    }

    fn resolved(model: &str) -> ResolvedSeat {
        ResolvedSeat {
            model: Some(model.to_owned()),
            thinking: Some(XHIGH.to_owned()),
        }
    }

    fn model(model: &str, thinking: Option<&str>) -> Seat {
        Seat::Model {
            model: model.to_owned(),
            thinking: thinking.map(str::to_owned),
        }
    }

    fn row(kind: Kind, seats: Vec<ResolvedSeat>, source: Source) -> RoleRow {
        RoleRow {
            kind,
            seats,
            source,
        }
    }

    /// Every kind on auto: Hardest on Opus, the Panel on Opus and Grok.
    fn auto_table() -> RoleTable {
        let panel = vec![resolved(OPUS), resolved(GROK)];
        RoleTable {
            rows: vec![
                row(
                    Kind::Code,
                    vec![ResolvedSeat::default()],
                    Source::Auto(AutoReason::SubagentJob),
                ),
                row(
                    Kind::Hardest,
                    vec![resolved(OPUS)],
                    Source::Auto(AutoReason::BestFavorite),
                ),
                row(
                    Kind::Panel,
                    panel.clone(),
                    Source::Auto(AutoReason::PanelFavorites),
                ),
                row(Kind::CrossJudge, panel, Source::Auto(AutoReason::FromPanel)),
            ],
            task_model_choice: true,
            effort: DEFAULT_EFFORT.to_owned(),
        }
    }

    fn edited(roles: &mut StoredRoles, table: &RoleTable, edit: RoleEdit) {
        apply(roles, table, edit, levels_of);
    }

    #[test_case(None, MAX ; "the_default_steps_to_max")]
    #[test_case(Some(MAX), MINIMAL ; "max_wraps_to_minimal")]
    #[test_case(Some(LOW), MEDIUM ; "low_steps_to_medium")]
    fn effort_steps_up_the_ladder(current: Option<&str>, expected: &str) {
        assert_eq!(next_effort(current), expected);
    }

    #[test_case(None, GROK_LEVELS, Some(LOW) ; "effort_steps_to_the_lowest_level")]
    #[test_case(Some(LOW), GROK_LEVELS, Some(HIGH) ; "a_level_steps_to_the_next")]
    #[test_case(Some(HIGH), GROK_LEVELS, None ; "the_top_level_steps_back_to_effort")]
    #[test_case(Some(UNKNOWN_LEVEL), GROK_LEVELS, Some(LOW) ; "an_unknown_level_restarts")]
    #[test_case(None, &[], None ; "a_model_without_levels_stays_on_effort")]
    fn thinking_steps_through_the_models_levels_then_effort(
        current: Option<&str>,
        levels: &[&str],
        expected: Option<&str>,
    ) {
        assert_eq!(next_thinking(current, &owned(levels)).as_deref(), expected);
    }

    #[test]
    fn picking_a_model_for_an_auto_seat_pins_the_others() {
        let mut roles = StoredRoles::default();
        let value = model(SONNET, None);
        edited(
            &mut roles,
            &auto_table(),
            RoleEdit::Seat {
                kind: Kind::Panel,
                seat: Some(1),
                value: value.clone(),
            },
        );
        assert_eq!(roles.kinds[&Kind::Panel], [model(OPUS, None), value]);
        assert!(!roles.kinds.contains_key(&Kind::CrossJudge));
    }

    #[test]
    fn a_picked_seat_without_an_index_is_added() {
        let mut roles = StoredRoles::default();
        edited(
            &mut roles,
            &auto_table(),
            RoleEdit::Seat {
                kind: Kind::CrossJudge,
                seat: None,
                value: Seat::Inherit,
            },
        );
        assert_eq!(
            roles.kinds[&Kind::CrossJudge],
            [model(OPUS, None), model(GROK, None), Seat::Inherit]
        );
    }

    #[test_case(HIGH, Some(HIGH) ; "a_level_the_new_model_takes_is_kept")]
    #[test_case(MAX, None ; "a_level_the_new_model_lacks_follows_effort")]
    fn a_new_model_keeps_the_seats_thinking_it_takes(pinned: &str, expected: Option<&str>) {
        let mut roles = StoredRoles {
            kinds: BTreeMap::from([(Kind::Hardest, vec![model(OPUS, Some(pinned))])]),
            ..StoredRoles::default()
        };
        let mut table = auto_table();
        table.rows[1].source = Source::Set;
        edited(
            &mut roles,
            &table,
            RoleEdit::Seat {
                kind: Kind::Hardest,
                seat: Some(0),
                value: model(GROK, None),
            },
        );
        assert_eq!(roles.kinds[&Kind::Hardest], [model(GROK, expected)]);
    }

    #[test]
    fn thinking_on_an_auto_row_pins_its_model_first() {
        let mut roles = StoredRoles::default();
        edited(
            &mut roles,
            &auto_table(),
            RoleEdit::Thinking {
                kind: Kind::Hardest,
                seat: 0,
                thinking: Some(LOW.to_owned()),
            },
        );
        assert_eq!(roles.kinds[&Kind::Hardest], [model(OPUS, Some(LOW))]);
    }

    #[test]
    fn reset_returns_the_kind_to_auto() {
        let mut roles = StoredRoles {
            kinds: BTreeMap::from([(Kind::Code, vec![Seat::Inherit])]),
            ..StoredRoles::default()
        };
        edited(&mut roles, &auto_table(), RoleEdit::Reset(Kind::Code));
        assert!(roles.kinds.is_empty());
    }

    #[test]
    fn removing_an_auto_seat_keeps_the_other() {
        let mut roles = StoredRoles::default();
        edited(
            &mut roles,
            &auto_table(),
            RoleEdit::Remove {
                kind: Kind::Panel,
                seat: 0,
            },
        );
        assert_eq!(roles.kinds[&Kind::Panel], [model(GROK, None)]);
    }

    #[test]
    fn removing_the_last_seat_returns_to_auto() {
        let mut roles = StoredRoles {
            kinds: BTreeMap::from([(Kind::Panel, vec![model(OPUS, None)])]),
            ..StoredRoles::default()
        };
        let mut table = auto_table();
        table.rows[2] = row(Kind::Panel, vec![resolved(OPUS)], Source::Set);
        edited(
            &mut roles,
            &table,
            RoleEdit::Remove {
                kind: Kind::Panel,
                seat: 0,
            },
        );
        assert!(!roles.kinds.contains_key(&Kind::Panel));
    }

    #[test]
    fn effort_is_stored_and_cleared() {
        let mut roles = StoredRoles::default();
        edited(
            &mut roles,
            &auto_table(),
            RoleEdit::Effort(Some(MAX.to_owned())),
        );
        assert_eq!(roles.effort.as_deref(), Some(MAX));
        edited(&mut roles, &auto_table(), RoleEdit::Effort(None));
        assert_eq!(roles.effort, None);
    }

    #[test]
    fn list_kinds_end_on_an_add_row() {
        let shown = rows(&auto_table());
        assert_eq!(
            shown,
            [
                PstackRow::Effort,
                PstackRow::Seat {
                    kind: Kind::Code,
                    seat: 0
                },
                PstackRow::Seat {
                    kind: Kind::Hardest,
                    seat: 0
                },
                PstackRow::Seat {
                    kind: Kind::Panel,
                    seat: 0
                },
                PstackRow::Seat {
                    kind: Kind::Panel,
                    seat: 1
                },
                PstackRow::Add(Kind::Panel),
                PstackRow::Seat {
                    kind: Kind::CrossJudge,
                    seat: 0
                },
                PstackRow::Seat {
                    kind: Kind::CrossJudge,
                    seat: 1
                },
                PstackRow::Add(Kind::CrossJudge),
            ]
        );
    }

    #[test]
    fn a_stored_model_names_the_seat_while_task_cannot_choose() {
        let mut table = auto_table();
        table.task_model_choice = false;
        table.rows[1] = row(Kind::Hardest, vec![ResolvedSeat::default()], Source::Set);
        let roles = PstackRoles::new(
            table,
            StoredRoles {
                kinds: BTreeMap::from([(Kind::Hardest, vec![model(GROK, Some(HIGH))])]),
                ..StoredRoles::default()
            },
            levels_of,
        );
        assert_eq!(roles.seat_model(Kind::Hardest, 0), Some(GROK));
        assert_eq!(roles.stored_thinking(Kind::Hardest, 0), Some(HIGH));
        assert_eq!(roles.levels(GROK), owned(GROK_LEVELS));
    }
}
