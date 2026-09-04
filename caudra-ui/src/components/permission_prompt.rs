use std::collections::{HashSet, VecDeque};

use caudra_agent::permissions::{
    DEFAULT_DENY_GUIDANCE, PermissionAnswer, PermissionLifetime, PermissionRequest, PermissionRisk,
    PermissionRuleOption, StructuredPermissionEffect,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use serde_json::{Map, Value};

use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::{
    ModalScroll, Overlay, VisualRows, escape_terminal_controls, hint_line_hovered, hover_style,
    is_ctrl, visual_rows,
};
use crate::text_buffer::TextBuffer;
use crate::theme;

const NARROW_WIDTH: u16 = 60;

const KEY_ALLOW_ONCE: &str = "y";
const KEY_ALLOW_SESSION: &str = "s";
const KEY_ALLOW_LOCAL: &str = "a";
const KEY_ALLOW_GLOBAL: &str = "A";
const KEY_GUIDE_DENY: &str = "n";
const KEY_DENY_LOCAL: &str = "d";
const KEY_DENY_GLOBAL: &str = "D";
const KEY_DETAILS: &str = "f";
const KEY_INPUT: &str = "i";
const HINT_ENTER: &str = "Enter";
const HINT_ESC: &str = "Esc";
const HINT_TAB: &str = "Tab";
/// Two keys, one action: the hint names `Enter` first and a click follows it.
const HINT_CONFIRM: &str = "Enter/y";
/// Scrolling is the wheel's job, so this hint is a label rather than a button.
const HINT_SCROLL: &str = "↑/↓";

type HintPairs = &'static [(&'static str, &'static str)];

enum FooterRow {
    Hints(HintPairs),
    /// The deny-guidance editor: an input, not a row of controls.
    Guidance,
}

/// The lines to draw plus where the reusable authorities landed among them,
/// in the order `cycle_option` walks. Both come out of one pass so a click
/// and `Tab` can never disagree about which option is which.
struct PromptBody {
    lines: Vec<Line<'static>>,
    authority_lines: Vec<u16>,
}

/// The options that can be granted beyond this one call.
fn authorities(request: &PermissionRequest) -> impl Iterator<Item = &PermissionRuleOption> {
    request.options.iter().filter(|option| {
        option.rule.effect == StructuredPermissionEffect::Allow
            && option
                .allowed_lifetimes
                .iter()
                .any(|lifetime| *lifetime != PermissionLifetime::Once)
    })
}

/// The key a hint stands for, so a click can press it. A hint listing
/// alternatives names the one to synthesise first.
fn hint_key(label: &str) -> Option<KeyEvent> {
    let code = match label {
        HINT_ESC => KeyCode::Esc,
        HINT_TAB => KeyCode::Tab,
        _ => match label.split('/').next()? {
            HINT_ENTER => KeyCode::Enter,
            first => {
                let mut chars = first.chars();
                let single = chars.next().filter(|c| c.is_ascii_alphanumeric())?;
                chars.next().is_none().then_some(KeyCode::Char(single))?
            }
        },
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysLocal,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysLocal,
    ConfirmDenyAlwaysGlobal,
    DenyEditing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionDecision {
    pub request_id: String,
    pub answer: PermissionAnswer,
}

struct QueuedPermission {
    request: Box<PermissionRequest>,
    requester: Option<String>,
}

pub struct PermissionPrompt {
    requests: VecDeque<QueuedPermission>,
    request_ids: HashSet<String>,
    state: PromptState,
    buffer: TextBuffer,
    scroll: ModalScroll,
    full_details: bool,
    input_expanded: bool,
    selected_option: String,
    row_hits: Vec<PromptHit>,
    mouse_down: Option<PromptTarget>,
    /// What the pointer is resting on, so a control can say it is about to act.
    hover: Option<PromptTarget>,
    /// Where the prompt last drew, so a wheel event can tell whether it landed
    /// on the prompt or on the transcript above it.
    area: Rect,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum PromptTarget {
    /// Select the authority the row shows, by id.
    Authority(String),
    /// Press the key the hint stands for.
    Hint(KeyEvent),
}

struct PromptHit {
    area: Rect,
    target: PromptTarget,
}

/// What a mouse event did, so the app knows whether anything else may still
/// act on it. The prompt is not modal, so a miss has to fall through.
pub(crate) enum PromptMouse {
    Passthrough,
    Consumed,
    Decided(PermissionDecision),
}

impl Overlay for PermissionPrompt {
    fn is_open(&self) -> bool {
        !self.requests.is_empty()
    }

    fn is_modal(&self) -> bool {
        false
    }

    fn close(&mut self) {
        self.requests.clear();
        self.request_ids.clear();
        self.reset_view();
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self {
            requests: VecDeque::new(),
            request_ids: HashSet::new(),
            state: PromptState::Normal,
            buffer: TextBuffer::new(String::new()),
            scroll: ModalScroll::new_top(),
            full_details: false,
            input_expanded: false,
            selected_option: "allow_exact".into(),
            row_hits: Vec::new(),
            mouse_down: None,
            hover: None,
            area: Rect::default(),
        }
    }

    pub fn enqueue(&mut self, request: Box<PermissionRequest>, requester: Option<String>) -> bool {
        if !self.request_ids.insert(request.id.clone()) {
            return false;
        }
        if self.requests.is_empty()
            && let Some(option) = request.options.iter().find(|option| option.is_default)
        {
            self.selected_option = option.id.clone();
        }
        self.requests
            .push_back(QueuedPermission { request, requester });
        true
    }

    #[cfg(test)]
    pub(crate) fn open(
        &mut self,
        id: String,
        tool: caudra_config::ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
    ) {
        self.enqueue(
            Box::new(PermissionRequest::from_legacy(
                id,
                tool,
                scopes,
                Value::Null,
                std::path::Path::new("/project"),
                true,
            )),
            subagent_id,
        );
    }

    pub(crate) fn tool(&self) -> Option<&caudra_config::ToolKey> {
        self.current().map(|request| &request.tool)
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.requests.len()
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.current().map(|request| request.id.as_str())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PermissionDecision> {
        let request_id = self.request_id()?.to_owned();
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            return Some(PermissionDecision {
                request_id,
                answer: PermissionAnswer::Deny,
            });
        }

        if self.state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = self.buffer.value().trim().to_string();
                    Some(PermissionDecision {
                        request_id,
                        answer: if text.is_empty() {
                            PermissionAnswer::Deny
                        } else {
                            PermissionAnswer::DenyWithGuidance(text)
                        },
                    })
                }
                KeyCode::Esc => {
                    self.state = PromptState::Normal;
                    self.buffer = TextBuffer::new(String::new());
                    None
                }
                _ => {
                    self.buffer.handle_key(key);
                    None
                }
            };
        }

        if let Some(answer) = self.confirm_answer() {
            if let Some(phrase) = self.confirmation_phrase() {
                return match key.code {
                    KeyCode::Enter if self.buffer.value().trim() == phrase => {
                        Some(PermissionDecision { request_id, answer })
                    }
                    KeyCode::Esc => {
                        self.state = PromptState::Normal;
                        self.buffer = TextBuffer::new(String::new());
                        self.scroll.reset();
                        None
                    }
                    _ => {
                        self.buffer.handle_key(key);
                        None
                    }
                };
            }
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    Some(PermissionDecision { request_id, answer })
                }
                KeyCode::Esc => {
                    self.state = PromptState::Normal;
                    self.scroll.reset();
                    None
                }
                _ => None,
            };
        }

        if key.code == KeyCode::Esc {
            return Some(PermissionDecision {
                request_id,
                answer: PermissionAnswer::Deny,
            });
        }
        if self.scroll.handle_key(key) {
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('y') => Some(PermissionDecision {
                request_id,
                answer: PermissionAnswer::AllowOnce,
            }),
            KeyCode::Char('n') => {
                self.state = PromptState::DenyEditing;
                None
            }
            KeyCode::Char('a') => {
                self.open_allow_confirmation(
                    PromptState::ConfirmAllowAlwaysLocal,
                    PermissionLifetime::Project,
                );
                None
            }
            KeyCode::Char('A') => {
                self.open_allow_confirmation(
                    PromptState::ConfirmAllowAlwaysGlobal,
                    PermissionLifetime::Global,
                );
                None
            }
            KeyCode::Char('d') => {
                self.open_confirmation(PromptState::ConfirmDenyAlwaysLocal);
                None
            }
            KeyCode::Char('D') => {
                self.open_confirmation(PromptState::ConfirmDenyAlwaysGlobal);
                None
            }
            KeyCode::Char('s') => {
                self.open_allow_confirmation(
                    PromptState::ConfirmAllowSession,
                    PermissionLifetime::Conversation,
                );
                None
            }
            KeyCode::Tab => {
                self.cycle_option(false);
                None
            }
            KeyCode::BackTab => {
                self.cycle_option(true);
                None
            }
            KeyCode::Char('f') => {
                self.full_details = !self.full_details;
                self.scroll.reset();
                None
            }
            KeyCode::Char('i') => {
                self.input_expanded = !self.input_expanded;
                self.scroll.reset();
                None
            }
            _ => None,
        }
    }

    pub fn resolve(&mut self, request_id: &str) -> bool {
        let Some(request) = self.requests.front() else {
            return false;
        };
        if request.request.id != request_id {
            return false;
        }
        let request = self.requests.pop_front().expect("front request exists");
        self.request_ids.remove(&request.request.id);
        self.reset_view();
        true
    }

    pub fn resolve_pending(&mut self, request_id: &str) -> bool {
        let Some(index) = self
            .requests
            .iter()
            .position(|queued| queued.request.id == request_id)
        else {
            return false;
        };
        self.requests.remove(index);
        self.request_ids.remove(request_id);
        if index == 0 {
            self.reset_view();
        }
        true
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if (self.state != PromptState::DenyEditing && self.confirmation_phrase().is_none())
            || !self.is_open()
        {
            return false;
        }
        self.buffer.insert_text(text);
        true
    }

    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.area.contains(pos)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        let Some(request) = self.current() else {
            self.row_hits.clear();
            return;
        };
        let authority_ids: Vec<_> = authorities(request)
            .map(|option| option.id.clone())
            .collect();
        let body = self.body(request);
        let footer_width = area.width.saturating_sub(2);
        let footer_rows = self.footer_rows(footer_width);
        let footer_lines = self.footer_lines(footer_width);
        let title = format!(" Permission Required ({} pending) ", self.pending_count());
        let t = theme::current();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_top(Line::from(title).left_aligned())
            .title_style(t.panel_title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let footer_height = footer_lines.len().min(inner.height as usize) as u16;
        let [body_area, footer_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).areas(inner);
        let body_width = body_area.width.max(1);
        let rows = visual_rows(&body.lines, body_width);
        let total = rows.total;
        self.scroll.update_dimensions(total, body_area.height);
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(body.lines)
                .style(Style::new().fg(t.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            body_area,
        );
        let scrolling = total > body_area.height;
        if scrolling {
            render_vertical_scrollbar(frame, body_area, total, offset);
        }
        frame.render_widget(Paragraph::new(footer_lines), footer_area);

        self.row_hits.clear();
        // The scrollbar sits in the last column and takes its own clicks.
        let clickable = Rect {
            width: body_area.width.saturating_sub(u16::from(scrolling)),
            ..body_area
        };
        for (id, line) in authority_ids.into_iter().zip(body.authority_lines) {
            self.push_body_hit(&rows, clickable, offset, line, PromptTarget::Authority(id));
        }
        for (index, row) in footer_rows.into_iter().enumerate() {
            let FooterRow::Hints(pairs) = row else {
                continue;
            };
            let line = Rect {
                y: footer_area.y + index as u16,
                height: 1,
                ..footer_area
            };
            if line.y >= footer_area.bottom() {
                break;
            }
            for (area, key) in super::hint_hits(pairs, line)
                .into_iter()
                .zip(pairs.iter().map(|(label, _)| hint_key(label)))
            {
                if let Some(key) = key {
                    self.row_hits.push(PromptHit {
                        area,
                        target: PromptTarget::Hint(key),
                    });
                }
            }
        }
    }

    /// The rows one body line occupies once wrapped and scrolled, clipped to
    /// the viewport: a line scrolled out of sight must not stay clickable.
    fn push_body_hit(
        &mut self,
        rows: &VisualRows,
        body: Rect,
        offset: u16,
        line: u16,
        target: PromptTarget,
    ) {
        let Some(top) = rows.row_of(line).checked_sub(offset) else {
            return;
        };
        let height = rows.height_of(line).min(body.height.saturating_sub(top));
        if height == 0 {
            return;
        }
        self.row_hits.push(PromptHit {
            area: Rect {
                y: body.y + top,
                height,
                ..body
            },
            target,
        });
    }

    fn target_at(&self, pos: Position) -> Option<&PromptTarget> {
        self.row_hits
            .iter()
            .find(|hit| hit.area.contains(pos))
            .map(|hit| &hit.target)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> PromptMouse {
        if !self.is_open() {
            return PromptMouse::Passthrough;
        }
        let pos = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = self.target_at(pos).cloned() else {
                    return PromptMouse::Passthrough;
                };
                self.mouse_down = Some(target);
                PromptMouse::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed) = self.mouse_down.take() else {
                    return PromptMouse::Passthrough;
                };
                // A press that drifts off its target is a cancelled click.
                if self.target_at(pos) != Some(&pressed) {
                    return PromptMouse::Consumed;
                }
                match pressed {
                    PromptTarget::Authority(id) => {
                        self.select_authority(id);
                        PromptMouse::Consumed
                    }
                    PromptTarget::Hint(key) => match self.handle_key(key) {
                        Some(decision) => PromptMouse::Decided(decision),
                        None => PromptMouse::Consumed,
                    },
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.mouse_down.is_none() {
                    return PromptMouse::Passthrough;
                }
                PromptMouse::Consumed
            }
            // Hover is read on the next frame, so it is recorded even where
            // the move itself is nothing the prompt needs to act on.
            MouseEventKind::Moved => {
                self.hover = self.target_at(pos).cloned();
                match self.hover {
                    Some(_) => PromptMouse::Consumed,
                    None => PromptMouse::Passthrough,
                }
            }
            _ => PromptMouse::Passthrough,
        }
    }

    pub fn height(&self, width: u16) -> u16 {
        let Some(request) = self.current() else {
            return 0;
        };
        let inner_width = width.saturating_sub(2).max(1);
        let body = visual_rows(&self.body(request).lines, inner_width).total;
        let footer = self.footer_rows(inner_width).len() as u16;
        body.saturating_add(footer).saturating_add(2)
    }

    fn current(&self) -> Option<&PermissionRequest> {
        self.requests.front().map(|queued| queued.request.as_ref())
    }

    fn reset_view(&mut self) {
        self.state = PromptState::Normal;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
        self.full_details = false;
        self.input_expanded = false;
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.selected_option = self
            .current()
            .and_then(|request| {
                request.options.iter().find(|option| {
                    option.is_default && option.rule.effect == StructuredPermissionEffect::Allow
                })
            })
            .map(|option| option.id.clone())
            .unwrap_or_else(|| "allow_exact".into());
    }

    fn open_confirmation(&mut self, state: PromptState) {
        self.state = state;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
    }

    fn open_allow_confirmation(&mut self, state: PromptState, lifetime: PermissionLifetime) {
        if self
            .selected_authority()
            .is_some_and(|option| option.allowed_lifetimes.contains(&lifetime))
        {
            self.open_confirmation(state);
        }
    }

    fn selected_authority(&self) -> Option<&caudra_agent::permissions::PermissionRuleOption> {
        self.current()?.options.iter().find(|option| {
            option.id == self.selected_option
                && option.rule.effect == StructuredPermissionEffect::Allow
        })
    }

    fn cycle_option(&mut self, reverse: bool) {
        let Some(request) = self.current() else {
            return;
        };
        let options: Vec<_> = authorities(request).collect();
        if options.is_empty() {
            return;
        }
        let current = options
            .iter()
            .position(|option| option.id == self.selected_option)
            .unwrap_or(0);
        let next = if reverse {
            current.checked_sub(1).unwrap_or(options.len() - 1)
        } else {
            (current + 1) % options.len()
        };
        self.select_authority(options[next].id.clone());
    }

    fn select_authority(&mut self, id: String) {
        self.selected_option = id;
        // The selected authority grows a description line, so what was under
        // the pointer is no longer what is under it now.
        self.scroll.reset();
    }

    fn confirm_answer(&self) -> Option<PermissionAnswer> {
        match self.state {
            PromptState::ConfirmAllowAlwaysLocal => Some(PermissionAnswer::AllowOption {
                option_id: self.selected_option.clone(),
                lifetime: PermissionLifetime::Project,
            }),
            PromptState::ConfirmAllowAlwaysGlobal => Some(PermissionAnswer::AllowOption {
                option_id: self.selected_option.clone(),
                lifetime: PermissionLifetime::Global,
            }),
            PromptState::ConfirmAllowSession => Some(PermissionAnswer::AllowOption {
                option_id: self.selected_option.clone(),
                lifetime: PermissionLifetime::Conversation,
            }),
            PromptState::ConfirmDenyAlwaysLocal => Some(PermissionAnswer::DenyAlwaysLocal),
            PromptState::ConfirmDenyAlwaysGlobal => Some(PermissionAnswer::DenyAlwaysGlobal),
            PromptState::Normal | PromptState::DenyEditing => None,
        }
    }

    fn confirmation_phrase(&self) -> Option<&str> {
        if !matches!(
            self.state,
            PromptState::ConfirmAllowSession
                | PromptState::ConfirmAllowAlwaysLocal
                | PromptState::ConfirmAllowAlwaysGlobal
        ) {
            return None;
        }
        self.selected_authority()?.confirmation.as_deref()
    }

    fn body(&self, request: &PermissionRequest) -> PromptBody {
        let t = theme::current();
        let label = t.tool_dim;
        let value = Style::new().fg(t.foreground);
        let safe = |text: &str| escape_terminal_controls(text);
        let mut lines = Vec::new();
        let mut authority_lines = Vec::new();
        if let Some(requester) = self
            .requests
            .front()
            .and_then(|queued| queued.requester.as_deref())
        {
            lines.push(field_line(
                "Requester",
                format!("subtask {}", safe(requester)),
                label,
                value,
            ));
        }
        lines.extend([
            field_line("Action", safe(&request.presentation.action), label, value),
            field_line(
                "Risk",
                format!(
                    "{}: {}",
                    risk_name(&request.presentation.risk),
                    safe(&request.presentation.risk_summary)
                ),
                label,
                value,
            ),
            Line::default(),
            Line::from(Span::styled("  Resources", t.panel_title)),
        ]);
        if request.presentation.resources.is_empty() {
            lines.push(Line::from(Span::styled("    none declared", t.tool_dim)));
        } else {
            for covered in [false, true] {
                lines.extend(
                    request
                        .presentation
                        .resources
                        .iter()
                        .filter(|resource| resource.covered == covered)
                        .map(|resource| {
                            let access = resource
                                .access
                                .as_ref()
                                .map(|access| format!("{:?} ", access).to_lowercase())
                                .unwrap_or_default();
                            let kind = format!("{:?}", resource.kind).to_lowercase();
                            let protected = if resource.protected {
                                " [protected]"
                            } else {
                                ""
                            };
                            let summary_style = if resource.covered { t.tool_dim } else { value };
                            let mut spans = vec![
                                Span::styled("    - ", t.tool_dim),
                                Span::styled(format!("{access}{kind}: "), label),
                                Span::styled(
                                    format!("{}{protected}", safe(&resource.summary)),
                                    summary_style,
                                ),
                            ];
                            if resource.covered {
                                spans.push(Span::styled(" [already allowed]", t.tool_dim));
                            }
                            Line::from(spans)
                        }),
                );
            }
        }
        let options: Vec<_> = authorities(request).collect();
        if !options.is_empty() {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Reusable authority", t.panel_title)),
            ]);
            for option in options {
                let selected = option.id == self.selected_option;
                let on =
                    matches!(&self.hover, Some(PromptTarget::Authority(id)) if *id == option.id);
                authority_lines.push(lines.len() as u16);
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "  > " } else { "    " },
                        hover_style(t.status_notice, on),
                    ),
                    Span::styled(
                        safe(&option.label),
                        hover_style(if selected { value } else { label }, on),
                    ),
                    Span::styled(
                        if option.is_default {
                            " [recommended]"
                        } else {
                            ""
                        },
                        hover_style(t.tool_success, on),
                    ),
                ]));
                if selected {
                    lines.push(Line::from(Span::styled(
                        format!("      {}", safe(&option.description)),
                        t.tool_dim,
                    )));
                }
            }
        }
        let masked = masked_json(&request.input);
        lines.extend([
            Line::default(),
            Line::from(Span::styled(
                "  Not sent to tool until approved",
                t.status_notice,
            )),
            Line::default(),
            Line::from(Span::styled(
                "  Input (validated JSON; likely secrets masked)",
                t.panel_title,
            )),
        ]);
        if self.input_expanded {
            lines.extend(masked.lines().map(|line| Line::from(format!("    {line}"))));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {} · {KEY_INPUT} to expand",
                    input_summary(&request.input, masked.len())
                ),
                t.tool_dim,
            )));
        }

        if self.full_details {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Technical details", t.panel_title)),
                field_line("Tool", safe(&request.tool.to_string()), label, value),
                field_line(
                    "Executor",
                    safe(&format!("{:?}", request.executor)),
                    label,
                    value,
                ),
                field_line(
                    "Subject",
                    safe(&format!("{:?}", request.subject)),
                    label,
                    value,
                ),
                field_line("Input SHA-256", safe(&request.input_digest), label, value),
            ]);
        }

        if let Some(authority) = self.confirmation_authority(request) {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Confirm future authority", t.panel_title)),
                Line::from(format!("    {authority}")),
                Line::from(Span::styled(
                    format!("    Action: {}", safe(&request.presentation.action)),
                    t.tool_dim,
                )),
            ]);
            if let Some(phrase) = self.confirmation_phrase() {
                lines.extend([
                    Line::from(Span::styled(
                        format!("    Type {phrase} to confirm."),
                        t.error,
                    )),
                    self.confirmation_input_line(),
                ]);
            }
        }
        PromptBody {
            lines,
            authority_lines,
        }
    }

    /// The footer a row at a time. `footer_lines` draws these and the hit
    /// rects are measured from them, so a click can never land on a hint the
    /// footer is no longer showing.
    fn footer_rows(&self, width: u16) -> Vec<FooterRow> {
        match self.state {
            PromptState::Normal if width < NARROW_WIDTH => vec![
                FooterRow::Hints(&[(KEY_ALLOW_ONCE, "once"), (KEY_ALLOW_SESSION, "convo")]),
                FooterRow::Hints(&[(KEY_ALLOW_LOCAL, "project"), (KEY_ALLOW_GLOBAL, "global")]),
                FooterRow::Hints(&[
                    (KEY_GUIDE_DENY, "guide deny"),
                    (KEY_DENY_LOCAL, "deny project"),
                ]),
                FooterRow::Hints(&[
                    (KEY_DENY_GLOBAL, "deny global"),
                    (HINT_TAB, "authority"),
                    (KEY_INPUT, "input"),
                    (KEY_DETAILS, "details"),
                ]),
            ],
            PromptState::Normal => vec![
                FooterRow::Hints(&[
                    (KEY_ALLOW_ONCE, "Allow once"),
                    (KEY_ALLOW_SESSION, "Selected conversation"),
                ]),
                FooterRow::Hints(&[
                    (KEY_ALLOW_LOCAL, "Selected project"),
                    (KEY_ALLOW_GLOBAL, "Selected global"),
                ]),
                FooterRow::Hints(&[
                    (KEY_GUIDE_DENY, "Guidance"),
                    (KEY_DENY_LOCAL, "Deny project"),
                    (KEY_DENY_GLOBAL, "Deny global"),
                    (HINT_TAB, "Authority"),
                    (KEY_INPUT, "Input"),
                    (KEY_DETAILS, "Details"),
                    (HINT_SCROLL, "Inspect"),
                ]),
            ],
            PromptState::DenyEditing => vec![
                FooterRow::Guidance,
                FooterRow::Hints(&[(HINT_ENTER, "Deny"), (HINT_ESC, "Back")]),
            ],
            PromptState::ConfirmAllowAlwaysLocal
            | PromptState::ConfirmAllowAlwaysGlobal
            | PromptState::ConfirmAllowSession
            | PromptState::ConfirmDenyAlwaysLocal
            | PromptState::ConfirmDenyAlwaysGlobal => {
                if self.confirmation_phrase().is_some() {
                    vec![FooterRow::Hints(&[
                        (HINT_ENTER, "Confirm phrase"),
                        (HINT_ESC, "Back"),
                    ])]
                } else {
                    vec![FooterRow::Hints(&[
                        (HINT_CONFIRM, "Confirm authority"),
                        (HINT_ESC, "Back"),
                    ])]
                }
            }
        }
    }

    fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let hovered = match &self.hover {
            Some(PromptTarget::Hint(key)) => Some(*key),
            _ => None,
        };
        self.footer_rows(width)
            .into_iter()
            .map(|row| match row {
                FooterRow::Hints(pairs) => {
                    // The hover names a key, so the row that advertises it is
                    // the one that marks it and the others are left alone.
                    let index = hovered.and_then(|key| {
                        pairs
                            .iter()
                            .position(|(label, _)| hint_key(label) == Some(key))
                    });
                    hint_line_hovered(pairs, index)
                }
                FooterRow::Guidance => self.guidance_line(),
            })
            .collect()
    }

    fn guidance_line(&self) -> Line<'static> {
        let t = theme::current();
        let text = self.buffer.value();
        let (display, cursor) = if text.is_empty() {
            (DEFAULT_DENY_GUIDANCE.to_string(), 0)
        } else {
            (
                escape_terminal_controls(&text),
                TextBuffer::char_to_byte(&text, self.buffer.x()),
            )
        };
        let cursor = cursor.min(display.len());
        let (before, after) = display.split_at(cursor);
        let mut chars = after.chars();
        let cursor_ch = chars.next().unwrap_or(' ');
        let rest: String = chars.collect();
        Line::from(vec![
            Span::styled("  Guidance ", t.tool_dim),
            Span::raw(before.to_string()),
            Span::styled(cursor_ch.to_string(), Style::new().reversed()),
            Span::raw(rest),
        ])
    }

    fn confirmation_input_line(&self) -> Line<'static> {
        let t = theme::current();
        let text = escape_terminal_controls(&self.buffer.value());
        Line::from(vec![
            Span::styled("    > ", t.tool_dim),
            Span::styled(text, Style::new().fg(t.foreground)),
            Span::styled(" ", Style::new().reversed()),
        ])
    }

    fn confirmation_authority(&self, request: &PermissionRequest) -> Option<String> {
        let selected = || {
            request
                .options
                .iter()
                .find(|option| option.id == self.selected_option)
                .map(|option| escape_terminal_controls(&option.description))
        };
        match self.state {
            PromptState::ConfirmAllowSession
            | PromptState::ConfirmAllowAlwaysLocal
            | PromptState::ConfirmAllowAlwaysGlobal => selected(),
            PromptState::ConfirmDenyAlwaysLocal => {
                Some("Deny this exact action, resources, and input for this project.".into())
            }
            PromptState::ConfirmDenyAlwaysGlobal => {
                Some("Deny this exact action, resources, and input globally.".into())
            }
            PromptState::Normal | PromptState::DenyEditing => None,
        }
    }
}

