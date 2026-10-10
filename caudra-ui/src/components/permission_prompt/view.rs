use std::iter::repeat_n;

use caudra_agent::permissions::review::command_template_values;
use caudra_agent::permissions::{
    PermissionCaution, PermissionLifetime, PermissionRequest, PermissionResourceAccess,
    PermissionResourceKind, PermissionRowGrant, PromptReason, grade_command_pattern,
};
use caudra_agent::tools::native::plan;
use caudra_config::ToolKey;
use caudra_grab::grab_scope;
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField};
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::choices::{Choice, ONCE_ONLY, grant_label, grant_names_commands, grant_template};
use super::customize::{Effect, Field, ScopeItem, scope_item_label, scope_item_names_commands};
use super::decision::{covered, grant_option, main_ladder, row_positions};
use super::details::{review_lines, review_text};
use super::notes::{
    Note, RowStatus, Tone, action_lines, coverage_phrase, is_shell, row_coverage, row_status,
};
use super::scope::{option_model, pattern_model};
use super::step_through::{
    PageItem, REVIEW_CHOICES, ReviewChoice, StepThrough, item_label, lifetime_phrase,
    rung_lifetimes,
};
use super::{Panel, PermissionPrompt, PromptHit, PromptState, PromptTarget};
use crate::components::command_text::{
    code_spans_in, command_lines, command_spans, ellipsize_spans, marked_spans, overlay, pad_spans,
    scope_spans,
};
use crate::components::permission_scope::model::ScopeModel;
use crate::components::permission_scope::pattern::PatternPanel;
use crate::components::permission_scope::view::ScopeView;
use crate::components::tab_bar::{Tab, tab_spans};
use crate::components::{
    Hint, field_styles, hanging_lines, hanging_spans, hint_hits, hint_line_hovered, hover_style,
};
use crate::theme::{self, Theme};

const MAX_CONTENT_WIDTH: u16 = 112;
const MIN_WIDTH: u16 = 32;
const MIN_HEIGHT: u16 = 8;
const BORDER_ROWS: u16 = 2;
/// The blank row above the footer, and the footer.
const FOOTER_ROWS: u16 = 2;
const MARGIN: u16 = 2;
const SCROLLBAR_COLUMNS: u16 = 1;
const MIN_ACTION_ROWS: usize = 3;
const MAX_NOTES: usize = 3;
const MAX_ALLOWED_ROWS: usize = 3;
const MAX_STATUS_ROWS: usize = 3;
const STATUS_WIDTH: usize = 9;
const MIN_COLUMN: usize = 6;
const FIELD_LABEL_WIDTH: u16 = 11;
const NUMBER_INDENT: usize = 5;
const POINTER: &str = "❯ ";
const NO_POINTER: &str = "  ";
const ROW_FOCUS: &str = "▸ ";
const FIELD_PROMPT: &str = "› ";
const HANG: &str = "  ";
const WARNING_MARK: &str = "⚠ ";
const ELLIPSIS: char = '…';
const COLUMN_GAP: &str = "  ";
const BADGE_SEPARATOR: &str = " · ";
const TITLE_SEPARATOR: &str = " · ";
const RESIZE_MESSAGE: &str = "Make the terminal larger to answer. Esc says no.";
pub(super) const REARM_MESSAGE: &str = "Press Tab, then press the key again.";
pub(super) const GUIDANCE_PLACEHOLDER: &str = "what to do instead (optional)";
const PATTERN_PLACEHOLDER: &str = "a pattern ending in ` *` or `/*`, such as cargo test *";
pub(super) const PRESS_AGAIN: &str = "Press Enter again to allow, or Esc to go back.";
const DETAILS_TITLE: &str = "Details";
const CUSTOMIZE_TITLE: &str = "Customize";
const PLAN_READ_QUESTION: &str = "Allow reading the plan?";
const PLAN_WRITE_QUESTION: &str = "Allow changing the plan?";
const OVERFLOW_HINT: &str = "? shows all";
const REMEMBER_FOR: &str = "Remember for ";
const EFFECT_LABEL: &str = "Effect";
const REMEMBER_LABEL: &str = "Remember";
pub(super) const BROAD_WHILE_PLANNING: &str =
    "While planning, broad scopes last for this conversation.";
const SCOPE_LABEL: &str = "Scope";
const ASKS_EVERY_TIME: &str = "asks every time";
const ALREADY_ALLOWED: &str = "already allowed";
const PATTERN_MATCHES: &str = "Matches this command.";
const PATTERN_BROAD: &str = "⚠ Any use of this program. You'll confirm it before it's saved.";
const PATTERN_ASKING: &str = "⚠ Overlaps commands Caudra always asks about.";

/// A key and what it does, for the footer. A key that is only a direction,
/// like `←→`, cannot be clicked.
pub(super) struct KeyHint {
    label: &'static str,
    description: &'static str,
    code: Option<KeyCode>,
}

impl KeyHint {
    pub(super) fn key(label: &'static str, description: &'static str, code: KeyCode) -> Self {
        Self {
            label,
            description,
            code: Some(code),
        }
    }

    pub(super) fn char(label: &'static str, description: &'static str) -> Self {
        let code = label.chars().next().map(KeyCode::Char);
        Self {
            label,
            description,
            code,
        }
    }

    pub(super) fn inert(label: &'static str, description: &'static str) -> Self {
        Self {
            label,
            description,
            code: None,
        }
    }

    fn hint(&self, compact: bool) -> Hint {
        let description = if compact { "" } else { self.description };
        match self.code {
            Some(code) => Hint::key(self.label, code, description),
            None => Hint::inert(self.label, description),
        }
    }
}

/// A widget drawn into the body at its own place: the rule a scope stores,
/// or the template being edited.
enum Insert {
    Scope(Box<ScopeModel>),
    Pattern(PatternPanel),
}

/// Every row the body draws and what each part does when pressed. One
/// function builds it for drawing and for measuring, so the two agree.
struct Body {
    width: u16,
    lines: Vec<Line<'static>>,
    hits: Vec<PromptHit>,
    inserts: Vec<(Rect, Insert)>,
    /// The rows to keep in sight: the highlighted choice, or a field.
    reveal: Option<(u16, u16)>,
    /// How many rows the request's text takes before it is capped.
    action_rows: usize,
}

impl Body {
    fn new(width: u16) -> Self {
        Self {
            width,
            lines: Vec::new(),
            hits: Vec::new(),
            inserts: Vec::new(),
            reveal: None,
            action_rows: 0,
        }
    }

    fn row(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }

    fn push(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.lines.extend(lines);
    }

    /// Lines that together are one control.
    fn control(&mut self, target: PromptTarget, lines: Vec<Line<'static>>, revealed: bool) {
        let top = self.row();
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.lines.extend(lines);
        self.hits.push(PromptHit {
            area: Rect::new(0, top, self.width, height),
            target,
        });
        if revealed {
            self.reveal = Some((top, height));
        }
    }

    /// Controls side by side, continuing on rows `indent` columns in when
    /// they do not fit.
    fn spans(&mut self, parts: Vec<(Span<'static>, Option<PromptTarget>)>, indent: u16) {
        let mut line = Vec::new();
        let mut x: u16 = 0;
        for (span, target) in parts {
            let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
            if x > indent && x.saturating_add(width) > self.width {
                self.lines.push(Line::from(std::mem::take(&mut line)));
                line.push(Span::raw(" ".repeat(usize::from(indent))));
                x = indent;
            }
            if let Some(target) = target {
                self.hits.push(PromptHit {
                    area: Rect::new(x, self.row(), width.min(self.width.saturating_sub(x)), 1),
                    target,
                });
            }
            x = x.saturating_add(width);
            line.push(span);
        }
        self.lines.push(Line::from(line));
    }

    fn insert(&mut self, height: u16, insert: Insert) {
        let top = self.row();
        self.lines.extend((0..height).map(|_| Line::default()));
        self.inserts
            .push((Rect::new(0, top, self.width, height), insert));
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

fn body_width(inner: u16) -> u16 {
    inner.saturating_sub(MARGIN + SCROLLBAR_COLUMNS).max(1)
}

/// `text` wrapped to `width`, its later rows hanging two columns in.
fn hang(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let mut lines = hanging_lines(
        Span::styled(HANG, style),
        Span::styled(text.to_owned(), style),
        width,
    );
    if let Some(first) = lines.first_mut() {
        first.spans.remove(0);
    }
    lines
}

/// [`hang`] for a line already in colour.
fn hang_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let mut lines = hanging_spans(Span::raw(HANG), spans, width);
    if let Some(first) = lines.first_mut() {
        first.spans.remove(0);
    }
    lines
}

/// `text` wrapped under a fixed indent.
fn indented(indent: usize, text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    hanging_lines(
        Span::styled(" ".repeat(indent), style),
        Span::styled(text.to_owned(), style),
        width,
    )
}

fn note_lines(note: &Note, width: u16, t: &Theme) -> Vec<Line<'static>> {
    match note.tone {
        Tone::Warning => hang(
            &format!("{WARNING_MARK}{}", note.text),
            t.tool_warning,
            width,
        ),
        Tone::Danger => hang(&format!("{WARNING_MARK}{}", note.text), t.error, width),
        Tone::Muted => hang(&note.text, t.tool_dim, width),
    }
}

/// `text` cut to `width` columns, ending in `…` when anything was cut.
fn ellipsize(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut shown = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or_default();
        if used + columns + 1 > width {
            break;
        }
        shown.push(character);
        used += columns;
    }
    shown.push(ELLIPSIS);
    shown
}

fn pad(mut text: String, width: usize) -> String {
    let columns = text.width();
    text.extend(repeat_n(' ', width.saturating_sub(columns)));
    text
}

/// `style` for a row already allowed: dimmed, so its colours do not outshine
/// the rows that need an answer.
fn settled(style: Style, status: RowStatus) -> Style {
    match status {
        RowStatus::Allowed => style.add_modifier(Modifier::DIM),
        RowStatus::New | RowStatus::Asks => style,
    }
}

