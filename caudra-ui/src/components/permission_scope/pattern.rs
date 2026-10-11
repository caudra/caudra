use caudra_storage::permission_patterns::{
    ArgumentDomain, OptionLikePolicy, PatternDefinition, SlotCombinations, SlotId,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use std::collections::BTreeSet;

use super::controls::domain_name;
use super::model::{literal, safe};
use super::view::{ScopeControl, ScopeView, tuple_rows};
use crate::components::input::CLICK_OR_PRESS;
use crate::components::tooltip::TipArea;
use crate::theme::Theme;

pub(crate) const PATTERN_CHIP_ROWS: u16 = 3;
pub(crate) const EVIDENCE_LABEL: &str = "Seen before (for reference): ";
pub(crate) const INDEPENDENT_VALUES: &str = "Any combination, including new ones";
pub(crate) const REFUSED_OPTIONS: &str = "Values that look like options are refused.";
const DATA_OPTIONS: &str = "Values that look like options are allowed only where data is expected.";
pub(crate) const SAME_VALUE: &str = "A ◆ value that appears twice must be the same in both places.";
const FIELD_LABEL_WIDTH: u16 = 20;
const FIELD_HEADER_ROWS: u16 = 1;
const TIP_NAME: &str = "A display label only. It doesn't change what matches.";
const TIP_SLOT: &str = "The variable argument being edited, numbered ◆ in the chips above.";
const TIP_SLOT_NAME: &str = "A label such as <package>, for display only.";
const TIP_VALUES: &str = "Values: only the selected values seen before.";
const TIP_EXACT: &str = "Exact: one exact literal value.";
const TIP_GLOB: &str = "Glob: one whole argument matched by a glob. * stops at /, ** crosses it.";
const TIP_REGEX: &str = "Regex: one whole argument matched by a regular expression, without look-around or backreferences.";
const TIP_ANY: &str = "Any: any one literal argument. Values that look like options stay refused.";
const TIP_WIDENED: &str = "Glob, Regex and Any need Enter again before they're stored.";
const TIP_CONSTRAINT: &str = "The values, glob or expression this slot accepts.";
const TIP_TUPLES: &str = "Only combinations of values seen together are allowed.";
const TIP_INDEPENDENT: &str =
    "Each slot's values combine freely, new pairs included. Needs Enter again before it's stored.";
const TIP_OBSERVATIONS: &str = "Values this slot was seen with, for reference.";
const TIP_OBSERVED_VALUE: &str = "[x] values are allowed, [ ] values are left out.";
const TIP_TOGGLE: &str = "Click to include or exclude it.";
const TIP_SLOT_CHIP: &str = "a variable argument.";
const TIP_EDIT: &str = "Click to edit it.";
const KEYS_NAME: &str = "N to edit it";
const KEYS_SLOT: &str = "↑ ↓ to pick another";
const KEYS_SLOT_NAME: &str = "n to edit it";
const KEYS_MODE: &str = "← → or 1-5 to change it";
const KEYS_CONSTRAINT: &str = "e to edit it";
const KEYS_COMBINATIONS: &str = "c to switch";
const KEYS_OBSERVATIONS: &str = "o to show or hide them";

/// How a slot treats values that start with a dash.
pub(crate) fn option_values(policy: &OptionLikePolicy) -> &'static str {
    match policy {
        OptionLikePolicy::Reject => REFUSED_OPTIONS,
        OptionLikePolicy::AllowForProvenData => DATA_OPTIONS,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatternControl {
    Name,
    Slot,
    SlotName,
    Mode,
    Constraint,
    Combinations,
    Observations,
    ObservedValue(usize),
    SelectSlot(SlotId),
}

pub(crate) struct PatternField {
    pub(crate) control: PatternControl,
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

pub(crate) struct PatternPanel {
    pub(crate) definition: Box<PatternDefinition>,
    pub(crate) slot: usize,
    pub(crate) supplied: BTreeSet<String>,
    pub(crate) show_values: bool,
    pub(crate) caution: Option<String>,
    pub(crate) evidence: String,
}

impl PatternPanel {
    pub(crate) fn fields(&self) -> Vec<PatternField> {
        let mut fields = vec![PatternField {
            control: PatternControl::Name,
            label: "Name (N)",
            value: safe(&self.definition.name),
        }];
        if let Some(slot) = self.definition.slots.get(self.slot) {
            fields.extend([
                PatternField {
                    control: PatternControl::Slot,
                    label: "Slot (↑↓)",
                    value: format!(
                        "{} of {} · ◆{}",
                        self.slot + 1,
                        self.definition.slots.len(),
                        slot.id.0
                    ),
                },
                PatternField {
                    control: PatternControl::SlotName,
                    label: "Slot name (n)",
                    value: safe(&slot.label),
                },
                PatternField {
                    control: PatternControl::Mode,
                    label: "Match (←→)",
                    value: domain_name(&slot.domain).into(),
                },
                PatternField {
                    control: PatternControl::Constraint,
                    label: "Value (e)",
                    value: match &slot.domain {
                        ArgumentDomain::ObservedSet { values } => {
                            format!("{} allowed · o lists the values seen", values.len())
                        }
                        ArgumentDomain::Exact { value } => literal(value),
                        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
                            literal(pattern)
                        }
                        ArgumentDomain::AnyLiteralArgument => {
                            "Any one argument, taken literally".into()
                        }
                    },
                },
            ]);
        }
        fields.push(PatternField {
            control: PatternControl::Combinations,
            label: "Combinations (c)",
            value: match &self.definition.combinations {
                SlotCombinations::ObservedTuples { tuples } => {
                    format!("Only the {} combinations seen before", tuples.len())
                }
                SlotCombinations::Independent => INDEPENDENT_VALUES.into(),
            },
        });
        fields.push(PatternField {
            control: PatternControl::Observations,
            label: "Values seen (o)",
            value: format!(
                "[{}] {} values",
                if self.show_values { "-" } else { "+" },
                self.supplied.len()
            ),
        });
        if self.show_values
            && let Some(slot) = self.definition.slots.get(self.slot)
            && let ArgumentDomain::ObservedSet { values } = &slot.domain
        {
            fields.extend(
                self.supplied
                    .iter()
                    .enumerate()
                    .map(|(index, value)| PatternField {
                        control: PatternControl::ObservedValue(index),
                        label: if values.contains(value) {
                            "[x] Allowed"
                        } else {
                            "[ ] Excluded"
                        },
                        value: literal(value),
                    }),
            );
        }
        fields
    }

    pub(crate) fn details(&self, theme: &Theme) -> Vec<Line<'static>> {
        let mut details = Vec::new();
        if let Some(slot) = self.definition.slots.get(self.slot) {
            details.push(Line::styled(
                option_values(&slot.option_like),
                theme.item_desc,
            ));
            details.push(Line::styled(SAME_VALUE, theme.item_desc));
        }
        if self.show_values {
            details.extend(tuple_rows(&self.definition));
            details.push(Line::from(format!(
                "{EVIDENCE_LABEL}{}",
                safe(&self.evidence)
            )));
            details.extend(
                self.supplied
                    .iter()
                    .map(|value| Line::from(format!("Seen: {}", literal(value)))),
            );
        }
        details
    }

    pub(crate) fn height(&self, width: u16, theme: &Theme) -> u16 {
        let value_width = width
            .saturating_sub(FIELD_LABEL_WIDTH.min(width / 2))
            .max(1);
        let fields = self
            .fields()
            .iter()
            .map(|field| wrapped_height(&field.value, value_width))
            .fold(0u16, u16::saturating_add);
        let details = Paragraph::new(self.details(theme))
            .wrap(Wrap { trim: false })
            .line_count(width.max(1));
        PATTERN_CHIP_ROWS
            .saturating_add(FIELD_HEADER_ROWS)
            .saturating_add(fields)
            .saturating_add(u16::try_from(details).unwrap_or(u16::MAX))
            .saturating_add(
                self.caution
                    .as_deref()
                    .map_or(0, |caution| wrapped_height(caution, width)),
            )
    }

    pub(crate) fn render(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
        focused: Option<&PatternControl>,
    ) -> (Vec<(Rect, PatternControl)>, Vec<TipArea>) {
        let mut hits = Vec::new();
        let mut y = area.y;
        if let Some(caution) = &self.caution {
            let height = wrapped_height(caution, area.width).min(area.bottom().saturating_sub(y));
            Paragraph::new(caution.as_str())
                .style(theme.tool_warning)
                .wrap(Wrap { trim: false })
                .render(Rect { y, height, ..area }, buffer);
            y += height;
        }
        let chips = Rect {
            y,
            height: PATTERN_CHIP_ROWS.min(area.bottom().saturating_sub(y)),
            ..area
        };
        let selected = self.definition.slots.get(self.slot).map(|slot| slot.id);
        let (chip_hits, tips) =
            ScopeView::pattern_chips(&self.definition, selected, chips, buffer, theme);
        hits.extend(chip_hits.into_iter().filter_map(|hit| {
            if let ScopeControl::Slot(id) = hit.control {
                Some((hit.area, PatternControl::SelectSlot(id)))
            } else {
                None
            }
        }));
        y += chips.height;
        if y < area.bottom() {
            Paragraph::new("Field               Setting")
                .style(theme.panel_title)
                .render(
                    Rect {
                        y,
                        height: FIELD_HEADER_ROWS,
                        ..area
                    },
                    buffer,
                );
            y += FIELD_HEADER_ROWS;
        }
        for field in self.fields() {
            if y >= area.bottom() {
                break;
            }
            let value_width = area
                .width
                .saturating_sub(FIELD_LABEL_WIDTH.min(area.width / 2))
                .max(1);
            let height =
                wrapped_height(&field.value, value_width).min(area.bottom().saturating_sub(y));
            let row = Rect { y, height, ..area };
            let [label, value] = Layout::horizontal([
                Constraint::Length(FIELD_LABEL_WIDTH.min(area.width / 2)),
                Constraint::Min(1),
            ])
            .areas(row);
            let style = if focused == Some(&field.control) {
                theme.item_selected
            } else {
                theme.item
            };
            Paragraph::new(field.label)
                .style(style)
                .render(label, buffer);
            Paragraph::new(field.value)
                .style(style)
                .wrap(Wrap { trim: false })
                .render(value, buffer);
            hits.push((row, field.control));
            y += height;
        }
        Paragraph::new(self.details(theme))
            .style(theme.item)
            .wrap(Wrap { trim: false })
            .render(
                Rect {
                    y,
                    height: area.bottom().saturating_sub(y),
                    ..area
                },
                buffer,
            );
        (hits, tips)
    }

    /// What a control means and how to change it, for its tooltip.
    pub(crate) fn tip(&self, control: &PatternControl) -> Option<String> {
        let slot = self.definition.slots.get(self.slot);
        let (meaning, keys) = match control {
            PatternControl::Name => (TIP_NAME.into(), KEYS_NAME),
            PatternControl::Slot => (TIP_SLOT.into(), KEYS_SLOT),
            PatternControl::SlotName => (TIP_SLOT_NAME.into(), KEYS_SLOT_NAME),
            PatternControl::Mode => {
                let mode = match &slot?.domain {
                    ArgumentDomain::ObservedSet { .. } => TIP_VALUES,
                    ArgumentDomain::Exact { .. } => TIP_EXACT,
                    ArgumentDomain::Glob { .. } => TIP_GLOB,
                    ArgumentDomain::Regex { .. } => TIP_REGEX,
                    ArgumentDomain::AnyLiteralArgument => TIP_ANY,
                };
                (format!("{mode}\n{TIP_WIDENED}"), KEYS_MODE)
            }
            PatternControl::Constraint => (TIP_CONSTRAINT.into(), KEYS_CONSTRAINT),
            PatternControl::Combinations => (
                match self.definition.combinations {
                    SlotCombinations::ObservedTuples { .. } => TIP_TUPLES,
                    SlotCombinations::Independent => TIP_INDEPENDENT,
                }
                .into(),
                KEYS_COMBINATIONS,
            ),
            PatternControl::Observations => (TIP_OBSERVATIONS.into(), KEYS_OBSERVATIONS),
            PatternControl::ObservedValue(_) => {
                return Some(format!("{TIP_OBSERVED_VALUE}\n{TIP_TOGGLE}"));
            }
            PatternControl::SelectSlot(id) => {
                return Some(format!("Slot ◆{}: {TIP_SLOT_CHIP}\n{TIP_EDIT}", id.0));
            }
        };
        Some(format!("{meaning}\n{CLICK_OR_PRESS}{keys}."))
    }
}

fn wrapped_height(text: &str, width: u16) -> u16 {
    u16::try_from(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width.max(1)),
    )
    .unwrap_or(u16::MAX)
    .max(1)
}

