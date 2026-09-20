use caudra_grab::grab_scope;
use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, OptionLikePolicy, PatternDefinition, PatternToken,
    SlotCombinations, SlotId,
};
use caudra_storage::permission_state::{PermissionArgumentConstraint, PermissionResourceSelector};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;

use super::controls::domain_name;
use super::model::{
    ALL_RESOURCES, ScopeModel, ScopeSource, access_name, literal, resource_kind, safe,
    selector_mode, selector_value,
};
use crate::theme::Theme;

const TABLE_WIDE: u16 = 68;
const TARGET_ROWS: usize = 3;
const CHIP_ROWS: u16 = 3;
const PROPERTY_ROWS: u16 = 4;
const MIN_SCOPE_HEIGHT: u16 = 14;
const NAVIGATION_WIDTH: u16 = 7;
const NAVIGATION_BUTTON_WIDTH: u16 = 3;
const PAGER_BUTTON_WIDTH: u16 = 6;
const PAGER_GAP: u16 = 1;
const PAGER_WIDTH: u16 = (PAGER_BUTTON_WIDTH + PAGER_GAP) * 2;
const DISCLOSURES: [(Disclosure, &str); 4] = [
    (Disclosure::Conditions, "[ALL OF]"),
    (Disclosure::Combinations, "[Tuples]"),
    (Disclosure::Identity, "[ID]"),
    (Disclosure::Evidence, "[Evidence]"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Disclosure {
    Conditions,
    Combinations,
    Identity,
    Evidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopeControl {
    Target(usize),
    Slot(SlotId),
    Disclosure(Disclosure),
    Scroll(i16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScopeHit {
    pub(crate) area: Rect,
    pub(crate) control: ScopeControl,
}

#[derive(Clone, Default)]
pub(crate) struct ScopeView {
    pub(crate) target: usize,
    pub(crate) slot: Option<SlotId>,
    pub(crate) disclosure: Option<Disclosure>,
    pub(crate) offset: u16,
    pub(crate) hits: Vec<ScopeHit>,
    focus: usize,
    pressed: Option<ScopeHit>,
    area: Rect,
}

impl ScopeView {
    pub(crate) fn summary_height(model: &ScopeModel, width: u16) -> u16 {
        let targets = model.rule().map_or(1, |rule| rule.resources.len());
        let (has_pattern, has_slots) = (0..targets)
            .filter_map(|target| model.pattern(target))
            .fold((false, false), |(_, slots), pattern| {
                (true, slots || !pattern.slots.is_empty())
            });
        let chips = u16::from(has_pattern) * CHIP_ROWS + u16::from(has_slots);
        let target_rows = model.rule().map_or(0, |rule| {
            1 + if rule.resources.is_empty() {
                0
            } else if width >= TABLE_WIDE {
                1 + rule.resources.len().min(TARGET_ROWS) as u16
            } else {
                2
            }
        });
        let (_, tabs) = DISCLOSURES
            .iter()
            .fold((0, 1), |(mut x, mut rows), (_, label)| {
                let label_width = label.width() as u16;
                if x + label_width > width {
                    x = 0;
                    rows += 1;
                }
                (x + label_width + 1, rows)
            });
        (badge_lines(model, width).len() as u16)
            .saturating_add(
                u16::try_from(warning_paragraph(model).line_count(width.max(1)))
                    .unwrap_or(u16::MAX),
            )
            .saturating_add(chips + target_rows + tabs + PROPERTY_ROWS + 1)
            .max(MIN_SCOPE_HEIGHT)
    }

    pub(crate) fn pattern_chips(
        definition: &PatternDefinition,
        selected: Option<SlotId>,
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
    ) -> Vec<ScopeHit> {
        let mut view = Self {
            slot: selected,
            ..Self::default()
        };
        let mut y = area.y;
        view.chips(definition, area, &mut y, buffer, theme);
        view.hits
    }
    pub(crate) fn activate(&mut self, control: ScopeControl) {
        match control {
            ScopeControl::Scroll(delta) => {
                self.scroll(i32::from(delta));
                return;
            }
            ScopeControl::Target(index) => {
                self.target = index;
                self.slot = None;
            }
            ScopeControl::Slot(id) => {
                self.slot = Some(id);
                self.disclosure = None;
            }
            ScopeControl::Disclosure(disclosure) => {
                self.disclosure = if self.disclosure.as_ref() == Some(&disclosure) {
                    None
                } else {
                    Some(disclosure)
                };
            }
        }
        self.offset = 0;
        self.pressed = None;
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        self.offset = self
            .offset
            .saturating_add_signed(delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        self.pressed = None;
    }

    pub(crate) fn handle_key(&mut self, event: KeyEvent) -> bool {
        if event.kind == KeyEventKind::Release {
            return false;
        }
        match event.code {
            KeyCode::PageDown => self.scroll(PROPERTY_ROWS.into()),
            KeyCode::PageUp => self.scroll(-i32::from(PROPERTY_ROWS)),
            KeyCode::Left | KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Right | KeyCode::Down => {
                self.focus = (self.focus + 1).min(self.hits.len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Char(' ') if event.kind == KeyEventKind::Press => {
                if let Some(hit) = self.hits.get(self.focus) {
                    self.activate(hit.control.clone());
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let position = Position::new(event.column, event.row);
        if !self.area.contains(position) {
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.pressed = None;
            }
            return false;
        }
        let hit = self
            .hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .cloned();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.pressed = hit,
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(pressed) = self.pressed.take()
                    && Some(&pressed) == hit.as_ref()
                {
                    self.activate(pressed.control);
                }
            }
            MouseEventKind::ScrollDown => self.scroll(1),
            MouseEventKind::ScrollUp => self.scroll(-1),
            _ => {}
        }
        true
    }

    pub(crate) fn render(
        &mut self,
        model: &ScopeModel,
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
    ) {
        grab_scope!("permission_scope", area);
        self.render_with_properties(model, area, buffer, theme, None);
    }

    pub(crate) fn render_with_properties(
        &mut self,
        model: &ScopeModel,
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
        properties: Option<Vec<Line<'static>>>,
    ) {
        let old_hits = self.hits.clone();
        if self.area != area {
            self.pressed = None;
        }
        self.area = area;
        self.hits.clear();
        if area.is_empty() {
            return;
        }
        buffer.set_style(area, theme.surface_style());
        let short = area.height < MIN_SCOPE_HEIGHT;
        let mut y = area.y;
        for line in
            badge_lines(model, area.width)
                .into_iter()
                .take(if short { 1 } else { usize::MAX })
        {
            paint(buffer, area, &mut y, Line::styled(line, theme.panel_title));
        }
        let warnings = warning_paragraph(model);
        let warning_height = u16::try_from(warnings.line_count(area.width.max(1)))
            .unwrap_or(u16::MAX)
            .min(area.bottom().saturating_sub(y));
        warnings.style(theme.tool_warning).render(
            Rect {
                y,
                height: warning_height,
                ..area
            },
            buffer,
        );
        y = y.saturating_add(warning_height);
        if let Some(pattern) = model.pattern(self.target) {
            let chips = Rect {
                y: y.min(area.bottom()),
                height: area
                    .bottom()
                    .saturating_sub(y)
                    .min(if short { 1 } else { CHIP_ROWS }),
                ..area
            };
            self.chips(pattern, chips, &mut y, buffer, theme);
            if !pattern.slots.is_empty() {
                let index = self
                    .slot
                    .and_then(|id| pattern.slots.iter().position(|slot| slot.id == id))
                    .unwrap_or_default();
                let row = Rect {
                    y: y.min(area.bottom()),
                    height: u16::from(y < area.bottom()),
                    ..area
                };
                paint(
                    buffer,
                    area,
                    &mut y,
                    Line::styled(
                        format!("Slots · {} / {}", index + 1, pattern.slots.len()),
                        theme.panel_title,
                    ),
                );
                self.navigation(
                    row,
                    ScopeControl::Slot(
                        pattern.slots[(index + pattern.slots.len() - 1) % pattern.slots.len()].id,
                    ),
                    ScopeControl::Slot(pattern.slots[(index + 1) % pattern.slots.len()].id),
                    buffer,
                    theme,
                );
            }
        }
        if let Some(rule) = model.rule() {
            let count = rule.resources.len();
            let title_y = y;
            paint(
                buffer,
                area,
                &mut y,
                Line::styled(format!("Targets · ANY OF ({count})"), theme.panel_title),
            );
            if count > 0 {
                self.target = self.target.min(count - 1);
                if count > 1 {
                    self.navigation(
                        Rect {
                            y: title_y.min(area.bottom()),
                            height: u16::from(title_y < area.bottom()),
                            ..area
                        },
                        ScopeControl::Target((self.target + count - 1) % count),
                        ScopeControl::Target((self.target + 1) % count),
                        buffer,
                        theme,
                    );
                }
                let wide = area.width >= TABLE_WIDE;
                let columns = target_columns(Rect {
                    y: y.min(area.bottom()),
                    height: u16::from(y < area.bottom()),
                    ..area
                });
                for (column, title) in columns
                    .iter()
                    .zip(["Resource/access", "Match mode", "Target", "ALL OF"])
                    .filter(|_| !short && wide)
                {
                    Paragraph::new(title)
                        .style(theme.item_desc)
                        .render(*column, buffer);
                }
                y = y.saturating_add(u16::from(!short && wide));
                let visible = if short || !wide { 1 } else { TARGET_ROWS };
                let start = self.target.saturating_sub(visible - 1);
                for index in start..count.min(start + visible) {
                    if y >= area.bottom() {
                        break;
                    }
                    let resource = &rule.resources[index];
                    let row = Rect {
                        y,
                        height: (u16::from(!wide && !short) + 1).min(area.bottom() - y),
                        ..area
                    };
                    let selected = index == self.target;
                    let style = if selected {
                        theme.item_selected
                    } else {
                        theme.item
                    };
                    buffer.set_style(row, style);
                    let cells = [
                        format!(
                            "{} {}/{}",
                            index + 1,
                            resource_kind(&resource.kind),
                            access_name(resource.access.as_ref())
                        ),
                        selector_mode(&resource.selector).into(),
                        model.target_text(index),
                        format!("{} guards", resource.attributes.len() + 2),
                    ];
                    if wide {
                        for (column, value) in target_columns(row).into_iter().zip(cells) {
                            Paragraph::new(value).style(style).render(column, buffer);
                        }
                    } else {
                        Paragraph::new(vec![
                            Line::from(cells[0].clone()),
                            Line::from(format!("{} · {}", cells[1], cells[2])),
                        ])
                        .style(style)
                        .render(row, buffer);
                    }
                    self.hits.push(ScopeHit {
                        area: row,
                        control: ScopeControl::Target(index),
                    });
                    y += row.height;
                }
            }
        }
        let mut x = area.x;
        for (disclosure, label) in DISCLOSURES {
            let width = label.width() as u16;
            if x + width > area.right() {
                y += 1;
                x = area.x;
            }
            if y >= area.bottom() {
                break;
            }
            let tab = Rect::new(x, y, width.min(area.width), 1);
            let style = if self.disclosure.as_ref() == Some(&disclosure) {
                theme.item_selected
            } else {
                theme.keybind_key
            };
            Paragraph::new(label).style(style).render(tab, buffer);
            self.hits.push(ScopeHit {
                area: tab,
                control: ScopeControl::Disclosure(disclosure),
            });
            x += width + 1;
        }
        y += 1;
        let footer_height = u16::from(y < area.bottom());
        let property = Rect {
            y: y.min(area.bottom()),
            height: area
                .bottom()
                .saturating_sub(y)
                .saturating_sub(footer_height),
            ..area
        };
        let rows = properties.unwrap_or_else(|| self.properties(model));
        let paragraph = Paragraph::new(rows)
            .style(theme.item)
            .wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(property.width.max(1))).unwrap_or(u16::MAX);
        self.offset = self.offset.min(total.saturating_sub(property.height));
        paragraph.scroll((self.offset, 0)).render(property, buffer);
        if footer_height > 0 {
            let footer = Rect {
                y: area.bottom() - 1,
                height: 1,
                ..area
            };
            let paging = total > property.height && footer.width >= PAGER_WIDTH;
            let hint = if total > property.height {
                format!("↑↓ choose · {}/{}", self.offset + 1, total)
            } else {
                "↑↓ choose · Space inspect · Tab back".into()
            };
            Paragraph::new(hint).style(theme.item_desc).render(
                Rect {
                    width: footer
                        .width
                        .saturating_sub(if paging { PAGER_WIDTH } else { 0 }),
                    ..footer
                },
                buffer,
            );
            if paging {
                for (index, (label, delta)) in [
                    ("[PgUp]", -(PROPERTY_ROWS as i16)),
                    ("[PgDn]", PROPERTY_ROWS as i16),
                ]
                .into_iter()
                .enumerate()
                {
                    let area = Rect::new(
                        footer.right() - PAGER_WIDTH
                            + index as u16 * (PAGER_BUTTON_WIDTH + PAGER_GAP),
                        footer.y,
                        PAGER_BUTTON_WIDTH,
                        1,
                    );
                    Paragraph::new(label)
                        .style(theme.keybind_key)
                        .render(area, buffer);
                    self.hits.push(ScopeHit {
                        area,
                        control: ScopeControl::Scroll(delta),
                    });
                }
            }
        }
        self.focus = self.focus.min(self.hits.len().saturating_sub(1));
        if let Some(hit) = self.hits.get(self.focus) {
            buffer.set_style(
                hit.area,
                Style::default().add_modifier(Modifier::UNDERLINED),
            );
        }
        if self.hits != old_hits {
            self.pressed = None;
        }
    }

    fn navigation(
        &mut self,
        row: Rect,
        previous: ScopeControl,
        next: ScopeControl,
        buffer: &mut Buffer,
        theme: &Theme,
    ) {
        if row.is_empty() || row.width < NAVIGATION_WIDTH {
            return;
        }
        for (x, label, control) in [
            (row.right() - NAVIGATION_WIDTH, "[<]", previous),
            (row.right() - NAVIGATION_BUTTON_WIDTH, "[>]", next),
        ] {
            let area = Rect::new(x, row.y, NAVIGATION_BUTTON_WIDTH, 1);
            Paragraph::new(label)
                .style(theme.keybind_key)
                .render(area, buffer);
            self.hits.push(ScopeHit { area, control });
        }
    }

    fn chips(
        &mut self,
        pattern: &PatternDefinition,
        area: Rect,
        y: &mut u16,
        buffer: &mut Buffer,
        theme: &Theme,
    ) {
        let start = *y;
        let mut x = area.x;
        for token in &pattern.argv {
            let (label, style, control) = match token {
                PatternToken::Exact { value, role } => {
                    let tag = match role {
                        ArgumentRole::Executable => "exe",
                        ArgumentRole::Operation => "op",
                        ArgumentRole::Flag | ArgumentRole::OptionTerminator => "flag",
                        ArgumentRole::Data => "fixed",
                        ArgumentRole::Unknown => "?",
                        ArgumentRole::Sensitive | ArgumentRole::Payload => "hidden",
                    };
                    let value = if matches!(role, ArgumentRole::Sensitive | ArgumentRole::Payload) {
                        "[redacted]".into()
                    } else {
                        literal(value)
                    };
                    (format!("[{tag} {value}]"), theme.inline_code, None)
                }
                PatternToken::Slot { id, .. } => {
                    let label = pattern
                        .slots
                        .iter()
                        .find(|slot| slot.id == *id)
                        .map_or_else(|| format!("#{}", id.0), |slot| safe(&slot.label));
                    let style = if self.slot == Some(*id) {
                        theme.item_selected
                    } else {
                        theme.accent
                    };
                    (
                        format!("[◆{} {label}]", id.0),
                        style,
                        Some(ScopeControl::Slot(*id)),
                    )
                }
            };
            let width = (label.width() as u16).min(area.width);
            let next_y = (*y).saturating_add(u16::from(x + width > area.right()));
            if next_y >= area.bottom() || next_y >= start + CHIP_ROWS {
                break;
            }
            if next_y != *y {
                x = area.x;
            }
            *y = next_y;
            let cell = Rect::new(x, *y, width, 1);
            Paragraph::new(label).style(style).render(cell, buffer);
            if let Some(control) = control {
                self.hits.push(ScopeHit {
                    area: cell,
                    control,
                });
            }
            x += width + 1;
        }
        *y = (*y).saturating_add(1).min(area.bottom());
    }

    fn properties(&self, model: &ScopeModel) -> Vec<Line<'static>> {
        match self.disclosure {
            Some(Disclosure::Identity) => identity_rows(model, self.target),
            Some(Disclosure::Evidence) => evidence_rows(model),
            Some(Disclosure::Combinations) => model
                .pattern(self.target)
                .map(tuple_rows)
                .unwrap_or_else(|| {
                    vec![Line::from(
                        "No command slots or cross-resource tuple relationships.",
                    )]
                }),
            _ => {
                if self.disclosure.is_none()
                    && let Some(pattern) = model.pattern(self.target)
                    && let Some(id) = self.slot
                {
                    return slot_rows(pattern, id);
                }
                let mut rows = Vec::new();
                if let Some(rule) = model.rule() {
                    if let Some(resource) = rule.resources.get(self.target) {
                        rows.push(Line::from(format!(
                            "ALL OF · target {} · {}",
                            self.target + 1,
                            resource_kind(&resource.kind)
                        )));
                        rows.push(Line::from(format!(
                            "{}  {}",
                            selector_mode(&resource.selector),
                            model.target_text(self.target)
                        )));
                        rows.push(Line::from(format!(
                            "Access {}  ·  Protection {}",
                            access_name(resource.access.as_ref()),
                            match resource.protected {
                                Some(true) => "protected only",
                                Some(false) => "unprotected only",
                                None => "ANY",
                            }
                        )));
                        for (name, selector) in &resource.attributes {
                            rows.push(Line::from(format!(
                                "{}  {}  {}",
                                safe(name),
                                selector_mode(selector),
                                selector_detail(
                                    selector,
                                    matches!(model.source, ScopeSource::Live { .. })
                                )
                            )));
                        }
                    }
                    rows.extend(argument_rows(&rule.arguments));
                    rows.push(Line::from(ALL_RESOURCES));
                } else if let Some(pattern) = model.pattern(self.target) {
                    rows.push(Line::from(
                        "Select a ◆slot chip to inspect its allowed domain.",
                    ));
                    rows.extend(tuple_rows(pattern));
                }
                rows
            }
        }
    }
}

fn warning_paragraph(model: &ScopeModel) -> Paragraph<'static> {
    Paragraph::new(
        model
            .warnings()
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false })
}

fn target_columns(area: Rect) -> [Rect; 4] {
    let mut columns = Layout::horizontal([
        Constraint::Length(21),
        Constraint::Length(15),
        Constraint::Min(1),
        Constraint::Length(10),
    ])
    .areas::<4>(area);
    for column in &mut columns[..3] {
        column.width = column.width.saturating_sub(1);
    }
    columns
}

fn paint(buffer: &mut Buffer, area: Rect, y: &mut u16, line: Line<'static>) {
    if *y < area.bottom() {
        Paragraph::new(line).render(
            Rect {
                y: *y,
                height: 1,
                ..area
            },
            buffer,
        );
        *y += 1;
    }
}

fn badge_lines(model: &ScopeModel, width: u16) -> Vec<String> {
    let mut rows = vec![String::new()];
    for badge in model.badges() {
        let badge = format!("[{badge}]");
        if rows.last().is_some_and(|row| {
            !row.is_empty() && row.width() + badge.width() + 1 > usize::from(width)
        }) {
            rows.push(String::new());
        }
        if let Some(row) = rows.last_mut() {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&badge);
        }
    }
    rows
}

fn slot_rows(pattern: &PatternDefinition, id: SlotId) -> Vec<Line<'static>> {
    let Some(slot) = pattern.slots.iter().find(|slot| slot.id == id) else {
        return Vec::new();
    };
    let occurrences = pattern
        .argv
        .iter()
        .filter(|token| matches!(token, PatternToken::Slot { id: token_id, .. } if *token_id == id))
        .count();
    let mut rows = vec![
        Line::from(format!(
            "◆{} {} · {}",
            id.0,
            safe(&slot.label),
            domain_name(&slot.domain)
        )),
        Line::from(format!("Same ID = equal values · {occurrences} uses")),
        Line::from(match slot.option_like {
            OptionLikePolicy::Reject => "Leading '-' rejected",
            OptionLikePolicy::AllowForProvenData => "Leading '-' allowed by host-proven data role",
        }),
    ];
    match &slot.domain {
        ArgumentDomain::ObservedSet { values } => {
            rows.push(Line::from(
                "Allowed values · not a historical support claim",
            ));
            rows.extend(
                values
                    .iter()
                    .map(|value| Line::from(format!("  • {}", literal(value)))),
            );
        }
        ArgumentDomain::Exact { value } => rows.push(Line::from(literal(value))),
        ArgumentDomain::Glob { pattern } | ArgumentDomain::Regex { pattern } => {
            rows.push(Line::from(literal(pattern)));
            rows.push(Line::from(
                "Matches one complete argument; no shell expansion.",
            ));
        }
        ArgumentDomain::AnyLiteralArgument => rows.push(Line::from(
            "One argument, not executable syntax or a command fragment.",
        )),
    }
    rows
}

pub(super) fn tuple_rows(pattern: &PatternDefinition) -> Vec<Line<'static>> {
    match &pattern.combinations {
        SlotCombinations::Independent => vec![
            Line::from("INDEPENDENT · new cross-products allowed"),
            Line::from("Repeated occurrences of the same slot still require equality."),
        ],
        SlotCombinations::ObservedTuples { tuples } => {
            let mut rows = vec![Line::from(format!(
                "LISTED TUPLES · ANY OF {} rows; ALL OF each row",
                tuples.len()
            ))];
            rows.push(Line::from(
                pattern
                    .slots
                    .iter()
                    .map(|slot| format!("◆{} {}", slot.id.0, safe(&slot.label)))
                    .collect::<Vec<_>>()
                    .join(" │ "),
            ));
            for tuple in tuples {
                rows.push(Line::from(
                    pattern
                        .slots
                        .iter()
                        .map(|slot| {
                            tuple
                                .get(&slot.id)
                                .map_or_else(|| "MISSING".into(), |value| literal(value))
                        })
                        .collect::<Vec<_>>()
                        .join(" │ "),
                ));
            }
            rows.push(Line::from(
                "Allowed combinations are separate from observed evidence.",
            ));
            rows
        }
    }
}

fn selector_detail(selector: &PermissionResourceSelector, redact: bool) -> String {
    match selector {
        PermissionResourceSelector::Digest { digest }
        | PermissionResourceSelector::FilesystemSubtreeDigest { digest }
        | PermissionResourceSelector::UrlSubtreeDigest { digest }
        | PermissionResourceSelector::UrlOriginDigest { digest } => {
            format!("opaque SHA-256 {}", safe(digest))
        }
        _ => selector_value(selector, redact),
    }
}

fn argument_rows(arguments: &PermissionArgumentConstraint) -> Vec<Line<'static>> {
    match arguments {
        PermissionArgumentConstraint::Exact { digest } => vec![Line::from(format!(
            "Whole rule · EXACT input · opaque SHA-256 {}",
            safe(digest)
        ))],
        PermissionArgumentConstraint::SelectedDigest { pointers, digest } => vec![
            Line::from(format!(
                "Whole rule · SELECTED {}",
                pointers
                    .iter()
                    .map(|pointer| literal(pointer))
                    .collect::<Vec<_>>()
                    .join(" + ")
            )),
            Line::from(format!(
                "Opaque SHA-256 {} · other arguments may vary",
                safe(digest)
            )),
        ],
        PermissionArgumentConstraint::Selected { arguments } => arguments
            .iter()
            .map(|argument| {
                Line::from(format!(
                    "Whole rule · {} = opaque SHA-256 {}",
                    literal(&argument.pointer),
                    safe(&argument.digest)
                ))
            })
            .collect(),
        PermissionArgumentConstraint::Unconstrained => {
            vec![Line::from("Whole rule · UNCONSTRAINED input")]
        }
    }
}