/// A row's command in one column: coloured, cut, and padded to `width`, under
/// `style`'s modifiers.
fn command_cell(command: &str, width: usize, style: Style) -> Vec<Span<'static>> {
    overlay(
        pad_spans(ellipsize_spans(command_spans(command), width), width),
        style,
    )
}

/// The request's own text, a command coloured as shell.
fn request_lines(request: &PermissionRequest) -> Vec<Vec<Span<'static>>> {
    let lines = action_lines(request);
    if is_shell(request) {
        return command_lines(&lines);
    }
    lines
        .into_iter()
        .map(|line| vec![Span::raw(line)])
        .collect()
}

/// One row's command, every line of it coloured as shell.
fn row_lines(request: &PermissionRequest, row: usize) -> Vec<Vec<Span<'static>>> {
    command_lines(
        &request
            .resources
            .get(row)
            .map(|resource| review_lines(&resource.value))
            .unwrap_or_default(),
    )
}

/// A numbered answer: `❯ 1. Yes`, its later rows hanging under the text,
/// with any badges at the right edge when they fit there.
fn numbered(
    index: usize,
    style: Style,
    highlighted: bool,
    mut label: Vec<Span<'static>>,
    badges: &[String],
    width: u16,
    t: &Theme,
) -> Vec<Line<'static>> {
    let pointer = if highlighted { POINTER } else { NO_POINTER };
    let prefix = format!("{pointer}{}. ", index + 1);
    let badge = badges.join(BADGE_SEPARATOR);
    let room = usize::from(width).saturating_sub(prefix.width());
    let label_width: usize = label.iter().map(Span::width).sum();
    if !badge.is_empty() && label_width + COLUMN_GAP.width() + badge.width() <= room {
        let gap = room - label_width - badge.width();
        let mut spans = vec![Span::styled(prefix, style)];
        spans.extend(label);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(badge, t.tool_dim));
        return vec![Line::from(spans)];
    }
    if !badge.is_empty() {
        label.push(Span::styled(format!("{COLUMN_GAP}{badge}"), style));
    }
    hanging_spans(Span::styled(prefix, style), label, width)
}

fn choice_style(highlighted: bool, hovered: bool, t: &Theme) -> Style {
    hover_style(
        if highlighted {
            t.active
        } else {
            Style::default()
        },
        hovered,
    )
}

/// The executable and its subcommand, which name a command's page.
fn tab_label(command: &str) -> String {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return String::new();
    };
    match words.next() {
        Some(word)
            if word.starts_with(|character: char| character.is_ascii_lowercase())
                && word
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '-') =>
        {
            format!("{program} {word}")
        }
        _ => program.to_owned(),
    }
}

fn row_command(request: &PermissionRequest, row: usize) -> String {
    request
        .resources
        .get(row)
        .map(|resource| review_text(&resource.value).replace('\n', " "))
        .unwrap_or_default()
}

fn remember_name(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => "Once",
        PermissionLifetime::Conversation => "This conversation",
        PermissionLifetime::Project => "This project",
        PermissionLifetime::Global => "All projects",
    }
}

/// What a typed pattern would do, said under the field.
fn pattern_feedback(pattern: &str, command: &str, t: &Theme) -> (String, Style) {
    if pattern.trim().is_empty() {
        return (PATTERN_PLACEHOLDER.into(), t.tool_dim);
    }
    match grade_command_pattern(pattern.trim(), command) {
        Err(fault) => {
            let text = fault.to_string();
            let mut characters = text.chars();
            let text = characters
                .next()
                .map(|first| first.to_uppercase().chain(characters).collect())
                .unwrap_or(text);
            (format!("{text}."), t.error)
        }
        Ok(grade) => match grade.caution {
            Some(PermissionCaution::Danger) => (PATTERN_BROAD.into(), t.error),
            Some(PermissionCaution::Warn) => (PATTERN_ASKING.into(), t.tool_warning),
            None => (PATTERN_MATCHES.into(), t.tool_dim),
        },
    }
}

