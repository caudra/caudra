use std::collections::{HashSet, VecDeque};

use caudra_agent::permissions::{
    DEFAULT_DENY_GUIDANCE, PermissionAnswer, PermissionLifetime, PermissionRequest, PermissionRisk,
    StructuredPermissionEffect,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use serde_json::{Map, Value};

use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::{ModalScroll, Overlay, escape_terminal_controls, hint_line, is_ctrl};
use crate::text_buffer::TextBuffer;
use crate::theme;

const NARROW_WIDTH: u16 = 60;

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
    selected_option: String,
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
            selected_option: "allow_exact".into(),
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

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if (self.state != PromptState::DenyEditing && self.confirmation_phrase().is_none())
            || !self.is_open()
        {
            return false;
        }
        self.buffer.insert_text(text);
        true
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        let Some(request) = self.current() else {
            return;
        };
        let body_lines = self.body_lines(request);
        let footer_lines = self.footer_lines(area.width.saturating_sub(2), request);
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
        let total = Paragraph::new(body_lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(body_width) as u16;
        self.scroll.update_dimensions(total, body_area.height);
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(body_lines)
                .style(Style::new().fg(t.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            body_area,
        );
        if total > body_area.height {
            render_vertical_scrollbar(frame, body_area, total, offset);
        }
        frame.render_widget(Paragraph::new(footer_lines), footer_area);
    }

    pub fn height(&self, width: u16) -> u16 {
        let Some(request) = self.current() else {
            return 0;
        };
        let inner_width = width.saturating_sub(2).max(1);
        let body = Paragraph::new(self.body_lines(request))
            .wrap(Wrap { trim: false })
            .line_count(inner_width) as u16;
        let footer = self.footer_lines(inner_width, request).len() as u16;
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
        let options: Vec<_> = request
            .options
            .iter()
            .filter(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && option
                        .allowed_lifetimes
                        .iter()
                        .any(|lifetime| *lifetime != PermissionLifetime::Once)
            })
            .collect();
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
        self.selected_option = options[next].id.clone();
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

    fn body_lines(&self, request: &PermissionRequest) -> Vec<Line<'static>> {
        let t = theme::current();
        let label = t.tool_dim;
        let value = Style::new().fg(t.foreground);
        let safe = |text: &str| escape_terminal_controls(text);
        let mut lines = Vec::new();
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
            lines.extend(request.presentation.resources.iter().map(|resource| {
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
                Line::from(vec![
                    Span::styled("    - ", t.tool_dim),
                    Span::styled(format!("{access}{kind}: "), label),
                    Span::styled(format!("{}{protected}", safe(&resource.summary)), value),
                ])
            }));
        }
        let authorities: Vec<_> = request
            .options
            .iter()
            .filter(|option| {
                option.rule.effect == StructuredPermissionEffect::Allow
                    && option
                        .allowed_lifetimes
                        .iter()
                        .any(|lifetime| *lifetime != PermissionLifetime::Once)
            })
            .collect();
        if !authorities.is_empty() {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Reusable authority", t.panel_title)),
            ]);
            for option in authorities {
                let selected = option.id == self.selected_option;
                lines.push(Line::from(vec![
                    Span::styled(if selected { "  > " } else { "    " }, t.status_notice),
                    Span::styled(safe(&option.label), if selected { value } else { label }),
                    Span::styled(
                        if option.is_default {
                            " [recommended]"
                        } else {
                            ""
                        },
                        t.tool_success,
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
        lines.extend(
            masked_json(&request.input)
                .lines()
                .map(|line| Line::from(format!("    {line}"))),
        );

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
        lines
    }

    fn footer_lines(&self, width: u16, _request: &PermissionRequest) -> Vec<Line<'static>> {
        match self.state {
            PromptState::Normal if width < NARROW_WIDTH => vec![
                hint_line(&[("y", "once"), ("s", "convo")]),
                hint_line(&[("a", "project"), ("A", "global")]),
                hint_line(&[("n", "guide deny"), ("d", "deny project")]),
                hint_line(&[("D", "deny global"), ("Tab", "authority"), ("f", "details")]),
            ],
            PromptState::Normal => vec![
                hint_line(&[
                    ("y", "Allow once".to_string()),
                    ("s", "Selected conversation".to_string()),
                ]),
                hint_line(&[
                    ("a", "Selected project".to_string()),
                    ("A", "Selected global".to_string()),
                ]),
                hint_line(&[
                    ("n", "Guidance".to_string()),
                    ("d", "Deny project".to_string()),
                    ("D", "Deny global".to_string()),
                    ("Tab", "Authority".to_string()),
                    ("f", "Details".to_string()),
                    ("↑/↓", "Inspect".to_string()),
                ]),
            ],
            PromptState::DenyEditing => vec![
                self.guidance_line(),
                hint_line(&[("Enter", "Deny"), ("Esc", "Back")]),
            ],
            PromptState::ConfirmAllowAlwaysLocal
            | PromptState::ConfirmAllowAlwaysGlobal
            | PromptState::ConfirmAllowSession
            | PromptState::ConfirmDenyAlwaysLocal
            | PromptState::ConfirmDenyAlwaysGlobal => {
                if self.confirmation_phrase().is_some() {
                    vec![hint_line(&[("Enter", "Confirm phrase"), ("Esc", "Back")])]
                } else {
                    vec![hint_line(&[
                        ("Enter/y", "Confirm authority"),
                        ("Esc", "Back"),
                    ])]
                }
            }
        }
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

    use super::{PermissionPrompt, PromptState};

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
    fn action_and_full_input_render_before_controls() {
        let mut prompt = open_prompt();
        let screen = render(&mut prompt, 100, 24);
        let action = screen.find("Run native tool bash").unwrap();
        let input = screen.find("cargo test").unwrap();
        let controls = screen.find("Allow once").unwrap();
        assert!(action < input && input < controls);
        assert!(screen.contains("Not sent to tool until approved"));
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
}