fn identity_rows(model: &ScopeModel, target: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    if let Some(rule) = model.rule() {
        if let Ok(subject) = serde_json::to_string_pretty(&rule.subject) {
            rows.extend(subject.lines().map(|line| Line::from(safe(line))));
        }
        rows.push(Line::from(format!(
            "Executor {:?} · family {:?}",
            rule.executor, rule.family
        )));
        for (index, resource) in rule.resources.iter().enumerate() {
            rows.push(Line::from(format!(
                "Target {} · {:?}",
                index + 1,
                resource.kind
            )));
            rows.push(Line::from(selector_detail(
                &resource.selector,
                matches!(model.source, ScopeSource::Live { .. }),
            )));
            if let PermissionResourceSelector::RemoteResource { identity, .. }
            | PermissionResourceSelector::RemoteSubtree { identity, .. } = &resource.selector
                && let Ok(identity) = serde_json::to_string_pretty(identity)
            {
                rows.extend(identity.lines().map(|line| Line::from(safe(line))));
            }
            for (key, selector) in &resource.attributes {
                rows.push(Line::from(format!(
                    "{} · {} · {}",
                    safe(key),
                    selector_mode(selector),
                    selector_detail(selector, matches!(model.source, ScopeSource::Live { .. }))
                )));
            }
        }
        rows.extend(argument_rows(&rule.arguments));
    }
    match &model.source {
        ScopeSource::Record(record) => {
            rows.push(Line::from(format!(
                "Record {} · created {} · revoked {:?}",
                safe(&record.id),
                record.created_at,
                record.revoked_at
            )));
            rows.push(Line::from(format!(
                "Project {}",
                record
                    .project
                    .as_ref()
                    .map_or_else(|| "none".into(), |path| literal(&path.to_string_lossy()))
            )));
        }
        ScopeSource::Live { project, .. } => rows.push(Line::from(format!(
            "Project {}",
            project
                .as_ref()
                .map_or_else(|| "none".into(), |path| literal(&path.to_string_lossy()))
        ))),
        _ => {}
    }
    if let Some(pattern) = model.pattern(target) {
        for (key, value) in pattern.context.fields() {
            rows.push(Line::from(format!("{key} · {}", literal(value))));
        }
        for (index, token) in pattern.argv.iter().enumerate() {
            rows.push(Line::from(format!(
                "argv[{index}] · {}",
                match token {
                    PatternToken::Exact { value, role } =>
                        if matches!(role, ArgumentRole::Sensitive | ArgumentRole::Payload) {
                            "[redacted]".into()
                        } else {
                            format!("{role:?} {}", literal(value))
                        },
                    PatternToken::Slot { id, role } => format!("{role:?} ◆{}", id.0),
                }
            )));
        }
    }
    rows
}