fn field_line(name: &str, value: String, label_style: Style, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {name:<12}"), label_style),
        Span::styled(value, value_style),
    ])
}

fn risk_name(risk: &PermissionRisk) -> &'static str {
    match risk {
        PermissionRisk::Low => "low",
        PermissionRisk::Medium => "medium",
        PermissionRisk::High => "high",
        PermissionRisk::Critical => "critical",
        PermissionRisk::Unknown => "unknown",
    }
}

/// Describes a collapsed input by shape and size, so the reviewer knows how
/// much the expanded block holds before spending a screen on it.
fn input_summary(input: &Value, bytes: usize) -> String {
    let shape = match input {
        Value::Object(fields) => plural(fields.len(), "field"),
        Value::Array(items) => plural(items.len(), "item"),
        Value::String(_) => "string".into(),
        Value::Number(_) => "number".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Null => "null".into(),
    };
    format!("{shape} · {}", plural(bytes, "byte"))
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn masked_json(input: &Value) -> String {
    serde_json::to_string_pretty(&mask_secrets(input)).unwrap_or_else(|_| "null".into())
}

fn mask_secrets(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if likely_secret_key(key) {
                        Value::String(format!("<redacted:{}>", json_type(value)))
                    } else {
                        mask_secrets(value)
                    };
                    (key.clone(), value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(mask_secrets).collect()),
        Value::String(value) => Value::String(redact_url_query(value)),
        _ => value.clone(),
    }
}

