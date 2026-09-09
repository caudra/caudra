use std::collections::{HashSet, VecDeque};

use caudra_agent::permissions::{
    COMPOSABLE_SHELL_OPTIONS, DEFAULT_DENY_GUIDANCE, PatternFault, PatternGrade, PermissionAnswer,
    PermissionCaution, PermissionLifetime, PermissionRequest, PermissionRisk, PermissionRowGrant,
    PermissionRuleOption, StructuredPermissionEffect, grade_command_pattern,
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
const HINT_ENTER: &str = "Enter";
const HINT_ESC: &str = "Esc";
/// Two keys, one action: the hint names `Enter` first and a click follows it.
const HINT_CONFIRM: &str = "Enter/y";
/// Arrow hints are labels rather than buttons: an arrow is not a key a click
/// can synthesise from its name.
const HINT_SELECT: &str = "↑/↓";
const HINT_WIDEN: &str = "←/→";
const HINT_WIDEN_ALL: &str = "</>";
const HINT_PAGE: &str = "PgUp/PgDn";
const WIDEN_MARK: &str = " ←/→";
const BADGE_RECOMMENDED: &str = " [recommended]";
const BADGE_WARN: &str = " [outside repo]";
const BADGE_DANGER: &str = " [outside home]";
const BADGE_ASK_FAMILY: &str = " [always-ask family]";
const BADGE_ANY_INVOCATION: &str = " [any invocation]";
const KEY_WIDEN_ALL_IN: char = '<';
const KEY_WIDEN_ALL_OUT: char = '>';
const CHIP_ARROW: &str = " → ";
const CHIP_ONCE: &str = "this call only";
const CHIP_COVERED: &str = "already allowed";
const WRITTEN_MARK: &str = " (typed)";
const COMMANDS_HEADING: &str = "  Commands";
const BLANKET_HEADING: &str = "  Or grant broadly instead";

type HintPairs = Vec<(&'static str, &'static str)>;

enum FooterRow {
    Hints(HintPairs),
    /// The deny-guidance editor: an input, not a row of controls.
    Guidance,
}

/// The lines to draw plus which authority each selectable row stands for and
/// where it landed. Both come out of one pass so a click and the arrow keys can
/// never disagree about which option is which.
struct PromptBody {
    lines: Vec<Line<'static>>,
    entries: Vec<(String, u16)>,
}

/// The options that can be granted beyond this one call and speak for the whole
/// request.
///
/// Per-command rungs are excluded because they are rows of their own, and so
/// are the aggregates those rows reproduce exactly: `<` and `>` land on them.
fn authorities(request: &PermissionRequest) -> impl Iterator<Item = &PermissionRuleOption> {
    let per_command = !command_ladders(request).is_empty();
    request.options.iter().filter(move |option| {
        option.rule.effect == StructuredPermissionEffect::Allow
            && option
                .allowed_lifetimes
                .iter()
                .any(|lifetime| *lifetime != PermissionLifetime::Once)
            && option
                .group
                .as_ref()
                .is_none_or(|group| group.resource.is_none())
            && !(per_command && COMPOSABLE_SHELL_OPTIONS.contains(&option.id.as_str()))
    })
}

/// The rungs offered for each resource, narrowest first, or nothing when the
/// request offers no per-command choice at all.
///
/// A request ladders every resource or none of them, so one empty ladder means
/// the prompt answers as a whole.
fn command_ladders(request: &PermissionRequest) -> Vec<Vec<&PermissionRuleOption>> {
    let mut ladders = vec![Vec::new(); request.resources.len()];
    for option in &request.options {
        if option.rule.effect != StructuredPermissionEffect::Allow {
            continue;
        }
        if let Some(index) = option.group.as_ref().and_then(|group| group.resource)
            && let Some(ladder) = ladders.get_mut(index)
        {
            ladder.push(option);
        }
    }
    if ladders.iter().any(Vec::is_empty) {
        return Vec::new();
    }
    ladders
}

/// Where one command row sits on its ladder, and the pattern typed for it.
///
/// Rung 0 grants nothing beyond this call; then come the offered rungs in
/// order, and last the written pattern once the row has one. The text survives
/// stepping away from it so stepping back finds it again.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct RowChoice {
    rung: usize,
    written: Option<String>,
}

impl RowChoice {
    fn ladder_len(&self, offered: usize) -> usize {
        1 + offered + usize::from(self.written.is_some())
    }

    /// What this row contributes to the answer, or `None` for this call only.
    fn grant(&self, offered: &[&PermissionRuleOption]) -> Option<PermissionRowGrant> {
        match self.rung.checked_sub(1)? {
            rung if rung < offered.len() => {
                Some(PermissionRowGrant::Offered(offered[rung].id.clone()))
            }
            _ => self.written.clone().map(PermissionRowGrant::Written),
        }
    }
}

/// One authority per row, with every ladder collapsed to the rung in use.
struct AuthorityRow {
    /// The rung the row currently stands for.
    chosen: String,
    /// Every rung on the row, narrowest first, so `←`/`→` can walk it.
    rungs: Vec<String>,
}

/// The authorities as rows. Options sharing a group key are one ladder and
/// occupy one row; everything else is a row of its own.
fn authority_rows(request: &PermissionRequest, selected: &str) -> Vec<AuthorityRow> {
    let mut keys: Vec<Option<&str>> = Vec::new();
    let mut rows: Vec<AuthorityRow> = Vec::new();
    for option in authorities(request) {
        let key = option.group.as_ref().map(|group| group.key.as_str());
        let existing = key.and_then(|key| keys.iter().position(|found| *found == Some(key)));
        match existing.map(|index| &mut rows[index]) {
            Some(row) => {
                if option.id == selected {
                    row.chosen.clone_from(&option.id);
                }
                row.rungs.push(option.id.clone());
            }
            None => {
                keys.push(key);
                rows.push(AuthorityRow {
                    chosen: option.id.clone(),
                    rungs: vec![option.id.clone()],
                });
            }
        }
    }
    rows
}