impl PermissionPrompt {
    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("permission_prompt", area);
        self.view_with_theme(frame, area, &theme::current());
    }

    pub(super) fn view_with_theme(&mut self, frame: &mut Frame, area: Rect, t: &Theme) {
        let area = content_area(area);
        if self.area != area {
            let focus = self.focus.clone();
            self.invalidate_controls();
            self.focus = focus;
        }
        self.area = area;
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
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
            self.hits.clear();
            return;
        }
        let pressed_area = self.mouse_down.as_ref().and_then(|target| {
            self.hits
                .iter()
                .find(|hit| hit.target == *target)
                .map(|hit| hit.area)
        });
        let block = self.block(t);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let pinned = self.pinned_status(body_width(inner.width), t);
        let pinned_rows = u16::try_from(pinned.len()).unwrap_or(u16::MAX);
        let body_area = Rect {
            x: inner.x + MARGIN,
            width: body_width(inner.width),
            height: inner.height.saturating_sub(FOOTER_ROWS + pinned_rows),
            ..inner
        };
        let footer_area = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(pinned),
            Rect {
                y: footer_area.y.saturating_sub(pinned_rows),
                height: pinned_rows,
                ..body_area
            },
        );
        let Body {
            lines,
            hits: body_hits,
            inserts,
            reveal,
            ..
        } = self.fitted_body(body_area.width, body_area.height, t);
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.scroll.update_dimensions(total, body_area.height);
        let mut buffer = Buffer::empty(Rect::new(0, 0, body_area.width, total));
        for (y, line) in lines.iter().enumerate() {
            buffer.set_line(0, y as u16, line, body_area.width);
        }
        let mut hits = body_hits;
        for (rect, insert) in inserts {
            match insert {
                Insert::Scope(model) => {
                    self.scope_view.render(&model, rect, &mut buffer, t);
                    hits.extend(self.scope_view.hits.iter().map(|hit| PromptHit {
                        area: hit.area,
                        target: PromptTarget::VisualScope(hit.control.clone()),
                    }));
                }
                Insert::Pattern(panel) => {
                    let focused = match self.hover.as_ref().or(self.focus.as_ref()) {
                        Some(PromptTarget::Inspector(control)) => Some(control.clone()),
                        _ => None,
                    };
                    hits.extend(
                        panel
                            .render(rect, &mut buffer, t, focused.as_ref())
                            .into_iter()
                            .map(|(area, control)| PromptHit {
                                area,
                                target: PromptTarget::Inspector(control),
                            }),
                    );
                }
            }
        }
        if std::mem::take(&mut self.reveal) {
            let focused = self
                .focus
                .as_ref()
                .and_then(|focus| hits.iter().find(|hit| hit.target == *focus))
                .map(|hit| (hit.area.y, hit.area.height));
            if let Some((top, height)) = focused.or(reveal) {
                self.scroll.reveal(top, height);
            }
        }
        let offset = self.scroll.offset();
        for y in 0..body_area.height.min(total.saturating_sub(offset)) {
            for x in 0..body_area.width {
                frame.buffer_mut()[(body_area.x + x, body_area.y + y)] =
                    buffer[(x, offset + y)].clone();
            }
        }
        self.scrollbar.draw(
            frame,
            Rect {
                width: body_area.width + SCROLLBAR_COLUMNS,
                ..body_area
            },
            total,
            offset,
        );
        self.hits.clear();
        let viewport = Rect::new(0, offset, body_area.width, body_area.height);
        for hit in hits {
            let clipped = hit.area.intersection(viewport);
            if !clipped.is_empty() {
                self.hits.push(PromptHit {
                    area: Rect {
                        x: body_area.x + clipped.x,
                        y: body_area.y + clipped.y - offset,
                        ..clipped
                    },
                    target: hit.target,
                });
            }
        }
        self.draw_footer(frame, footer_area, t);
        self.awaiting_review = false;
        if let Some(pressed_area) = pressed_area
            && !self.hits.iter().any(|hit| {
                Some(&hit.target) == self.mouse_down.as_ref() && hit.area == pressed_area
            })
        {
            self.mouse_down = None;
        }
        if self
            .focus
            .as_ref()
            .is_some_and(|target| !self.hits.iter().any(|hit| hit.target == *target))
        {
            self.focus = None;
        }
    }

    pub fn height(&self, width: u16) -> u16 {
        if self.current().is_none() {
            return 0;
        }
        let inner = width.min(MAX_CONTENT_WIDTH).saturating_sub(BORDER_ROWS);
        let t = theme::current();
        let pinned =
            u16::try_from(self.pinned_status(body_width(inner), &t).len()).unwrap_or(u16::MAX);
        self.body(body_width(inner), usize::MAX, &t)
            .row()
            .saturating_add(BORDER_ROWS + FOOTER_ROWS + pinned)
            .max(MIN_HEIGHT)
    }

    /// Whether the template being edited fits this command, kept above the
    /// footer so it stays in sight however far the panel scrolls.
    fn pinned_status(&self, width: u16, t: &Theme) -> Vec<Line<'static>> {
        if self.panel == Panel::Details {
            return Vec::new();
        }
        self.inspector_status(t)
            .map(|(status, style)| {
                hang(&status, style, width)
                    .into_iter()
                    .take(MAX_STATUS_ROWS)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn block(&self, t: &Theme) -> Block<'static> {
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_top(Line::styled(format!(" {} ", self.title()), t.panel_title));
        let place = self.place();
        if !place.is_empty() {
            block = block.title_top(Line::styled(format!(" {place} "), t.tool_dim).right_aligned());
        }
        block
    }

    /// The question the prompt asks, chosen by what the tool touches.
    pub(super) fn question(&self) -> String {
        let Some(request) = self.current() else {
            return String::new();
        };
        if self.shell() {
            return if self.batch() {
                "Allow shell commands?"
            } else {
                "Allow shell command?"
            }
            .into();
        }
        if let ToolKey::McpTool { server, tool } = &request.tool {
            return format!(
                "Allow MCP tool {}?",
                review_text(&format!("{server}/{tool}"))
            );
        }
        let Some(resource) = request.resources.first() else {
            return format!("Allow {}?", review_text(&request.tool.to_string()));
        };
        if plan::is_plan_subject(&request.subject) {
            return match resource.access {
                Some(PermissionResourceAccess::Write) => PLAN_WRITE_QUESTION,
                _ => PLAN_READ_QUESTION,
            }
            .into();
        }
        match (&resource.kind, &resource.access) {
            (PermissionResourceKind::Url, _) => "Allow fetching a web page?".into(),
            (PermissionResourceKind::Query, _) => "Allow a web search?".into(),
            (_, Some(PermissionResourceAccess::Write)) => "Allow editing a file?".into(),
            (_, Some(PermissionResourceAccess::List)) => "Allow listing a folder?".into(),
            (_, Some(PermissionResourceAccess::Search)) => "Allow searching files?".into(),
            (
                PermissionResourceKind::File
                | PermissionResourceKind::Directory
                | PermissionResourceKind::RemoteFile { .. }
                | PermissionResourceKind::RemoteDirectory { .. },
                _,
            ) => "Allow reading a file?".into(),
            _ => format!("Allow {}?", review_text(&request.tool.to_string())),
        }
    }

    fn title(&self) -> String {
        match self.panel {
            Panel::Details => DETAILS_TITLE.into(),
            Panel::Customize => format!("{}{TITLE_SEPARATOR}{CUSTOMIZE_TITLE}", self.question()),
            _ => self.question(),
        }
    }

    /// Who asks, where this prompt stands in the queue, and how many of a
    /// batch's commands need an answer.
    fn place(&self) -> String {
        let mut parts = Vec::new();
        if let Some(requester) = self.requester_name() {
            parts.push(review_text(requester));
        }
        if self.pending_count() > 1 {
            parts.push(format!("1 of {}", self.pending_count()));
        }
        if let Some(request) = self.current()
            && self.batch()
        {
            let listed = self.listed_rows();
            let new = listed.iter().filter(|row| !covered(request, **row)).count();
            parts.push(format!("{new} of {} new", listed.len()));
        }
        parts.join(TITLE_SEPARATOR)
    }

    /// The body, its request text capped so the choices fit `viewport` rows
    /// when they can.
    fn fitted_body(&self, width: u16, viewport: u16, t: &Theme) -> Body {
        let body = self.body(width, usize::MAX, t);
        let overflow = usize::from(body.row()).saturating_sub(usize::from(viewport));
        if overflow == 0 {
            return body;
        }
        let cap = body
            .action_rows
            .saturating_sub(overflow)
            .max(MIN_ACTION_ROWS);
        if cap >= body.action_rows {
            return body;
        }
        self.body(width, cap, t)
    }

    fn body(&self, width: u16, cap: usize, t: &Theme) -> Body {
        let mut body = Body::new(width);
        if self.current().is_none() {
            return body;
        }
        if self.panel == Panel::Details {
            self.details_body(&mut body, t);
        } else if self.inspector.is_some() {
            self.inspector_body(&mut body, cap, t);
        } else {
            match self.panel {
                Panel::StepThrough => self.step_body(&mut body, cap, t),
                Panel::Customize => self.customize_body(&mut body, cap, t),
                Panel::Main | Panel::Details => self.main_body(&mut body, cap, t),
            }
        }
        body
    }

    /// The request's own text, capped at `cap` rows with the rest counted.
    fn action_block(&self, body: &mut Body, lines: Vec<Vec<Span<'static>>>, cap: usize, t: &Theme) {
        let rows: Vec<Line<'static>> = lines
            .into_iter()
            .flat_map(|spans| hang_spans(spans, body.width))
            .collect();
        body.action_rows = rows.len();
        if rows.len() <= cap {
            body.push(rows);
            return;
        }
        let shown = cap.saturating_sub(1).max(1);
        let hidden = rows.len() - shown;
        body.push(rows.into_iter().take(shown));
        let lines = if hidden == 1 { "line" } else { "lines" };
        body.push([Line::styled(
            format!("{ELLIPSIS} {hidden} more {lines}{TITLE_SEPARATOR}{OVERFLOW_HINT}"),
            t.tool_dim,
        )]);
    }

    fn context_lines(&self, body: &mut Body, t: &Theme) {
        for note in self.context() {
            body.push(note_lines(&note, body.width, t));
        }
    }

    /// Every caution, then the reasons while there is room for them.
    fn notes_block(&self, body: &mut Body, t: &Theme) {
        let notes = self.notes();
        if notes.is_empty() {
            return;
        }
        body.blank();
        let mut shown = 0;
        for note in &notes {
            if note.tone == Tone::Muted && shown >= MAX_NOTES {
                continue;
            }
            body.push(note_lines(note, body.width, t));
            shown += 1;
        }
    }

    fn main_body(&self, body: &mut Body, cap: usize, t: &Theme) {
        let Some(request) = self.current() else {
            return;
        };
        body.blank();
        self.action_block(body, request_lines(request), cap, t);
        self.context_lines(body, t);
        if self.batch() {
            body.blank();
            self.command_rows(body, t);
        }
        self.notes_block(body, t);
        body.blank();
        let names_commands = self.scope_names_commands();
        for (index, choice) in self.choices().into_iter().enumerate() {
            if choice == Choice::Deny
                && let Some(learned) = self.learned_line()
            {
                body.push(indented(NUMBER_INDENT, &learned, t.tool_dim, body.width));
            }
            let highlighted = choice == self.highlight;
            let target = PromptTarget::Choice(choice);
            let style = choice_style(highlighted, self.hover.as_ref() == Some(&target), t);
            let label = marked_spans(
                &self.choice_sentence(choice),
                style,
                t.accent,
                names_commands,
            );
            let lines = numbered(index, style, highlighted, label, &[], body.width, t);
            body.control(target, lines, highlighted);
            if choice == Choice::Deny && self.state == PromptState::Guidance {
                self.field_line(body, GUIDANCE_PLACEHOLDER, NUMBER_INDENT, t);
            }
        }
        self.pending_lines(body, t);
    }

    /// One row per command: whether it needs an answer, the command, and the
    /// scope it gets or what already settles it.
    fn command_rows(&self, body: &mut Body, t: &Theme) {
        let Some(request) = self.current() else {
            return;
        };
        let rows = self.listed_rows();
        let allowed = rows
            .iter()
            .filter(|row| row_status(request, **row) == RowStatus::Allowed)
            .count();
        let collapse = allowed > MAX_ALLOWED_ROWS;
        let rest = usize::from(body.width)
            .saturating_sub(ROW_FOCUS.width() + STATUS_WIDTH + COLUMN_GAP.width());
        let longest = rows
            .iter()
            .map(|row| row_command(request, *row).width())
            .max()
            .unwrap_or_default();
        let command_width = longest.min(rest * 11 / 20).max(MIN_COLUMN);
        let scope_width = rest.saturating_sub(command_width).max(MIN_COLUMN);
        for row in rows {
            let status = row_status(request, row);
            if collapse && status == RowStatus::Allowed {
                continue;
            }
            let new = self.is_new_row(row);
            let focused = new && self.focus_row == Some(row);
            let target = PromptTarget::Row(row);
            let hovered = self.hover.as_ref() == Some(&target);
            let (scope, names_commands) = match status {
                RowStatus::New if new => match self.row_grant(row) {
                    Some(grant) => (
                        format!("‹{}›", grant_label(request, row, &grant)),
                        grant_names_commands(request, row, &grant),
                    ),
                    None => (format!("‹{ONCE_ONLY}›"), false),
                },
                RowStatus::New => (String::new(), false),
                RowStatus::Asks | RowStatus::Allowed => (
                    row_coverage(request, row)
                        .map(coverage_phrase)
                        .unwrap_or_default(),
                    false,
                ),
            };
            let (status_style, text_style) = match status {
                RowStatus::New => (t.accent, Style::default()),
                RowStatus::Asks => (t.tool_warning, Style::default()),
                RowStatus::Allowed => (t.tool_dim, t.tool_dim),
            };
            let command_style = settled(if focused { t.active } else { text_style }, status);
            let mut spans = vec![
                Span::styled(if focused { ROW_FOCUS } else { NO_POINTER }, t.accent),
                Span::styled(pad(status.word().into(), STATUS_WIDTH), status_style),
            ];
            spans.extend(command_cell(
                &row_command(request, row),
                command_width,
                hover_style(command_style, hovered),
            ));
            spans.push(Span::raw(COLUMN_GAP));
            spans.extend(ellipsize_spans(
                marked_spans(
                    &scope,
                    hover_style(text_style, hovered),
                    t.accent,
                    names_commands,
                ),
                scope_width,
            ));
            let line = Line::from(spans);
            if new {
                body.control(target, vec![line], focused);
            } else {
                body.push([line]);
            }
        }
        if collapse {
            body.push([Line::styled(
                format!("{NO_POINTER}+ {allowed} {ALREADY_ALLOWED}"),
                t.tool_dim,
            )]);
        }
    }

    /// The field being typed into, under the choice or item that opened it.
    fn field_line(&self, body: &mut Body, placeholder: &str, indent: usize, t: &Theme) {
        let prefix = format!("{}{FIELD_PROMPT}", " ".repeat(indent));
        let width = usize::from(body.width).saturating_sub(prefix.width());
        let styles = FieldStyles {
            caret: t.cursor,
            placeholder: t.input_placeholder,
            ..field_styles(Style::new().fg(t.foreground))
        };
        let text = self.field.text();
        let display = review_text(&text);
        let mut line = if display == text {
            self.field.paint(width, &styles, true, placeholder)
        } else {
            TextField::with_text(FieldKind::Line, &display).paint(width, &styles, true, placeholder)
        };
        line.spans.insert(0, Span::styled(prefix, t.tool_dim));
        let top = body.row();
        body.push([line]);
        body.reveal = Some((top, 1));
    }

    /// What a typed pattern would do, under its field.
    fn pattern_field(&self, body: &mut Body, row: usize, indent: usize, t: &Theme) {
        self.field_line(body, PATTERN_PLACEHOLDER, indent, t);
        let Some(command) = self
            .current()
            .and_then(|request| request.resources.get(row))
        else {
            return;
        };
        let (text, style) = pattern_feedback(&self.field.text(), &command.value, t);
        body.push(indented(
            indent + FIELD_PROMPT.width(),
            &text,
            style,
            body.width,
        ));
    }

    /// The red line a grant waits under, and how to give it.
    fn pending_lines(&self, body: &mut Body, t: &Theme) {
        let Some(pending) = &self.pending else {
            return;
        };
        body.blank();
        let top = body.row();
        body.push(hang(
            &format!("{WARNING_MARK}{}", pending.warning),
            t.error,
            body.width,
        ));
        match pending.phrases.first() {
            None => body.push(indented(HANG.width(), PRESS_AGAIN, t.tool_dim, body.width)),
            Some(phrase) => {
                body.push(indented(
                    HANG.width(),
                    &format!("Type ‹{phrase}› to allow, or press Esc to go back."),
                    t.tool_dim,
                    body.width,
                ));
                self.field_line(body, "", HANG.width(), t);
            }
        }
        body.reveal = Some((top, body.row() - top));
    }

    fn step_body(&self, body: &mut Body, cap: usize, t: &Theme) {
        let (Some(request), Some(step)) = (self.current(), &self.step) else {
            return;
        };
        body.blank();
        let pages = self.new_rows();
        let tabs: Vec<Tab<PromptTarget>> = pages
            .iter()
            .map(|row| Tab {
                label: tab_label(&row_command(request, *row)),
                done: step.visited.get(*row).copied().unwrap_or_default(),
                target: PromptTarget::Tab(*row),
            })
            .collect();
        let active = step
            .page
            .and_then(|row| pages.iter().position(|page| *page == row));
        body.spans(
            tab_spans(
                &tabs,
                active,
                PromptTarget::Review,
                self.hover.as_ref(),
                body.width,
            ),
            0,
        );
        body.blank();
        match step.page {
            Some(row) => self.page_body(body, request, step, row, cap, t),
            None => self.review_body(body, request, step, t),
        }
    }

    /// One command's page: the command, its ladder, a pattern of one's own,
    /// and how long to remember it.
    fn page_body(
        &self,
        body: &mut Body,
        request: &PermissionRequest,
        step: &StepThrough,
        row: usize,
        cap: usize,
        t: &Theme,
    ) {
        self.action_block(body, row_lines(request, row), cap, t);
        self.notes_block(body, t);
        body.blank();
        for (index, item) in step.items(request, row).iter().enumerate() {
            let (label, badges) = item_label(request, row, item);
            let highlighted = index == step.highlight;
            let target = PromptTarget::Item(index);
            let style = choice_style(highlighted, self.hover.as_ref() == Some(&target), t);
            let names_commands = matches!(
                item,
                PageItem::Grant(Some(grant)) if grant_names_commands(request, row, grant)
            );
            let label = scope_spans(&label, style, t.accent, names_commands);
            let lines = numbered(index, style, highlighted, label, &badges, body.width, t);
            body.control(target, lines, highlighted);
            if *item == PageItem::OwnPattern && self.state == PromptState::PatternEditing {
                self.pattern_field(body, row, NUMBER_INDENT, t);
            }
        }
        let Some(grant) = step.grant(request, row) else {
            return;
        };
        let lifetime = step
            .lifetimes
            .get(row)
            .cloned()
            .unwrap_or(PermissionLifetime::Conversation);
        let allowed = rung_lifetimes(request, row, &grant, self.project_available());
        let next = allowed
            .iter()
            .position(|found| *found == lifetime)
            .and_then(|index| allowed.get((index + 1) % allowed.len()))
            .filter(|next| **next != lifetime)
            .cloned();
        body.blank();
        body.spans(
            vec![
                (Span::styled(REMEMBER_FOR, t.tool_dim), None),
                (
                    Span::styled(format!("‹{}›", lifetime_phrase(&lifetime)), t.active),
                    next.map(PromptTarget::Remember),
                ),
            ],
            0,
        );
    }

    /// Every row, the scope it gets, and how long, then the answers.
    fn review_body(
        &self,
        body: &mut Body,
        request: &PermissionRequest,
        step: &StepThrough,
        t: &Theme,
    ) {
        let width = usize::from(body.width);
        let command_width = (width * 2 / 5).max(MIN_COLUMN);
        let scope_width = (width * 3 / 10).max(MIN_COLUMN);
        let rest = width
            .saturating_sub(command_width + scope_width + 2 * COLUMN_GAP.width())
            .max(MIN_COLUMN);
        for row in self.listed_rows() {
            let status = row_status(request, row);
            let coverage = || {
                row_coverage(request, row)
                    .map(coverage_phrase)
                    .unwrap_or_default()
            };
            let (scope, how_long, style, names_commands) = match status {
                RowStatus::New => match step.grant(request, row) {
                    Some(grant) => (
                        grant_label(request, row, &grant),
                        step.lifetimes
                            .get(row)
                            .map(lifetime_phrase)
                            .unwrap_or_default()
                            .to_owned(),
                        Style::default(),
                        grant_names_commands(request, row, &grant),
                    ),
                    None => (ONCE_ONLY.into(), String::new(), Style::default(), false),
                },
                RowStatus::Asks => (ASKS_EVERY_TIME.into(), coverage(), t.tool_warning, false),
                RowStatus::Allowed => (ALREADY_ALLOWED.into(), coverage(), t.tool_dim, false),
            };
            let mut spans = command_cell(
                &row_command(request, row),
                command_width,
                settled(style, status),
            );
            spans.push(Span::styled(COLUMN_GAP, style));
            spans.extend(pad_spans(
                ellipsize_spans(
                    scope_spans(&scope, style, t.accent, names_commands),
                    scope_width,
                ),
                scope_width,
            ));
            spans.push(Span::styled(COLUMN_GAP, style));
            spans.push(Span::styled(ellipsize(&how_long, rest), style));
            body.push([Line::from(spans)]);
        }
        body.blank();
        for (index, choice) in REVIEW_CHOICES.iter().enumerate() {
            let highlighted = index == step.highlight;
            let target = PromptTarget::Item(index);
            let style = choice_style(highlighted, self.hover.as_ref() == Some(&target), t);
            let lines = numbered(
                index,
                style,
                highlighted,
                vec![Span::styled(self.review_sentence(*choice), style)],
                &[],
                body.width,
                t,
            );
            body.control(target, lines, highlighted);
            if *choice == ReviewChoice::Deny && self.state == PromptState::Guidance {
                self.field_line(body, GUIDANCE_PLACEHOLDER, NUMBER_INDENT, t);
            }
        }
        self.pending_lines(body, t);
    }

    fn customize_body(&self, body: &mut Body, cap: usize, t: &Theme) {
        let (Some(request), Some(customize)) = (self.current(), &self.customize) else {
            return;
        };
        body.blank();
        self.action_block(body, request_lines(request), cap, t);
        self.context_lines(body, t);
        self.notes_block(body, t);
        body.blank();
        let label = |text: &str, field: Field| {
            let style = if customize.field == field {
                t.active
            } else {
                t.tool_dim
            };
            Span::styled(pad(text.into(), usize::from(FIELD_LABEL_WIDTH)), style)
        };
        let option = |text: &str, selected: bool, target: PromptTarget| {
            let hovered = self.hover.as_ref() == Some(&target);
            let (text, style) = if selected {
                (format!("[{text}]"), t.active)
            } else {
                (format!(" {text} "), Style::default())
            };
            (
                Span::styled(text, hover_style(style, hovered)),
                Some(target),
            )
        };
        let mut effects = vec![(label(EFFECT_LABEL, Field::Effect), None)];
        for (effect, name) in [(Effect::Allow, "Allow"), (Effect::Deny, "Deny")] {
            effects.push(option(
                name,
                customize.effect == effect,
                PromptTarget::Effect(effect),
            ));
            effects.push((Span::raw(" "), None));
        }
        body.spans(effects, FIELD_LABEL_WIDTH);
        let mut lifetimes = vec![(label(REMEMBER_LABEL, Field::Remember), None)];
        for lifetime in self.customize_lifetimes() {
            lifetimes.push(option(
                remember_name(&lifetime),
                customize.lifetime == lifetime,
                PromptTarget::Remember(lifetime.clone()),
            ));
            lifetimes.push((Span::raw(" "), None));
        }
        body.spans(lifetimes, FIELD_LABEL_WIDTH);
        let indent = usize::from(FIELD_LABEL_WIDTH);
        if request.presentation.reason == PromptReason::Plan
            && customize.effect == Effect::Allow
            && customize.broad(request)
        {
            body.push(indented(
                indent,
                BROAD_WHILE_PLANNING,
                t.tool_dim,
                body.width,
            ));
        }
        for (index, item) in self.customize_items().iter().enumerate() {
            let (text, badges, selectable) =
                scope_item_label(request, customize, item, self.batch());
            let highlighted = index == customize.highlight;
            let target = PromptTarget::Item(index);
            let hovered = self.hover.as_ref() == Some(&target);
            let style = if !selectable {
                t.tool_dim
            } else {
                choice_style(highlighted, hovered, t)
            };
            let lead = if index == 0 {
                label(SCOPE_LABEL, Field::Scope)
            } else {
                Span::raw(" ".repeat(indent))
            };
            let pointer = if highlighted { POINTER } else { NO_POINTER };
            let names_commands = selectable && scope_item_names_commands(request, customize, item);
            let mut spans = scope_spans(&text, style, t.accent, names_commands);
            if !badges.is_empty() {
                spans.push(Span::styled(
                    format!("{COLUMN_GAP}{}", badges.join(BADGE_SEPARATOR)),
                    style,
                ));
            }
            let mut lines = hanging_spans(
                Span::styled(format!("{}{pointer}", " ".repeat(indent)), style),
                spans,
                body.width,
            );
            if let Some(first) = lines.first_mut() {
                first.spans[0] = Span::styled(pointer, style);
                first.spans.insert(0, lead);
            }
            if highlighted
                && let ScopeItem::Row(grant) = item
                && let Some(values) = customize
                    .row
                    .and_then(|row| grant_template(request, row, grant))
                    .and_then(command_template_values)
            {
                lines.extend(indented(
                    indent + POINTER.width(),
                    &values,
                    t.tool_dim,
                    body.width,
                ));
            }
            body.control(target, lines, highlighted);
            if *item == ScopeItem::OwnPattern
                && self.state == PromptState::PatternEditing
                && let Some(row) = customize.row
            {
                self.pattern_field(body, row, indent + POINTER.width(), t);
            }
        }
        if customize.advanced
            && let Some(model) = self.customize_model()
        {
            body.blank();
            let height = ScopeView::summary_height(&model, body.width);
            body.insert(height, Insert::Scope(Box::new(model)));
        }
        self.pending_lines(body, t);
    }

    /// The rule Customize's chosen scope would store, for Advanced.
    pub(super) fn customize_model(&self) -> Option<ScopeModel> {
        let request = self.current()?;
        let customize = self.customize.as_ref()?;
        let lifetime = customize.lifetime.clone();
        match &customize.chosen {
            ScopeItem::Row(PermissionRowGrant::Pattern { definition, .. }) => {
                Some(pattern_model(definition.clone(), lifetime))
            }
            ScopeItem::Row(grant @ PermissionRowGrant::Offered(_)) => Some(option_model(
                request,
                grant_option(request, customize.row?, grant)?,
                lifetime,
            )),
            ScopeItem::Whole(id) => Some(option_model(
                request,
                request.options.iter().find(|option| option.id == *id)?,
                lifetime,
            )),
            ScopeItem::Row(PermissionRowGrant::Written(_)) | ScopeItem::OwnPattern => None,
        }
    }

    fn inspector_body(&self, body: &mut Body, cap: usize, t: &Theme) {
        let (Some(request), Some(inspector)) = (self.current(), &self.inspector) else {
            return;
        };
        body.blank();
        self.action_block(body, row_lines(request, inspector.row()), cap, t);
        if let Some(panel) = self.inspector_panel() {
            body.blank();
            let height = panel.height(body.width, t);
            body.insert(height, Insert::Pattern(panel));
        }
    }

    fn details_body(&self, body: &mut Body, t: &Theme) {
        for section in self.details() {
            body.blank();
            body.push([Line::styled(section.heading, t.panel_title)]);
            let lines = match section.command {
                true => command_lines(&section.lines),
                false => section
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| match section.patterns.contains(&index) {
                        true => marked_spans(line, Style::default(), t.accent, true),
                        false => code_spans_in(line, Style::default()),
                    })
                    .collect(),
            };
            for spans in lines {
                body.push(hanging_spans(Span::raw(HANG), spans, body.width));
            }
        }
    }

    /// Whether `←` `→` can move the main view's scope.
    fn scope_movable(&self) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if self.opacity().is_some() {
            return false;
        }
        if self.per_row() {
            return self.focus_row.is_some_and(|row| {
                row_positions(request, row, &self.row_grant(row), self.batch()).len() > 1
            });
        }
        main_ladder(request).len() > 1
    }

    /// The keys that do something right now, in the order they matter.
    fn footer_hints(&self) -> Vec<KeyHint> {
        let enter = |description| KeyHint::key("Enter", description, KeyCode::Enter);
        let esc = |description| KeyHint::key("Esc", description, KeyCode::Esc);
        if self.inspector.is_some() && self.panel != Panel::Details {
            return self.inspector_hints();
        }
        if self.pending.is_some() {
            return vec![enter("allow"), esc("back")];
        }
        match self.state {
            PromptState::Guidance => return vec![enter("send"), esc("cancel")],
            PromptState::PatternEditing => return vec![enter("use"), esc("back")],
            PromptState::Normal => {}
        }
        match self.panel {
            Panel::Details => vec![
                KeyHint::inert("↑↓", "scroll"),
                KeyHint::char("?", "back"),
                esc("no"),
            ],
            Panel::Customize => {
                let mut hints = vec![
                    KeyHint::key("Tab", "next field", KeyCode::Tab),
                    KeyHint::inert("↑↓", "choose"),
                    enter("apply"),
                ];
                if self.customize_model().is_some() {
                    hints.push(KeyHint::char("v", "advanced"));
                }
                if self.inspectable() {
                    hints.push(KeyHint::char("i", "edit template"));
                }
                hints.push(esc("back"));
                hints
            }
            Panel::StepThrough if self.step.as_ref().is_some_and(|step| step.page.is_none()) => {
                vec![
                    KeyHint::inert("↑↓", "choose"),
                    enter("confirm"),
                    KeyHint::key("Shift-Tab", "back", KeyCode::BackTab),
                    esc("main view"),
                ]
            }
            Panel::StepThrough => {
                let mut hints = vec![KeyHint::inert("↑↓", "scope")];
                if self.page_lifetimes().len() > 1 {
                    hints.push(KeyHint::inert("←→", "how long"));
                }
                hints.extend([enter("choose"), KeyHint::key("Tab", "next", KeyCode::Tab)]);
                if self.inspectable() {
                    hints.push(KeyHint::char("i", "edit template"));
                }
                hints.push(esc("back"));
                hints
            }
            Panel::Main => {
                let several = self.batch() && self.per_row() && self.new_rows().len() > 1;
                let mut hints = Vec::new();
                if several {
                    hints.push(KeyHint::key("Tab", "next", KeyCode::Tab));
                }
                if self.scope_movable() {
                    hints.extend([
                        KeyHint::inert("←", "broader"),
                        KeyHint::inert("→", "narrower"),
                    ]);
                }
                if several {
                    hints.push(KeyHint::inert("<>", "all"));
                }
                hints.push(KeyHint::char(
                    "e",
                    if self.batch() && self.per_row() && !self.new_rows().is_empty() {
                        "one by one"
                    } else if self.choices().len() > 2 {
                        "customize"
                    } else {
                        "more options"
                    },
                ));
                hints.extend([KeyHint::char("?", "details"), esc("no")]);
                hints
            }
        }
    }

    /// Every key's description when they all fit, else only the last one's.
    fn draw_footer(&mut self, frame: &mut Frame, area: Rect, t: &Theme) {
        if self.decision_needs_rearm() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("{}{REARM_MESSAGE}", " ".repeat(usize::from(MARGIN))),
                    t.status_notice,
                )),
                area,
            );
            return;
        }
        let hints = self.footer_hints();
        let full: Vec<Hint> = hints.iter().map(|hint| hint.hint(false)).collect();
        let hints = if hint_hits(&full, area).len() == full.len() {
            full
        } else {
            hints
                .iter()
                .enumerate()
                .map(|(index, hint)| hint.hint(index + 1 < hints.len()))
                .collect()
        };
        let hits = hint_hits(&hints, area);
        let shown = &hints[..hits.len()];
        let hovered = self.hover.as_ref().or(self.focus.as_ref());
        let hovered = shown.iter().position(|hint| {
            hint.press()
                .is_some_and(|key| hovered == Some(&PromptTarget::Hint(key)))
        });
        frame.render_widget(Paragraph::new(hint_line_hovered(shown, hovered)), area);
        for (area, hint) in hits.into_iter().zip(shown) {
            if let Some(key) = hint.press() {
                self.hits.push(PromptHit {
                    area,
                    target: PromptTarget::Hint(key),
                });
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::fs::{self, OpenOptions, Permissions};
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    use caudra_agent::permissions::{
        AutoNote, CONFINED_READ_AUTHORITY, EngineFlag, OPACITY_ATTRIBUTE, PermissionAdvisory,
        PermissionAuthorityProfile, PermissionExecutorKind, PermissionLifetime, PermissionRequest,
        PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
        PermissionSubject, PromptReason, ResourceCoverage, RuleOrigin, ScriptLanguage,
        ShellOpacity, StructuredPermissionEffect,
    };
    use caudra_agent::tools::native;
    use caudra_agent::tools::native::plan::{self, PlanAccess};
    use caudra_agent::tools::{PermissionIntent, PermissionScopes};
    use caudra_config::ToolKey;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use serde_json::{Value, json};
    #[cfg(unix)]
    use tempfile::Builder;
    use test_case::test_case;

    use super::super::customize::BROAD;
    use super::super::decision::tests::{
        PROJECT, command_resource, commands_request, native_shell_request, shell_request,
    };
    use super::super::inspector::tests::learned_request;
    use super::super::{Panel, PermissionPrompt};
    use super::{PLAN_READ_QUESTION, PLAN_WRITE_QUESTION};
    use crate::components::buffer_text;
    use crate::components::command_text::tests::{
        COLOUR_THEME, NOT_COLOURED, NOT_TOLD_APART, PYTHON_SYNTAX, SHELL_SYNTAX, assert_drawn_in,
        coloured, drawn_colours, syntax_colour,
    };
    use crate::theme::{self, Theme};

    pub(crate) const ROOMY_WIDTH: u16 = 140;
    pub(crate) const ROOMY_HEIGHT: u16 = 40;
    pub(crate) const SINGLE_COMMAND: &str = "cargo test -p caudra-agent permissions::structured";
    pub(crate) const BATCH: [&str; 6] = [
        "cargo fmt -p caudra-agent",
        "cargo clippy -p caudra-agent --tests",
        "rm -rf target/tmp",
        "git push origin HEAD",
        "rg -n TODO src",
        "head",
    ];
    const ASK_RULE: &str = "git push *";
    const DYNAMIC_ROWS: [&str; 2] = ["ls \"$TARGET\"", "wc -l \"$TARGET\""];
    const HEREDOC: &str = "python3 - <<'EOF'\nimport json, pathlib\nfor path in pathlib.Path(\"logs\").glob(\"*.json\"):\n    print(json.loads(path.read_text())[\"event\"])\nEOF";
    const HEREDOC_BODY: &str = "import json, pathlib";
    const PYTHON_KEYWORD: &str = "import";
    pub(crate) const PAGE: &str =
        "https://docs.rs/ratatui/latest/ratatui/widgets/struct.Paragraph.html";
    pub(crate) const OUTSIDE_FILE: &str = "/etc/caudra/caudra.toml";
    const READ_CONTRACT: &str = "file.read.v1";
    const WORKCELL_OWNER: &str = "workcell";
    const LOCAL_PLAN: &str = "/project/plan.md";
    const REMOTE_PLAN: &str = "plan-0123456789abcdef0123456789abcdef";
    const PLAN_DOCUMENT_RESOURCE: &str = "local_document";
    const PLAN_SCOPE_PREFIX: &str = "plan:";
    const OPERATION_ATTRIBUTE: &str = "operation";
    const PLAN_PLUGIN: &str = "planner";
    const GENERIC_READ_QUESTION: &str = "Allow reading a file?";
    #[cfg(unix)]
    const PRIVATE_ARTIFACT_MODE: u32 = 0o600;
    #[cfg(unix)]
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    pub(crate) const THEMES: [&str; 2] = ["ayu_dark", "ayu_light"];
    pub(crate) const WIDTHS: [u16; 3] = [40, 80, 140];
    const FIT_WIDTH: u16 = 80;
    const FIT_HEIGHT: u16 = 24;
    const FRAME: &str = "│╭╮╰╯─";
    const MIN_HEX_RUN: usize = 32;
    const HASH_TAG_DIGITS: usize = 8;
    const ADVISORY_PROBABILITY: f64 = 0.875;
    const DELETE_CAUTION: &str = "May delete files (88%)";
    const BARE_RUNG: &str = "cargo *";
    pub(crate) const LEARNED_SEEN: usize = 4;
    const LEARNED_SCOPE: &str = "‹cargo check -p <value> --tests›";
    pub(crate) const TEMPLATE_VALUES: &str = "<value> is caudra-agent or caudra-ui.";
    const LEARNED_LINE: &str =
        "Learned from 4 similar commands. <value> is caudra-agent or caudra-ui.";
    const LEARNED_WORDS: &str = "Learned from";
    const OTHER_COMMAND: &str = "ls src";
    pub(crate) const INTERNAL_TERMS: [&str; 12] = [
        "SHA-256",
        "sha256",
        "preimage",
        "digest",
        "ANY OF",
        "ALL OF",
        "guard",
        "Canonical",
        "Opaque",
        "opaque",
        "rule family",
        "option id",
    ];

    pub(crate) fn request(id: &str, input: Value) -> Box<PermissionRequest> {
        let mut request = PermissionRequest::from_legacy(
            id.into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            input,
            Path::new(PROJECT),
            false,
        );
        request.presentation.project = Some(PROJECT.into());
        Box::new(request)
    }

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn render(prompt: &mut PermissionPrompt, width: u16, height: u16) -> String {
        buffer_text(&themed_buffer(prompt, width, height, &theme::current()))
    }

    pub(crate) fn themed_buffer(
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

    pub(crate) fn buffer_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    pub(crate) fn screen(prompt: &mut PermissionPrompt, width: u16, height: u16) -> String {
        buffer_rows(&themed_buffer(prompt, width, height, &theme::current())).join("\n")
    }

    /// The screen's words in reading order with the frame dropped, so a
    /// sentence is found wherever it wraps.
    pub(crate) fn prose(prompt: &mut PermissionPrompt, width: u16, height: u16) -> String {
        screen(prompt, width, height)
            .split(|character: char| character.is_whitespace() || FRAME.contains(character))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn prompt_for(request: PermissionRequest) -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(Box::new(request), None);
        prompt
    }

    pub(crate) fn shell_prompt(command: &str) -> PermissionPrompt {
        prompt_for(native_shell_request(command))
    }

    pub(crate) fn cover(
        request: &mut PermissionRequest,
        row: usize,
        origin: RuleOrigin,
        authority: &str,
        asks: bool,
    ) {
        request.presentation.resources[row].coverage = Some(ResourceCoverage {
            origin,
            authority: authority.into(),
            asks,
        });
    }

    /// The §2 script: three new commands, one an ask rule covers, and two
    /// read-only ones.
    pub(crate) fn batch_request() -> PermissionRequest {
        let mut request = commands_request(&BATCH);
        cover(&mut request, 3, RuleOrigin::Config, ASK_RULE, true);
        for row in [4, 5] {
            cover(
                &mut request,
                row,
                RuleOrigin::Builtin,
                CONFINED_READ_AUTHORITY,
                false,
            );
        }
        request
    }

    pub(crate) fn heredoc_request() -> PermissionRequest {
        let mut line = command_resource(HEREDOC);
        line.protected = true;
        line.requires_prompt = true;
        line.attributes.insert(
            OPACITY_ATTRIBUTE.into(),
            ShellOpacity::InlineScript {
                language: ScriptLanguage::Python,
            }
            .to_string(),
        );
        let mut request = shell_request(HEREDOC, vec![command_resource("python3 -"), line]);
        request.presentation.auto = Some(AutoNote::EngineNeeded);
        request
    }

    pub(crate) fn fetch_request() -> PermissionRequest {
        let mut request = PermissionRequest::from_legacy(
            "fetch".into(),
            ToolKey::native("webfetch"),
            vec![PAGE.into()],
            json!({"url": PAGE}),
            Path::new(PROJECT),
            false,
        );
        request.presentation.project = Some(PROJECT.into());
        request
    }

    pub(crate) fn file_request(path: &str, access: PermissionResourceAccess) -> PermissionRequest {
        let intent = PermissionIntent::new(
            PermissionScopes::single(path.into()),
            vec![PermissionResource {
                kind: PermissionResourceKind::File,
                value: path.into(),
                access: Some(access),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Medium,
        )
        .with_authority(PermissionAuthorityProfile::Filesystem {
            input_pointers: Vec::new(),
        });
        let mut request = PermissionRequest::from_intent_with_identity(
            "file".into(),
            ToolKey::native("file_read"),
            &intent,
            json!({"filePath": path}),
            Path::new(PROJECT),
            PermissionSubject::Native {
                owner: WORKCELL_OWNER.into(),
                contract: READ_CONTRACT.into(),
            },
            PermissionExecutorKind::Native,
        );
        request.presentation.project = Some(PROJECT.into());
        request
    }

    /// A `plan` call the way the native tool asks about it: a local plan by
    /// its path, a remote one by its document reference.
    fn plan_request(remote: bool, access: PlanAccess) -> PermissionRequest {
        let operation = access.operation();
        let (kind, value, locator) = if remote {
            (
                PermissionResourceKind::Custom {
                    name: PLAN_DOCUMENT_RESOURCE.into(),
                },
                format!("{PLAN_SCOPE_PREFIX}{REMOTE_PLAN}"),
                REMOTE_PLAN,
            )
        } else {
            (PermissionResourceKind::File, LOCAL_PLAN.into(), LOCAL_PLAN)
        };
        let intent = PermissionIntent::new(
            PermissionScopes::single(format!("{PLAN_SCOPE_PREFIX}{operation}:{locator}")),
            vec![PermissionResource {
                kind,
                value,
                access: Some(access.resource_access()),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::from([(OPERATION_ATTRIBUTE.into(), operation.into())]),
            }],
            PermissionRisk::Low,
        );
        let mut request = PermissionRequest::from_intent_with_identity(
            plan::NAME.into(),
            ToolKey::native(plan::NAME),
            &intent,
            json!({"action": operation}),
            Path::new(PROJECT),
            PermissionSubject::Native {
                owner: native::OWNER.into(),
                contract: plan::permission_contract().into(),
            },
            PermissionExecutorKind::Native,
        );
        request.presentation.project = Some(PROJECT.into());
        request
    }

    /// A plugin's tool that happens to be called `plan`.
    fn plugin_plan_request() -> PermissionRequest {
        let mut request = plan_request(false, PlanAccess::Read);
        request.subject = PermissionSubject::Lua {
            plugin: PLAN_PLUGIN.into(),
            tool: plan::NAME.into(),
            contract: plan::permission_contract().into(),
        };
        request.executor = PermissionExecutorKind::Lua;
        request
    }

    /// A request the way plan mode sends it: no allow lasts for all
    /// projects, and a broad one lasts this conversation at most.
    pub(crate) fn planning(mut request: PermissionRequest) -> PermissionRequest {
        request.presentation.reason = PromptReason::Plan;
        for option in &mut request.options {
            if option.rule.effect != StructuredPermissionEffect::Allow {
                continue;
            }
            let broad = option.confirmation.is_some();
            option.allowed_lifetimes.retain(|lifetime| match lifetime {
                PermissionLifetime::Once | PermissionLifetime::Conversation => true,
                PermissionLifetime::Project => !broad,
                PermissionLifetime::Global => false,
            });
        }
        request
    }

    pub(crate) fn advised(mut request: PermissionRequest) -> PermissionRequest {
        request.presentation.advisories.push(PermissionAdvisory {
            flag: EngineFlag::Deletes,
            probability: ADVISORY_PROBABILITY,
        });
        request
    }

    /// Every state a reviewer should see, by name.
    pub(crate) fn surfaces() -> Vec<(&'static str, PermissionPrompt)> {
        let mut surfaces = vec![
            ("single", shell_prompt(SINGLE_COMMAND)),
            ("batch", prompt_for(batch_request())),
            ("heredoc", prompt_for(heredoc_request())),
            ("fetch", prompt_for(fetch_request())),
            (
                "outside-file",
                prompt_for(file_request(OUTSIDE_FILE, PermissionResourceAccess::Read)),
            ),
            (
                "plan",
                prompt_for(planning(native_shell_request(SINGLE_COMMAND))),
            ),
            (
                "remote-plan-read",
                prompt_for(plan_request(true, PlanAccess::Read)),
            ),
            (
                "remote-plan-write",
                prompt_for(plan_request(true, PlanAccess::Write)),
            ),
            (
                "caution",
                prompt_for(advised(native_shell_request(
                    "rm -rf target/debug/incremental",
                ))),
            ),
            ("learned", prompt_for(learned_request(LEARNED_SEEN, &[]))),
        ];
        let mut guidance = shell_prompt(SINGLE_COMMAND);
        guidance.open_guidance();
        guidance
            .field
            .set_text("use cargo clean -p caudra-agent instead");
        surfaces.push(("guidance", guidance));
        let mut customize = shell_prompt(SINGLE_COMMAND);
        customize.open_customize(false);
        surfaces.push(("customize", customize));
        let mut learned_customize = prompt_for(learned_request(LEARNED_SEEN, &[]));
        learned_customize.open_customize(false);
        surfaces.push(("learned-customize", learned_customize));
        let mut advanced = shell_prompt(SINGLE_COMMAND);
        advanced.open_customize(false);
        if let Some(customize) = advanced.customize.as_mut() {
            customize.advanced = true;
        }
        surfaces.push(("advanced", advanced));
        let mut broad = shell_prompt(SINGLE_COMMAND);
        broad.open_customize(false);
        let last = broad.customize_items().len() - 1;
        broad.highlight_scope(last);
        broad.awaiting_review = false;
        broad.apply_customize();
        surfaces.push(("confirm", broad));
        let mut details = shell_prompt(SINGLE_COMMAND);
        details.toggle_details();
        surfaces.push(("details", details));
        let mut batch_details = prompt_for(batch_request());
        batch_details.toggle_details();
        surfaces.push(("batch-details", batch_details));
        let mut page = prompt_for(batch_request());
        page.open_step_through();
        surfaces.push(("page", page));
        let mut review = prompt_for(batch_request());
        review.open_step_through();
        review.go_to_page(None);
        surfaces.push(("review", review));
        surfaces
    }

    fn hex_run(text: &str) -> bool {
        let mut run = 0;
        text.chars().any(|character| {
            run = if character.is_ascii_hexdigit() {
                run + 1
            } else {
                0
            };
            run >= MIN_HEX_RUN
        })
    }

    fn hash_tag(text: &str) -> bool {
        text.match_indices('#').any(|(index, _)| {
            text[index + 1..]
                .chars()
                .take(HASH_TAG_DIGITS)
                .filter(char::is_ascii_hexdigit)
                .count()
                == HASH_TAG_DIGITS
        })
    }

    pub(crate) fn assert_plain(rows: &[String], surface: &str) {
        for row in rows {
            for term in INTERNAL_TERMS {
                assert!(!row.contains(term), "{surface}: {term} in {row:?}");
            }
            assert!(!hex_run(row), "{surface}: hex run in {row:?}");
            assert!(!hash_tag(row), "{surface}: hash tag in {row:?}");
        }
    }

    #[test]
    #[ignore = "prints every surface for a visual review"]
    fn dump_permission_prompt_surfaces() {
        for (width, height) in [(80, 24), (40, 24)] {
            for (name, mut prompt) in surfaces() {
                println!("== {name} {width}x{height}");
                println!("{}", screen(&mut prompt, width, height));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "writes private visual review buffers under /tmp"]
    fn export_permission_prompt_buffers() {
        let directory = Builder::new()
            .prefix("caudra-permission-prompt-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(
            directory.path(),
            Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .unwrap();
        for name in THEMES {
            let t = theme::load_by_name(name).unwrap();
            for (width, height) in [(40, 24), (80, 24), (80, 16), (140, 32)] {
                for (surface, mut prompt) in surfaces() {
                    let buffer = themed_buffer(&mut prompt, width, height, &t);
                    let stem = format!("{surface}-{name}-{width}x{height}");
                    for (extension, contents) in [
                        ("txt", buffer_rows(&buffer).join("\n")),
                        ("cells", format!("{buffer:#?}")),
                    ] {
                        let mut file = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(PRIVATE_ARTIFACT_MODE)
                            .open(directory.path().join(format!("{stem}.{extension}")))
                            .unwrap();
                        file.write_all(contents.as_bytes()).unwrap();
                    }
                }
            }
        }
        println!("Permission prompt buffers: {}", directory.keep().display());
    }

    #[test]
    fn common_prompts_fit_80x24_without_scrolling() {
        for (name, mut prompt) in surfaces() {
            if matches!(name, "details" | "batch-details" | "advanced") {
                continue;
            }
            let rows = buffer_rows(&themed_buffer(
                &mut prompt,
                FIT_WIDTH,
                FIT_HEIGHT,
                &theme::current(),
            ));
            assert!(
                prompt.height(FIT_WIDTH) <= FIT_HEIGHT,
                "{name} needs {} rows:\n{}",
                prompt.height(FIT_WIDTH),
                rows.join("\n")
            );
            assert_eq!(prompt.scroll.offset(), 0, "{name}");
        }
    }

    #[test]
    fn prompt_surfaces_never_show_internal_terms() {
        for name in THEMES {
            let t = theme::load_by_name(name).unwrap();
            for width in WIDTHS {
                for (surface, mut prompt) in surfaces() {
                    let height = prompt.height(width);
                    let rows = buffer_rows(&themed_buffer(&mut prompt, width, height, &t));
                    assert_plain(&rows, surface);
                }
            }
        }
    }

    #[test_case(native_shell_request(SINGLE_COMMAND), "Allow shell command?"; "single_command")]
    #[test_case(commands_request(&["cargo fmt", "cargo clippy"]), "Allow shell commands?"; "batch")]
    #[test_case(fetch_request(), "Allow fetching a web page?"; "web_fetch")]
    #[test_case(file_request(OUTSIDE_FILE, PermissionResourceAccess::Read), GENERIC_READ_QUESTION; "file_read")]
    #[test_case(file_request(OUTSIDE_FILE, PermissionResourceAccess::Write), "Allow editing a file?"; "file_write")]
    #[test_case(plan_request(false, PlanAccess::Read), PLAN_READ_QUESTION; "local_plan_read")]
    #[test_case(plan_request(false, PlanAccess::Write), PLAN_WRITE_QUESTION; "local_plan_write")]
    #[test_case(plan_request(true, PlanAccess::Read), PLAN_READ_QUESTION; "remote_plan_read")]
    #[test_case(plan_request(true, PlanAccess::Write), PLAN_WRITE_QUESTION; "remote_plan_write")]
    #[test_case(plugin_plan_request(), GENERIC_READ_QUESTION; "a_plugin_named_plan")]
    fn the_title_asks_about_the_tool(request: PermissionRequest, question: &str) {
        let mut prompt = prompt_for(request);
        assert_eq!(prompt.question(), question);
        assert!(render(&mut prompt, FIT_WIDTH, FIT_HEIGHT).contains(question));
    }

    /// A local plan is named by its path. A remote plan's reference means
    /// nothing to a reader, so it is named as the session's plan instead.
    #[test_case(false, PlanAccess::Read, LOCAL_PLAN; "local_read")]
    #[test_case(false, PlanAccess::Write, LOCAL_PLAN; "local_write")]
    #[test_case(true, PlanAccess::Read, plan::SESSION_PLAN_LABEL; "remote_read")]
    #[test_case(true, PlanAccess::Write, plan::SESSION_PLAN_LABEL; "remote_write")]
    fn a_plan_prompt_names_the_plan_not_its_reference(
        remote: bool,
        access: PlanAccess,
        target: &str,
    ) {
        let mut prompt = prompt_for(plan_request(remote, access));
        let text = prose(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains(target), "{text}");
        assert!(!text.contains(REMOTE_PLAN), "{text}");
    }

    #[test]
    fn single_command_reads_as_numbered_sentences() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        for line in [
            "❯ 1. Yes",
            "2. Yes, and allow ‹cargo test *› for this conversation",
            "3. Yes, and always allow ‹cargo test *› in this project",
            "4. No, and tell the agent what to do instead",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn plan_mode_keeps_the_project_choice_and_says_why() {
        let mut prompt = prompt_for(planning(native_shell_request(SINGLE_COMMAND)));
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        for line in [
            "in this project",
            "4. No, and tell the agent what to do instead",
            super::super::notes::PLAN_NOTE,
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn opaque_lines_offer_only_once_or_no() {
        let mut prompt = prompt_for(heredoc_request());
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        for line in [
            "Runs inline Python that Caudra can't check",
            AutoNote::EngineNeeded.phrase(),
            "❯ 1. Yes, run it once",
            "2. No, and tell the agent what to do instead",
            "e more options",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(!text.contains("3."), "{text}");
    }

    #[test]
    fn engine_cautions_render_as_warning_notes() {
        let mut prompt = prompt_for(advised(native_shell_request(SINGLE_COMMAND)));
        assert!(render(&mut prompt, FIT_WIDTH, FIT_HEIGHT).contains(DELETE_CAUTION));
        prompt.toggle_details();
        assert!(render(&mut prompt, FIT_WIDTH, ROOMY_HEIGHT).contains(DELETE_CAUTION));
    }

    #[test_case(None; "manual")]
    #[test_case(Some(AutoNote::EngineFlagged); "auto")]
    fn auto_note_renders_only_in_auto(auto: Option<AutoNote>) {
        let mut request = native_shell_request(SINGLE_COMMAND);
        request.presentation.auto = auto;
        let mut prompt = prompt_for(request);
        let text = render(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert_eq!(text.contains("Auto asked"), auto.is_some(), "{text}");
    }

    #[test]
    fn allowed_rows_name_their_origin() {
        let mut prompt = prompt_for(batch_request());
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        let rows: Vec<&str> = text.lines().filter(|row| row.contains("allowed")).collect();
        assert_eq!(rows.len(), 2, "{text}");
        assert!(rows.iter().all(|row| row.contains("read-only")), "{text}");
        assert!(text.contains("3 of 6 new"), "{text}");
    }

    #[test]
    fn opaque_batches_count_every_uncovered_row_as_new() {
        let mut line = command_resource(&DYNAMIC_ROWS.join(" && "));
        line.protected = true;
        line.requires_prompt = true;
        line.attributes
            .insert(OPACITY_ATTRIBUTE.into(), ShellOpacity::Dynamic.to_string());
        let mut resources: Vec<_> = DYNAMIC_ROWS
            .iter()
            .map(|command| command_resource(command))
            .collect();
        resources.push(line);
        let mut prompt = prompt_for(shell_request(&DYNAMIC_ROWS.join(" && "), resources));
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains("2 of 2 new"), "{text}");
    }

    #[test]
    fn ask_covered_rows_say_asks() {
        let mut prompt = prompt_for(batch_request());
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        let row = text
            .lines()
            .find(|row| row.contains("git push origin HEAD") && row.contains("asks"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(row.contains("git push * · config"), "{row}");
        assert!(!prompt.new_rows().contains(&3));
    }

    #[test]
    fn many_allowed_rows_collapse_into_a_count() {
        let commands = ["cargo test", "rg a", "rg b", "rg c", "rg d"];
        let mut request = commands_request(&commands);
        for row in 1..commands.len() {
            cover(
                &mut request,
                row,
                RuleOrigin::Builtin,
                CONFINED_READ_AUTHORITY,
                false,
            );
        }
        let mut prompt = prompt_for(request);
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains("+ 4 already allowed"), "{text}");
        assert_eq!(
            text.lines().filter(|row| row.contains("allowed")).count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn step_through_review_lists_every_row() {
        let mut prompt = prompt_for(batch_request());
        prompt.open_step_through();
        prompt.go_to_page(None);
        let text = screen(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        for command in BATCH {
            assert!(text.contains(command), "{command}\n{text}");
        }
        for phrase in [
            "asks every time",
            "already allowed",
            "Yes, and remember as listed",
            "More options for the whole script…",
        ] {
            assert!(text.contains(phrase), "{phrase}\n{text}");
        }
    }

    #[test]
    fn long_scripts_cap_the_action_block() {
        let script: Vec<String> = (0..40).map(|line| format!("echo line {line}")).collect();
        let mut prompt = shell_prompt(&script.join("\n"));
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains("more lines · ? shows all"), "{text}");
        assert!(
            text.contains("4. No, and tell the agent what to do instead"),
            "{text}"
        );
    }

    #[test_case(20, 6; "too_narrow_and_short")]
    #[test_case(31, 24; "too_narrow")]
    #[test_case(80, 7; "too_short")]
    fn unusable_layouts_have_no_targets(width: u16, height: u16) {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        render(&mut prompt, width, height);
        assert!(prompt.hits.is_empty());
        assert!(prompt.handle_key(key(KeyCode::Char('y'))).is_none());
    }

    #[test]
    fn colours_leave_every_surface_word_for_word() {
        let draw = || {
            surfaces()
                .into_iter()
                .map(|(name, mut prompt)| (name, screen(&mut prompt, FIT_WIDTH, ROOMY_HEIGHT)))
                .collect::<Vec<_>>()
        };
        let plain = draw();
        coloured();
        assert_eq!(draw(), plain);
    }

    #[test]
    fn a_heredoc_body_is_drawn_in_the_language_it_feeds() {
        coloured();
        let python = syntax_colour(PYTHON_SYNTAX, HEREDOC_BODY, PYTHON_KEYWORD);
        assert_ne!(
            python,
            syntax_colour(SHELL_SYNTAX, HEREDOC_BODY, PYTHON_KEYWORD),
            "{NOT_TOLD_APART}"
        );
        let mut prompt = prompt_for(heredoc_request());
        let buffer = themed_buffer(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT, &theme::current());
        assert_drawn_in(&buffer, PYTHON_KEYWORD, 0, python);
    }

    /// The named surface drawn roomy, colours on.
    fn coloured_surface(name: &str) -> Buffer {
        coloured();
        let (_, mut prompt) = surfaces()
            .into_iter()
            .find(|(surface, _)| *surface == name)
            .unwrap();
        themed_buffer(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT, &theme::current())
    }

    /// The colour shell gives `command`'s program.
    fn program_colour(command: &str) -> Option<Color> {
        coloured();
        let program = command.split_whitespace().next().unwrap_or_default();
        let colour = syntax_colour(SHELL_SYNTAX, command, program);
        assert!(colour.is_some(), "{NOT_COLOURED}");
        colour
    }

    /// What `before` a command tells which part of a surface shows it.
    #[test_case("single", "", SINGLE_COMMAND; "request")]
    #[test_case("single", "‹", "cargo test *"; "remembered_scope")]
    #[test_case("batch", "", "rm -rf target/tmp"; "batch_rows")]
    #[test_case("batch", "‹", "cargo fmt *"; "row_scope")]
    #[test_case("customize", "", "cargo test -p caudra-agent *"; "customize_scope")]
    #[test_case("details", "‹", "cargo test *"; "details_scope")]
    #[test_case("details", "`", "cargo test"; "details_sentence")]
    #[test_case("batch-details", "‹", "cargo fmt *"; "details_row_scope")]
    #[test_case("batch-details", "`", "rg -n TODO src"; "details_allowed")]
    #[test_case("batch-details", "`", "git push origin HEAD"; "details_asks")]
    #[test_case("page", "", "cargo fmt -p caudra-agent"; "page_command")]
    #[test_case("page", "3. ", "cargo fmt *"; "page_scope")]
    #[test_case("review", "", "rm -rf target/tmp"; "review_command")]
    #[test_case("review", "", "cargo clippy *"; "review_scope")]
    fn commands_are_drawn_in_shell_colours(surface: &str, before: &str, command: &str) {
        assert_drawn_in(
            &coloured_surface(surface),
            &format!("{before}{command}"),
            before.chars().count(),
            program_colour(command),
        );
    }

    /// What will run comes first in Details. The tool's input, further down,
    /// lists the same command as raw words.
    #[test]
    fn details_draw_what_will_run_in_shell_colours() {
        let places = drawn_colours(&coloured_surface("details"), SINGLE_COMMAND);
        assert_eq!(
            places.first().and_then(|colours| colours.first()).copied(),
            program_colour(SINGLE_COMMAND)
        );
    }

    /// A scope named in words is drawn in one colour wherever it shows.
    #[test_case("caution", "‹this exact command›"; "choice")]
    #[test_case("batch", "‹this exact command›"; "row_scope")]
    #[test_case("batch-details", "‹this exact command›"; "details_row_scope")]
    #[test_case("fetch", "‹this page and below›"; "page_scope")]
    fn scopes_in_words_stay_words(surface: &str, scope: &str) {
        let places = drawn_colours(&coloured_surface(surface), scope);
        assert!(!places.is_empty(), "{scope}\n{surface}");
        for colours in places {
            assert!(
                colours.iter().all(|colour| *colour == colours[0]),
                "{scope}: {colours:?}"
            );
        }
    }

    /// Colours change nothing in a prompt that runs no command.
    #[test_case(false; "prompt")]
    #[test_case(true; "details")]
    fn only_commands_take_shell_colours(details: bool) {
        theme::set(theme::load_by_name(COLOUR_THEME).unwrap());
        let draw = || {
            let mut prompt = prompt_for(fetch_request());
            if details {
                prompt.toggle_details();
            }
            themed_buffer(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT, &theme::current())
        };
        let plain = draw();
        coloured();
        assert_eq!(draw(), plain);
    }

    #[test]
    fn details_drop_hashes_and_name_the_tool() {
        let mut prompt = shell_prompt(SINGLE_COMMAND);
        prompt.toggle_details();
        assert_eq!(prompt.panel, Panel::Details);
        let height = prompt.height(ROOMY_WIDTH);
        let rows = buffer_rows(&themed_buffer(
            &mut prompt,
            ROOMY_WIDTH,
            height,
            &theme::current(),
        ));
        let text = rows.join("\n");
        for heading in [
            "What will run",
            "Where",
            "Why Caudra is asking",
            "Tool",
            "Input",
        ] {
            assert!(text.contains(heading), "{heading}\n{text}");
        }
        assert!(text.contains("(this project)"), "{text}");
        assert_plain(&rows, "details");
    }

    /// The bare executable is the one rung of a command's ladder that needs
    /// confirming, so a page marks it the way Customize does.
    #[test_case("customize"; "customize")]
    #[test_case("page"; "page")]
    fn the_bare_rung_is_marked_broad(surface: &str) {
        let (_, mut prompt) = surfaces()
            .into_iter()
            .find(|(name, _)| *name == surface)
            .unwrap();
        let text = screen(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        let rung = text
            .lines()
            .find(|line| line.contains(BARE_RUNG))
            .unwrap_or_else(|| panic!("{BARE_RUNG} is not listed\n{text}"));
        assert!(rung.contains(BROAD), "{rung}");
    }

    fn never_remembered(mut request: PermissionRequest) -> PermissionRequest {
        for option in &mut request.options {
            option.allowed_lifetimes = vec![PermissionLifetime::Once];
        }
        request
    }

    #[test_case(learned_request(LEARNED_SEEN, &[]), true; "learned_template")]
    #[test_case(native_shell_request(SINGLE_COMMAND), false; "prefix")]
    #[test_case(never_remembered(learned_request(LEARNED_SEEN, &[])), false; "once_only")]
    fn only_a_learned_scope_to_remember_says_it_was_learned(
        request: PermissionRequest,
        learned: bool,
    ) {
        let mut prompt = prompt_for(request);
        let text = prose(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert_eq!(text.contains(LEARNED_WORDS), learned, "{text}");
    }

    /// The learned line speaks for the scope the choices remember, so it
    /// goes once `←` widens past the template and comes back with `→`.
    #[test]
    fn the_learned_line_follows_the_scope() {
        let mut prompt = prompt_for(learned_request(LEARNED_SEEN, &[]));
        let text = prose(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains(LEARNED_SCOPE), "{text}");
        assert!(text.contains(LEARNED_LINE), "{text}");
        prompt.handle_key(key(KeyCode::Left));
        let text = prose(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(!text.contains(LEARNED_WORDS), "{text}");
        prompt.handle_key(key(KeyCode::Right));
        let text = prose(&mut prompt, FIT_WIDTH, FIT_HEIGHT);
        assert!(text.contains(LEARNED_LINE), "{text}");
    }

    #[test]
    fn the_learned_line_follows_the_focused_command() {
        let mut prompt = prompt_for(learned_request(LEARNED_SEEN, &[OTHER_COMMAND]));
        let text = prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(text.contains(LEARNED_LINE), "{text}");
        prompt.handle_key(key(KeyCode::Tab));
        let text = prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!text.contains(LEARNED_WORDS), "{text}");
        prompt.handle_key(key(KeyCode::BackTab));
        let text = prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(text.contains(LEARNED_LINE), "{text}");
    }

    #[test]
    fn customize_says_what_a_highlighted_template_stands_for() {
        let mut prompt = prompt_for(learned_request(LEARNED_SEEN, &[]));
        prompt.open_customize(false);
        let text = prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(text.contains(TEMPLATE_VALUES), "{text}");
        assert!(!text.contains(LEARNED_WORDS), "{text}");
        prompt.handle_key(key(KeyCode::Down));
        let text = prose(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!text.contains(TEMPLATE_VALUES), "{text}");
    }
}