#[cfg(test)]
mod tests {
    use caudra_storage::permission_patterns::{
        ArgumentDomain, ArgumentRole, PatternToken, SlotCombinations, SlotId,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use test_case::test_case;

    use super::{
        KEYS_MODE, PatternControl, PatternPanel, TIP_ANY, TIP_EXACT, TIP_GLOB, TIP_INDEPENDENT,
        TIP_REGEX, TIP_TUPLES, TIP_VALUES,
    };
    use crate::components::buffer_text;
    use crate::components::input::CLICK_OR_PRESS;
    use crate::components::permission_scope::tests::template;
    use crate::components::permission_scope::view::{ALLOWED_COMBINATIONS, TIP_EXECUTABLE};
    use crate::theme;

    const WIDTH: u16 = 80;
    const SLOT: SlotId = SlotId(1);
    const SECRET: &str = "never-show-sensitive-payload";
    const EVIDENCE: &str = "Host-supplied proposal evidence";
    const EMPTY_LITERAL: &str = "\"\"";
    const TUPLE_PATH: &str = "src/main.rs";
    const GLOB: &str = "src/*.rs";
    const NO_TIP: &str = "this control must raise a tooltip";
    const WRONG_TIP: &str = "the tooltip does not describe the control's current setting";

    fn panel() -> PatternPanel {
        PatternPanel {
            definition: Box::new(template()),
            slot: 0,
            supplied: Default::default(),
            show_values: false,
            caution: None,
            evidence: EVIDENCE.into(),
        }
    }

    #[test_case(40, "ayu_dark"; "narrow_dark")]
    #[test_case(80, "ayu_dark"; "normal_dark")]
    #[test_case(140, "ayu_dark"; "wide_dark")]
    #[test_case(40, "ayu_light"; "narrow_light")]
    #[test_case(80, "ayu_light"; "normal_light")]
    #[test_case(140, "ayu_light"; "wide_light")]
    fn shared_panel_preserves_typed_cells_and_linked_slot_hits(width: u16, name: &str) {
        let panel = panel();
        let theme = theme::load_by_name(name).unwrap();
        let area = Rect::new(0, 0, width, panel.height(width, &theme));
        let mut buffer = Buffer::empty(area);
        let (hits, _) = panel.render(area, &mut buffer, &theme, Some(&PatternControl::Mode));
        assert!(buffer_text(&buffer).contains(EMPTY_LITERAL));
        let linked: Vec<_> = hits
            .iter()
            .filter(|(_, control)| *control == PatternControl::SelectSlot(SLOT))
            .collect();
        assert_eq!(linked.len(), 2);
        for (hit, _) in linked {
            for x in hit.x..hit.right() {
                assert_eq!(buffer[(x, hit.y)].bg, theme.item_selected.bg.unwrap());
            }
        }
        let (mode, _) = hits
            .iter()
            .find(|(_, control)| *control == PatternControl::Mode)
            .unwrap();
        assert_eq!(buffer[(mode.x, mode.y)].bg, theme.item_selected.bg.unwrap());
        for (hit, _) in &hits {
            assert_eq!(area.intersection(*hit), *hit);
        }
        for field in panel.fields() {
            assert!(hits.iter().any(|(_, control)| *control == field.control));
        }
    }

    #[test_case(ArgumentRole::Sensitive; "sensitive")]
    #[test_case(ArgumentRole::Payload; "payload")]
    fn shared_panel_redacts_fixed_sensitive_arguments(role: ArgumentRole) {
        let mut panel = panel();
        panel.definition.argv.push(PatternToken::Exact {
            value: SECRET.into(),
            role,
        });
        let theme = theme::load_by_name("ayu_dark").unwrap();
        let area = Rect::new(0, 0, WIDTH, panel.height(WIDTH, &theme));
        let mut buffer = Buffer::empty(area);
        panel.render(area, &mut buffer, &theme, None);
        let text = buffer_text(&buffer);
        assert!(text.contains("[redacted]"));
        assert!(!text.contains(SECRET));
    }

    #[test_case(false; "collapsed")]
    #[test_case(true; "disclosed")]
    fn tuple_matrix_and_evidence_follow_disclosure_without_changing_authority(disclosed: bool) {
        let mut panel = panel();
        let original = panel.definition.clone();
        panel.show_values = disclosed;
        let details = panel.details(&theme::load_by_name("ayu_dark").unwrap());
        let text = details
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.contains(TUPLE_PATH), disclosed);
        assert_eq!(text.contains(EVIDENCE), disclosed);
        assert_eq!(text.contains(ALLOWED_COMBINATIONS), disclosed);
        assert_eq!(panel.definition, original);
    }

