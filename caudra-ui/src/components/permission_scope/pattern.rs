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
use crate::theme::Theme;

pub(crate) const PATTERN_CHIP_ROWS: u16 = 3;
const FIELD_LABEL_WIDTH: u16 = 20;
const FIELD_HEADER_ROWS: u16 = 1;

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
                    label: "Slot (Up/Down)",
                    value: format!(
                        "{} / {} · ◆{}",
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
                    label: "Match (Left/Right)",
                    value: domain_name(&slot.domain).into(),
                },
                PatternField {
                    control: PatternControl::Constraint,
                    label: "Constraint (e)",
                    value: match &slot.domain {
                        ArgumentDomain::ObservedSet { values } => format!(
                            "{} allowed values · inspect supplied values below",
                            values.len()
                        ),
                        ArgumentDomain::Exact { value } => literal(value),
                        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
                            literal(pattern)
                        }
                        ArgumentDomain::AnyLiteralArgument => {
                            "One literal argument, not command syntax".into()
                        }
                    },
                },
            ]);
        }
        fields.push(PatternField {
            control: PatternControl::Combinations,
            label: "Combinations (c)",
            value: match &self.definition.combinations {
                SlotCombinations::ObservedTuples { tuples } => format!(
                    "LISTED · {} allowed tuples · ANY OF rows / ALL OF cells",
                    tuples.len()
                ),
                SlotCombinations::Independent => {
                    "INDEPENDENT · new cross-products permitted".into()
                }
            },
        });
        fields.push(PatternField {
            control: PatternControl::Observations,
            label: "Supplied values (o)",
            value: format!(
                "[{}] {} values · evidence is not editable authority",
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
            details.push(Line::styled(match slot.option_like { OptionLikePolicy::Reject => "Option-looking values: rejected (fixed guard).", OptionLikePolicy::AllowForProvenData => "Option-looking values: allowed only at proven data positions (fixed guard)." }, theme.item_desc));
            details.push(Line::styled(
                "Repeated ◆IDs require equal arguments; roles and eligibility are host-derived.",
                theme.item_desc,
            ));
        }
        if self.show_values {
            details.extend(tuple_rows(&self.definition));
            details.push(Line::styled(
                "Supplied proposal values are not a historical support count.",
                theme.item_desc,
            ));
            details.push(Line::from(format!(
                "Proposal evidence: {}",
                safe(&self.evidence)
            )));
            details.extend(
                self.supplied
                    .iter()
                    .map(|value| Line::from(format!("Supplied: {}", literal(value)))),
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
    ) -> Vec<(Rect, PatternControl)> {
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
        hits.extend(
            ScopeView::pattern_chips(&self.definition, selected, chips, buffer, theme)
                .into_iter()
                .filter_map(|hit| {
                    if let ScopeControl::Slot(id) = hit.control {
                        Some((hit.area, PatternControl::SelectSlot(id)))
                    } else {
                        None
                    }
                }),
        );
        y += chips.height;
        if y < area.bottom() {
            Paragraph::new("Argument / property     Match and allowed constraint")
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
        hits
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
    use caudra_storage::permission_patterns::{ArgumentRole, PatternToken, SlotId};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use test_case::test_case;

    use super::{PatternControl, PatternPanel};
    use crate::components::buffer_text;
    use crate::components::permission_scope::tests::template;
    use crate::theme;

    const WIDTH: u16 = 80;
    const SLOT: SlotId = SlotId(1);
    const SECRET: &str = "never-show-sensitive-payload";
    const EVIDENCE: &str = "Host-supplied proposal evidence";
    const EMPTY_LITERAL: &str = "\"\"";
    const TUPLE_PATH: &str = "src/main.rs";

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
        let hits = panel.render(area, &mut buffer, &theme, Some(&PatternControl::Mode));
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
        assert_eq!(text.contains("ANY OF"), disclosed);
        assert_eq!(text.contains("ALL OF"), disclosed);
        assert_eq!(panel.definition, original);
    }
}
