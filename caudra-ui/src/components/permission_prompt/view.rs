use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::details::{details_body, review_text, sensitive_text};
use super::scope::{
    INCOMPLETE_REVIEW, NO_POLICY_REASON, ReviewDocument, ReviewField, WHOLE_CALL, policy_reason,
};
use super::{
    Block, BorderType, Borders, CHIP_COVERED, COVERAGE_SEPARATOR, Constraint,
    DEFAULT_DENY_GUIDANCE, FooterRow, Frame, HINT_CONFIRM, HINT_ENTER, HINT_ESC, KEY_ALLOW_GLOBAL,
    KEY_ALLOW_LOCAL, KEY_ALLOW_ONCE, KEY_ALLOW_SESSION, KEY_COVERED, KEY_DENY_GLOBAL,
    KEY_DENY_LOCAL, KEY_DETAILS, KEY_GUIDE_DENY, Layout, Line, MIN_REVIEW_HEIGHT, MIN_REVIEW_WIDTH,
    Panel, Paragraph, PermissionCaution, PermissionLifetime, PermissionPrompt, PromptBody,
    PromptHit, PromptState, PromptTarget, Rect, ResourceCoverage, Span, Style, Wrap,
    command_ladders, grade_command_pattern, hint_key, hover_style, theme, visual_rows,
};
use crate::components::permission_scope::pattern::PatternPanel;
use crate::components::permission_scope::{
    model::ScopeModel,
    view::{Disclosure, ScopeView},
};
use crate::theme::Theme;

const CONTROL_GAP: &str = "  ";
const RESIZE_MESSAGE: &str = "Resize to review permission. Esc denies.";
const BORDER_ROWS: u16 = 2;
const FOOTER_SEPARATOR_ROWS: u16 = 1;
const MAX_CONTENT_WIDTH: u16 = 112;
const FIELD_COLUMNS_WIDTH: u16 = 52;
const LABEL_WIDTH: u16 = 18;
const CARD_INSET: u16 = 2;
const CARD_GAP: u16 = 1;
const CANONICAL_SCOPE_DETAILS: &str = "Canonical scope details";
pub(super) const REARM_MESSAGE: &str = "Tab, then retry the blocked key.";

pub(super) fn coverage_chip(coverage: &ResourceCoverage) -> String {
    format!(
        "{CHIP_COVERED}{COVERAGE_SEPARATOR}{}{COVERAGE_SEPARATOR}{}",
        coverage.origin.label(),
        review_text(&coverage.authority)
    )
}

enum CardContent {
    Text(PromptBody),
    Fields(Vec<ReviewField>),
    Scope(usize, ScopeModel, Box<ScopeView>, Vec<ReviewField>),
    Pattern(PatternPanel),
}

enum CardTone {
    Action,
    Scope,
    Context,
    Warning,
}

struct ReviewCard {
    title: String,
    content: CardContent,
    tone: CardTone,
    target: Option<PromptTarget>,
}

impl ReviewCard {
    fn text(title: impl Into<String>, lines: Vec<Line<'static>>, tone: CardTone) -> Self {
        Self {
            title: title.into(),
            content: CardContent::Text(PromptBody {
                lines,
                entries: Vec::new(),
            }),
            tone,
            target: None,
        }
    }

    fn fields(title: impl Into<String>, fields: Vec<ReviewField>, tone: CardTone) -> Self {
        Self {
            title: title.into(),
            content: CardContent::Fields(fields),
            tone,
            target: None,
        }
    }

    fn height(&self, width: u16) -> u16 {
        let width = width.saturating_sub(CARD_INSET * 2).max(1);
        let height = match &self.content {
            CardContent::Pattern(panel) => panel.height(width, &theme::current()),
            CardContent::Scope(_, model, _, fields) => fields_height(fields, width)
                .saturating_add(ScopeView::summary_height(model, width))
                .saturating_add(1),
            CardContent::Text(body) => visual_rows(&body.lines, width).total,
            CardContent::Fields(fields) => fields_height(fields, width),
        };
        height.max(1).saturating_add(BORDER_ROWS)
    }

    fn render(
        &mut self,
        area: Rect,
        buffer: &mut Buffer,
        t: &Theme,
        focused: Option<&PromptTarget>,
    ) -> Vec<PromptHit> {
        let (base, border) = match self.tone {
            CardTone::Action => (t.code_block, t.accent),
            CardTone::Scope => (t.tool_bg.fg(t.foreground), t.panel_border),
            CardTone::Context => (Style::new().fg(t.foreground), t.panel_border),
            CardTone::Warning => (Style::new().fg(t.foreground), t.tool_warning),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border)
            .style(base)
            .title_top(Line::styled(
                format!(" {} ", self.title),
                hover_style(
                    border.add_modifier(Modifier::BOLD),
                    self.target
                        .as_ref()
                        .is_some_and(|target| Some(target) == focused),
                ),
            ));
        let mut inner = block.inner(area);
        inner.x += 1;
        inner.width = inner.width.saturating_sub(2);
        block.render(area, buffer);
        let mut hits = Vec::new();
        if let Some(target) = &self.target {
            hits.push(PromptHit {
                area: Rect { height: 1, ..area },
                target: target.clone(),
            });
        }
        match &mut self.content {
            CardContent::Pattern(panel) => {
                let control = if let Some(PromptTarget::Inspector(control)) = focused {
                    Some(control)
                } else {
                    None
                };
                hits.extend(panel.render(inner, buffer, t, control).into_iter().map(
                    |(area, control)| PromptHit {
                        area,
                        target: PromptTarget::Inspector(control),
                    },
                ));
            }
            CardContent::Scope(authority, model, state, fields) => {
                let [summary, label, details] = Layout::vertical([
                    Constraint::Length(ScopeView::summary_height(model, inner.width)),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .areas(inner);
                let properties = matches!(
                    state.disclosure,
                    Some(Disclosure::Identity | Disclosure::Evidence)
                )
                .then(|| {
                    fields
                        .iter()
                        .flat_map(|field| {
                            [
                                Line::styled(field.label.clone(), t.item_desc),
                                Line::from(field.value.clone()),
                            ]
                        })
                        .collect()
                });
                state.render_with_properties(model, summary, buffer, t, properties);
                if self.target.is_some() {
                    for hit in &state.hits {
                        let target = PromptTarget::VisualScope(*authority, hit.control.clone());
                        if focused == Some(&target) {
                            buffer.set_style(hit.area, hover_style(Style::default(), true));
                        }
                        hits.push(PromptHit {
                            area: hit.area,
                            target,
                        });
                    }
                }
                Paragraph::new(CANONICAL_SCOPE_DETAILS)
                    .style(t.panel_title)
                    .render(label, buffer);
                render_fields(fields, details, buffer, t);
            }
            CardContent::Text(body) => {
                let rows = visual_rows(&body.lines, inner.width);
                Paragraph::new(body.lines.clone())
                    .wrap(Wrap { trim: false })
                    .render(inner, buffer);
                for (target, line) in &body.entries {
                    hits.push(PromptHit {
                        area: Rect {
                            y: inner.y + rows.row_of(*line),
                            height: rows.height_of(*line),
                            ..inner
                        },
                        target: target.clone(),
                    });
                }
            }
            CardContent::Fields(fields) => render_fields(fields, inner, buffer, t),
        }
        hits
    }
}

fn fields_height(fields: &[ReviewField], width: u16) -> u16 {
    fields
        .iter()
        .map(|field| field_height(field, width))
        .fold(0u16, u16::saturating_add)
}

fn render_fields(fields: &[ReviewField], inner: Rect, buffer: &mut Buffer, theme: &Theme) {
    let mut y = inner.y;
    for field in fields {
        let height = field_height(field, inner.width);
        let row = Rect { y, height, ..inner };
        if field.label.is_empty() {
            Paragraph::new(field.value.as_str())
                .wrap(Wrap { trim: false })
                .render(row, buffer);
        } else if inner.width < FIELD_COLUMNS_WIDTH {
            Paragraph::new(field.label.as_str())
                .style(theme.tool_dim)
                .wrap(Wrap { trim: false })
                .render(Rect { height: 1, ..row }, buffer);
            Paragraph::new(field.value.as_str())
                .wrap(Wrap { trim: false })
                .render(
                    Rect {
                        y: y + 1,
                        height: height.saturating_sub(1),
                        ..row
                    },
                    buffer,
                );
        } else {
            let [label, value] =
                Layout::horizontal([Constraint::Length(LABEL_WIDTH), Constraint::Min(1)])
                    .areas(row);
            Paragraph::new(field.label.as_str())
                .style(theme.tool_dim)
                .wrap(Wrap { trim: false })
                .render(label, buffer);
            Paragraph::new(field.value.as_str())
                .wrap(Wrap { trim: false })
                .render(value, buffer);
        }
        y += height;
    }
}

fn field_height(field: &ReviewField, width: u16) -> u16 {
    let measure = |text: &str, width| {
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    };
    if field.label.is_empty() {
        measure(&field.value, width).max(1)
    } else if width < FIELD_COLUMNS_WIDTH {
        1 + measure(&field.value, width).max(1)
    } else {
        measure(&field.label, LABEL_WIDTH)
            .max(measure(&field.value, width.saturating_sub(LABEL_WIDTH)))
            .max(1)
    }
}

struct ReviewLayout {
    cards: Vec<(Rect, ReviewCard)>,
    width: u16,
    height: u16,
}

impl ReviewLayout {
    fn new(width: u16) -> Self {
        Self {
            cards: Vec::new(),
            width,
            height: 0,
        }
    }

    fn push(&mut self, card: ReviewCard) {
        let height = card.height(self.width);
        self.cards
            .push((Rect::new(0, self.height, self.width, height), card));
        self.height = self.height.saturating_add(height + CARD_GAP);
    }

    fn total(&self) -> u16 {
        self.height.saturating_sub(CARD_GAP)
    }
}

fn content_area(area: Rect) -> Rect {
    let width = area.width.min(MAX_CONTENT_WIDTH);
    Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..area
    }
}

impl PermissionPrompt {
    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.view_with_theme(frame, area, &theme::current());
    }