/// The key a hint stands for, so a click can press it. A hint listing
/// alternatives names the one to synthesise first.
fn hint_key(label: &str) -> Option<KeyEvent> {
    let code = match label {
        HINT_ESC => KeyCode::Esc,
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
    /// Writing a pattern for the selected command row.
    PatternEditing,
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
    selected_option: String,
    /// Where each command row sits on its ladder, positional with the
    /// request's resources.
    scopes: Vec<RowChoice>,
    /// An authority the next draw has to bring into view, once the layout it
    /// lands in is known.
    pending_reveal: Option<String>,
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
            selected_option: "allow_exact".into(),
            scopes: Vec::new(),
            pending_reveal: None,
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
        let first = self.requests.is_empty();
        self.requests
            .push_back(QueuedPermission { request, requester });
        if first {
            self.reset_selection();
        }
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

        if self.state == PromptState::PatternEditing {
            match key.code {
                // Inert while the pattern is unusable, so the reason under the
                // field is what answers the keypress.
                KeyCode::Enter => self.commit_written_pattern(),
                KeyCode::Esc => {
                    self.state = PromptState::Normal;
                    self.buffer = TextBuffer::new(String::new());
                }
                _ => {
                    self.buffer.handle_key(key);
                }
            }
            return None;
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
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain && self.steer(key.code) {
            return None;
        }
        if self.scroll.handle_key(key) || !plain {
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
            _ => None,
        }
    }

    /// Arrows steer the row list: `↑`/`↓` pick a row, `←`/`→` walk the ladder
    /// on it, and `<`/`>` walk every command row at once. `Enter` opens the
    /// pattern editor for the selected command. They are claimed before the
    /// scroll sees them, so the page keys are what moves the viewport.
    fn steer(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Up => self.move_selection(true),
            KeyCode::Down => self.move_selection(false),
            KeyCode::Left => self.widen(false),
            KeyCode::Right => self.widen(true),
            KeyCode::Char(KEY_WIDEN_ALL_IN) => self.widen_all(false),
            KeyCode::Char(KEY_WIDEN_ALL_OUT) => self.widen_all(true),
            KeyCode::Enter => return self.open_pattern_editor(),
            _ => return false,
        }
        true
    }

    /// Opens the editor on the selected command row, seeded with the pattern
    /// already written for it, else the rung the request suggested, else the
    /// command itself for the user to cut down.
    fn open_pattern_editor(&mut self) -> bool {
        let Some(row) = self.command_row() else {
            return false;
        };
        let Some(request) = self.current() else {
            return false;
        };
        let seed = self.scopes[row]
            .written
            .clone()
            .or_else(|| {
                command_ladders(request)[row]
                    .get(1)
                    .and_then(|rung| rung.group.as_ref())
                    .map(|group| group.value.clone())
            })
            .or_else(|| {
                request
                    .resources
                    .get(row)
                    .map(|resource| resource.value.clone())
            })
            .unwrap_or_default();
        self.buffer = TextBuffer::new(seed);
        self.buffer.move_end();
        self.state = PromptState::PatternEditing;
        true
    }

    /// Takes the written pattern, but only once it is one this row can be
    /// granted with. The row then stands on it.
    fn commit_written_pattern(&mut self) {
        let Some(row) = self.command_row() else {
            return;
        };
        let pattern = self.buffer.value().trim().to_owned();
        let usable = self
            .current()
            .and_then(|request| request.resources.get(row))
            .is_some_and(|resource| grade_command_pattern(&pattern, &resource.value).is_ok());
        if !usable {
            return;
        }
        let offered = self
            .current()
            .map_or(0, |request| command_ladders(request)[row].len());
        self.scopes[row].written = Some(pattern);
        self.scopes[row].rung = offered + 1;
        self.state = PromptState::Normal;
        self.buffer = TextBuffer::new(String::new());
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
        let editing = matches!(
            self.state,
            PromptState::DenyEditing | PromptState::PatternEditing
        );
        if (!editing && self.confirmation_phrase().is_none()) || !self.is_open() {
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
        let body = self.body(request);
        let footer_width = area.width.saturating_sub(2);
        // The overflow hint joins a row rather than adding one, so the footer's
        // height is known before the body has been measured against it.
        let footer_height = self.footer_rows(footer_width, false).len();
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

        let footer_height = footer_height.min(inner.height as usize) as u16;
        let [body_area, footer_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).areas(inner);
        let body_width = body_area.width.max(1);
        let rows = visual_rows(&body.lines, body_width);
        let total = rows.total;
        let scrolling = total > body_area.height;
        self.scroll.update_dimensions(total, body_area.height);
        self.follow_selection(&rows, &body.entries);
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(body.lines)
                .style(Style::new().fg(t.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            body_area,
        );
        if scrolling {
            render_vertical_scrollbar(frame, body_area, total, offset);
        }
        let footer_rows = self.footer_rows(footer_width, scrolling);
        frame.render_widget(
            Paragraph::new(self.footer_lines(footer_width, scrolling)),
            footer_area,
        );

        self.row_hits.clear();
        // The scrollbar sits in the last column and takes its own clicks.
        let clickable = Rect {
            width: body_area.width.saturating_sub(u16::from(scrolling)),
            ..body_area
        };
        for (id, line) in body.entries {
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
            for (area, key) in super::hint_hits(&pairs, line)
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

    /// Brings a freshly selected authority into view, together with the
    /// description it just grew, now that the wrapped layout says where it is.
    fn follow_selection(&mut self, rows: &VisualRows, entries: &[(String, u16)]) {
        let Some(id) = self.pending_reveal.take() else {
            return;
        };
        let Some((_, line)) = entries.iter().find(|(entry, _)| *entry == id) else {
            return;
        };
        self.scroll.reveal(
            rows.row_of(*line),
            rows.height_of(*line)
                .saturating_add(rows.height_of(line + 1)),
        );
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
        let footer = self.footer_rows(inner_width, false).len() as u16;
        body.saturating_add(footer).saturating_add(2)
    }

    fn current(&self) -> Option<&PermissionRequest> {
        self.requests.front().map(|queued| queued.request.as_ref())
    }

    fn reset_view(&mut self) {
        self.state = PromptState::Normal;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
        self.pending_reveal = None;
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.reset_selection();
    }

    /// Where a fresh prompt starts. Command rows begin pinned to the command
    /// reviewed, except the ones already allowed, which begin granting nothing
    /// so a plain `s` remembers what was actually undecided and no more. The
    /// selection lands on the first row still waiting on an answer.
    fn reset_selection(&mut self) {
        let Some(request) = self.current() else {
            self.scopes.clear();
            self.selected_option = "allow_exact".into();
            return;
        };
        let covered = |row: usize| {
            request
                .presentation
                .resources
                .get(row)
                .is_some_and(|shown| shown.covered())
        };
        let ladders = command_ladders(request);
        let scopes = (0..ladders.len())
            .map(|row| RowChoice {
                rung: usize::from(!covered(row)),
                written: None,
            })
            .collect();
        let default_authority = || {
            request
                .options
                .iter()
                .find(|option| {
                    option.is_default && option.rule.effect == StructuredPermissionEffect::Allow
                })
                .map_or_else(|| "allow_exact".into(), |option| option.id.clone())
        };
        let selected = ladders
            .iter()
            .enumerate()
            .find(|(row, _)| !covered(*row))
            .or_else(|| ladders.iter().enumerate().next())
            .map_or_else(default_authority, |(_, ladder)| {
                ladder[0]
                    .group
                    .as_ref()
                    .map_or_else(|| ladder[0].id.clone(), |group| group.key.clone())
            });
        self.scopes = scopes;
        self.selected_option = selected;
    }

    fn open_confirmation(&mut self, state: PromptState) {
        self.state = state;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
    }

    fn open_allow_confirmation(&mut self, state: PromptState, lifetime: PermissionLifetime) {
        if self.grants_lifetime(&lifetime) {
            self.open_confirmation(state);
        }
    }

    /// Whether the selection can be granted for this long. A composition is as
    /// durable as its least durable granted row, and a composition that grants
    /// nothing is durable at any lifetime because it stores nothing.
    fn grants_lifetime(&self, lifetime: &PermissionLifetime) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if self.command_row().is_none() {
            return self
                .selected_authority()
                .is_some_and(|option| option.allowed_lifetimes.contains(lifetime));
        }
        command_ladders(request)
            .iter()
            .zip(&self.scopes)
            .filter_map(|(offered, choice)| match choice.grant(offered)? {
                PermissionRowGrant::Offered(id) => offered.iter().find(|rung| rung.id == id),
                // A written pattern is stored under the authority of the row's
                // narrowest reusable rung, so that is what gates it.
                PermissionRowGrant::Written(_) => offered.first(),
            })
            .all(|option| option.allowed_lifetimes.contains(lifetime))
    }

    fn selected_authority(&self) -> Option<&PermissionRuleOption> {
        self.current()?.options.iter().find(|option| {
            option.id == self.selected_option
                && option.rule.effect == StructuredPermissionEffect::Allow
        })
    }

    /// Which command row the selection sits on, if any. A command row is keyed
    /// by its ladder's group key, so the key survives widening the row.
    fn command_row(&self) -> Option<usize> {
        command_ladders(self.current()?)
            .iter()
            .position(|ladder| self.row_key(ladder[0]) == self.selected_option)
    }

    /// A command row is one row however many rungs it has, so it is keyed by
    /// its ladder rather than by the rung showing.
    fn row_key(&self, option: &PermissionRuleOption) -> String {
        option
            .group
            .as_ref()
            .filter(|group| group.resource.is_some())
            .map_or_else(|| option.id.clone(), |group| group.key.clone())
    }

    /// Every selectable row in draw order: the commands, then the authorities
    /// that speak for the whole request.
    fn row_keys(&self) -> Vec<String> {
        let Some(request) = self.current() else {
            return Vec::new();
        };
        command_ladders(request)
            .iter()
            .map(|ladder| self.row_key(ladder[0]))
            .chain(
                authority_rows(request, &self.selected_option)
                    .into_iter()
                    .map(|row| row.chosen),
            )
            .collect()
    }

    /// Moves between rows. A ladder counts as one step however many rungs it
    /// has, and a blanket ladder lands on its narrowest.
    fn move_selection(&mut self, reverse: bool) {
        let keys = self.row_keys();
        if keys.is_empty() {
            return;
        }
        let current = keys
            .iter()
            .position(|key| *key == self.selected_option)
            .unwrap_or_default();
        let next = if reverse {
            current.checked_sub(1).unwrap_or(keys.len() - 1)
        } else {
            (current + 1) % keys.len()
        };
        self.select_authority(keys[next].clone());
    }

    /// Walks the ladder the selected row stands for. Ends clamp rather than
    /// wrap: stepping off the widest rung must not quietly land on the
    /// narrowest.
    fn widen(&mut self, forward: bool) {
        if let Some(row) = self.command_row() {
            self.step_command_row(row, forward);
            self.pending_reveal = Some(self.selected_option.clone());
            return;
        }
        let Some(request) = self.current() else {
            return;
        };
        let rows = authority_rows(request, &self.selected_option);
        let Some(row) = rows
            .iter()
            .find(|row| row.rungs.contains(&self.selected_option))
        else {
            return;
        };
        let rung = row
            .rungs
            .iter()
            .position(|rung| *rung == self.selected_option)
            .unwrap_or_default();
        let next = if forward {
            (rung + 1).min(row.rungs.len() - 1)
        } else {
            rung.saturating_sub(1)
        };
        self.select_authority(row.rungs[next].clone());
    }

    /// Walks every command row at once, so the common answer stays two
    /// keystrokes however many commands were batched.
    fn widen_all(&mut self, forward: bool) {
        let Some(request) = self.current() else {
            return;
        };
        for row in 0..command_ladders(request).len() {
            self.step_command_row(row, forward);
        }
    }

    fn step_command_row(&mut self, row: usize, forward: bool) {
        let Some(request) = self.current() else {
            return;
        };
        let Some(offered) = command_ladders(request).get(row).map(Vec::len) else {
            return;
        };
        let Some(choice) = self.scopes.get_mut(row) else {
            return;
        };
        choice.rung = if forward {
            (choice.rung + 1).min(choice.ladder_len(offered) - 1)
        } else {
            choice.rung.saturating_sub(1)
        };
    }

    fn select_authority(&mut self, id: String) {
        // The selected authority grows a description line, so the row may need
        // following once the draw knows where it landed.
        self.pending_reveal = Some(id.clone());
        self.selected_option = id;
    }

    fn confirm_answer(&self) -> Option<PermissionAnswer> {
        let lifetime = match self.state {
            PromptState::ConfirmAllowAlwaysLocal => PermissionLifetime::Project,
            PromptState::ConfirmAllowAlwaysGlobal => PermissionLifetime::Global,
            PromptState::ConfirmAllowSession => PermissionLifetime::Conversation,
            PromptState::ConfirmDenyAlwaysLocal => return Some(PermissionAnswer::DenyAlwaysLocal),
            PromptState::ConfirmDenyAlwaysGlobal => {
                return Some(PermissionAnswer::DenyAlwaysGlobal);
            }
            PromptState::Normal | PromptState::DenyEditing | PromptState::PatternEditing => {
                return None;
            }
        };
        Some(self.allow_answer(lifetime))
    }

    /// The selection as an answer. A selection on a command row answers with
    /// every row's choice, because the call is one call: rows are breadths to
    /// remember, never a way to run part of it.
    fn allow_answer(&self, lifetime: PermissionLifetime) -> PermissionAnswer {
        let Some(request) = self.current().filter(|_| self.command_row().is_some()) else {
            return PermissionAnswer::AllowOption {
                option_id: self.selected_option.clone(),
                lifetime,
            };
        };
        PermissionAnswer::AllowComposed {
            rows: command_ladders(request)
                .iter()
                .zip(&self.scopes)
                .map(|(offered, choice)| choice.grant(offered))
                .collect(),
            lifetime,
        }
    }

    /// The phrase a durable grant has to be typed out for. A composition takes
    /// the gravest phrase any of its rows earned, so one danger-graded pattern
    /// gates the whole answer.
    fn confirmation_phrase(&self) -> Option<&str> {
        if !matches!(
            self.state,
            PromptState::ConfirmAllowSession
                | PromptState::ConfirmAllowAlwaysLocal
                | PromptState::ConfirmAllowAlwaysGlobal
        ) {
            return None;
        }
        if self.command_row().is_none() {
            return self.selected_authority()?.confirmation.as_deref();
        }
        self.written_grades()
            .find_map(|(_, grade)| grade.ok()?.confirmation)
    }

    /// Every written pattern in the answer, graded, in row order.
    fn written_grades(&self) -> impl Iterator<Item = (usize, Result<PatternGrade, PatternFault>)> {
        let request = self.current();
        self.scopes
            .iter()
            .enumerate()
            .filter_map(move |(row, choice)| {
                let pattern = choice.written.as_deref()?;
                let command = &request?.resources.get(row)?.value;
                Some((row, grade_command_pattern(pattern, command)))
            })
    }

    fn body(&self, request: &PermissionRequest) -> PromptBody {
        let t = theme::current();
        let label = t.tool_dim;
        let value = Style::new().fg(t.foreground);
        let safe = |text: &str| escape_terminal_controls(text);
        let mut lines = Vec::new();
        let mut entries = Vec::new();
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
            action_line(request, label, value),
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
        ]);
        let ladders = command_ladders(request);
        if !ladders.is_empty() {
            lines.push(Line::from(Span::styled(COMMANDS_HEADING, t.panel_title)));
            self.command_lines(request, &ladders, &mut lines, &mut entries);
            lines.extend([
                Line::default(),
                Line::from(Span::styled(BLANKET_HEADING, t.panel_title)),
            ]);
            self.authority_lines(request, &mut lines, &mut entries);
            return self.tail(request, PromptBody { lines, entries });
        }
        lines.push(Line::from(Span::styled("  Resources", t.panel_title)));
        if request.presentation.resources.is_empty() {
            lines.push(Line::from(Span::styled("    none declared", t.tool_dim)));
        } else {
            for covered in [false, true] {
                lines.extend(
                    request
                        .presentation
                        .resources
                        .iter()
                        .filter(|resource| resource.covered() == covered)
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
                            let summary_style = if resource.covered() {
                                t.tool_dim
                            } else {
                                value
                            };
                            let mut spans = vec![
                                Span::styled("    - ", t.tool_dim),
                                Span::styled(format!("{access}{kind}: "), label),
                                Span::styled(
                                    format!("{}{protected}", safe(&resource.summary)),
                                    summary_style,
                                ),
                            ];
                            if resource.covered() {
                                spans.push(Span::styled(" [already allowed]", t.tool_dim));
                            }
                            Line::from(spans)
                        }),
                );
            }
        }
        if !authority_rows(request, &self.selected_option).is_empty() {
            lines.extend([
                Line::default(),
                Line::from(Span::styled("  Reusable authority", t.panel_title)),
            ]);
            self.authority_lines(request, &mut lines, &mut entries);
        }
        self.tail(request, PromptBody { lines, entries })
    }

    /// One row per reviewed command: the command verbatim, then the authority
    /// that row contributes. The command text is the review surface, so it
    /// leads and the chip follows it.
    fn command_lines(
        &self,
        request: &PermissionRequest,
        ladders: &[Vec<&PermissionRuleOption>],
        lines: &mut Vec<Line<'static>>,
        entries: &mut Vec<(String, u16)>,
    ) {
        let t = theme::current();
        let value = Style::new().fg(t.foreground);
        let safe = |text: &str| escape_terminal_controls(text);
        for (row, offered) in ladders.iter().enumerate() {
            let key = self.row_key(offered[0]);
            let selected = key == self.selected_option;
            let on = matches!(&self.hover, Some(PromptTarget::Authority(id)) if *id == key);
            let choice = self.scopes.get(row).cloned().unwrap_or_default();
            let covered = request
                .presentation
                .resources
                .get(row)
                .is_some_and(|shown| shown.covered());
            let summary = request
                .presentation
                .resources
                .get(row)
                .map_or_else(String::new, |shown| safe(&shown.summary));
            let (chip, caution) = self.chip(row, offered, &choice, covered);
            entries.push((key, lines.len() as u16));
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "  > " } else { "    " },
                    hover_style(t.status_notice, on),
                ),
                Span::styled(
                    summary,
                    hover_style(if choice.rung == 0 { t.tool_dim } else { value }, on),
                ),
                Span::styled(CHIP_ARROW, hover_style(t.tool_dim, on)),
                Span::styled(chip, hover_style(caution_style(caution, t.tool_dim), on)),
                Span::styled(
                    if selected && choice.ladder_len(offered.len()) > 1 {
                        WIDEN_MARK
                    } else {
                        ""
                    },
                    hover_style(t.status_notice, on),
                ),
                Span::styled(
                    match caution {
                        Some(PermissionCaution::Danger) => BADGE_ANY_INVOCATION,
                        Some(PermissionCaution::Warn) => BADGE_ASK_FAMILY,
                        None => "",
                    },
                    hover_style(caution_style(caution, t.tool_dim), on),
                ),
            ]));
            if selected && self.state == PromptState::PatternEditing {
                lines.push(self.confirmation_input_line());
                lines.push(Line::from(Span::styled(
                    format!("      {}", self.pattern_feedback(row)),
                    match self.pattern_caution(row) {
                        Ok(caution) => caution_style(caution, t.tool_dim),
                        Err(()) => t.error,
                    },
                )));
            }
        }
    }

    /// What a row's current rung is called, and how grave it is.
    fn chip(
        &self,
        row: usize,
        offered: &[&PermissionRuleOption],
        choice: &RowChoice,
        covered: bool,
    ) -> (String, Option<PermissionCaution>) {
        match choice.rung.checked_sub(1) {
            None if covered => (CHIP_COVERED.into(), None),
            None => (CHIP_ONCE.into(), None),
            Some(rung) if rung < offered.len() => (
                offered[rung]
                    .group
                    .as_ref()
                    .map_or_else(|| offered[rung].label.clone(), |group| group.value.clone()),
                None,
            ),
            Some(_) => (
                format!(
                    "{}{WRITTEN_MARK}",
                    escape_terminal_controls(choice.written.as_deref().unwrap_or_default())
                ),
                self.pattern_caution(row).unwrap_or_default(),
            ),
        }
    }

    /// How grave the pattern written for a row is, or `Err` when it is not one
    /// this row can be granted with.
    fn pattern_caution(&self, row: usize) -> Result<Option<PermissionCaution>, ()> {
        self.written_grades()
            .find(|(written, _)| *written == row)
            .map_or(Ok(None), |(_, grade)| {
                grade.map(|grade| grade.caution).map_err(|_| ())
            })
    }

    /// What the editor says about what has been typed so far.
    fn pattern_feedback(&self, row: usize) -> String {
        let pattern = self.buffer.value();
        let Some(command) = self
            .current()
            .and_then(|request| request.resources.get(row))
        else {
            return String::new();
        };
        match grade_command_pattern(pattern.trim(), &command.value) {
            Err(fault) => fault.to_string(),
            Ok(grade) => match grade.caution {
                Some(PermissionCaution::Danger) => "every invocation of this program".to_owned(),
                Some(PermissionCaution::Warn) => "overlaps an always-ask family".to_owned(),
                None => "matches this command".to_owned(),
            },
        }
    }

    /// The authorities that speak for the whole request, one row each.
    fn authority_lines(
        &self,
        request: &PermissionRequest,
        lines: &mut Vec<Line<'static>>,
        entries: &mut Vec<(String, u16)>,
    ) {
        let t = theme::current();
        let label = t.tool_dim;
        let value = Style::new().fg(t.foreground);
        let safe = |text: &str| escape_terminal_controls(text);
        {
            for row in authority_rows(request, &self.selected_option) {
                let Some(option) = request
                    .options
                    .iter()
                    .find(|option| option.id == row.chosen)
                else {
                    continue;
                };
                let selected = option.id == self.selected_option;
                let on =
                    matches!(&self.hover, Some(PromptTarget::Authority(id)) if *id == option.id);
                let ladder = row.rungs.len() > 1;
                entries.push((option.id.clone(), lines.len() as u16));
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "  > " } else { "    " },
                        hover_style(t.status_notice, on),
                    ),
                    Span::styled(
                        safe(&option.label),
                        hover_style(if selected { value } else { label }, on),
                    ),
                    // The rung in use, so widening changes the row and not
                    // only the sentence under it.
                    Span::styled(
                        option
                            .group
                            .as_ref()
                            .map(|group| format!(" {}", safe(&group.value)))
                            .unwrap_or_default(),
                        hover_style(t.tool_dim, on),
                    ),
                    Span::styled(
                        if ladder && selected { WIDEN_MARK } else { "" },
                        hover_style(t.status_notice, on),
                    ),
                    Span::styled(
                        match option.caution {
                            Some(PermissionCaution::Danger) => BADGE_DANGER,
                            Some(PermissionCaution::Warn) => BADGE_WARN,
                            None if option.is_default => BADGE_RECOMMENDED,
                            None => "",
                        },
                        hover_style(caution_style(option.caution, t.tool_success), on),
                    ),
                ]));
                if selected {
                    lines.push(Line::from(Span::styled(
                        format!("      {}", safe(&option.description)),
                        caution_style(option.caution, t.tool_dim),
                    )));
                }
            }
        }
    }

    /// What every prompt ends with: the reassurance, and the confirmation
    /// screen when one is open.
    fn tail(&self, request: &PermissionRequest, body: PromptBody) -> PromptBody {
        let PromptBody { mut lines, entries } = body;
        let t = theme::current();
        let safe = |text: &str| escape_terminal_controls(text);
        lines.extend([
            Line::default(),
            Line::from(Span::styled(
                "  Not sent to tool until approved",
                t.status_notice,
            )),
        ]);

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
        PromptBody { lines, entries }
    }

    /// The footer a row at a time. `footer_lines` draws these and the hit
    /// rects are measured from them, so a click can never land on a hint the
    /// footer is no longer showing.
    fn footer_rows(&self, width: u16, scrolling: bool) -> Vec<FooterRow> {
        match self.state {
            PromptState::Normal => self.normal_footer(width, scrolling),
            PromptState::DenyEditing => vec![
                FooterRow::Guidance,
                FooterRow::Hints(vec![(HINT_ENTER, "Deny"), (HINT_ESC, "Back")]),
            ],
            PromptState::PatternEditing => vec![FooterRow::Hints(vec![
                (HINT_ENTER, "Use pattern"),
                (HINT_ESC, "Back"),
            ])],
            PromptState::ConfirmAllowAlwaysLocal
            | PromptState::ConfirmAllowAlwaysGlobal
            | PromptState::ConfirmAllowSession
            | PromptState::ConfirmDenyAlwaysLocal
            | PromptState::ConfirmDenyAlwaysGlobal => {
                if self.confirmation_phrase().is_some() {
                    vec![FooterRow::Hints(vec![
                        (HINT_ENTER, "Confirm phrase"),
                        (HINT_ESC, "Back"),
                    ])]
                } else {
                    vec![FooterRow::Hints(vec![
                        (HINT_CONFIRM, "Confirm authority"),
                        (HINT_ESC, "Back"),
                    ])]
                }
            }
        }
    }

    /// The controls the prompt is offering right now. Widening is named only
    /// for a selection with somewhere to go, and the page keys only once the
    /// body does not fit, so the footer never advertises a key that does
    /// nothing.
    fn normal_footer(&self, width: u16, scrolling: bool) -> Vec<FooterRow> {
        let narrow = width < NARROW_WIDTH;
        let mut rows = if narrow {
            vec![
                FooterRow::Hints(vec![(KEY_ALLOW_ONCE, "once"), (KEY_ALLOW_SESSION, "convo")]),
                FooterRow::Hints(vec![
                    (KEY_ALLOW_LOCAL, "project"),
                    (KEY_ALLOW_GLOBAL, "global"),
                ]),
                FooterRow::Hints(vec![
                    (KEY_GUIDE_DENY, "guide deny"),
                    (KEY_DENY_LOCAL, "deny project"),
                ]),
                FooterRow::Hints(vec![
                    (KEY_DENY_GLOBAL, "deny global"),
                    (HINT_SELECT, "pick"),
                ]),
            ]
        } else {
            vec![
                FooterRow::Hints(vec![
                    (KEY_ALLOW_ONCE, "Allow once"),
                    (KEY_ALLOW_SESSION, "Selected conversation"),
                ]),
                FooterRow::Hints(vec![
                    (KEY_ALLOW_LOCAL, "Selected project"),
                    (KEY_ALLOW_GLOBAL, "Selected global"),
                ]),
                FooterRow::Hints(vec![
                    (KEY_GUIDE_DENY, "Guidance"),
                    (KEY_DENY_LOCAL, "Deny project"),
                    (KEY_DENY_GLOBAL, "Deny global"),
                ]),
                // Steering has a row of its own so the hints it grows in a
                // per-command prompt cannot push the decision keys off the
                // line they share.
                FooterRow::Hints(vec![(HINT_SELECT, "Select")]),
            ]
        };
        let mut offered: HintPairs = Vec::new();
        if self.can_widen() {
            offered.push((HINT_WIDEN, if narrow { "scope" } else { "Scope" }));
        }
        // The pattern editor is a wide-terminal control: naming it in a narrow
        // footer costs a row the decision keys need more.
        if !narrow && self.command_row().is_some() {
            offered.push((HINT_WIDEN_ALL, "Scope all"));
            offered.push((HINT_ENTER, "Edit pattern"));
        }
        if scrolling {
            offered.push((HINT_PAGE, if narrow { "scroll" } else { "Scroll" }));
        }
        if let Some(FooterRow::Hints(pairs)) = rows.last_mut() {
            pairs.extend(offered);
        }
        rows
    }

    /// Whether the selected row is a ladder with another rung to take.
    fn can_widen(&self) -> bool {
        let Some(request) = self.current() else {
            return false;
        };
        if let Some(row) = self.command_row() {
            return self
                .scopes
                .get(row)
                .is_some_and(|choice| choice.ladder_len(command_ladders(request)[row].len()) > 1);
        }
        authority_rows(request, &self.selected_option)
            .iter()
            .any(|row| row.rungs.contains(&self.selected_option) && row.rungs.len() > 1)
    }

    fn footer_lines(&self, width: u16, scrolling: bool) -> Vec<Line<'static>> {
        let hovered = match &self.hover {
            Some(PromptTarget::Hint(key)) => Some(*key),
            _ => None,
        };
        self.footer_rows(width, scrolling)
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
                    hint_line_hovered(&pairs, index)
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
            PromptState::Normal | PromptState::DenyEditing | PromptState::PatternEditing => None,
        }
    }
}

