use std::collections::{HashSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::permissions::{
    DEFAULT_DENY_GUIDANCE, PermissionAnswer, PermissionRequest, PermissionRisk,
};
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

const MAX_HEIGHT: u16 = 18;
const NARROW_WIDTH: u16 = 60;
const BROAD_UNAVAILABLE: &str = "Unavailable pending a richer scope chooser.";

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
        }
    }

    pub fn enqueue(&mut self, request: Box<PermissionRequest>, requester: Option<String>) -> bool {
        if !self.request_ids.insert(request.id.clone()) {
            return false;
        }
        self.requests
            .push_back(QueuedPermission { request, requester });
        true
    }

    #[cfg(test)]
    pub(crate) fn open(
        &mut self,
        id: String,
        tool: maki_config::ToolKey,
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

    pub(crate) fn tool(&self) -> Option<&maki_config::ToolKey> {
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
                self.open_confirmation(PromptState::ConfirmAllowAlwaysLocal);
                None
            }
            KeyCode::Char('A') => {
                self.open_confirmation(PromptState::ConfirmAllowAlwaysGlobal);
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
                self.open_confirmation(PromptState::ConfirmAllowSession);
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
        if self.state != PromptState::DenyEditing || !self.is_open() {
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
        body.saturating_add(footer)
            .saturating_add(2)
            .min(MAX_HEIGHT)
    }

    fn current(&self) -> Option<&PermissionRequest> {
        self.requests.front().map(|queued| queued.request.as_ref())
    }

    fn reset_view(&mut self) {
        self.state = PromptState::Normal;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
        self.full_details = false;
    }

    fn open_confirmation(&mut self, state: PromptState) {
        self.state = state;
        self.scroll.reset();
    }

    fn confirm_answer(&self) -> Option<PermissionAnswer> {
        match self.state {
            PromptState::ConfirmAllowAlwaysLocal => Some(PermissionAnswer::AllowAlwaysLocal),
            PromptState::ConfirmAllowAlwaysGlobal => Some(PermissionAnswer::AllowAlwaysGlobal),
            PromptState::ConfirmAllowSession => Some(PermissionAnswer::AllowSession),
            PromptState::ConfirmDenyAlwaysLocal => Some(PermissionAnswer::DenyAlwaysLocal),
            PromptState::ConfirmDenyAlwaysGlobal => Some(PermissionAnswer::DenyAlwaysGlobal),
            PromptState::Normal | PromptState::DenyEditing => None,
        }
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
            for option in request.options.iter().filter(|option| option.broad) {
                lines.extend([
                    Line::default(),
                    Line::from(Span::styled("  Broad MCP authority", t.panel_title)),
                    Line::from(format!("    {}", safe(&option.label))),
                    Line::from(Span::styled(
                        format!("    {} {BROAD_UNAVAILABLE}", safe(&option.description)),
                        t.tool_dim,
                    )),
                ]);
            }
        }

        if let Some(authority) = self.confirmation_authority(request) {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Confirm future authority", t.panel_title)),
                Line::from(format!("    {authority}")),
                Line::from(Span::styled(
                    format!("    Exact action: {}", safe(&request.presentation.action)),
                    t.tool_dim,
                )),
                Line::from(Span::styled(
                    format!("    Exact input SHA-256: {}", safe(&request.input_digest)),
                    t.tool_dim,
                )),
            ]);
        }
        lines
    }

    fn footer_lines(&self, width: u16, request: &PermissionRequest) -> Vec<Line<'static>> {
        match self.state {
            PromptState::Normal if width < NARROW_WIDTH => vec![
                hint_line(&[("y", "once"), ("s", "convo")]),
                hint_line(&[("a", "project"), ("A", "global")]),
                hint_line(&[("n", "guide deny"), ("d", "deny project")]),
                hint_line(&[("D", "deny global"), ("f", "details")]),
            ],
            PromptState::Normal => vec![
                hint_line(&[
                    ("y", option_label(request, "allow_once", "Allow once")),
                    ("s", "Exact conversation".to_string()),
                ]),
                hint_line(&[
                    ("a", "Exact project".to_string()),
                    ("A", "Exact global".to_string()),
                ]),
                hint_line(&[
                    ("n", "Guidance".to_string()),
                    ("d", "Deny project".to_string()),
                    ("D", "Deny global".to_string()),
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
                vec![hint_line(&[
                    ("Enter/y", "Confirm exact authority"),
                    ("Esc", "Back"),
                ])]
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

    fn confirmation_authority(&self, request: &PermissionRequest) -> Option<String> {
        let option = |id: &str| {
            request
                .options
                .iter()
                .find(|option| option.id == id && !option.broad)
                .map(|option| escape_terminal_controls(&option.description))
        };
        match self.state {
            PromptState::ConfirmAllowSession => option("allow_conversation")
                .or_else(|| Some("Allow only this exact call for this conversation.".into())),
            PromptState::ConfirmAllowAlwaysLocal => option("allow_project")
                .or_else(|| Some("Allow only this exact call for this project.".into())),
            PromptState::ConfirmAllowAlwaysGlobal => option("allow_global")
                .or_else(|| Some("Allow only this exact call globally.".into())),
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

fn option_label(request: &PermissionRequest, id: &str, fallback: &str) -> String {
    request
        .options
        .iter()
        .find(|option| option.id == id && !option.broad)
        .map(|option| escape_terminal_controls(&option.label))
        .unwrap_or_else(|| fallback.to_string())
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
        _ => value.clone(),
    }
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

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::permissions::{PermissionAnswer, PermissionRequest};
    use maki_config::ToolKey;
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

    #[test_case('s', PermissionAnswer::AllowSession, PromptState::ConfirmAllowSession, "Remember only this exact call for the conversation." ; "conversation")]
    #[test_case('a', PermissionAnswer::AllowAlwaysLocal, PromptState::ConfirmAllowAlwaysLocal, "Remember only this exact call for this project." ; "project_allow")]
    #[test_case('A', PermissionAnswer::AllowAlwaysGlobal, PromptState::ConfirmAllowAlwaysGlobal, "Remember only this exact call globally." ; "global_allow")]
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
    fn broad_mcp_authority_is_details_only_and_unavailable() {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "mcp".into(),
                ToolKey::parse("github.create_issue").unwrap(),
                vec!["repository".into()],
                json!({"repository": "maki", "title": "Issue"}),
                Path::new("/project"),
                true,
            )),
            None,
        );
        let primary = render(&mut prompt, 100, 24);
        assert!(!primary.contains("Allow whole MCP tool"));

        prompt.handle_key(key(KeyCode::Char('f')));
        let details = render(&mut prompt, 100, 30);
        assert!(details.contains("Broad MCP authority"));
        for _ in 0..10 {
            prompt.handle_key(key(KeyCode::Down));
        }
        let scrolled = render(&mut prompt, 100, 30);
        assert!(scrolled.contains("Unavailable pending"));
    }
}