fn evidence_rows(model: &ScopeModel) -> Vec<Line<'static>> {
    let mut rows = vec![Line::from("DISPLAY EVIDENCE · not verified preimages")];
    if let ScopeSource::Candidate(candidate) = &model.source {
        rows.push(Line::from(safe(&candidate.evidence.review_summary())));
        for tuple in &candidate.evidence.tuples {
            rows.push(Line::from(format!(
                "{} observations · {}",
                tuple.support.observations,
                tuple
                    .values
                    .iter()
                    .map(|(slot, value)| format!("◆{}={}", slot.0, literal(value)))
                    .collect::<Vec<_>>()
                    .join(" │ ")
            )));
        }
        for source in &candidate.evidence.sources {
            rows.push(Line::from(format!(
                "Untrusted source label: {}",
                literal(source)
            )));
        }
    } else if let Some(review) = model.review() {
        rows.push(Line::from(format!(
            "Review {:?} · {} · {}",
            review.source,
            safe(&review.tool),
            safe(&review.authority)
        )));
        for resource in &review.resources {
            rows.push(Line::from(format!(
                "Target {} display: {}",
                resource.index + 1,
                resource
                    .value
                    .as_deref()
                    .map_or_else(|| "unavailable".into(), literal)
            )));
            for (key, value) in &resource.attributes {
                rows.push(Line::from(format!(
                    "{} display: {}",
                    safe(key),
                    literal(value)
                )));
            }
        }
        if let Some(input) = &review.input
            && let Ok(input) = serde_json::to_string_pretty(input)
        {
            rows.extend(input.lines().map(|line| Line::from(safe(line))));
        }
    } else {
        rows.push(Line::from(
            "No historical evidence or review preimages available.",
        ));
    }
    rows
}