fn redact_url_query(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_owned();
    };
    if !matches!(url.scheme(), "http" | "https") || url.query().is_none() {
        return value.to_owned();
    }
    let query = url
        .query_pairs()
        .map(|(key, _)| format!("{key}=<redacted>"))
        .collect::<Vec<_>>()
        .join("&");
    url.set_query(Some(&query));
    url.to_string()
}

fn likely_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "authorization",
        "cookie",
        "credential",
        "privatekey",
        "accesskey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use caudra_agent::permissions::{PermissionAnswer, PermissionLifetime, PermissionRequest};
    use caudra_config::ToolKey;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use test_case::test_case;

    use crate::components::buffer_text;
    use ratatui::style::Modifier;

    use super::*;

    fn request(id: &str, input: serde_json::Value) -> Box<PermissionRequest> {
        Box::new(PermissionRequest::from_legacy(
            id.into(),
            ToolKey::native("bash"),
            vec!["cargo test".into()],
            input,
            Path::new("/project"),
            true,
        ))
    }

    fn open_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request("id", json!({"command": "cargo test"})), None);
        prompt
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render(prompt: &mut PermissionPrompt, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| prompt.view(frame, frame.area()))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn ctrl_c_and_escape_deny_primary_prompt() {
        for event in [ctrl_c(), key(KeyCode::Esc)] {
            let mut prompt = open_prompt();
            let decision = prompt.handle_key(event).unwrap();
            assert_eq!(decision.request_id, "id");
            assert_eq!(decision.answer, PermissionAnswer::Deny);
        }
    }

    #[test]
    fn escape_returns_from_guidance_and_confirmation() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('t')));
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.buffer.value().is_empty());

        prompt.handle_key(key(KeyCode::Char('s')));
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(prompt.state, PromptState::Normal);
    }

    #[test]
    fn deny_guidance_is_returned_with_request_id() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste("Use a read-only command");
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(decision.request_id, "id");
        assert_eq!(
            decision.answer,
            PermissionAnswer::DenyWithGuidance("Use a read-only command".into())
        );
    }

    #[test_case('s', PermissionAnswer::AllowOption { option_id: "allow_exact".into(), lifetime: PermissionLifetime::Conversation }, PromptState::ConfirmAllowSession, "Allow only these exact arguments and resources." ; "conversation")]
    #[test_case('a', PermissionAnswer::AllowOption { option_id: "allow_exact".into(), lifetime: PermissionLifetime::Project }, PromptState::ConfirmAllowAlwaysLocal, "Allow only these exact arguments and resources." ; "project_allow")]
    #[test_case('A', PermissionAnswer::AllowOption { option_id: "allow_exact".into(), lifetime: PermissionLifetime::Global }, PromptState::ConfirmAllowAlwaysGlobal, "Allow only these exact arguments and resources." ; "global_allow")]
    #[test_case('d', PermissionAnswer::DenyAlwaysLocal, PromptState::ConfirmDenyAlwaysLocal, "Deny this exact action, resources, and input for this project." ; "project_deny")]
    #[test_case('D', PermissionAnswer::DenyAlwaysGlobal, PromptState::ConfirmDenyAlwaysGlobal, "Deny this exact action, resources, and input globally." ; "global_deny")]
    fn every_persistent_decision_requires_confirmation(
        shortcut: char,
        expected: PermissionAnswer,
        state: PromptState,
        authority: &str,
    ) {
        let mut prompt = open_prompt();
        assert_eq!(prompt.handle_key(key(KeyCode::Char(shortcut))), None);
        assert_eq!(prompt.state, state);
        let screen = render(&mut prompt, 100, 24);
        assert!(screen.contains("Confirm future authority"));
        assert!(screen.contains(authority));
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(decision.answer, expected);
        assert_eq!(decision.request_id, "id");
    }

    #[test]
    fn queue_is_fifo_and_deduplicated_by_request_id() {
        let mut prompt = PermissionPrompt::new();
        assert!(prompt.enqueue(request("first", json!({"n": 1})), Some("task-1".into())));
        assert!(prompt.enqueue(request("second", json!({"n": 2})), None));
        assert!(!prompt.enqueue(request("first", json!({"n": 3})), None));
        assert_eq!(prompt.pending_count(), 2);
        assert_eq!(prompt.request_id(), Some("first"));
        assert!(render(&mut prompt, 100, 24).contains("subtask task-1"));

        let first = prompt.handle_key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(first.request_id, "first");
        assert!(prompt.resolve(&first.request_id));
        assert_eq!(prompt.request_id(), Some("second"));
        let second = prompt.handle_key(key(KeyCode::Esc)).unwrap();
        assert_eq!(second.request_id, "second");
    }

    #[test]
    fn resolving_a_covered_request_preserves_unmatched_fifo_order() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request("first", json!({"n": 1})), None);
        prompt.enqueue(request("covered", json!({"n": 2})), None);
        prompt.enqueue(request("third", json!({"n": 3})), None);

        assert!(prompt.resolve_pending("covered"));
        assert_eq!(prompt.pending_count(), 2);
        assert_eq!(prompt.request_id(), Some("first"));
        assert!(prompt.resolve_pending("first"));
        assert_eq!(prompt.request_id(), Some("third"));
        assert!(!prompt.resolve_pending("missing"));
    }

    #[test]
    fn action_and_full_input_render_before_controls() {
        let mut prompt = open_prompt();
        let screen = render(&mut prompt, 100, 24);
        let action = screen.find("Run native tool bash").unwrap();
        let input = screen.find("cargo test").unwrap();
        let controls = screen.find("Allow once").unwrap();
        assert!(action < input && input < controls);
        assert!(screen.contains("Not sent to tool until approved"));
    }

    fn prompt_with_mixed_resource_coverage() -> PermissionPrompt {
        let mut structured = request("coverage", json!({"command": "cargo test"}));
        let resource = structured.presentation.resources[0].clone();
        let mut covered_first = resource.clone();
        covered_first.summary = "covered\u{1b}[31m-first".into();
        covered_first.protected = false;
        covered_first.covered = true;
        let mut uncovered = resource.clone();
        uncovered.summary = "needs-approval".into();
        uncovered.protected = false;
        let mut covered_last = resource;
        covered_last.summary = "covered-last".into();
        covered_last.covered = true;
        structured.presentation.resources = vec![covered_first, uncovered, covered_last];

        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(structured, None);
        prompt
    }

    #[test]
    fn covered_resources_render_after_uncovered_with_marker() {
        let mut prompt = prompt_with_mixed_resource_coverage();
        let screen = render(&mut prompt, 120, 60);
        let uncovered = screen.find("needs-approval").unwrap();
        let covered_first = screen.find(r"covered\u{1b}[31m-first").unwrap();
        let covered_last = screen.find("covered-last").unwrap();

        assert!(uncovered < covered_first && covered_first < covered_last);
        assert_eq!(screen.matches("[already allowed]").count(), 2);
        assert!(screen.contains("execute command: needs-approval"));
        assert!(screen.contains("covered-last [protected] [already allowed]"));
        assert!(!screen.contains('\u{1b}'));
    }

    #[test]
    fn covered_resource_rows_are_dimmed() {
        let prompt = prompt_with_mixed_resource_coverage();
        let body = prompt.body(prompt.current().unwrap());
        let covered = body
            .lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("covered-last"))
            })
            .unwrap();
        let uncovered = body
            .lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("needs-approval"))
            })
            .unwrap();

        assert!(
            covered
                .spans
                .iter()
                .all(|span| span.style == theme::current().tool_dim)
        );
        assert_eq!(
            uncovered.spans[2].style,
            Style::new().fg(theme::current().foreground)
        );
        assert_eq!(uncovered.spans.len(), 3);
    }

    #[test]
    fn long_mcp_input_is_inspectable_by_scrolling() {
        let mut prompt = PermissionPrompt::new();
        let tail = "TAIL-VALUE";
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "mcp".into(),
                ToolKey::parse("github.create_issue").unwrap(),
                vec!["repository".into()],
                json!({"payload": "x".repeat(240), "zz_last": tail}),
                Path::new("/project"),
                true,
            )),
            None,
        );
        prompt.handle_key(key(KeyCode::Char('i')));
        let first = render(&mut prompt, 60, 12);
        assert!(!first.contains(tail));
        for _ in 0..20 {
            prompt.handle_key(key(KeyCode::Down));
        }
        let scrolled = render(&mut prompt, 60, 12);
        assert!(scrolled.contains(tail));
        assert!(scrolled.contains("convo"));
    }

    #[test]
    fn input_is_collapsed_until_requested() {
        let mut prompt = PermissionPrompt::new();
        let secret_free_value = "PAYLOAD-VALUE";
        prompt.enqueue(
            request("collapsed", json!({"command": secret_free_value})),
            None,
        );

        let collapsed = render(&mut prompt, 100, 24);
        assert!(!collapsed.contains(secret_free_value));
        assert!(collapsed.contains("1 field"));
        assert!(collapsed.contains("i to expand"));

        prompt.handle_key(key(KeyCode::Char('i')));
        let expanded = render(&mut prompt, 100, 24);
        assert!(expanded.contains(secret_free_value));
        assert!(!expanded.contains("i to expand"));
    }

    #[test]
    fn collapsing_the_input_shortens_the_prompt() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            request(
                "tall",
                json!({"values": (0..24).map(|value| format!("line-{value}")).collect::<Vec<_>>() }),
            ),
            None,
        );

        let collapsed = prompt.height(100);
        prompt.handle_key(key(KeyCode::Char('i')));

        assert!(prompt.height(100) > collapsed);
    }

    #[test]
    fn resolving_a_request_recollapses_the_input() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(request("first", json!({"command": "one"})), None);
        prompt.enqueue(request("second", json!({"command": "two"})), None);
        prompt.handle_key(key(KeyCode::Char('i')));
        prompt.resolve("first");

        assert!(render(&mut prompt, 100, 24).contains("i to expand"));
    }

    #[test]
    fn decision_controls_remain_visible_at_40_by_10() {
        let mut prompt = open_prompt();
        let screen = render(&mut prompt, 40, 10);
        for control in [
            "once",
            "convo",
            "project",
            "global",
            "guide deny",
            "deny project",
            "deny global",
        ] {
            assert!(screen.contains(control), "missing {control}: {screen}");
        }
    }

    #[test]
    fn prompt_uses_available_height_instead_of_fixed_eighteen_rows() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            request(
                "tall",
                json!({"values": (0..24).map(|value| format!("line-{value}")).collect::<Vec<_>>() }),
            ),
            None,
        );
        assert!(prompt.height(100) > 18);
    }

    #[test]
    fn controls_are_escaped_and_secrets_are_masked_with_types() {
        let mut structured = request(
            "safe",
            json!({
                "api_token": "do-not-show",
                "nested": {"password": 123, "visible": "ok\u{1b}[31m"}
            }),
        );
        structured.presentation.action = "run\u{1b}[2Jdanger".into();
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(structured, None);
        prompt.handle_key(key(KeyCode::Char('i')));
        let screen = render(&mut prompt, 100, 24);
        assert!(!screen.contains('\u{1b}'));
        assert!(!screen.contains("do-not-show"));
        assert!(screen.contains("api_token"));
        assert!(screen.contains("<redacted:string>"));
        assert!(screen.contains("<redacted:number>"));
        assert!(screen.contains("\\u{1b}"));
    }

    #[test]
    fn webfetch_shows_url_authorities_and_masks_query_values() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "webfetch".into(),
                ToolKey::native("webfetch"),
                vec!["https://untraind.com/metronome/?token=secret".into()],
                json!({"url": "https://untraind.com/metronome/?token=secret"}),
                Path::new("/project"),
                false,
            )),
            None,
        );
        let screen = render(&mut prompt, 120, 40);
        for authority in [
            "This exact URL",
            "This page and subpages",
            "Any page on this origin",
            "Any public HTTP(S) URL",
        ] {
            assert!(screen.contains(authority), "missing {authority}: {screen}");
        }
        assert!(!screen.contains("token=secret"));

        for _ in 0..4 {
            prompt.handle_key(key(KeyCode::Tab));
        }
        prompt.handle_key(key(KeyCode::Char('a')));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.handle_paste("ALLOW ANY URL"));
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            decision.answer,
            PermissionAnswer::AllowOption {
                option_id: "allow_any_url".into(),
                lifetime: PermissionLifetime::Project,
            }
        );
    }

    #[test]
    fn broad_mcp_authority_requires_explicit_selection_and_phrase() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "mcp".into(),
                ToolKey::parse("github.create_issue").unwrap(),
                vec!["repository".into()],
                json!({"repository": "caudra", "title": "Issue"}),
                Path::new("/project"),
                true,
            )),
            None,
        );
        let primary = render(&mut prompt, 100, 24);
        assert!(primary.contains("Allow whole MCP tool"));
        prompt.handle_key(key(KeyCode::Tab));
        let selected = render(&mut prompt, 100, 24);
        assert!(selected.contains("Broad: allow this MCP tool"));
        prompt.handle_key(key(KeyCode::Char('s')));
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.handle_paste("ALLOW MCP TOOL"));
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            decision.answer,
            PermissionAnswer::AllowOption {
                option_id: "allow_whole_mcp_tool_conversation".into(),
                lifetime: PermissionLifetime::Conversation,
            }
        );
    }

    #[test]
    fn broad_allow_phrase_does_not_apply_to_exact_deny() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "webfetch".into(),
                ToolKey::native("webfetch"),
                vec!["https://example.com/path".into()],
                json!({"url": "https://example.com/path"}),
                Path::new("/project"),
                false,
            )),
            None,
        );
        for _ in 0..4 {
            prompt.handle_key(key(KeyCode::Tab));
        }

        prompt.handle_key(key(KeyCode::Char('d')));
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(decision.answer, PermissionAnswer::DenyAlwaysLocal);
    }

    /// Wide enough for the three-row footer and tall enough that nothing in
    /// the body is cut off, so hits are not lost to clipping.
    const ROOMY_WIDTH: u16 = 100;
    const ROOMY_HEIGHT: u16 = 60;

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(prompt: &mut PermissionPrompt, area: Rect) -> PromptMouse {
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area))
    }

    fn hit_area(prompt: &PermissionPrompt, target: &PromptTarget) -> Rect {
        prompt
            .row_hits
            .iter()
            .find(|hit| hit.target == *target)
            .unwrap_or_else(|| panic!("{target:?} was not drawn"))
            .area
    }

    fn hint_target(label: &str) -> PromptTarget {
        PromptTarget::Hint(hint_key(label).expect("the hint names a key"))
    }

    /// Four reusable authorities, so there is always one other than the
    /// selected default to click.
    fn prompt_with_authorities() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "webfetch".into(),
                ToolKey::native("webfetch"),
                vec!["https://example.com/path".into()],
                json!({"url": "https://example.com/path"}),
                Path::new("/project"),
                false,
            )),
            None,
        );
        prompt
    }

    fn authority_targets(prompt: &PermissionPrompt) -> Vec<String> {
        prompt
            .row_hits
            .iter()
            .filter_map(|hit| match &hit.target {
                PromptTarget::Authority(id) => Some(id.clone()),
                PromptTarget::Hint(_) => None,
            })
            .collect()
    }

    #[test]
    fn clicking_a_footer_hint_answers_the_prompt() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let area = hit_area(&prompt, &hint_target(KEY_ALLOW_ONCE));
        let PromptMouse::Decided(decision) = click(&mut prompt, area) else {
            panic!("the click answered");
        };
        assert_eq!(decision.answer, PermissionAnswer::AllowOnce);
    }

    #[test_case(KEY_GUIDE_DENY, PromptState::DenyEditing ; "guidance_editor")]
    #[test_case(KEY_DENY_LOCAL, PromptState::ConfirmDenyAlwaysLocal ; "deny_confirmation")]
    fn clicking_a_footer_hint_opens_what_its_key_opens(label: &str, expected: PromptState) {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let area = hit_area(&prompt, &hint_target(label));
        assert!(matches!(click(&mut prompt, area), PromptMouse::Consumed));
        assert_eq!(prompt.state, expected);
    }

    /// The two hints share a row, so a hit rect that is too wide would hand
    /// the second one's clicks to the first.
    #[test]
    fn neighbouring_hints_do_not_share_a_hit() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let once = hit_area(&prompt, &hint_target(KEY_ALLOW_ONCE));
        let session = hit_area(&prompt, &hint_target(KEY_ALLOW_SESSION));
        assert_eq!(once.y, session.y);
        assert_eq!(once.right(), session.x);
    }

    /// A label the prompt does not act on must not become a button.
    #[test]
    fn the_scroll_hint_is_not_clickable() {
        assert!(hint_key(HINT_SCROLL).is_none());
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!prompt.row_hits.iter().any(|hit| {
            matches!(&hit.target, PromptTarget::Hint(k) if !matches!(k.code, KeyCode::Char(c) if c.is_ascii_alphanumeric()) && k.code != KeyCode::Tab && k.code != KeyCode::Esc && k.code != KeyCode::Enter)
        }));
    }

    #[test]
    fn clicking_an_authority_selects_it() {
        let mut prompt = prompt_with_authorities();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let ids = authority_targets(&prompt);
        let other = ids
            .iter()
            .find(|id| **id != prompt.selected_option)
            .expect("more than one authority is offered")
            .clone();
        let area = hit_area(&prompt, &PromptTarget::Authority(other.clone()));
        assert!(matches!(click(&mut prompt, area), PromptMouse::Consumed));
        assert_eq!(prompt.selected_option, other);
    }

    /// Clicking an authority then allowing has to grant the one that was
    /// clicked, not the one `Tab` would have landed on.
    #[test]
    fn a_clicked_authority_is_the_one_granted() {
        let mut prompt = prompt_with_authorities();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let request = prompt.current().expect("a request is queued");
        let wanted = authorities(request)
            .find(|option| option.id != prompt.selected_option && option.confirmation.is_none())
            .expect("an authority that grants without a typed phrase")
            .id
            .clone();
        let row = hit_area(&prompt, &PromptTarget::Authority(wanted.clone()));
        click(&mut prompt, row);
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);

        let allow = hit_area(&prompt, &hint_target(KEY_ALLOW_SESSION));
        let decision = match click(&mut prompt, allow) {
            PromptMouse::Decided(decision) => decision,
            // A reusable grant asks to be confirmed before it is recorded.
            _ => prompt
                .handle_key(key(KeyCode::Enter))
                .expect("the confirmation answered"),
        };
        assert_eq!(
            decision.answer,
            PermissionAnswer::AllowOption {
                option_id: wanted,
                lifetime: PermissionLifetime::Conversation,
            }
        );
    }

    /// The prompt is not modal, so anything it did not draw on stays the
    /// transcript's to handle.
    #[test]
    fn a_click_off_the_prompt_falls_through() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let outside = Rect::new(0, 0, 1, 1);
        assert!(matches!(
            prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), outside)),
            PromptMouse::Passthrough
        ));
    }

    /// A press that drifts off its target is a cancelled click, not a
    /// different one.
    #[test]
    fn a_release_elsewhere_cancels_the_click() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let once = hit_area(&prompt, &hint_target(KEY_ALLOW_ONCE));
        let session = hit_area(&prompt, &hint_target(KEY_ALLOW_SESSION));
        prompt.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), once));
        assert!(matches!(
            prompt.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), session)),
            PromptMouse::Consumed
        ));
        assert!(prompt.is_open(), "no answer was sent");
    }

    /// Short enough that the body has to scroll, so the hits are exercised at
    /// every offset rather than only the unscrolled one.
    const CRAMPED_HEIGHT: u16 = 12;
    const BODY_PROBE_ROWS: usize = 40;
    const EXPECT_ON_GLYPHS: &str = "the hit rect has to sit on the row it claims";

    fn screen_rows(prompt: &mut PermissionPrompt, height: u16) -> Vec<String> {
        let backend = TestBackend::new(ROOMY_WIDTH, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| prompt.view(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..ROOMY_WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn authority_label(prompt: &PermissionPrompt, id: &str) -> String {
        authorities(prompt.current().expect("a request is queued"))
            .find(|option| option.id == id)
            .expect("the id came from the same list")
            .label
            .clone()
    }

    /// Walks the body past its end. A hit placed against the unwrapped or
    /// unscrolled rows drifts off its label, and a row scrolled out of sight
    /// that stayed clickable lands on whatever replaced it.
    #[test]
    fn an_authority_hit_sits_on_its_own_row_at_every_offset() {
        let mut prompt = prompt_with_authorities();
        let mut seen = 0;
        for _ in 0..BODY_PROBE_ROWS {
            let rows = screen_rows(&mut prompt, CRAMPED_HEIGHT);
            for id in authority_targets(&prompt) {
                let area = hit_area(&prompt, &PromptTarget::Authority(id.clone()));
                let drawn = &rows[area.y as usize];
                let label = authority_label(&prompt, &id);
                assert!(
                    drawn.contains(&label),
                    "{EXPECT_ON_GLYPHS}: {label} vs {drawn}"
                );
                seen += 1;
            }
            prompt.scroll(-1);
        }
        assert!(seen > 0, "no authority was ever clickable");
    }

    /// The scrollbar owns the last column, so a row hit must stop short of it.
    #[test]
    fn an_authority_hit_leaves_the_scrollbar_its_column() {
        let mut prompt = prompt_with_authorities();
        let mut narrower_than_full_width = false;
        for _ in 0..BODY_PROBE_ROWS {
            render(&mut prompt, ROOMY_WIDTH, CRAMPED_HEIGHT);
            for id in authority_targets(&prompt) {
                let area = hit_area(&prompt, &PromptTarget::Authority(id));
                assert!(
                    area.right() < ROOMY_WIDTH - 1,
                    "the scrollbar column is taken"
                );
                narrower_than_full_width = true;
            }
            prompt.scroll(-1);
        }
        assert!(narrower_than_full_width, "no authority was ever clickable");
    }

    #[test_case(HINT_ESC, KeyCode::Esc ; "named_esc")]
    #[test_case(HINT_TAB, KeyCode::Tab ; "named_tab")]
    #[test_case(HINT_ENTER, KeyCode::Enter ; "named_enter")]
    #[test_case(HINT_CONFIRM, KeyCode::Enter ; "first_of_two")]
    #[test_case(KEY_ALLOW_GLOBAL, KeyCode::Char('A') ; "case_is_kept")]
    fn hint_key_names_the_key_the_label_shows(label: &str, expected: KeyCode) {
        assert_eq!(
            hint_key(label).expect("the label names a key").code,
            expected
        );
    }
    const EXPECT_MARKED: &str = "the control under the pointer has to be marked";
    const EXPECT_UNMARKED: &str = "nothing else may be marked";

    fn reversed_cells(prompt: &mut PermissionPrompt, height: u16) -> Vec<Position> {
        let backend = TestBackend::new(ROOMY_WIDTH, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| prompt.view(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .flat_map(|y| (0..ROOMY_WIDTH).map(move |x| Position::new(x, y)))
            .filter(|pos| buffer[(pos.x, pos.y)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

    fn move_to(prompt: &mut PermissionPrompt, area: Rect) -> PromptMouse {
        prompt.handle_mouse(mouse(MouseEventKind::Moved, area))
    }

    #[test]
    fn hovering_a_hint_marks_only_that_hint() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(
            reversed_cells(&mut prompt, ROOMY_HEIGHT).is_empty(),
            "{EXPECT_UNMARKED}"
        );

        let area = hit_area(&prompt, &hint_target(KEY_ALLOW_SESSION));
        assert!(matches!(move_to(&mut prompt, area), PromptMouse::Consumed));
        let marked = reversed_cells(&mut prompt, ROOMY_HEIGHT);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        assert!(
            marked.iter().all(|pos| area.contains(*pos)),
            "{EXPECT_UNMARKED}: {marked:?} outside {area:?}"
        );
    }

    #[test]
    fn hovering_an_authority_marks_only_that_row() {
        let mut prompt = prompt_with_authorities();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let other = authority_targets(&prompt)
            .into_iter()
            .find(|id| *id != prompt.selected_option)
            .expect("more than one authority is offered");
        let area = hit_area(&prompt, &PromptTarget::Authority(other));

        move_to(&mut prompt, area);
        let marked = reversed_cells(&mut prompt, ROOMY_HEIGHT);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        assert!(
            marked.iter().all(|pos| area.contains(*pos)),
            "{EXPECT_UNMARKED}: {marked:?} outside {area:?}"
        );
    }

    /// The prompt is not modal, so a move it does not use has to fall
    /// through to the transcript behind it.
    #[test]
    fn a_move_off_every_control_unmarks_them_and_falls_through() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let once = hit_area(&prompt, &hint_target(KEY_ALLOW_ONCE));
        move_to(&mut prompt, once);
        assert!(
            !reversed_cells(&mut prompt, ROOMY_HEIGHT).is_empty(),
            "{EXPECT_MARKED}"
        );

        let outside = Rect::new(0, 0, 1, 1);
        assert!(matches!(
            move_to(&mut prompt, outside),
            PromptMouse::Passthrough
        ));
        assert!(
            reversed_cells(&mut prompt, ROOMY_HEIGHT).is_empty(),
            "{EXPECT_UNMARKED}"
        );
    }

    /// The hover marks what a click would press, or a control lights up and
    /// then ignores the press.
    #[test]
    fn the_marked_hint_is_the_one_a_click_presses() {
        let mut prompt = open_prompt();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let area = hit_area(&prompt, &hint_target(KEY_ALLOW_ONCE));
        move_to(&mut prompt, area);
        assert_eq!(prompt.hover, Some(hint_target(KEY_ALLOW_ONCE)));
        let PromptMouse::Decided(decision) = click(&mut prompt, area) else {
            panic!("the marked hint answered");
        };
        assert_eq!(decision.answer, PermissionAnswer::AllowOnce);
    }
}