    #[test_case(ArgumentDomain::ObservedSet { values: Default::default() }, TIP_VALUES ; "values")]
    #[test_case(ArgumentDomain::Exact { value: SECRET.into() }, TIP_EXACT ; "exact")]
    #[test_case(ArgumentDomain::Glob { pattern: GLOB.into() }, TIP_GLOB ; "glob")]
    #[test_case(ArgumentDomain::Regex { pattern: GLOB.into() }, TIP_REGEX ; "regex")]
    #[test_case(ArgumentDomain::AnyLiteralArgument, TIP_ANY ; "any")]
    fn the_match_tip_explains_the_current_mode(domain: ArgumentDomain, meaning: &str) {
        let mut panel = panel();
        panel.definition.slots[0].domain = domain;
        let text = panel.tip(&PatternControl::Mode).expect(NO_TIP);
        assert!(text.starts_with(meaning), "{WRONG_TIP}: {text}");
        assert!(
            text.ends_with(&format!("{CLICK_OR_PRESS}{KEYS_MODE}.")),
            "{WRONG_TIP}: {text}"
        );
    }

    #[test_case(SlotCombinations::Independent, TIP_INDEPENDENT ; "independent")]
    #[test_case(SlotCombinations::ObservedTuples { tuples: Default::default() }, TIP_TUPLES ; "tuples")]
    fn the_combinations_tip_follows_the_setting(combinations: SlotCombinations, meaning: &str) {
        let mut panel = panel();
        panel.definition.combinations = combinations;
        let text = panel.tip(&PatternControl::Combinations).expect(NO_TIP);
        assert!(text.starts_with(meaning), "{WRONG_TIP}: {text}");
    }

    #[test]
    fn fixed_chips_explain_their_role_and_slots_stay_controls() {
        let panel = panel();
        let theme = theme::load_by_name("ayu_dark").unwrap();
        let area = Rect::new(0, 0, WIDTH, panel.height(WIDTH, &theme));
        let mut buffer = Buffer::empty(area);
        let (hits, tips) = panel.render(area, &mut buffer, &theme, None);
        assert!(
            tips.iter().any(|tip| tip.text == TIP_EXECUTABLE),
            "{NO_TIP}"
        );
        for tip in &tips {
            assert!(
                !hits.iter().any(|(hit, _)| hit.intersects(tip.area)),
                "a fixed chip must not overlap a slot chip"
            );
        }
    }
}