/// What the tool was asked to do, followed by the inputs its own summary does
/// not already name. The prompt shows no separate input block, so this line is
/// where an argument that would change the meaning of the call has to appear.
fn action_line(request: &PermissionRequest, label: Style, value: Style) -> Line<'static> {
    let action = escape_terminal_controls(&request.presentation.action);
    let args = super::tool_display::compact_args_for(
        &request.tool.to_string(),
        &request.presentation.action,
        Some(&mask_secrets(&request.input)),
        None,
    );
    let mut line = field_line("Action", action, label, value);
    if let Some(args) = args {
        line.push_span(Span::styled(escape_terminal_controls(&args), label));
    }
    line
}

fn field_line(name: &str, value: String, label_style: Style, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {name:<12}"), label_style),
        Span::styled(value, value_style),
    ])
}

/// How far an authority reaches, said in colour. An uncautioned option keeps
/// whatever style its row already called for.
fn caution_style(caution: Option<PermissionCaution>, plain: Style) -> Style {
    let t = theme::current();
    match caution {
        Some(PermissionCaution::Danger) => t.error,
        Some(PermissionCaution::Warn) => t.tool_warning,
        None => plain,
    }
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

    use caudra_agent::permissions::{
        PermissionAnswer, PermissionLifetime, PermissionRequest, ResourceCoverage, RuleOrigin,
    };
    use caudra_config::ToolKey;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use test_case::test_case;

    use crate::components::buffer_text;
    use crate::components::keybindings::key as kb;
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

    const COVERING_PATTERN: &str = "cargo test *";

    fn project_coverage() -> ResourceCoverage {
        ResourceCoverage {
            origin: RuleOrigin::Project,
            authority: COVERING_PATTERN.into(),
        }
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
    fn the_action_carries_its_arguments_before_the_controls() {
        let mut prompt = open_prompt();
        let screen = render(&mut prompt, 100, 24);
        let action = screen.find("Run native tool bash").unwrap();
        let args = screen.find("[command=cargo test]").unwrap();
        let controls = screen.find("Allow once").unwrap();
        assert!(action < args && args < controls);
        assert!(screen.contains("Not sent to tool until approved"));
    }

    /// An argument the action already names would only be said twice.
    #[test]
    fn the_action_does_not_repeat_what_it_already_says() {
        let mut prompt = PermissionPrompt::new();
        let mut duplicated = request("echo", json!({"command": "cargo test"}));
        duplicated.presentation.action = "Run cargo test".into();
        prompt.enqueue(duplicated, None);
        let screen = render(&mut prompt, 100, 24);
        assert!(!screen.contains("command="), "{screen}");
    }

    fn prompt_with_mixed_resource_coverage() -> PermissionPrompt {
        let mut structured = request("coverage", json!({"command": "cargo test"}));
        let resource = structured.presentation.resources[0].clone();
        let mut covered_first = resource.clone();
        covered_first.summary = "covered\u{1b}[31m-first".into();
        covered_first.protected = false;
        covered_first.coverage = Some(project_coverage());
        let mut uncovered = resource.clone();
        uncovered.summary = "needs-approval".into();
        uncovered.protected = false;
        let mut covered_last = resource;
        covered_last.summary = "covered-last".into();
        covered_last.coverage = Some(project_coverage());
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

    /// The page keys move the body; the arrows are the selection's.
    #[test]
    fn the_page_keys_scroll_a_body_that_does_not_fit() {
        let mut prompt = prompt_with_authorities();
        let first = render(&mut prompt, 60, CRAMPED_HEIGHT);
        prompt.handle_key(kb::SCROLL_HALF_DOWN.to_key_event());
        let scrolled = render(&mut prompt, 60, CRAMPED_HEIGHT);
        assert_ne!(first, scrolled, "{EXPECT_SCROLLED}");
        assert!(prompt.scroll.offset() > 0, "{EXPECT_SCROLLED}");
    }

    /// The page hint is a promise about what the keys will do, so it may not
    /// appear on a body that is already whole.
    #[test]
    fn the_page_hint_appears_only_when_the_body_overflows() {
        let mut prompt = prompt_with_authorities();
        assert!(!render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(HINT_PAGE));
        assert!(render(&mut prompt, ROOMY_WIDTH, CRAMPED_HEIGHT).contains(HINT_PAGE));
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

    /// The prompt asks for the room its own contents need rather than a fixed
    /// number of rows.
    #[test]
    fn prompt_height_grows_with_what_it_has_to_show() {
        let one = open_prompt().height(100);
        let many = prompt_with_mixed_resource_coverage().height(100);
        assert!(many > one, "{many} vs {one}");
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
        assert!(screen.contains("api_token=<redacted:string>"));
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
            prompt.handle_key(key(KeyCode::Down));
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
        prompt.handle_key(key(KeyCode::Down));
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
            prompt.handle_key(key(KeyCode::Down));
        }

        prompt.handle_key(key(KeyCode::Char('d')));
        let decision = prompt.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(decision.answer, PermissionAnswer::DenyAlwaysLocal);
    }

    const LADDER_PATH: &str = "/project/src/main.rs";
    const EXPECT_LADDER: &str = "the file request offers a subtree ladder";

    /// A file request, whose reusable authorities include the subtree ladder
    /// the arrows widen along.
    fn prompt_with_a_ladder() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "read".into(),
                ToolKey::native("file_read"),
                vec![LADDER_PATH.into()],
                json!({ "filePath": LADDER_PATH }),
                Path::new("/project"),
                true,
            )),
            None,
        );
        prompt
    }

    fn ladder_rungs(prompt: &PermissionPrompt) -> Vec<String> {
        authority_rows(
            prompt.current().expect("a request is queued"),
            &prompt.selected_option,
        )
        .into_iter()
        .find(|row| row.rungs.len() > 1)
        .expect(EXPECT_LADDER)
        .rungs
    }

    /// What one authority row reads as, without the lines around it.
    fn row_text(prompt: &PermissionPrompt, id: &str) -> String {
        let body = prompt.body(prompt.current().expect("a request is queued"));
        let line = body
            .entries
            .iter()
            .find(|(entry, _)| entry == id)
            .map(|(_, line)| *line)
            .expect("the row is drawn");
        body.lines[line as usize]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Selects the ladder, whichever row it landed on.
    fn select_ladder(prompt: &mut PermissionPrompt) -> Vec<String> {
        let rungs = ladder_rungs(prompt);
        prompt.select_authority(rungs[0].clone());
        rungs
    }

    /// However many rungs a ladder has, it is one row and one step of the
    /// selection: widening is what walks it.
    #[test]
    fn a_ladder_occupies_a_single_row() {
        let mut prompt = prompt_with_a_ladder();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let request = prompt.current().expect("a request is queued");
        let offered = authorities(request).count();
        let drawn = authority_targets(&prompt).len();
        assert!(drawn < offered, "{drawn} rows for {offered} authorities");
        assert_eq!(
            drawn,
            authority_rows(request, &prompt.selected_option).len()
        );
    }

    #[test]
    fn the_arrows_walk_the_ladder_and_stop_at_its_ends() {
        let mut prompt = prompt_with_a_ladder();
        let rungs = select_ladder(&mut prompt);
        let widest = rungs.last().expect(EXPECT_LADDER);
        for _ in 0..rungs.len() {
            prompt.handle_key(key(KeyCode::Right));
        }
        assert_eq!(prompt.selected_option, *widest);
        prompt.handle_key(key(KeyCode::Right));
        assert_eq!(prompt.selected_option, *widest);

        for _ in 0..rungs.len() {
            prompt.handle_key(key(KeyCode::Left));
        }
        assert_eq!(prompt.selected_option, rungs[0]);
        prompt.handle_key(key(KeyCode::Left));
        assert_eq!(prompt.selected_option, rungs[0]);
    }

    /// A widened rung is a choice about this prompt, not a mode: leaving the
    /// row and coming back offers the narrowest reach again.
    #[test]
    fn leaving_a_widened_ladder_returns_it_to_its_narrowest_rung() {
        let mut prompt = prompt_with_a_ladder();
        let rungs = select_ladder(&mut prompt);
        prompt.handle_key(key(KeyCode::Right));
        assert_eq!(prompt.selected_option, rungs[1]);
        prompt.handle_key(key(KeyCode::Down));
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.selected_option, rungs[0]);
    }

    /// The arrows are the selection's, so they must not reach the scroll.
    #[test]
    fn up_and_down_select_rather_than_scroll() {
        let mut prompt = prompt_with_a_ladder();
        render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        let first = prompt.selected_option.clone();
        prompt.handle_key(key(KeyCode::Down));
        assert_ne!(prompt.selected_option, first);
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.selected_option, first);
    }

    /// A selection that walks past the fold has to be followed, or the prompt
    /// grants something the reader cannot see.
    #[test]
    fn selecting_past_the_fold_brings_the_row_into_view() {
        let mut prompt = prompt_with_a_ladder();
        render(&mut prompt, ROOMY_WIDTH, CRAMPED_HEIGHT);
        let rows = authority_rows(
            prompt.current().expect("a request is queued"),
            &prompt.selected_option,
        )
        .len();
        for _ in 0..rows - 1 {
            prompt.handle_key(key(KeyCode::Down));
            render(&mut prompt, ROOMY_WIDTH, CRAMPED_HEIGHT);
            let selected = PromptTarget::Authority(prompt.selected_option.clone());
            hit_area(&prompt, &selected);
        }
        assert!(prompt.scroll.offset() > 0, "{EXPECT_SCROLLED}");
    }

    #[test]
    fn a_widened_rung_is_the_one_granted() {
        let mut prompt = prompt_with_a_ladder();
        let rungs = select_ladder(&mut prompt);
        prompt.handle_key(key(KeyCode::Right));
        prompt.handle_key(key(KeyCode::Char('s')));
        let decision = prompt
            .handle_key(key(KeyCode::Enter))
            .expect("the confirmation answered");
        assert_eq!(
            decision.answer,
            PermissionAnswer::AllowOption {
                option_id: rungs[1].clone(),
                lifetime: PermissionLifetime::Conversation,
            }
        );
    }

    /// Widening is offered where it exists and nowhere else, in the footer and
    /// on the row alike.
    #[test]
    fn the_widen_controls_appear_only_on_a_ladder() {
        let mut prompt = prompt_with_a_ladder();
        select_ladder(&mut prompt);
        let ladder = render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(ladder.contains(HINT_WIDEN), "{ladder}");
        assert!(ladder.contains(WIDEN_MARK.trim_start()), "{ladder}");

        prompt.select_authority("allow_exact".into());
        let exact = render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT);
        assert!(!exact.contains(HINT_WIDEN), "{exact}");
        assert!(!exact.contains(WIDEN_MARK.trim_start()), "{exact}");
    }

    /// Every rung is labelled the same, so the row has to name the reach it
    /// currently stands for or widening looks like it did nothing.
    #[test]
    fn widening_changes_what_the_row_shows() {
        let mut prompt = prompt_with_a_ladder();
        let rungs = select_ladder(&mut prompt);
        assert!(row_text(&prompt, &rungs[0]).contains("/project/src/**"));

        prompt.handle_key(key(KeyCode::Right));
        let widened = row_text(&prompt, &rungs[1]);
        assert!(widened.contains("/project/**"), "{widened}");
        assert!(!widened.contains("/project/src/**"), "{widened}");
    }

    /// A rung that reaches past the home directory has to say so where the
    /// reader is looking, not only in the sentence below it.
    #[test]
    fn the_widest_rung_is_badged_and_coloured() {
        let mut prompt = prompt_with_a_ladder();
        let rungs = select_ladder(&mut prompt);
        prompt.select_authority(rungs.last().expect(EXPECT_LADDER).clone());
        assert!(render(&mut prompt, ROOMY_WIDTH, ROOMY_HEIGHT).contains(BADGE_DANGER));

        let body = prompt.body(prompt.current().expect("a request is queued"));
        let badge = body
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == BADGE_DANGER)
            .expect("the widest rung is badged");
        assert_eq!(badge.style, theme::current().error);
    }

    /// Wide enough for the three-row footer and tall enough that nothing in
    /// the body is cut off, so hits are not lost to clipping.
    const FIRST_COMMAND: &str = "git status --short";
    const SECOND_COMMAND: &str = "cargo test";
    const FIRST_ROW: &str = "command_0";
    const SECOND_ROW: &str = "command_1";
    const WORKDIR: &str = "/project";
    const EXPECT_COMPOSED: &str = "a batched shell prompt answers per command";
    const EXACT_CHIP: &str = "this command";
    const PATTERN_CHIP: &str = "git status *";

    fn prompt_with_commands() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.enqueue(
            Box::new(PermissionRequest::from_legacy(
                "batch".into(),
                ToolKey::native("bash"),
                vec![FIRST_COMMAND.into(), SECOND_COMMAND.into()],
                json!({"command": format!("{FIRST_COMMAND} && {SECOND_COMMAND}")}),
                Path::new("/project"),
                false,
            )),
            None,
        );
        prompt
    }

    /// The text of every row, keyed by the row it stands for.
    fn rows_of(prompt: &PermissionPrompt) -> Vec<String> {
        let body = prompt.body(prompt.current().unwrap());
        body.entries
            .iter()
            .map(|(_, line)| {
                body.lines[*line as usize]
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn row_keys_of(prompt: &PermissionPrompt) -> Vec<String> {
        prompt
            .body(prompt.current().unwrap())
            .entries
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    #[test]
    fn each_command_is_a_row_carrying_the_authority_it_contributes() {
        let prompt = prompt_with_commands();

        assert_eq!(
            row_keys_of(&prompt)[..2],
            [FIRST_ROW.to_owned(), SECOND_ROW.to_owned()]
        );
        assert_eq!(
            rows_of(&prompt)[..2],
            [
                format!("  > {FIRST_COMMAND} in {WORKDIR}{CHIP_ARROW}{EXACT_CHIP}{WIDEN_MARK}"),
                format!("    {SECOND_COMMAND} in {WORKDIR}{CHIP_ARROW}{EXACT_CHIP}"),
            ]
        );
    }

    /// `<` and `>` are what keep the common answer two keystrokes however many
    /// commands were batched.
    #[test]
    fn the_arrows_scope_one_row_and_the_angle_keys_scope_them_all() {
        let mut prompt = prompt_with_commands();
        prompt.handle_key(key(KeyCode::Right));

        assert!(rows_of(&prompt)[0].contains(PATTERN_CHIP));
        assert!(rows_of(&prompt)[1].contains(EXACT_CHIP));

        prompt.handle_key(key(KeyCode::Char(KEY_WIDEN_ALL_IN)));
        prompt.handle_key(key(KeyCode::Char(KEY_WIDEN_ALL_IN)));
        assert!(
            rows_of(&prompt)
                .iter()
                .take(2)
                .all(|row| row.contains(CHIP_ONCE))
        );

        prompt.handle_key(key(KeyCode::Char(KEY_WIDEN_ALL_OUT)));
        assert!(
            rows_of(&prompt)
                .iter()
                .take(2)
                .all(|row| row.contains(EXACT_CHIP))
        );
    }

    /// Both ends hold: stepping off the narrowest rung must not wrap to the
    /// widest, and stepping off the widest must not run the rung past the
    /// ladder, which would make the next step back land on nothing.
    #[test]
    fn a_row_clamps_at_both_ends_of_its_ladder() {
        let mut prompt = prompt_with_commands();
        for _ in 0..3 {
            prompt.handle_key(key(KeyCode::Left));
        }

        assert!(rows_of(&prompt)[0].contains(CHIP_ONCE));
        assert_eq!(
            prompt.allow_answer(PermissionLifetime::Conversation),
            PermissionAnswer::AllowComposed {
                rows: vec![
                    None,
                    Some(PermissionRowGrant::Offered("command_exact_1".into())),
                ],
                lifetime: PermissionLifetime::Conversation,
            }
        );

        for _ in 0..4 {
            prompt.handle_key(key(KeyCode::Right));
        }
        assert!(rows_of(&prompt)[0].contains(PATTERN_CHIP));

        prompt.handle_key(key(KeyCode::Left));
        assert!(rows_of(&prompt)[0].contains(EXACT_CHIP));
    }

    #[test]
    fn the_answer_carries_the_choice_each_row_stands_on() {
        let mut prompt = prompt_with_commands();
        prompt.handle_key(key(KeyCode::Right));

        assert_eq!(
            prompt.allow_answer(PermissionLifetime::Project),
            PermissionAnswer::AllowComposed {
                rows: vec![
                    Some(PermissionRowGrant::Offered("command_pattern_0".into())),
                    Some(PermissionRowGrant::Offered("command_exact_1".into())),
                ],
                lifetime: PermissionLifetime::Project,
            }
        );
    }

    /// The two whole-request authorities `<` and `>` land on exactly are not
    /// worth a row of their own once the rows exist.
    #[test]
    fn the_authorities_the_rows_reproduce_are_not_offered_again() {
        let prompt = prompt_with_commands();
        let keys = row_keys_of(&prompt);

        assert!(
            keys.contains(&"allow_exact".to_owned()),
            "{EXPECT_COMPOSED}"
        );
        assert!(
            COMPOSABLE_SHELL_OPTIONS
                .iter()
                .all(|id| !keys.contains(&(*id).to_owned()))
        );
    }

    #[test]
    fn a_written_pattern_becomes_the_widest_rung_of_its_row() {
        let mut prompt = prompt_with_commands();
        prompt.handle_key(key(KeyCode::Enter));
        for _ in 0..FIRST_COMMAND.len() {
            prompt.handle_key(key(KeyCode::Backspace));
        }
        for character in "git *".chars() {
            prompt.handle_key(key(KeyCode::Char(character)));
        }
        prompt.handle_key(key(KeyCode::Enter));

        assert_eq!(prompt.state, PromptState::Normal);
        assert!(rows_of(&prompt)[0].contains(&format!("git *{WRITTEN_MARK}")));
        assert_eq!(
            prompt.allow_answer(PermissionLifetime::Conversation),
            PermissionAnswer::AllowComposed {
                rows: vec![
                    Some(PermissionRowGrant::Written("git *".into())),
                    Some(PermissionRowGrant::Offered("command_exact_1".into())),
                ],
                lifetime: PermissionLifetime::Conversation,
            }
        );
    }

    /// A pattern for another row's command would be authority laundered
    /// through a prompt about something else, so the editor refuses it and
    /// says why rather than silently doing nothing.
    #[test]
    fn a_pattern_that_misses_its_command_is_refused_with_the_reason() {
        let mut prompt = prompt_with_commands();
        prompt.handle_key(key(KeyCode::Enter));
        for _ in 0..FIRST_COMMAND.len() {
            prompt.handle_key(key(KeyCode::Backspace));
        }
        for character in "cargo *".chars() {
            prompt.handle_key(key(KeyCode::Char(character)));
        }
        prompt.handle_key(key(KeyCode::Enter));

        assert_eq!(prompt.state, PromptState::PatternEditing);
        assert_eq!(
            prompt.pattern_feedback(0),
            PatternFault::DoesNotMatch.to_string()
        );
    }

    /// A pattern naming one program is every invocation of it, so it costs the
    /// same typed phrase as a blanket shell grant.
    #[test]
    fn a_pattern_over_a_whole_program_demands_the_typed_phrase() {
        let mut prompt = prompt_with_commands();
        prompt.scopes[0].written = Some("git *".into());
        prompt.scopes[0].rung = 3;
        prompt.handle_key(key(KeyCode::Char('s')));

        assert_eq!(prompt.state, PromptState::ConfirmAllowSession);
        assert_eq!(
            prompt.confirmation_phrase(),
            Some("ALLOW BROAD SHELL ACCESS")
        );
    }

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

    /// A label the prompt does not act on must not become a button: an arrow
    /// or a page key is a caption, and no click can synthesise it.
    #[test_case(HINT_SELECT ; "select")]
    #[test_case(HINT_WIDEN ; "widen")]
    #[test_case(HINT_PAGE ; "page")]
    fn a_caption_hint_is_not_clickable(label: &str) {
        assert!(hint_key(label).is_none());
        let mut prompt = prompt_with_authorities();
        render(&mut prompt, ROOMY_WIDTH, CRAMPED_HEIGHT);
        assert!(!prompt.row_hits.iter().any(|hit| {
            matches!(&hit.target, PromptTarget::Hint(k) if !matches!(k.code, KeyCode::Char(c) if c.is_ascii_alphanumeric()) && k.code != KeyCode::Esc && k.code != KeyCode::Enter)
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
    const EXPECT_SCROLLED: &str = "the page key has to move the body";
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