    fn view_with_theme(&mut self, frame: &mut Frame, area: Rect, t: &Theme) {
        let area = content_area(area);
        if self.area != area {
            let focus = self.focus.clone();
            self.invalidate_controls();
            self.focus = focus;
        }
        self.area = area;
        let pressed_area = self.mouse_down.as_ref().and_then(|target| {
            self.row_hits
                .iter()
                .find(|hit| hit.target == *target)
                .map(|hit| hit.area)
        });
        let footer_height = self
            .footer_rows(area.width.saturating_sub(BORDER_ROWS))
            .len() as u16
            + FOOTER_SEPARATOR_ROWS;
        if area.width < MIN_REVIEW_WIDTH
            || area.height < MIN_REVIEW_HEIGHT
            || area.height <= BORDER_ROWS + footer_height
        {
            self.invalidate_controls();
            frame.render_widget(
                Paragraph::new(RESIZE_MESSAGE)
                    .style(t.tool_dim)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        if self.current().is_none() {
            self.row_hits.clear();
            return;
        }
        let title = if self.panel == Panel::Details {
            "Permission details"
        } else if self.confirmation.is_some() {
            "Confirm permission"
        } else if self.inspector.is_some() {
            "Pattern editor"
        } else if self.panel == Panel::Scopes {
            "Choose future scope"
        } else {
            "Permission required"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_top(Line::from(format!(
                " {title} · {} pending ",
                self.pending_count()
            )))
            .title_style(t.panel_title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body_area, footer_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(inner);
        let mut layout = self.review_layout(body_area.width.saturating_sub(1).max(1), t);
        self.scroll
            .update_dimensions(layout.total(), body_area.height);
        let mut buffer = Buffer::empty(Rect::new(0, 0, layout.width, layout.total()));
        let mut hits = Vec::new();
        for (rect, card) in &mut layout.cards {
            hits.extend(card.render(
                *rect,
                &mut buffer,
                t,
                self.hover.as_ref().or(self.focus.as_ref()),
            ));
            if self.confirmation.is_none()
                && let CardContent::Scope(authority, _, state, _) = &card.content
                && *authority == self.scope_authority
            {
                self.scope_view = state.as_ref().clone();
            }
        }
        if let Some(target) = self.pending_reveal.take()
            && let Some(hit) = hits.iter().find(|hit| hit.target == target)
        {
            self.scroll.reveal(hit.area.y, 1);
        }
        let offset = self.scroll.offset();
        for y in 0..body_area.height.min(layout.total().saturating_sub(offset)) {
            for x in 0..layout.width {
                frame.buffer_mut()[(body_area.x + x, body_area.y + y)] =
                    buffer[(x, offset + y)].clone();
            }
        }
        self.scrollbar
            .draw(frame, body_area, layout.total(), offset);
        self.row_hits.clear();
        let viewport = Rect::new(0, offset, layout.width, body_area.height);
        for hit in hits {
            let clipped = hit.area.intersection(viewport);
            if !clipped.is_empty() {
                self.row_hits.push(PromptHit {
                    area: Rect {
                        x: body_area.x + clipped.x,
                        y: body_area.y + clipped.y - offset,
                        ..clipped
                    },
                    target: hit.target,
                });
            }
        }
        frame.render_widget(
            Paragraph::new("─".repeat(usize::from(footer_area.width))).style(t.panel_border),
            Rect {
                height: FOOTER_SEPARATOR_ROWS,
                ..footer_area
            },
        );
        for (index, row) in self.footer_rows(footer_area.width).into_iter().enumerate() {
            let area = Rect {
                y: footer_area.y + FOOTER_SEPARATOR_ROWS + index as u16,
                height: 1,
                ..footer_area
            };
            let editing = matches!(row, FooterRow::Guidance | FooterRow::ConfirmationInput);
            let line = match row {
                FooterRow::Hints(pairs) => {
                    let mut x = area.x;
                    let mut spans = Vec::new();
                    if pairs.first().is_some_and(|(key, _)| *key == KEY_ALLOW_ONCE)
                        && self.confirmation.is_none()
                    {
                        spans.push(Span::styled("Allow: ", t.tool_dim));
                        x += "Allow: ".width() as u16;
                    }
                    for (index, (label, description)) in pairs.into_iter().enumerate() {
                        if index > 0 {
                            spans.push(Span::raw(CONTROL_GAP));
                            x += CONTROL_GAP.width() as u16;
                        }
                        let target = if label == "r" {
                            Some(PromptTarget::Scope)
                        } else {
                            hint_key(label).map(PromptTarget::Hint)
                        };
                        let on = target.as_ref().is_some_and(|target| {
                            self.hover.as_ref().or(self.focus.as_ref()) == Some(target)
                        });
                        spans.extend([
                            Span::styled("[", hover_style(t.tool_dim, on)),
                            Span::styled(label, hover_style(t.keybind_key, on)),
                            Span::styled(
                                format!(" {description}"),
                                hover_style(t.keybind_desc, on),
                            ),
                            Span::styled("]", hover_style(t.tool_dim, on)),
                        ]);
                        let width = button_width(label, description);
                        if x + width <= area.right()
                            && let Some(target) = target
                        {
                            self.row_hits.push(PromptHit {
                                area: Rect { x, width, ..area },
                                target,
                            });
                        }
                        x += width;
                    }
                    Line::from(spans)
                }
                FooterRow::Guidance => self.guidance_line(t),
                FooterRow::ConfirmationInput => self.input_line(t),
                FooterRow::InspectorStatus => self.inspector_status_line(t),
                FooterRow::Rearm => Line::styled(REARM_MESSAGE, t.status_notice),
            };
            let horizontal_scroll = if editing {
                let cursor = line
                    .spans
                    .iter()
                    .take_while(|span| !span.style.add_modifier.contains(Modifier::REVERSED))
                    .map(Span::width)
                    .sum::<usize>();
                u16::try_from(cursor.saturating_sub(usize::from(area.width.saturating_sub(1))))
                    .unwrap_or(u16::MAX)
            } else {
                0
            };
            frame.render_widget(Paragraph::new(line).scroll((0, horizontal_scroll)), area);
        }
        self.awaiting_review = false;
        if let Some(pressed_area) = pressed_area
            && !self.row_hits.iter().any(|hit| {
                Some(&hit.target) == self.mouse_down.as_ref() && hit.area == pressed_area
            })
        {
            self.mouse_down = None;
        }
        if self
            .focus
            .as_ref()
            .is_some_and(|target| !self.row_hits.iter().any(|hit| hit.target == *target))
        {
            self.focus = None;
        }
    }

    pub fn height(&self, width: u16) -> u16 {
        if self.current().is_none() {
            return 0;
        }
        let inner = width.min(MAX_CONTENT_WIDTH).saturating_sub(BORDER_ROWS);
        self.review_layout(inner.saturating_sub(1).max(1), &theme::current())
            .total()
            .saturating_add(self.footer_rows(inner).len() as u16)
            .saturating_add(BORDER_ROWS + FOOTER_SEPARATOR_ROWS)
            .max(MIN_REVIEW_HEIGHT)
    }

    fn review_layout(&self, width: u16, t: &Theme) -> ReviewLayout {
        let mut layout = ReviewLayout::new(width);
        let Some(request) = self.current() else {
            return layout;
        };
        if self.panel == Panel::Details {
            layout.push(ReviewCard {
                title: "Technical review · secrets redacted".into(),
                content: CardContent::Text(details_body(request)),
                tone: CardTone::Context,
                target: None,
            });
            return layout;
        }
        if let Some(panel) = self.inspector_panel() {
            layout.push(ReviewCard {
                title: "Pattern · edit without approving".into(),
                content: CardContent::Pattern(panel),
                tone: CardTone::Scope,
                target: None,
            });
            return layout;
        }
        if self.state == PromptState::PatternEditing {
            let mut lines = vec![self.input_line(t)];
            if let Some(row) = self.command_row() {
                lines.push(Line::styled(self.pattern_feedback(row), t.tool_warning));
            }
            layout.push(ReviewCard::text(
                "Custom command prefix",
                lines,
                CardTone::Scope,
            ));
        }
        let mut current;
        let document = if let Some(confirmation) = &self.confirmation {
            &confirmation.review
        } else {
            current = ReviewDocument::new(request, &self.allow_answer(self.lifetime.clone()));
            current
                .context
                .push(ReviewField::new("Needs approval", policy_reason(request)));
            if self.project_available()
                && !current.context.iter().any(|field| field.label == "Project")
                && let Some(project) = request
                    .presentation
                    .project
                    .as_deref()
                    .and_then(Path::to_str)
            {
                current.context.push(ReviewField::new("Project", project));
            }
            if let Some(requester) = self
                .requests
                .front()
                .and_then(|queued| queued.requester.as_deref())
            {
                current.context.insert(
                    1,
                    ReviewField::new("Requester", format!("subtask {requester}")),
                );
            }
            current.bound();
            &current
        };
        let command_style = t.code_block.fg(t.foreground).add_modifier(Modifier::BOLD);
        layout.push(ReviewCard::text(
            if document.shell && document.exact_call_workdir.is_none() {
                format!("Run command · {}", review_text(&request.tool.to_string()))
            } else if document.shell {
                "Run command".into()
            } else {
                "Requested action".into()
            },
            document
                .action
                .lines()
                .map(|line| Line::styled(line.to_owned(), command_style))
                .collect(),
            CardTone::Action,
        ));
        let lifetime = if self.confirmation.is_some() {
            match document.lifetime {
                PermissionLifetime::Once => "This call only; nothing remembered.",
                PermissionLifetime::Conversation => "This conversation only.",
                PermissionLifetime::Project => "This project, across conversations.",
                PermissionLifetime::Global => "All projects, across conversations.",
            }
        } else if self.panel == Panel::Scopes {
            "Choosing a scope does not approve execution."
        } else {
            "Choose Once, Conversation or Project below."
        };
        let mut lifetime_lines = vec![Line::styled(lifetime, t.status_notice)];
        if let Some(requester) = self
            .requests
            .front()
            .and_then(|queued| queued.requester.as_deref())
        {
            lifetime_lines.push(Line::styled(
                format!("Requester: subtask {}", review_text(requester)),
                t.tool_dim,
            ));
        }
        if document.exact_call_workdir.is_some() {
            lifetime_lines.extend(
                document
                    .context
                    .iter()
                    .filter(|field| {
                        field.label == "Project"
                            || (field.label == "Needs approval" && field.value != NO_POLICY_REASON)
                    })
                    .map(|field| {
                        Line::styled(format!("{}: {}", field.label, field.value), t.tool_dim)
                    }),
            );
        }
        if self.confirmation.is_none() && !self.grants_lifetime(&PermissionLifetime::Project) {
            lifetime_lines.push(Line::styled(
                if self.project_available() {
                    "Project persistence is not offered for this scope."
                } else {
                    "Project persistence unavailable: no local project binding."
                },
                t.tool_dim,
            ));
        }
        layout.push(ReviewCard::text(
            "Lifetime",
            lifetime_lines,
            CardTone::Context,
        ));
        let compact_exact = document.exact_call_workdir.is_some()
            && self.panel != Panel::Scopes
            && (document.lifetime == PermissionLifetime::Once || self.confirmation.is_some());
        if let Some(workdir) = document
            .exact_call_workdir
            .as_ref()
            .filter(|_| compact_exact)
        {
            let mut card = ReviewCard::fields(
                "Future scope",
                vec![
                    ReviewField::new("Scope", "Exact call only"),
                    ReviewField::new("Run from", workdir),
                    ReviewField::new("Context", "Same reviewed preparation"),
                ],
                CardTone::Scope,
            );
            if self.confirmation.is_none() {
                card.target = Some(PromptTarget::Scope);
            }
            layout.push(card);
        }
        if self
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| !confirmation.complete)
        {
            layout.push(ReviewCard::text(
                "Approval disabled",
                vec![Line::styled(INCOMPLETE_REVIEW, t.tool_warning)],
                CardTone::Warning,
            ));
        } else if !document.warnings.is_empty() {
            layout.push(ReviewCard::text(
                "Review carefully",
                document
                    .warnings
                    .iter()
                    .map(|warning| Line::styled(warning.clone(), t.tool_warning))
                    .collect(),
                CardTone::Warning,
            ));
        } else if self.confirmation.is_none() && !self.selected_summary().complete {
            layout.push(ReviewCard::text(
                "Future scope unavailable",
                vec![Line::styled(
                    "Use Once or inspect Details before choosing a reusable scope.",
                    t.tool_warning,
                )],
                CardTone::Warning,
            ));
        }
        if !compact_exact {
            let ladders = command_ladders(request);
            let subsumed = request.subsumed_rows(&self.row_grants(request));
            for (index, authority) in document.authorities.iter().enumerate() {
                if self.panel == Panel::Scopes
                    && self.command_row().is_some()
                    && authority.row != self.command_row()
                {
                    continue;
                }
                let covered = authority
                    .row
                    .and_then(|row| request.presentation.resources.get(row))
                    .and_then(|shown| shown.coverage.as_ref());
                if covered.is_some() && !self.expanded_covered && self.confirmation.is_none() {
                    continue;
                }
                let mut fields = vec![ReviewField::new("Scope", &authority.title)];
                fields.extend(
                    authority
                        .fields
                        .iter()
                        .map(|field| ReviewField::new(&field.label, &field.value)),
                );
                if let Some(coverage) = covered {
                    fields.push(ReviewField::new("Already allowed", coverage_chip(coverage)));
                }
                if self.confirmation.is_none()
                    && let Some(other) = authority
                        .row
                        .and_then(|row| subsumed.get(row))
                        .copied()
                        .flatten()
                {
                    fields.push(ReviewField::new(
                        "Also covered",
                        format!("Row {}: {}", other + 1, self.row_summary(other).label),
                    ));
                }
                let content = if let Some(scope) = &authority.scope {
                    CardContent::Scope(
                        index,
                        scope.clone(),
                        Box::new(if self.scope_authority == index {
                            self.scope_view.clone()
                        } else {
                            ScopeView::default()
                        }),
                        fields,
                    )
                } else {
                    CardContent::Fields(fields)
                };
                let mut card = ReviewCard {
                    title: if let Some(row) =
                        authority.row.filter(|_| document.authorities.len() > 1)
                    {
                        format!("Command {} · future scope", row + 1)
                    } else {
                        "Future scope".into()
                    },
                    content,
                    tone: CardTone::Scope,
                    target: None,
                };
                if self.confirmation.is_none() {
                    card.target = authority
                        .row
                        .filter(|_| document.authorities.len() > 1)
                        .and_then(|row| ladders.get(row))
                        .and_then(|ladder| ladder.first())
                        .map(|option| PromptTarget::Authority(self.row_key(option)))
                        .or(Some(PromptTarget::Scope));
                }
                layout.push(card);
            }
            if request.resources.len() > 1 && document.shell {
                let mut lines = Vec::new();
                if self.confirmation.is_none() {
                    lines.push(Line::styled(
                        format!(
                            "Needs approval: {} of {} commands",
                            request.resources.len().saturating_sub(self.covered_count()),
                            request.resources.len()
                        ),
                        t.panel_title,
                    ));
                }
                lines.push(Line::styled(WHOLE_CALL, t.tool_dim));
                lines.push(Line::styled(
                    "Rows not remembered still run with this call.",
                    t.tool_dim,
                ));
                layout.push(ReviewCard::text("Whole call", lines, CardTone::Context));
            }
            layout.push(ReviewCard::fields(
                "Context",
                document
                    .context
                    .iter()
                    .filter(|field| field.label != "Requester")
                    .map(|field| ReviewField::new(&field.label, &field.value))
                    .collect(),
                CardTone::Context,
            ));
        }
        if let Some(phrase) = self.confirmation_phrase() {
            layout.push(ReviewCard::text(
                "Type to confirm",
                vec![Line::styled(
                    review_text(phrase),
                    t.status_notice.add_modifier(Modifier::BOLD),
                )],
                CardTone::Warning,
            ));
        }
        layout
    }

    fn pattern_feedback(&self, row: usize) -> String {
        let pattern = self.buffer.value();
        let Some(command) = self
            .current()
            .and_then(|request| request.resources.get(row))
        else {
            return String::new();
        };
        if pattern.is_empty() && sensitive_text(&command.value) {
            return "Credential-bearing command: author a prefix explicitly; no command text was seeded.".into();
        }
        match grade_command_pattern(pattern.trim(), &command.value) {
            Err(fault) => fault.to_string(),
            Ok(grade) => match grade.caution {
                Some(PermissionCaution::Danger) => {
                    "Every invocation of this program; extra confirmation required.".into()
                }
                Some(PermissionCaution::Warn) => {
                    "Overlaps an always-ask family; extra confirmation required.".into()
                }
                None => "Matches this command; review the future prefix before approval.".into(),
            },
        }
    }

    fn footer_rows(&self, width: u16) -> Vec<FooterRow> {
        let mut rows = if self.panel == Panel::Details {
            let mut rows = vec![FooterRow::Hints(vec![("PgUp", "Up"), ("PgDn", "Down")])];
            if self.confirmation.is_none() {
                let mut denies = Vec::new();
                if self.project_available() {
                    denies.push((KEY_DENY_LOCAL, "Deny project"));
                }
                denies.push((KEY_DENY_GLOBAL, "Deny global"));
                rows.push(FooterRow::Hints(denies));
            }
            rows.push(FooterRow::Hints(vec![(HINT_ESC, "Back")]));
            rows
        } else if self.inspector.is_some() {
            self.inspector_footer()
        } else if let Some(confirmation) = &self.confirmation {
            let mut rows = Vec::new();
            if confirmation.complete {
                if confirmation.phrase.is_some() {
                    rows.push(FooterRow::ConfirmationInput);
                }
                rows.push(FooterRow::Hints(vec![(
                    if confirmation.phrase.is_some() {
                        HINT_ENTER
                    } else {
                        HINT_CONFIRM
                    },
                    "Confirm",
                )]));
            }
            rows.push(FooterRow::Hints(vec![
                ("F2", "Details"),
                (HINT_ESC, "Back"),
            ]));
            rows
        } else {
            match self.state {
                PromptState::DenyEditing => vec![
                    FooterRow::Guidance,
                    FooterRow::Hints(vec![(HINT_ENTER, "Deny with guidance"), (HINT_ESC, "Back")]),
                ],
                PromptState::PatternEditing => vec![FooterRow::Hints(vec![
                    (HINT_ENTER, "Use prefix"),
                    (HINT_ESC, "Back"),
                ])],
                _ if self.panel == Panel::Scopes => {
                    let mut advanced = Vec::new();
                    if self.suggested_pattern().is_some() {
                        advanced.push(("i", "Pattern"));
                    }
                    if self.command_row().is_some() {
                        advanced.push(("e", "Prefix"));
                    }
                    if self.grants_lifetime(&PermissionLifetime::Global) {
                        advanced.push((KEY_ALLOW_GLOBAL, "Global"));
                    }
                    let mut navigation = vec![("r", "Scope")];
                    if self.row_keys().len() > 1 {
                        navigation.extend([("↑", "Prev"), ("↓", "Next")]);
                    }
                    if self.can_widen() {
                        navigation.extend([("←", "Less"), ("→", "More")]);
                    }
                    let mut rows = vec![FooterRow::Hints(navigation)];
                    if !advanced.is_empty() {
                        rows.push(FooterRow::Hints(advanced));
                    }
                    rows.push(FooterRow::Hints(vec![
                        ("p", "Use scope"),
                        (KEY_DETAILS, "Details"),
                        (HINT_ESC, "Back"),
                    ]));
                    rows
                }
                _ => {
                    let mut grants = vec![(KEY_ALLOW_ONCE, "Once")];
                    if self.grants_lifetime(&PermissionLifetime::Conversation) {
                        grants.push((KEY_ALLOW_SESSION, "Conversation"));
                    }
                    if self.grants_lifetime(&PermissionLifetime::Project) {
                        grants.push((KEY_ALLOW_LOCAL, "Project"));
                    }
                    let mut rows = vec![
                        FooterRow::Hints(grants),
                        FooterRow::Hints(vec![
                            ("r", "Scope"),
                            (KEY_DETAILS, "Details"),
                            (KEY_GUIDE_DENY, "Guidance"),
                            (HINT_ESC, "Deny"),
                        ]),
                    ];
                    if self.covered_count() > 0 {
                        rows.push(FooterRow::Hints(vec![(KEY_COVERED, "Covered")]));
                    }
                    rows
                }
            }
        };
        if self.decision_needs_rearm() {
            rows.push(FooterRow::Rearm);
        }
        let mut wrapped = Vec::new();
        for row in rows {
            if let FooterRow::Hints(pairs) = row {
                let mut line = Vec::new();
                let mut used = 0;
                for pair in pairs {
                    let leading = if pair.0 == KEY_ALLOW_ONCE && self.confirmation.is_none() {
                        "Allow: ".width() as u16
                    } else {
                        0
                    };
                    let next = button_width(pair.0, pair.1)
                        + if line.is_empty() {
                            leading
                        } else {
                            CONTROL_GAP.width() as u16
                        };
                    if !line.is_empty() && used + next > width {
                        wrapped.push(FooterRow::Hints(line));
                        line = Vec::new();
                        used = 0;
                    }
                    used += button_width(pair.0, pair.1)
                        + if line.is_empty() {
                            leading
                        } else {
                            CONTROL_GAP.width() as u16
                        };
                    line.push(pair);
                }
                if !line.is_empty() {
                    wrapped.push(FooterRow::Hints(line));
                }
            } else {
                wrapped.push(row);
            }
        }
        wrapped
    }

    fn guidance_line(&self, t: &Theme) -> Line<'static> {
        self.editor_line("Guidance: ", DEFAULT_DENY_GUIDANCE, t)
    }

    fn input_line(&self, t: &Theme) -> Line<'static> {
        self.editor_line("> ", "", t)
    }

    fn editor_line(&self, prefix: &'static str, placeholder: &str, t: &Theme) -> Line<'static> {
        let input = self.buffer.value();
        let source = if input.is_empty() {
            placeholder
        } else {
            &input
        };
        let display = review_text(source);
        let cursor = if display == source {
            self.buffer.cursor_offset()
        } else {
            display.chars().count()
        };
        let mut chars = display.chars();
        let before = chars.by_ref().take(cursor).collect::<String>();
        let caret = chars.next().unwrap_or(' ');
        let after = chars.collect::<String>();
        Line::from(vec![
            Span::styled(prefix, t.tool_dim),
            Span::styled(before, Style::new().fg(t.foreground)),
            Span::styled(caret.to_string(), Style::new().reversed()),
            Span::styled(after, Style::new().fg(t.foreground)),
        ])
    }
}

fn button_width(key: &str, description: &str) -> u16 {
    (key.width() + description.width() + "[ ]".width()) as u16
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::fs::{self, Permissions};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    use caudra_agent::permissions::{
        PermissionAnswer, PermissionArgumentConstraint, PermissionAuthorityProfile,
        PermissionExecutorKind, PermissionLifetime, PermissionRequest, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionResourceSelector,
        PermissionRisk, PermissionSubject, RemotePermissionIdentity, ResourceCoverage, RuleOrigin,
    };
    use caudra_agent::tools::{PermissionIntent, PermissionScopes};
    use caudra_config::ToolKey;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Buffer,
        layout::{Position, Rect},
        style::{Color, Modifier, Style},
        text::Line,
        widgets::{Block, BorderType, Paragraph, Widget, Wrap},
    };
    use serde_json::{Value, json};
    use tempfile::Builder;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    use super::super::decision::tests::native_shell_request;
    use super::super::details::{INCOMPLETE_REDACTION, review_text};
    use super::super::inspector::InspectorControl;
    use super::super::inspector::tests::suggested_prompt;
    use super::super::{Panel, PromptMouse, PromptTarget};
    use super::{CANONICAL_SCOPE_DETAILS, CardContent, CardTone, PermissionPrompt, ReviewField};
    use crate::components::buffer_text;
    use crate::components::permission_scope::view::{Disclosure, ScopeControl, ScopeView};
    use crate::theme::{self, Theme};

    pub(crate) const ROOMY_WIDTH: u16 = 140;
    pub(crate) const ROOMY_HEIGHT: u16 = 40;
    const FULL_REVIEW_HEIGHT: u16 = 128;
    #[cfg(unix)]
    const PRIVATE_ARTIFACT_MODE: u32 = 0o600;
    const COMMAND: &str = "ls ~/.cache";
    const EXACT_WORKDIR: &str = "/project";
    const EXACT_PROJECT: &str = "/host/project-binding";
    const COMPACT_SCOPE: &str = "Exact call only";
    const COMPACT_CONTEXT: &str = "Same reviewed preparation";
    const COVERING: &str = "git status *";
    const PROTECTED: &str = "Includes protected resources. Review the scope before allowing.";
    const BROAD_PHRASE: &str = "ALLOW BROAD SHELL ACCESS";
    const PROTECTED_PHRASE: &str = "ALLOW PROTECTED ACCESS";
    const HIDDEN_SECRET: &str = "never-display-this";
    const REMOTE_IDS: [&str; 7] = [
        "anchor",
        "server",
        "workspace",
        "generation",
        "namespace",
        "principal",
        "project",
    ];
    const REMOTE_DISPLAY_PATH: &str = "/display-only/file";
    const REMOTE_ESCAPED_TARGET_KEY: &str = "/root/folder%2Ffile";
    const REMOTE_SPLIT_TARGET_KEY: &str = "/root/folder/file";
    const LIVE_SCOPE_HEIGHT: u16 = 24;
    const ALTERNATIVE_DIRECTORY: &str = "/work/alternative";
    const ATTRIBUTE_NAME: &str = "zone";
    const ATTRIBUTE_VALUE: &str = "reviewed-zone";
    const FIRST_COMMAND: &str = "git status";
    const TARGETS_LABEL: &str = "Targets · ANY OF (2)";
    const SECOND_CONDITIONS: &str = "ALL OF · target 2";
    const UNRESTRICTED_ATTRIBUTE: &str = "zoneAnyUNRESTRICTED";
    const MAX_PROPERTY_PAGES: usize = 32;
    const SCOPE_FIELDS: &[(&str, &str)] = &[
        ("Scope", COMPACT_SCOPE),
        ("Run from", EXACT_WORKDIR),
        ("Context", COMPACT_CONTEXT),
    ];

    fn prepared_protected_request(command: &str) -> PermissionRequest {
        let native = native_shell_request(command);
        let mut resources = native.resources;
        resources.push(PermissionResource {
            kind: PermissionResourceKind::Command,
            value: command.into(),
            access: Some(PermissionResourceAccess::Execute),
            protected: true,
            requires_prompt: true,
            attributes: BTreeMap::from([("workdir".into(), EXACT_WORKDIR.into())]),
        });
        let intent = PermissionIntent::new(
            PermissionScopes::single(command.into()),
            resources,
            PermissionRisk::Critical,
        )
        .with_authority(PermissionAuthorityProfile::Shell);
        let mut request = PermissionRequest::from_intent_with_identity(
            native.id,
            native.tool,
            &intent,
            json!({"command": command}),
            Path::new(EXACT_WORKDIR),
            native.subject,
            native.executor,
        );
        request.presentation.project = Some(EXACT_PROJECT.into());
        request
    }

    fn protected_prompt(command: &str) -> PermissionPrompt {
        exact_prompt(prepared_protected_request(command))
    }

    fn remote_request(ids: [&str; 7], parts: &[&str], subtree: bool) -> PermissionRequest {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(ids[0]).unwrap(),
            ids[1],
            ids[2],
            ids[3],
            ids[4],
        )
        .unwrap();
        let identity = RemotePermissionIdentity {
            principal: AuthenticatedPrincipalId::new(authority.clone(), ids[5]).unwrap(),
            project: ProjectIdentity::new(authority.clone(), ProjectKey::new(ids[6]).unwrap()),
            authority,
        };
        let intent = PermissionIntent::new(
            PermissionScopes::single("remote read".into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::RemoteFile {
                    identity: identity.clone(),
                },
                value: parts.join("\u{1f}"),
                access: Some(PermissionResourceAccess::Read),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([("display_path".into(), REMOTE_DISPLAY_PATH.into())]),
            }],
            PermissionRisk::Low,
        )
        .with_authority(PermissionAuthorityProfile::RemoteResource);
        let mut request = PermissionRequest::from_intent_with_identity(
            "remote".into(),
            ToolKey::native("file_read"),
            &intent,
            json!({"filePath": REMOTE_DISPLAY_PATH}),
            Path::new("/not-the-remote-project"),
            PermissionSubject::RemoteNative {
                identity: identity.clone(),
                owner: "workcell".into(),
                contract: "file.read.v1".into(),
            },
            PermissionExecutorKind::RemoteWorkcell,
        );
        request.presentation.action = "Run native tool file_read".into();
        if subtree {
            let resource = &mut request
                .options
                .iter_mut()
                .find(|option| option.id == "allow_exact")
                .unwrap()
                .rule
                .resources[0];
            resource.kind = PermissionResourceKind::RemoteDirectory {
                identity: identity.clone(),
            };
            resource.selector = PermissionResourceSelector::RemoteSubtree {
                identity,
                scope: vec![parts[0].into()],
            };
        }
        request
    }

    fn exact_prompt(request: PermissionRequest) -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        prompt.select_authority("allow_exact".into());
        prompt
    }

    fn confirm_request(request: PermissionRequest) -> PermissionPrompt {
        let mut prompt = exact_prompt(request);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        assert!(prompt.confirmation.is_some());
        prompt
    }

    pub(crate) fn request(id: &str, input: Value) -> Box<PermissionRequest> {
        let mut request = PermissionRequest::from_legacy(
            id.into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            input,
            Path::new("/project"),
            false,
        );
        request.presentation.project = Some("/project".into());
        Box::new(request)
    }
    pub(crate) fn open_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request("id", json!({"command": "cargo test"})), None);
        prompt
    }
    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    pub(crate) fn render(prompt: &mut PermissionPrompt, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| prompt.view(frame, frame.area()))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn themed_buffer(
        prompt: &mut PermissionPrompt,
        width: u16,
        height: u16,
        theme: &Theme,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| prompt.view_with_theme(frame, frame.area(), theme))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn typed_alternatives_prompt() -> PermissionPrompt {
        let mut request = native_shell_request(FIRST_COMMAND);
        request.resources[0]
            .attributes
            .insert(ATTRIBUTE_NAME.into(), ATTRIBUTE_VALUE.into());
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact")
            .unwrap();
        option.rule.arguments = PermissionArgumentConstraint::Unconstrained;
        let first = &mut option.rule.resources[0];
        first.selector = PermissionResourceSelector::Any;
        first.protected = Some(false);
        first.attributes.retain(|name, _| name == "workdir");
        first.attributes.insert(
            ATTRIBUTE_NAME.into(),
            PermissionResourceSelector::Exact {
                value: ATTRIBUTE_VALUE.into(),
            },
        );
        let mut second = first.clone();
        second.protected = Some(true);
        second.attributes.insert(
            "workdir".into(),
            PermissionResourceSelector::Exact {
                value: ALTERNATIVE_DIRECTORY.into(),
            },
        );
        second
            .attributes
            .insert(ATTRIBUTE_NAME.into(), PermissionResourceSelector::Any);
        option.rule.resources.push(second);
        exact_prompt(request)
    }

    fn typed_pattern_prompt() -> PermissionPrompt {
        let mut prompt = suggested_prompt();
        let id = prompt
            .current()
            .unwrap()
            .options
            .iter()
            .find(|option| {
                option.rule.resources.iter().any(|resource| {
                    matches!(
                        resource.selector,
                        PermissionResourceSelector::CommandTemplate { .. }
                    )
                })
            })
            .unwrap()
            .id
            .clone();
        prompt.select_authority(id);
        prompt
    }

    fn scope_card(prompt: &PermissionPrompt, t: &Theme) -> Rect {
        prompt
            .review_layout(prompt.area.width.saturating_sub(3), t)
            .cards
            .into_iter()
            .find(|(_, card)| matches!(card.content, CardContent::Scope(..)))
            .unwrap()
            .0
    }

    fn reveal_scope(prompt: &mut PermissionPrompt, width: u16, t: &Theme) -> Buffer {
        themed_buffer(prompt, width, LIVE_SCOPE_HEIGHT, t);
        prompt.scroll.scroll_to(scope_card(prompt, t).y);
        themed_buffer(prompt, width, LIVE_SCOPE_HEIGHT, t)
    }

    fn click_scope_control(
        prompt: &mut PermissionPrompt,
        width: u16,
        t: &Theme,
        control: ScopeControl,
    ) -> Buffer {
        themed_buffer(prompt, width, LIVE_SCOPE_HEIGHT, t);
        let y = prompt
            .scope_view
            .hits
            .iter()
            .find(|hit| hit.control == control)
            .unwrap()
            .area
            .y;
        if matches!(control, ScopeControl::Scroll(_)) {
            let properties = prompt
                .scope_view
                .hits
                .iter()
                .find(|hit| hit.control == ScopeControl::Disclosure(Disclosure::Conditions))
                .unwrap()
                .area
                .y;
            prompt.scroll.scroll_to(properties);
            prompt.scroll.reveal(y, 1);
        } else {
            prompt.scroll.scroll_to(y.saturating_sub(1));
        }
        themed_buffer(prompt, width, LIVE_SCOPE_HEIGHT, t);
        let target = PromptTarget::VisualScope(0, control);
        let area = prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == target)
            .unwrap()
            .area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(matches!(
                prompt.handle_mouse(MouseEvent {
                    kind,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE
                }),
                PromptMouse::Consumed
            ));
        }
        themed_buffer(prompt, width, LIVE_SCOPE_HEIGHT, t)
    }

    fn compact_cells(buffer: &Buffer) -> String {
        buffer_rows(buffer)
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '│')
            .collect()
    }

    fn displayed_scope_fields(prompt: &PermissionPrompt) -> Vec<ReviewField> {
        prompt
            .review_layout(
                prompt.area.width.saturating_sub(3).max(1),
                &theme::current(),
            )
            .cards
            .into_iter()
            .flat_map(|(_, card)| match (card.tone, card.content) {
                (
                    CardTone::Scope,
                    CardContent::Fields(fields) | CardContent::Scope(_, _, _, fields),
                ) => fields,
                _ => Vec::new(),
            })
            .collect()
    }

    fn assert_scrollable_body(
        prompt: &mut PermissionPrompt,
        width: u16,
        height: u16,
        t: &Theme,
    ) -> Buffer {
        themed_buffer(prompt, width, FULL_REVIEW_HEIGHT, t);
        prompt.handle_key(key(KeyCode::Home));
        let full = themed_buffer(prompt, width, FULL_REVIEW_HEIGHT, t);
        let first = themed_buffer(prompt, width, height, t);
        assert_eq!(prompt.scroll.offset(), 0);
        let footer_rows = prompt.footer_rows(prompt.area.width - 2).len() as u16;
        let footer_top = height - 2 - footer_rows;
        let body_height = footer_top - 1;
        let required_height = prompt.height(width);
        assert!(required_height > height);
        assert!(required_height <= FULL_REVIEW_HEIGHT);
        let max_offset = required_height - height;
        let left = prompt.area.x + 1;
        let right = prompt.area.right() - 2;
        let mut page = first.clone();
        for offset in 0..=max_offset {
            if offset > 0 {
                prompt.scroll(-1);
                page = themed_buffer(prompt, width, height, t);
            }
            assert_eq!(prompt.scroll.offset(), offset);
            for row in 0..body_height {
                for x in left..right {
                    assert_eq!(
                        page[(x, row + 1)],
                        full[(x, row + offset + 1)],
                        "offset {offset}, cell ({x}, {row})"
                    );
                }
            }
            for row in footer_top..height {
                for x in prompt.area.x..prompt.area.right() {
                    assert_eq!(page[(x, row)], first[(x, row)], "footer at offset {offset}");
                }
            }
        }
        assert_ne!(page, first);
        prompt.handle_key(key(KeyCode::Home));
        assert_eq!(themed_buffer(prompt, width, height, t), first);
        prompt.handle_key(key(KeyCode::End));
        assert_eq!(themed_buffer(prompt, width, height, t), page);
        assert!(prompt.row_hits.iter().all(|hit| {
            prompt.area.contains(Position::new(hit.area.x, hit.area.y))
                && hit.area.bottom() <= prompt.area.bottom()
        }));
        full
    }

    fn golden_button(
        buffer: &mut Buffer,
        x: u16,
        y: u16,
        key: &str,
        description: &str,
        t: &Theme,
    ) -> u16 {
        buffer.set_string(x, y, "[", t.tool_dim);
        buffer.set_string(x + 1, y, key, t.keybind_key);
        let x = x + 1 + key.width() as u16;
        buffer.set_string(x, y, format!(" {description}"), t.keybind_desc);
        let x = x + 1 + description.width() as u16;
        buffer.set_string(x, y, "]", t.tool_dim);
        x + 1
    }

    fn golden_card(
        buffer: &mut Buffer,
        area: Rect,
        title: &str,
        base: Style,
        border: Style,
        lines: Vec<Line<'static>>,
    ) -> u16 {
        let content_width = area.width - 4;
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let height = paragraph.line_count(content_width) as u16 + 2;
        let area = Rect { height, ..area };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border)
            .style(base)
            .title_top(Line::styled(
                format!(" {title} "),
                border.add_modifier(Modifier::BOLD),
            ));
        let inner = Rect::new(area.x + 2, area.y + 1, content_width, height - 2);
        block.render(area, buffer);
        paragraph.render(inner, buffer);
        height
    }

    fn golden_fields(
        buffer: &mut Buffer,
        area: Rect,
        title: &str,
        fields: &[(&str, &str)],
        base: Style,
        t: &Theme,
    ) -> u16 {
        let width = area.width - 4;
        let measure = |text: &str, width| {
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .line_count(width) as u16
        };
        let sizes = fields
            .iter()
            .map(|(name, value)| {
                if width < 52 {
                    1 + measure(value, width)
                } else {
                    measure(name, 18).max(measure(value, width - 18))
                }
            })
            .collect::<Vec<_>>();
        let height = sizes.iter().sum::<u16>() + 2;
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .style(base)
            .title_top(Line::styled(
                format!(" {title} "),
                t.panel_border.add_modifier(Modifier::BOLD),
            ))
            .render(Rect { height, ..area }, buffer);
        let mut y = area.y + 1;
        for ((name, value), height) in fields.iter().zip(sizes) {
            let (label, value_area) = if width < 52 {
                (
                    Rect::new(area.x + 2, y, width, 1),
                    Rect::new(area.x + 2, y + 1, width, height - 1),
                )
            } else {
                (
                    Rect::new(area.x + 2, y, 18, height),
                    Rect::new(area.x + 20, y, width - 18, height),
                )
            };
            Paragraph::new(*name)
                .style(t.tool_dim)
                .wrap(Wrap { trim: false })
                .render(label, buffer);
            Paragraph::new(*value)
                .wrap(Wrap { trim: false })
                .render(value_area, buffer);
            y += height;
        }
        height
    }

    fn golden_review(
        width: u16,
        height: u16,
        t: &Theme,
        command: &str,
        confirming: bool,
    ) -> Buffer {
        let panel_width = width.min(112);
        let left = (width - panel_width) / 2;
        let body_width = panel_width - 3;
        let mut document = Buffer::empty(Rect::new(0, 0, body_width, 128));
        let base = Style::new().fg(t.foreground);
        let mut y = golden_card(
            &mut document,
            Rect::new(0, 0, body_width, 0),
            "Run command",
            t.code_block,
            t.accent,
            vec![Line::styled(
                command.to_owned(),
                t.code_block.fg(t.foreground).add_modifier(Modifier::BOLD),
            )],
        ) + 1;
        let lifetime = if confirming {
            "This conversation only."
        } else {
            "Choose Once, Conversation or Project below."
        };
        let mut lifetime_lines = vec![Line::styled(lifetime, t.status_notice)];
        if !confirming {
            lifetime_lines.push(Line::styled(
                format!("Project: {EXACT_PROJECT}"),
                t.tool_dim,
            ));
        }
        y += golden_card(
            &mut document,
            Rect::new(0, y, body_width, 0),
            "Lifetime",
            base,
            t.panel_border,
            lifetime_lines,
        ) + 1;
        y += golden_fields(
            &mut document,
            Rect::new(0, y, body_width, 0),
            "Future scope",
            SCOPE_FIELDS,
            t.tool_bg.fg(t.foreground),
            t,
        ) + 1;
        y += golden_card(
            &mut document,
            Rect::new(0, y, body_width, 0),
            "Review carefully",
            base,
            t.tool_warning,
            vec![Line::styled(PROTECTED, t.tool_warning)],
        );
        let mut footer: Vec<Vec<(&str, &str)>> = Vec::new();
        let groups = if confirming {
            vec![
                vec![("Enter/y", "Confirm")],
                vec![("F2", "Details"), ("Esc", "Back")],
            ]
        } else {
            vec![
                vec![("y", "Once"), ("s", "Conversation"), ("a", "Project")],
                vec![
                    ("r", "Scope"),
                    ("v", "Details"),
                    ("g", "Guidance"),
                    ("Esc", "Deny"),
                ],
            ]
        };
        for group in groups {
            let mut row = Vec::new();
            let mut used = if group[0].0 == "y" { 7 } else { 0 };
            for (key, description) in group {
                let size = (key.width() + description.width() + 3) as u16;
                let gap = if row.is_empty() { 0 } else { 2 };
                if used + gap + size > panel_width - 2 {
                    footer.push(row);
                    row = Vec::new();
                    used = 0;
                }
                used += size + if row.is_empty() { 0 } else { 2 };
                row.push((key, description));
            }
            footer.push(row);
        }
        let body_height = height - 3 - footer.len() as u16;
        let mut expected = Buffer::empty(Rect::new(0, 0, width, height));
        let title = if confirming {
            "Confirm permission"
        } else {
            "Permission required"
        };
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_style(t.panel_title)
            .title_top(Line::from(format!(" {title} · 1 pending ")))
            .render(Rect::new(left, 0, panel_width, height), &mut expected);
        for row in 0..body_height.min(y) {
            for x in 0..body_width {
                expected[(left + 1 + x, row + 1)] = document[(x, row)].clone();
            }
        }
        if y > body_height {
            let thumb = ((body_height * body_height + y / 2) / y).max(1);
            for row in 1..=thumb {
                expected.set_string(
                    left + panel_width - 2,
                    row,
                    "▐",
                    Style::new().fg(Color::Reset).bg(Color::Reset),
                );
            }
        }
        expected.set_string(
            left + 1,
            body_height + 1,
            "─".repeat(usize::from(panel_width - 2)),
            t.panel_border,
        );
        for (index, row) in footer.iter().enumerate() {
            let mut x = left + 1;
            let y = body_height + 2 + index as u16;
            if row[0].0 == "y" {
                expected.set_string(x, y, "Allow: ", t.tool_dim);
                x += 7;
            }
            for (key, description) in row {
                x = golden_button(&mut expected, x, y, key, description, t) + 2;
            }
        }
        expected
    }

    #[test_case(40, 10; "narrow_short")]
    #[test_case(40, 48; "narrow_tall")]
    #[test_case(80, 10; "normal_short")]
    #[test_case(80, 24; "normal_exact_call")]
    #[test_case(80, 32; "normal_tall")]
    #[test_case(140, 10; "wide_short")]
    #[test_case(140, 32; "wide_tall")]
    fn protected_review_matches_every_cell_and_style(width: u16, height: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            for confirming in [false, true] {
                let t = theme::load_by_name(name).unwrap();
                let mut prompt = protected_prompt(COMMAND);
                if confirming {
                    themed_buffer(&mut prompt, width, height, &t);
                    assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
                    assert!(prompt.confirmation.as_ref().unwrap().complete);
                }
                let actual = themed_buffer(&mut prompt, width, height, &t);
                let expected = golden_review(width, height, &t, COMMAND, confirming);
                assert_eq!(
                    actual,
                    expected,
                    "{name}, confirming={confirming}\n{}",
                    buffer_text(&actual)
                );
            }
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn unicode_command_uses_real_cells_and_preserves_metadata(width: u16) {
        const UNICODE: &str = "printf '%s' '界é'";
        const PREFIX: &str = "printf '%s' '";
        const HEIGHT: u16 = 32;
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = protected_prompt(UNICODE);
            let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
            let frame = terminal
                .draw(|frame| prompt.view_with_theme(frame, frame.area(), &t))
                .unwrap();
            let expected = golden_review(width, HEIGHT, &t, UNICODE, false);
            assert_eq!(frame.buffer, &expected);
            let x = (width - width.min(112)) / 2 + 3 + PREFIX.width() as u16;
            let backend = terminal.backend().buffer();
            for (column, symbol) in [(x, "界"), (x + 2, "é"), (x + 3, "'")] {
                assert_eq!(backend[(column, 2)].symbol(), symbol);
                assert_eq!(backend[(column, 2)].style(), expected[(column, 2)].style());
            }
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn exact_protected_summary_deduplicates_values_but_keeps_obligations(width: u16) {
        let mut prompt = protected_prompt(COMMAND);
        render(&mut prompt, width, 64);
        prompt.handle_key(key(KeyCode::Char('s')));
        let screen = render(&mut prompt, width, 64);
        assert_eq!(screen.matches(COMMAND).count(), 1, "{screen}");
        assert_eq!(screen.matches("/project").count(), 1, "{screen}");
        for obligation in [
            COMPACT_SCOPE,
            COMPACT_CONTEXT,
            "Run from",
            "protected",
            "Lifetime",
            "Context",
        ] {
            assert!(screen.contains(obligation), "{obligation}: {screen}");
        }
        let frozen = prompt.confirmation.as_ref().unwrap();
        assert!(frozen.complete);
        for obligation in [
            "Arguments",
            "Directories",
            "1 · Protection",
            "2 · Protection",
        ] {
            assert!(frozen.review.text().contains(obligation));
        }
    }

    #[test_case(false; "main")]
    #[test_case(true; "confirmation")]
    fn prepared_cache_call_is_comfortable_at_normal_terminal_size(confirming: bool) {
        const WIDTH: u16 = 80;
        const HEIGHT: u16 = 24;
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = protected_prompt(COMMAND);
            let answer = prompt.allow_answer(PermissionLifetime::Conversation);
            themed_buffer(&mut prompt, WIDTH, HEIGHT, &t);
            if confirming {
                assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
                let frozen = prompt.confirmation.as_ref().unwrap();
                assert_eq!(frozen.answer, answer);
                assert!(frozen.complete);
            }
            let screen = buffer_rows(&themed_buffer(&mut prompt, WIDTH, HEIGHT, &t));
            assert!(prompt.height(WIDTH) <= HEIGHT, "{screen}");
            for text in [
                COMMAND,
                EXACT_WORKDIR,
                COMPACT_SCOPE,
                COMPACT_CONTEXT,
                "Lifetime",
                PROTECTED,
                if confirming {
                    "[Enter/y Confirm]"
                } else {
                    "[s Conversation]"
                },
                if confirming {
                    "This conversation only."
                } else {
                    "Choose Once, Conversation or Project below."
                },
            ] {
                assert!(screen.contains(text), "{text}:\n{screen}");
            }
            assert_eq!(screen.matches(COMMAND).count(), 1);
            assert_eq!(
                screen
                    .split_whitespace()
                    .filter(|word| *word == EXACT_WORKDIR)
                    .count(),
                1
            );
            assert_eq!(screen.matches(PROTECTED).count(), 1);
            for hidden in ["normalized", "Unrestricted", "Alternatives", "1 ·", "2 ·"] {
                assert!(!screen.contains(hidden), "{hidden}:\n{screen}");
            }
            let details = super::details_body(prompt.current().unwrap())
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            for technical in [
                "normalized_command",
                "possible_workdirs",
                "resources[1].protected",
                "input.command",
            ] {
                assert!(details.contains(technical), "{technical}");
            }
        }
    }

    #[test_case(KeyCode::Enter; "enter_is_fresh")]
    #[test_case(KeyCode::Char('y'); "y_is_fresh")]
    fn conversation_confirmation_does_not_ask_to_rearm_its_opening_shortcut(confirm: KeyCode) {
        let mut prompt = protected_prompt(COMMAND);
        render(&mut prompt, 80, 24);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
        assert!(!render(&mut prompt, 80, 24).contains(super::REARM_MESSAGE));
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
        assert!(!render(&mut prompt, 80, 24).contains(super::REARM_MESSAGE));
        assert_eq!(prompt.handle_key(key(confirm)).unwrap().answer, frozen);
    }

    #[test_case(false; "canonical_digest")]
    #[test_case(true; "equivalent_literals")]
    fn exact_call_compaction_is_proven_from_rules_not_labels(literal: bool) {
        const RENAMED: &str = "different-option-id";
        let mut request = prepared_protected_request(COMMAND);
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact")
            .unwrap();
        option.id = RENAMED.into();
        option.label = "Untrusted display label".into();
        if literal {
            for (constraint, resource) in option.rule.resources.iter_mut().zip(&request.resources) {
                constraint.selector = PermissionResourceSelector::Exact {
                    value: resource.value.clone(),
                };
                for (name, selector) in &mut constraint.attributes {
                    *selector = PermissionResourceSelector::Exact {
                        value: resource.attributes[name].clone(),
                    };
                }
            }
        }
        let answer = PermissionAnswer::AllowOption {
            option_id: RENAMED.into(),
            lifetime: PermissionLifetime::Conversation,
        };
        let review = super::ReviewDocument::new(&request, &answer);
        assert_eq!(review.exact_call_workdir.as_deref(), Some(EXACT_WORKDIR));
    }

    #[test_case("arguments"; "resource_based_grant")]
    #[test_case("selected"; "selected_input_grant")]
    #[test_case("input"; "changed_raw_input")]
    #[test_case("digest"; "mismatched_input_digest")]
    #[test_case("selector"; "any_resource")]
    #[test_case("prefix"; "non_exact_selector")]
    #[test_case("guard"; "any_guard")]
    #[test_case("missing"; "unbound_preparation")]
    #[test_case("protection"; "unbound_protection")]
    #[test_case("access"; "unbound_access")]
    #[test_case("alternative"; "broader_alternative")]
    #[test_case("workdir"; "different_starting_directories")]
    #[test_case("executor"; "non_native_executor")]
    fn exact_call_compaction_rejects_unproven_bounds(change: &str) {
        let mut request = prepared_protected_request(COMMAND);
        let rule = &mut request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact")
            .unwrap()
            .rule;
        match change {
            "arguments" => rule.arguments = PermissionArgumentConstraint::Unconstrained,
            "selected" => {
                rule.arguments = PermissionArgumentConstraint::Selected {
                    arguments: Vec::new(),
                }
            }
            "input" => request.input["timeout"] = json!(1000),
            "digest" => {
                rule.arguments = PermissionArgumentConstraint::Exact {
                    digest: "different".into(),
                }
            }
            "selector" => rule.resources[0].selector = PermissionResourceSelector::Any,
            "prefix" => {
                rule.resources[0].selector = PermissionResourceSelector::Prefix {
                    value: COMMAND.into(),
                }
            }
            "guard" => {
                rule.resources[0]
                    .attributes
                    .insert("workdir".into(), PermissionResourceSelector::Any);
            }
            "missing" => {
                rule.resources[0].attributes.remove("possible_workdirs");
            }
            "protection" => rule.resources[0].protected = None,
            "access" => rule.resources[0].access = None,
            "alternative" => {
                let mut extra = rule.resources[0].clone();
                extra.attributes.clear();
                rule.resources.push(extra);
            }
            "workdir" => {
                const OTHER: &str = "/elsewhere";
                request.resources[1]
                    .attributes
                    .insert("workdir".into(), OTHER.into());
                rule.resources[1].attributes.insert(
                    "workdir".into(),
                    PermissionResourceSelector::Exact {
                        value: OTHER.into(),
                    },
                );
            }
            "executor" => request.executor = PermissionExecutorKind::UnknownLegacy,
            _ => unreachable!(),
        }
        let mut prompt = exact_prompt(request);
        let review = super::ReviewDocument::new(
            prompt.current().unwrap(),
            &prompt.allow_answer(PermissionLifetime::Conversation),
        );
        assert!(review.exact_call_workdir.is_none());
        assert!(!render(&mut prompt, 80, FULL_REVIEW_HEIGHT).contains(COMPACT_CONTEXT));
    }

    #[test_case(40, false; "narrow_broad")]
    #[test_case(80, false; "normal_broad")]
    #[test_case(140, false; "wide_broad")]
    #[test_case(40, true; "narrow_protected_phrase")]
    #[test_case(80, true; "normal_protected_phrase")]
    #[test_case(140, true; "wide_protected_phrase")]
    fn phrase_review_remains_frozen_and_requires_exact_input(width: u16, protected: bool) {
        for name in ["ayu_dark", "ayu_light"] {
            let mut prompt = if protected {
                protected_prompt(COMMAND)
            } else {
                open_prompt()
            };
            if protected {
                prompt
                    .requests
                    .front_mut()
                    .unwrap()
                    .request
                    .options
                    .iter_mut()
                    .find(|option| option.id == "allow_exact")
                    .unwrap()
                    .confirmation = Some(PROTECTED_PHRASE.into());
            } else {
                prompt.select_authority("allow_any_command".into());
            }
            let t = theme::load_by_name(name).unwrap();
            themed_buffer(&mut prompt, width, 64, &t);
            prompt.handle_key(key(KeyCode::Char('s')));
            let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
            let before = themed_buffer(&mut prompt, width, FULL_REVIEW_HEIGHT, &t);
            prompt.selected_option = "allow_exact_resources".into();
            assert_eq!(
                themed_buffer(&mut prompt, width, FULL_REVIEW_HEIGHT, &t),
                before
            );
            let phrase = if protected {
                PROTECTED_PHRASE
            } else {
                BROAD_PHRASE
            };
            let screen = buffer_text(&before);
            assert!(screen.contains(phrase), "{screen}");
            assert!(screen.contains("Review carefully"));
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            prompt.handle_paste(phrase);
            prompt.handle_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ));
            assert_eq!(
                prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
                frozen
            );
        }
    }

    #[test_case(40, 10; "narrow_short")]
    #[test_case(80, 18; "normal")]
    #[test_case(140, 18; "wide")]
    fn inspector_scroll_geometry_keeps_footer_fixed(width: u16, height: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let mut prompt = suggested_prompt();
            prompt.handle_key(key(KeyCode::Char('r')));
            prompt.handle_key(key(KeyCode::Char('i')));
            let t = theme::load_by_name(name).unwrap();
            let full = assert_scrollable_body(&mut prompt, width, height, &t);
            assert!(buffer_text(&full).contains("Current row matches"));
        }
    }

    #[test]
    fn fitting_inspector_stays_at_the_top_when_scrolled_down() {
        let mut prompt = suggested_prompt();
        prompt.handle_key(key(KeyCode::Char('r')));
        prompt.handle_key(key(KeyCode::Char('i')));
        let t = theme::load_by_name("ayu_dark").unwrap();
        let before = themed_buffer(&mut prompt, 140, 28, &t);
        assert!(prompt.height(140) <= 28);
        assert!(buffer_text(&before).contains("Current row matches"));
        prompt.scroll(-8);
        assert_eq!(themed_buffer(&mut prompt, 140, 28, &t), before);
        assert_eq!(prompt.scroll.offset(), 0);
    }

    fn buffer_rows(buffer: &Buffer) -> String {
        let mut text = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += (symbol.width() as u16).max(1);
            }
            text.push('\n');
        }
        text
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn live_future_scope_renders_shared_targets_and_selected_conditions(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = typed_alternatives_prompt();
            let answer = prompt.allow_answer(PermissionLifetime::Conversation);
            let first = reveal_scope(&mut prompt, width, &t);
            assert!(buffer_text(&first).contains(TARGETS_LABEL));
            assert!(
                prompt
                    .scope_view
                    .hits
                    .iter()
                    .any(|hit| hit.control == ScopeControl::Disclosure(Disclosure::Conditions))
            );
            let height = prompt.height(width);
            click_scope_control(&mut prompt, width, &t, ScopeControl::Target(1));
            let conditions = click_scope_control(
                &mut prompt,
                width,
                &t,
                ScopeControl::Disclosure(Disclosure::Conditions),
            );
            assert_eq!(prompt.scope_view.target, 1);
            assert!(buffer_text(&conditions).contains(SECOND_CONDITIONS));
            assert!(compact_cells(&conditions).contains("Protectionprotectedonly"));
            let target =
                PromptTarget::VisualScope(0, ScopeControl::Disclosure(Disclosure::Conditions));
            let area = prompt
                .row_hits
                .iter()
                .find(|hit| hit.target == target)
                .unwrap()
                .area;
            prompt.focus = Some(target);
            let mut focused = conditions.clone();
            focused.set_style(area, Style::default().add_modifier(Modifier::REVERSED));
            assert_eq!(
                themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t),
                focused
            );
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            assert_eq!(prompt.scope_view.disclosure, None);
            let mut properties = compact_cells(&conditions);
            for _ in 0..MAX_PROPERTY_PAGES {
                let down = prompt
                    .scope_view
                    .hits
                    .iter()
                    .find_map(|hit| match hit.control {
                        ScopeControl::Scroll(delta) if delta > 0 => Some(hit.control.clone()),
                        _ => None,
                    })
                    .unwrap();
                properties.push_str(&compact_cells(&click_scope_control(
                    &mut prompt,
                    width,
                    &t,
                    down,
                )));
            }
            assert!(properties.contains(UNRESTRICTED_ATTRIBUTE));
            let clamped = prompt.scope_view.offset;
            let up = prompt
                .scope_view
                .hits
                .iter()
                .find_map(|hit| match hit.control {
                    ScopeControl::Scroll(delta) if delta < 0 => Some(hit.control.clone()),
                    _ => None,
                })
                .unwrap();
            click_scope_control(&mut prompt, width, &t, up);
            assert!(prompt.scope_view.offset < clamped);
            assert_eq!(prompt.height(width), height);
            assert_eq!(
                prompt.allow_answer(PermissionLifetime::Conversation),
                answer
            );
            assert!(prompt.confirmation.is_none());
        }
    }

    #[test_case(40, false; "narrow_alternatives")]
    #[test_case(80, false; "normal_alternatives")]
    #[test_case(140, false; "wide_alternatives")]
    #[test_case(40, true; "narrow_remote")]
    #[test_case(80, true; "normal_remote")]
    #[test_case(140, true; "wide_remote")]
    fn canonical_scope_details_keep_every_cell_reachable(width: u16, remote: bool) {
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = if remote {
                exact_prompt(remote_request(REMOTE_IDS, &["root", "folder/file"], true))
            } else {
                typed_alternatives_prompt()
            };
            let height = prompt.height(width) + 2;
            let full = themed_buffer(&mut prompt, width, height, &t);
            let (card, content) = prompt
                .review_layout(prompt.area.width - 3, &t)
                .cards
                .into_iter()
                .find(|(_, card)| matches!(card.content, CardContent::Scope(..)))
                .unwrap();
            let CardContent::Scope(_, model, _, fields) = content.content else {
                unreachable!()
            };
            if remote {
                for (label, value) in [
                    "Trust anchor",
                    "Server",
                    "Workspace",
                    "Generation",
                    "Namespace",
                    "Principal",
                    "Remote project",
                ]
                .into_iter()
                .zip(REMOTE_IDS)
                {
                    assert!(
                        fields
                            .iter()
                            .any(|field| field.label == label && field.value == value)
                    );
                }
                assert!(
                    fields.iter().any(|field| field.label == "Target key"
                        && field.value == REMOTE_ESCAPED_TARGET_KEY)
                );
            } else {
                for label in [
                    "1 · Protection",
                    "2 · Protection",
                    "1 · Starting directory",
                    "2 · Starting directory",
                    "1 · zone",
                    "2 · zone",
                ] {
                    assert!(fields.iter().any(|field| field.label == label));
                }
                assert!(
                    fields
                        .iter()
                        .any(|field| field.value.contains(ALTERNATIVE_DIRECTORY))
                );
            }
            let summary_height = ScopeView::summary_height(&model, card.width - 4);
            let start = card.y + summary_height + 2;
            let x = prompt.area.x + card.x + 3;
            let y = prompt.area.y + start + 1;
            let mut expected = Buffer::empty(Rect::new(0, 0, card.width, card.height));
            let pairs: Vec<_> = fields
                .iter()
                .map(|field| (field.label.as_str(), field.value.as_str()))
                .collect();
            let rows = golden_fields(
                &mut expected,
                Rect::new(0, 0, card.width, 0),
                CANONICAL_SCOPE_DETAILS,
                &pairs,
                t.tool_bg.fg(t.foreground),
                &t,
            ) - 2;
            assert!(buffer_rows(&full).contains(CANONICAL_SCOPE_DETAILS));
            for row in 0..rows {
                for column in 0..card.width - 4 {
                    assert_eq!(full[(x + column, y + row)], expected[(column + 2, row + 1)]);
                }
            }
            themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t);
            for row in 0..rows {
                prompt.scroll.scroll_to(start + row);
                let page = themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t);
                let visible_y = prompt.area.y + 1 + start + row - prompt.scroll.offset();
                for column in 0..card.width - 4 {
                    assert_eq!(page[(x + column, visible_y)], full[(x + column, y + row)]);
                }
            }
            assert!(prompt.confirmation.is_none());
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn live_slot_and_all_of_controls_inspect_without_changing_authority(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = typed_pattern_prompt();
            reveal_scope(&mut prompt, width, &t);
            let answer = prompt.allow_answer(PermissionLifetime::Conversation);
            let id = prompt
                .scope_view
                .hits
                .iter()
                .find_map(|hit| match hit.control {
                    ScopeControl::Slot(id) => Some(id),
                    _ => None,
                })
                .unwrap();
            let slot = click_scope_control(&mut prompt, width, &t, ScopeControl::Slot(id));
            assert!(compact_cells(&slot).contains("SameID=equalvalues"));
            let conditions = click_scope_control(
                &mut prompt,
                width,
                &t,
                ScopeControl::Disclosure(Disclosure::Conditions),
            );
            assert!(buffer_text(&conditions).contains("ALL OF · target 1"));
            click_scope_control(&mut prompt, width, &t, ScopeControl::Slot(id));
            assert_eq!(prompt.scope_view.slot, Some(id));
            assert_eq!(prompt.scope_view.disclosure, None);
            assert_eq!(
                prompt.allow_answer(PermissionLifetime::Conversation),
                answer
            );
            assert!(prompt.confirmation.is_none());
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn frozen_scope_controls_and_held_enter_cannot_change_or_confirm(width: u16) {
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let mut prompt = exact_prompt(remote_request(REMOTE_IDS, &["root", "file"], true));
            themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t);
            prompt.focus = Some(PromptTarget::Hint(key(KeyCode::Char('s'))));
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
            let scope = reveal_scope(&mut prompt, width, &t);
            assert!(buffer_text(&scope).contains("[ALL OF]"));
            assert!(
                prompt
                    .row_hits
                    .iter()
                    .all(|hit| !matches!(hit.target, PromptTarget::VisualScope(..)))
            );
            let state = prompt.scope_view.clone();
            for control in [
                ScopeControl::Scroll(4),
                ScopeControl::Target(1),
                ScopeControl::Disclosure(Disclosure::Identity),
            ] {
                assert!(
                    prompt
                        .activate(PromptTarget::VisualScope(0, control))
                        .is_none()
                );
            }
            assert_eq!(prompt.scope_view.offset, state.offset);
            assert_eq!(prompt.scope_view.target, state.target);
            assert_eq!(prompt.scope_view.disclosure, state.disclosure);
            assert!(
                prompt
                    .handle_key(KeyEvent::new_with_kind(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                        KeyEventKind::Repeat
                    ))
                    .is_none()
            );
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
            assert_eq!(prompt.confirmation.as_ref().unwrap().answer, frozen);
            assert!(
                prompt
                    .handle_key(KeyEvent::new_with_kind(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                        KeyEventKind::Release
                    ))
                    .is_none()
            );
            themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t);
            assert_eq!(
                prompt.handle_key(key(KeyCode::Enter)).unwrap().answer,
                frozen
            );
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn exact_once_scope_chooser_exposes_the_typed_rule(width: u16) {
        let t = theme::load_by_name("ayu_dark").unwrap();
        let mut prompt = protected_prompt(COMMAND);
        assert_eq!(prompt.lifetime, PermissionLifetime::Once);
        themed_buffer(&mut prompt, width, LIVE_SCOPE_HEIGHT, &t);
        assert!(prompt.handle_key(key(KeyCode::Char('r'))).is_none());
        assert!(prompt.panel == Panel::Scopes);
        let scope = reveal_scope(&mut prompt, width, &t);
        assert!(buffer_text(&scope).contains(TARGETS_LABEL));
        assert!(
            prompt
                .scope_view
                .hits
                .iter()
                .any(|hit| hit.control == ScopeControl::Disclosure(Disclosure::Conditions))
        );
        assert!(prompt.confirmation.is_none());
        assert_ne!(prompt.lifetime, PermissionLifetime::Once);
        assert!(prompt.handle_key(key(KeyCode::Char('p'))).is_none());
        assert!(prompt.panel == Panel::Main);
        assert!(buffer_text(&reveal_scope(&mut prompt, width, &t)).contains(TARGETS_LABEL));
        assert!(prompt.confirmation.is_none());
    }

    #[test]
    #[ignore = "writes private visual review buffers under /tmp"]
    fn export_permission_review_buffers() {
        let directory = Builder::new()
            .prefix("caudra-permission-review-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), Permissions::from_mode(0o700)).unwrap();
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            for (width, height) in [(40, 24), (40, 48), (80, 24), (80, 32), (140, 32)] {
                for panel in [
                    "main",
                    "confirm",
                    "broad",
                    "remote-main",
                    "remote-confirm",
                    "pattern-read",
                    "pattern-edit",
                    "pattern-tuples",
                    "typed-alternatives",
                    "typed-alternatives-selected",
                    "typed-canonical-details",
                    "typed-remote-scope",
                    "typed-remote-identities",
                    "typed-pattern-scope",
                    "typed-pattern-slot",
                    "typed-once-scope-chooser",
                ] {
                    let mut prompt = match panel {
                        "typed-alternatives"
                        | "typed-alternatives-selected"
                        | "typed-canonical-details" => typed_alternatives_prompt(),
                        "typed-remote-scope" | "typed-remote-identities" => {
                            exact_prompt(remote_request(REMOTE_IDS, &["root", "folder/file"], true))
                        }
                        "typed-pattern-scope" | "typed-pattern-slot" => typed_pattern_prompt(),
                        "broad" => open_prompt(),
                        "pattern-read" | "pattern-edit" | "pattern-tuples" => suggested_prompt(),
                        "remote-main" | "remote-confirm" => exact_prompt(remote_request(
                            REMOTE_IDS,
                            &["root", "folder/file"],
                            panel == "remote-confirm",
                        )),
                        _ => protected_prompt(COMMAND),
                    };
                    if panel == "broad" {
                        prompt.select_authority("allow_any_command".into());
                    }
                    if panel.starts_with("pattern-") {
                        prompt.open_scope_editor();
                        prompt.open_inspector();
                        match panel {
                            "pattern-edit" => prompt.activate_inspector(InspectorControl::Name),
                            "pattern-tuples" => {
                                prompt.activate_inspector(InspectorControl::Observations)
                            }
                            _ => {}
                        }
                    }
                    themed_buffer(&mut prompt, width, height, &t);
                    if matches!(panel, "confirm" | "broad" | "remote-confirm") {
                        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_none());
                        assert!(prompt.confirmation.is_some());
                    }
                    let buffer = themed_buffer(&mut prompt, width, height, &t);
                    let buffer = if panel.starts_with("typed-") {
                        if panel == "typed-once-scope-chooser" {
                            prompt.open_scope_editor();
                        }
                        reveal_scope(&mut prompt, width, &t);
                        match panel {
                            "typed-alternatives-selected" => {
                                click_scope_control(
                                    &mut prompt,
                                    width,
                                    &t,
                                    ScopeControl::Target(1),
                                );
                            }
                            "typed-remote-identities" => {
                                click_scope_control(
                                    &mut prompt,
                                    width,
                                    &t,
                                    ScopeControl::Disclosure(Disclosure::Identity),
                                );
                            }
                            "typed-pattern-slot" => {
                                let slot = prompt
                                    .scope_view
                                    .hits
                                    .iter()
                                    .find_map(|hit| match hit.control {
                                        ScopeControl::Slot(id) => Some(id),
                                        _ => None,
                                    })
                                    .unwrap();
                                click_scope_control(
                                    &mut prompt,
                                    width,
                                    &t,
                                    ScopeControl::Slot(slot),
                                );
                            }
                            "typed-canonical-details" => {
                                let card = scope_card(&prompt, &t);
                                let rule = prompt
                                    .review_layout(prompt.area.width - 3, &t)
                                    .cards
                                    .into_iter()
                                    .find_map(|(_, card)| match card.content {
                                        CardContent::Scope(_, model, _, _) => Some(model),
                                        _ => None,
                                    })
                                    .unwrap();
                                prompt.scroll.scroll_to(
                                    card.y + 1 + ScopeView::summary_height(&rule, card.width - 4),
                                );
                            }
                            _ => {}
                        }
                        themed_buffer(&mut prompt, width, height, &t)
                    } else {
                        buffer
                    };
                    let stem = format!("{panel}-{name}-{width}x{height}");
                    for (extension, contents) in [
                        ("txt", buffer_rows(&buffer)),
                        ("cells", format!("{buffer:#?}")),
                    ] {
                        let mut options = OpenOptions::new();
                        options.write(true).create_new(true);
                        #[cfg(unix)]
                        options.mode(PRIVATE_ARTIFACT_MODE);
                        let mut file = options
                            .open(directory.path().join(format!("{stem}.{extension}")))
                            .unwrap();
                        file.write_all(contents.as_bytes()).unwrap();
                    }
                }
            }
        }
        println!("Permission review buffers: {}", directory.keep().display());
    }

    #[test]
    fn incomplete_confirmation_does_not_unlock_at_the_end() {
        let mut prompt = open_prompt();
        let request = &mut prompt.requests.front_mut().unwrap().request;
        request.resources[0].attributes.insert(
            "workdir".into(),
            "/long/".repeat(super::super::details::MAX_REVIEW_CHARS),
        );
        prompt.select_authority("allow_commands_in_workdir".into());
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Char('s')));
        assert!(!prompt.confirmation.as_ref().unwrap().complete);
        render(&mut prompt, 40, 10);
        prompt.handle_key(key(KeyCode::End));
        render(&mut prompt, 40, 10);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(
            !prompt
                .row_hits
                .iter()
                .any(|hit| hit.target == PromptTarget::Hint(key(KeyCode::Enter)))
        );
    }

    #[test_case("pem"; "inert_pem")]
    #[test_case("query"; "inert_query")]
    #[test_case("fragment"; "inert_fragment")]
    fn inert_redacted_values_do_not_disable_an_exact_approval(kind: &str) {
        let command = match kind {
            "pem" => format!(
                "echo \"-----BEGIN PRIVATE KEY-----{HIDDEN_SECRET}-----END PRIVATE KEY-----\""
            ),
            "query" => format!("echo \"https://example.invalid/path?q={HIDDEN_SECRET}\""),
            _ => format!("echo \"https://example.invalid/path#{HIDDEN_SECRET}\""),
        };
        let shown = review_text(&command);
        assert!(!shown.contains(INCOMPLETE_REDACTION));
        assert!(!shown.contains(HIDDEN_SECRET));
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(native_shell_request(&command)), None);
        prompt.select_authority("allow_exact".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(prompt.handle_key(key(KeyCode::Char('s'))).is_some());
    }

    #[test_case("pem", "$(touch /tmp/marker)"; "pem_substitution")]
    #[test_case("pem", "`touch /tmp/marker`"; "pem_backticks")]
    #[test_case("pem", "$(\ntouch /tmp/marker\n)"; "pem_multiline")]
    #[test_case("pem", "<(touch /tmp/marker)"; "pem_process_substitution")]
    #[test_case("pem", ">\\\n(touch /tmp/marker)"; "pem_process_line_continuation")]
    #[test_case("query", "$(touch /tmp/marker)"; "query_substitution")]
    #[test_case("query", "`touch /tmp/marker`"; "query_backticks")]
    #[test_case("query", "$\\\n(touch /tmp/marker)"; "query_line_continuation")]
    #[test_case("query", "<(touch /tmp/marker)"; "query_process_substitution")]
    #[test_case("fragment", "$(\ntouch /tmp/marker\n)"; "fragment_multiline")]
    #[test_case("fragment", "`touch /tmp/marker`"; "fragment_backticks")]
    #[test_case("unquoted", "$(printf never-display-this)"; "split_unquoted_expansion")]
    #[test_case("unquoted", "`printf never-display-this`"; "split_unquoted_backticks")]
    #[test_case("userinfo", "$(touch /tmp/marker)"; "userinfo_substitution")]
    fn executable_text_hidden_by_redaction_disables_confirmation(kind: &str, expansion: &str) {
        let hidden = format!("{HIDDEN_SECRET}{expansion}");
        let command = match kind {
            "pem" => {
                format!("echo \"-----BEGIN PRIVATE KEY-----{hidden}-----END PRIVATE KEY-----\"")
            }
            "query" => format!("echo \"https://example.invalid/path?q={hidden}\""),
            "fragment" => format!("echo \"https://example.invalid/path#{hidden}\""),
            "userinfo" => format!("echo \"https://user:{hidden}@example.invalid/path\""),
            _ => format!("echo https://example.invalid/path?q={hidden}"),
        };
        let shown = review_text(&command);
        assert!(shown.contains(INCOMPLETE_REDACTION), "{shown}");
        assert!(!shown.contains(HIDDEN_SECRET), "{shown}");
        assert!(review_text(&shown).contains(INCOMPLETE_REDACTION));
        let mut prompt = confirm_request(native_shell_request(&command));
        let frozen = prompt.confirmation.as_ref().unwrap();
        assert!(!frozen.complete);
        assert!(frozen.review.text().contains(INCOMPLETE_REDACTION));
        assert!(!frozen.review.text().contains(HIDDEN_SECRET));
        for width in [40, 80, 140] {
            let screen = render(&mut prompt, width, 10);
            assert!(!screen.contains(HIDDEN_SECRET));
            assert!(!prompt.row_hits.iter().any(|hit| matches!(&hit.target, PromptTarget::Hint(event) if event.code == KeyCode::Enter)));
            prompt.handle_key(key(KeyCode::Tab));
            assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
            assert!(prompt.confirmation.is_some());
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn every_remote_binding_component_distinguishes_the_rendered_approval(width: u16) {
        let parts = ["root", "file"];
        for name in ["ayu_light", "ayu_dark"] {
            let t = theme::load_by_name(name).unwrap();
            let mut original = confirm_request(remote_request(REMOTE_IDS, &parts, true));
            assert!(original.confirmation.as_ref().unwrap().complete);
            let before = themed_buffer(&mut original, width, 80, &t);
            assert!(
                original
                    .row_hits
                    .iter()
                    .all(|hit| !matches!(hit.target, PromptTarget::VisualScope(_, _)))
            );
            let rendered = displayed_scope_fields(&original);
            for authority in &original.confirmation.as_ref().unwrap().review.authorities {
                assert!(
                    authority
                        .fields
                        .iter()
                        .all(|field| rendered.contains(field))
                );
            }
            let authority_row = buffer_rows(&before)
                .lines()
                .position(|line| line.contains("Authority SHA-256"))
                .unwrap() as u16;
            themed_buffer(&mut original, width, 18, &t);
            original.scroll.scroll_to(authority_row - 1);
            let compact_before = themed_buffer(&mut original, width, 18, &t);
            for index in 0..REMOTE_IDS.len() {
                let mut ids = REMOTE_IDS;
                ids[index] = "different";
                let mut changed = confirm_request(remote_request(ids, &parts, true));
                assert!(changed.confirmation.as_ref().unwrap().complete);
                assert_ne!(
                    themed_buffer(&mut changed, width, 80, &t),
                    before,
                    "identity field {index}"
                );
                themed_buffer(&mut changed, width, 18, &t);
                changed.scroll.scroll_to(authority_row - 1);
                assert_ne!(
                    themed_buffer(&mut changed, width, 18, &t),
                    compact_before,
                    "compact identity field {index}"
                );
            }
            let text = original.confirmation.as_ref().unwrap().review.text();
            for label in [
                "Trust anchor",
                "Server",
                "Workspace",
                "Generation",
                "Namespace",
                "Principal",
                "Remote project",
                "Target key",
                "Display path only",
            ] {
                assert!(text.contains(label), "{label}: {text}");
            }
        }
    }

    #[test_case(false; "exact")]
    #[test_case(true; "subtree")]
    fn remote_scope_and_target_keys_are_not_inferred_from_display_paths(subtree: bool) {
        let prepare = |request| {
            if subtree {
                confirm_request(request)
            } else {
                exact_prompt(request)
            }
        };
        let mut first = prepare(remote_request(
            REMOTE_IDS,
            &["root", "folder/file"],
            subtree,
        ));
        let mut second = prepare(remote_request(
            REMOTE_IDS,
            &["root", "folder", "file"],
            subtree,
        ));
        assert_ne!(render(&mut first, 80, 80), render(&mut second, 80, 80));
        for (prompt, target) in [
            (&mut first, REMOTE_ESCAPED_TARGET_KEY),
            (&mut second, REMOTE_SPLIT_TARGET_KEY),
        ] {
            let visible = (0..prompt.height(80)).any(|_| {
                if render(prompt, 80, 18).contains(target) {
                    return true;
                }
                prompt.scroll(-1);
                false
            });
            assert!(visible, "target key missing from compact review: {target}");
        }
        let original_fields = displayed_scope_fields(&first);
        let other_fields = displayed_scope_fields(&second);
        assert!(
            original_fields.iter().any(
                |field| field.label == "Target key" && field.value == REMOTE_ESCAPED_TARGET_KEY
            )
        );
        assert!(
            other_fields
                .iter()
                .any(|field| field.label == "Target key" && field.value == REMOTE_SPLIT_TARGET_KEY)
        );
        let scope = if subtree {
            "Scope key and descendants: /root"
        } else {
            "Exact scope key: /root/folder%2Ffile"
        };
        assert!(
            original_fields
                .iter()
                .any(|field| field.label == "Read" && field.value == scope)
        );
        let mut display_change = remote_request(REMOTE_IDS, &["root", "folder/file"], subtree);
        display_change.resources[0]
            .attributes
            .insert("display_path".into(), "/unrelated-display-path".into());
        let mut changed = prepare(display_change);
        render(&mut changed, 80, 18);
        let changed_fields = displayed_scope_fields(&changed);
        for label in ["Read", "Target key", "Authority SHA-256"] {
            assert!(
                original_fields.iter().find(|field| field.label == label)
                    == changed_fields.iter().find(|field| field.label == label)
            );
        }
        for prompt in [&mut first, &mut second, &mut changed] {
            if subtree {
                assert!(prompt.confirmation.as_ref().unwrap().complete);
            } else {
                let expected = prompt.allow_answer(PermissionLifetime::Conversation);
                assert_eq!(
                    prompt.handle_key(key(KeyCode::Char('s'))).unwrap().answer,
                    expected
                );
                assert!(prompt.confirmation.is_none());
            }
        }
    }

    #[test_case(false; "selector_binding_mismatch")]
    #[test_case(true; "principal_binding_mismatch")]
    fn unverified_remote_bindings_disable_confirmation(principal_mismatch: bool) {
        let mut request = remote_request(REMOTE_IDS, &["root", "file"], false);
        let other = AuthorityIdentity::new(
            SourceTrustAnchor::new("other").unwrap(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        if principal_mismatch {
            let PermissionSubject::RemoteNative { identity, .. } = &mut request.subject else {
                unreachable!()
            };
            identity.principal = AuthenticatedPrincipalId::new(other, "principal").unwrap();
        } else {
            let resource = &mut request
                .options
                .iter_mut()
                .find(|option| option.id == "allow_exact")
                .unwrap()
                .rule
                .resources[0];
            let PermissionResourceSelector::RemoteResource { identity, .. } =
                &mut resource.selector
            else {
                unreachable!()
            };
            identity.authority = other;
        }
        let mut prompt = confirm_request(request);
        assert!(!prompt.confirmation.as_ref().unwrap().complete);
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Tab));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
    }

    #[test_case("pem"; "future_pem_guard")]
    #[test_case("fragment"; "future_fragment_guard")]
    fn hidden_future_command_guards_cannot_borrow_a_lossy_projection(kind: &str) {
        let hidden = format!("$(printf '{HIDDEN_SECRET}')");
        let command = if kind == "pem" {
            format!("echo \"-----BEGIN PRIVATE KEY-----{hidden}-----END PRIVATE KEY-----\"")
        } else {
            format!("echo \"https://example.invalid/path#{hidden}\"")
        };
        let mut request = native_shell_request(COMMAND);
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact")
            .unwrap();
        option.rule.arguments = PermissionArgumentConstraint::Unconstrained;
        option.rule.resources[0].selector = PermissionResourceSelector::Any;
        option.rule.resources[0].attributes.insert(
            "normalized_command".into(),
            PermissionResourceSelector::Exact { value: command },
        );
        let prompt = confirm_request(request);
        let frozen = prompt.confirmation.as_ref().unwrap();
        assert!(!frozen.complete);
        assert!(frozen.review.text().contains(INCOMPLETE_REDACTION));
        assert!(!frozen.review.text().contains(HIDDEN_SECRET));
    }

    #[test_case("presence"; "guard_presence")]
    #[test_case("any"; "explicit_any_guard")]
    #[test_case("value"; "guard_value")]
    #[test_case("protected"; "protected_guard")]
    fn predicate_alternatives_never_merge_distinct_guards(change: &str) {
        const FIRST_COMMAND: &str = "git status";
        const OTHER_COMMAND: &str = "cargo test";
        let mut request = native_shell_request(FIRST_COMMAND);
        let option = request
            .options
            .iter_mut()
            .find(|option| option.id == "allow_exact")
            .unwrap();
        option.rule.arguments = PermissionArgumentConstraint::Unconstrained;
        let first = &mut option.rule.resources[0];
        first.selector = PermissionResourceSelector::Any;
        first.protected = Some(false);
        first.attributes.retain(|name, _| name == "workdir");
        first.attributes.insert(
            "normalized_command".into(),
            PermissionResourceSelector::Exact {
                value: FIRST_COMMAND.into(),
            },
        );
        let mut second = first.clone();
        match change {
            "presence" => {
                second.attributes.remove("normalized_command");
            }
            "value" => {
                second.attributes.insert(
                    "normalized_command".into(),
                    PermissionResourceSelector::Exact {
                        value: OTHER_COMMAND.into(),
                    },
                );
            }
            "any" => {
                second
                    .attributes
                    .insert("normalized_command".into(), PermissionResourceSelector::Any);
            }
            _ => second.protected = Some(true),
        }
        option.rule.resources.push(second);
        let mut prompt = confirm_request(request);
        let frozen = prompt.confirmation.as_ref().unwrap();
        assert!(frozen.complete);
        let fields = &frozen.review.authorities[0].fields;
        for label in [
            "1 · Execute",
            "2 · Execute",
            "1 · Protection",
            "2 · Protection",
            "1 · Preparation",
            "2 · Preparation",
        ] {
            assert!(
                fields.iter().any(|field| field.label == label),
                "{label}: {}",
                frozen.review.text()
            );
        }
        let expected = match change {
            "presence" => "Unrestricted".into(),
            "value" => format!("Exact: {OTHER_COMMAND}"),
            "any" => "Any value; attribute must be present".into(),
            _ => "Exact: git status".into(),
        };
        assert!(
            fields
                .iter()
                .any(|field| field.label == "2 · Preparation" && field.value == expected)
        );
        if change == "protected" {
            assert!(fields.iter().any(|field| field.label == "2 · Protection" && field.value == "Protected only"));
        }
        let screen = render(&mut prompt, 80, 64);
        assert!(screen.contains("Alternatives"));
        assert_eq!(screen.matches("/project").count(), 1);
    }

    #[test_case("invalid"; "malformed_metadata")]
    #[test_case("missing"; "unavailable_preimage")]
    #[test_case("truncated"; "truncated_metadata")]
    fn unavailable_metadata_never_exposes_an_approval_hit(failure: &str) {
        let mut prompt = protected_prompt(COMMAND);
        let request = &mut prompt.requests.front_mut().unwrap().request;
        let value = match failure {
            "invalid" => "not prepared directory metadata".into(),
            "missing" => json!({"kind": "known", "symbolic_paths": ["/another/root"]}).to_string(),
            _ => "/long/".repeat(super::super::details::MAX_REVIEW_CHARS),
        };
        request.resources[0]
            .attributes
            .insert("possible_workdirs".into(), value.clone());
        if failure == "invalid" {
            let option = request
                .options
                .iter_mut()
                .find(|option| option.id == "allow_exact")
                .unwrap();
            option.rule.resources[0].attributes.insert(
                "possible_workdirs".into(),
                PermissionResourceSelector::Exact { value },
            );
        }
        render(&mut prompt, 80, 18);
        prompt.handle_key(key(KeyCode::Char('s')));
        assert!(!prompt.confirmation.as_ref().unwrap().complete);
        for code in [KeyCode::Home, KeyCode::End] {
            prompt.handle_key(key(code));
            render(&mut prompt, 40, 10);
            assert!(prompt.row_hits.iter().all(|hit| !matches!(&hit.target, PromptTarget::Hint(event) if event.code == KeyCode::Enter)));
            assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn composed_commands_keep_their_own_directories(width: u16) {
        const FIRST_COMMAND: &str = "git status";
        const SECOND_COMMAND: &str = "cargo test";
        const FIRST_DIRECTORY: &str = "/work/first";
        const SECOND_DIRECTORY: &str = "/work/second";
        let mut request = PermissionRequest::from_legacy(
            "compound".into(),
            ToolKey::native("bash"),
            vec![FIRST_COMMAND.into(), SECOND_COMMAND.into()],
            json!({"command": "git status && cargo test"}),
            Path::new("/project"),
            false,
        );
        for (index, directory) in [FIRST_DIRECTORY, SECOND_DIRECTORY].into_iter().enumerate() {
            request.resources[index]
                .attributes
                .insert("workdir".into(), directory.into());
            request.resources[index].protected = true;
            for option in &mut request.options {
                if option.group.as_ref().and_then(|group| group.resource) == Some(index) {
                    option.rule.resources[0].attributes.insert(
                        "workdir".into(),
                        PermissionResourceSelector::Exact {
                            value: directory.into(),
                        },
                    );
                }
            }
        }
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        render(&mut prompt, width, 64);
        prompt.handle_key(key(KeyCode::Char('s')));
        let confirmation = prompt.confirmation.as_ref().unwrap();
        assert!(confirmation.complete);
        for (index, directory) in [FIRST_DIRECTORY, SECOND_DIRECTORY].into_iter().enumerate() {
            let authority = &confirmation.review.authorities[index];
            assert_eq!(authority.row, Some(index));
            assert!(authority.fields.iter().any(
                |field| field.label == "Starting directory" && field.value.ends_with(directory)
            ));
            assert!(
                !authority
                    .fields
                    .iter()
                    .any(|field| field.value.ends_with(if index == 0 {
                        SECOND_DIRECTORY
                    } else {
                        FIRST_DIRECTORY
                    }))
            );
        }
        let screen = buffer_text(&assert_scrollable_body(
            &mut prompt,
            width,
            LIVE_SCOPE_HEIGHT,
            &theme::current(),
        ));
        for text in [
            FIRST_DIRECTORY,
            SECOND_DIRECTORY,
            "Command 1",
            "Command 2",
            "Whole call",
        ] {
            assert!(screen.contains(text), "{text}: {screen}");
        }
    }

    #[test_case(40; "narrow")]
    #[test_case(80; "normal")]
    #[test_case(140; "wide")]
    fn long_paths_wrap_without_losing_cells_or_moving_actions(width: u16) {
        const PATH_END: &str = "unique-final-file.rs";
        let path = format!("/project/{}/{PATH_END}", "long-directory-".repeat(12));
        let mut request = PermissionRequest::from_legacy(
            "path".into(),
            ToolKey::native("file_read"),
            vec![path.clone()],
            json!({"filePath": &path}),
            Path::new("/project"),
            false,
        );
        request.presentation.project = Some("/project".into());
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        for name in ["ayu_dark", "ayu_light"] {
            let t = theme::load_by_name(name).unwrap();
            let full = assert_scrollable_body(&mut prompt, width, 10, &t);
            let text = buffer_text(&full)
                .chars()
                .filter(|character| !character.is_whitespace() && *character != '│')
                .collect::<String>();
            assert!(text.contains(&path));
        }
    }

    #[test_case(40, 10; "narrow")]
    #[test_case(80, 18; "normal")]
    #[test_case(140, 24; "wide")]
    fn footer_gaps_are_real_unstyled_nonclickable_cells(width: u16, height: u16) {
        let mut prompt = open_prompt();
        let theme = theme::load_by_name("ayu_dark").unwrap();
        let buffer = themed_buffer(&mut prompt, width, height, &theme);
        let hints = prompt
            .row_hits
            .iter()
            .filter(|hit| matches!(hit.target, PromptTarget::Hint(_)))
            .collect::<Vec<_>>();
        for (index, hit) in hints.iter().enumerate() {
            assert_eq!(buffer[(hit.area.x, hit.area.y)].symbol(), "[");
            assert_eq!(buffer[(hit.area.right() - 1, hit.area.y)].symbol(), "]");
            for other in &hints[index + 1..] {
                assert!(hit.area.intersection(other.area).is_empty());
            }
            if let Some(next) = hints
                .get(index + 1)
                .filter(|next| next.area.y == hit.area.y)
            {
                assert_eq!(next.area.x - hit.area.right(), 2);
                for x in hit.area.right()..next.area.x {
                    assert_eq!(buffer[(x, hit.area.y)].symbol(), " ");
                    assert_eq!(
                        buffer[(x, hit.area.y)].style(),
                        Style::default()
                            .fg(Color::Reset)
                            .bg(Color::Reset)
                            .underline_color(Color::Reset)
                    );
                    assert!(prompt.target_at(Position::new(x, hit.area.y)).is_none());
                }
            }
        }
    }

    #[test]
    fn hover_changes_only_the_hovered_button_cells() {
        let mut prompt = open_prompt();
        let theme = theme::load_by_name("ayu_light").unwrap();
        let before = themed_buffer(&mut prompt, 80, 18, &theme);
        let target = PromptTarget::Hint(key(KeyCode::Char('y')));
        let area = prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == target)
            .unwrap()
            .area;
        prompt.hover = Some(target);
        let after = themed_buffer(&mut prompt, 80, 18, &theme);
        let mut expected = before;
        expected.set_style(area, Style::new().add_modifier(Modifier::REVERSED));
        assert_eq!(after, expected);
    }

    #[test]
    fn guidance_cursor_tracks_unicode_edits_without_splitting_utf8() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('g')));
        prompt.handle_paste("界 test");
        prompt.handle_key(key(KeyCode::Home));
        prompt.handle_key(key(KeyCode::Right));
        let line = prompt.guidance_line(&theme::current());
        assert_eq!(line.to_string(), "Guidance: 界 test");
        let cursor = line
            .spans
            .iter()
            .find(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .unwrap();
        assert_eq!(cursor.content, " ");
        assert_eq!(line.spans[1].content, "界");
    }

    #[test]
    fn technical_authority_is_available_only_in_details() {
        let mut prompt = open_prompt();
        let digest = prompt.current().unwrap().input_digest.clone();
        let main = render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!main.contains(&digest));
        assert!(!main.contains("allow_exact"));
        prompt.select_authority("allow_any_command".into());
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        prompt.handle_key(key(KeyCode::Char('s')));
        let confirmation = render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!confirmation.contains(&digest));
        let frozen = prompt.confirmation.as_ref().unwrap().answer.clone();
        prompt.handle_key(key(KeyCode::F(2)));
        assert!(prompt.panel == Panel::Details);
        let details = super::details_body(prompt.current().unwrap())
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains(&digest));
        assert!(details.contains("allow_any_command"));
        assert!(details.contains("subject"));
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
        prompt.handle_key(key(KeyCode::Esc));
        assert_eq!(prompt.confirmation.as_ref().unwrap().answer, frozen);
    }

    #[test]
    fn covered_toggle_only_exists_for_covered_rows_and_never_changes_the_call() {
        let mut prompt = open_prompt();
        render(&mut prompt, 80, 18);
        assert!(
            !prompt
                .row_hits
                .iter()
                .any(|hit| hit.target == PromptTarget::Hint(key(KeyCode::Char('c'))))
        );
        prompt.handle_key(key(KeyCode::Char('c')));
        assert!(!prompt.expanded_covered);
        let mut request = PermissionRequest::from_legacy(
            "compound".into(),
            ToolKey::native("bash"),
            vec!["git status".into(), "cargo test".into()],
            json!({"command": "git status && cargo test"}),
            Path::new("/project"),
            false,
        );
        request.presentation.resources[0].coverage = Some(ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: COVERING.into(),
        });
        request.presentation.project = Some("/project".into());
        let input = request.input.clone();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        let grants = prompt.row_grants(prompt.current().unwrap());
        assert!(grants[0].is_none());
        let collapsed = render(&mut prompt, ROOMY_WIDTH, FULL_REVIEW_HEIGHT);
        assert!(collapsed.contains("Needs approval: 1 of 2 commands"));
        assert!(collapsed.contains(super::WHOLE_CALL));
        assert!(!collapsed.contains(COVERING));
        prompt.handle_key(key(KeyCode::Char('c')));
        let expanded = render(&mut prompt, ROOMY_WIDTH, FULL_REVIEW_HEIGHT);
        assert!(expanded.contains(COVERING));
        assert_eq!(prompt.row_grants(prompt.current().unwrap()), grants);
        assert_eq!(prompt.current().unwrap().input, input);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('s'))).unwrap().answer,
            PermissionAnswer::AllowComposed {
                rows: grants,
                lifetime: PermissionLifetime::Conversation
            }
        );
    }

    #[test]
    fn scope_chooser_never_stacks_all_authorities() {
        let mut prompt = open_prompt();
        prompt.open_scope_editor();
        let layout = prompt.review_layout(77, &theme::current());
        assert_eq!(
            layout
                .cards
                .iter()
                .filter(|(_, card)| card.title == "Future scope")
                .count(),
            1
        );
        assert!(layout.cards.len() <= 5);
    }

    #[test_case(20, 5; "tiny")]
    #[test_case(40, 7; "short")]
    fn unusable_layouts_have_no_approval_targets(width: u16, height: u16) {
        let mut prompt = open_prompt();
        render(&mut prompt, width, height);
        assert!(prompt.row_hits.is_empty());
        for shortcut in ['y', 's', 'a'] {
            assert!(prompt.handle_key(key(KeyCode::Char(shortcut))).is_none());
        }
        assert_eq!(
            prompt.handle_key(key(KeyCode::Esc)).unwrap().answer,
            PermissionAnswer::Deny
        );
    }
}
