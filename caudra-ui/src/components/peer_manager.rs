use caudra_agent::peers::{
    HeldMessageSummary, HeldReview, PeerDecision, PeerDecisionResult, PeerInboxSnapshot,
    PeerReviewToken, PeerSummary,
};
use caudra_config::InboundPolicy;
use caudra_grab::grab_scope;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use jiff::Zoned;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::components::document_view::{DocumentMouse, DocumentView, Painted};
use crate::components::keybindings::key;
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::{Overlay, field_styles, hover_style, input_text_style};
use crate::repaint::Cadence;
use crate::theme;

const TITLE: &str = " Peers ";
const WIDTH_PERCENT: u16 = 90;
const HEIGHT_PERCENT: u16 = 85;
const FOOTER_ROWS: u16 = 2;
const SPLIT_MIN_COLS: u16 = 96;
const LIST_COLS: u16 = 34;
const PANE_GAP: u16 = 2;
const H_PAD: u16 = 1;
const LIST_ROW_HEIGHT: u16 = 2;
const MIN_BODY_ROWS: u16 = 2;
const OPTIONAL_CHROME_ROWS: u16 = 10;
const FILTER_LIMIT: usize = 1024;
const WHEEL_STEP: i32 = 3;
const POLICY_HEADER_ROWS: usize = 4;
const POLICY_OPTION_ROWS: usize = 3;
const POLICY_ACTION_COLS: u16 = 10;
const MAX_LITERAL_COLS: usize = 4096;
const REFRESH_TIME_FORMAT: &str = "%H:%M:%S";
const APPROVAL_WARNING: &str = "Approving exposes this message to your model and may start a billable turn. It does not approve requested tool actions.";
const RELAX_WARNING: &str = "Already held messages may become eligible and start billable model turns. This changes only this session's inbound policy.";
const REJECT_WARNING: &str =
    "Reject permanently deletes this exact held message. There is no rejected-message archive.";
const RESIZE_GUIDANCE: &str =
    "Resize to show the message identity, warning, and body before deciding.";
const NO_LONGER_HELD: &str = "No longer held. Delivery has not been verified.";
const STALE_REVIEW: &str = "Controls changed. Press Enter to review this message again.";
const QUEUED: &str = "Queued for a safe boundary, not delivered. An idle session waits until this manager closes and its existing resume gates allow work.";
const REJECTED: &str = "Rejected. This message was deleted; no archive is kept.";
const POLICY_SAVED: &str =
    "This session's inbound policy was updated. Review held messages again before deciding.";
const FLOOR_BLOCKED: &str = "Disabled by the project inbound floor.";
const UNAVAILABLE: &str = "Unavailable";
const NO_MATCHES: &str = "No search matches.";
const NO_PEERS: &str = "No eligible live peers. Opt in to messaging in another eligible local Caudra session, then refresh.";
const NO_HELD: &str = "No held messages. This is not a message history.";
const SELECT_PEER: &str = "Select a peer to inspect its exact target.";
const SELECT_HELD: &str = "Select a held message, then press Enter to review it.";
const PEER_GONE: &str = "This exact peer target is unavailable in the latest discovery snapshot. Select another peer explicitly.";
const ALIAS_SCOPE: &str = "This exact alias belongs to this session's current live registration. It is not a globally shareable address.";
const PREVIEW_HINT: &str = "Passive preview. Enter or click these details to review the literal message. Selecting and refreshing do not authorize decisions.";
const POLICIES: [InboundPolicy; 4] = [
    InboundPolicy::Auto,
    InboundPolicy::Accept,
    InboundPolicy::Hold,
    InboundPolicy::Refuse,
];
const BINDINGS: [(Command, KeyCode, KeyModifiers, &str, &str); 12] = [
    (
        Command::Sessions,
        KeyCode::Char('1'),
        KeyModifiers::NONE,
        "1",
        " Sessions",
    ),
    (
        Command::Held,
        KeyCode::Char('2'),
        KeyModifiers::NONE,
        "2",
        " Held messages",
    ),
    (
        Command::Filter,
        KeyCode::Char('/'),
        KeyModifiers::NONE,
        "/",
        " Filter",
    ),
    (
        Command::Activate,
        KeyCode::Enter,
        KeyModifiers::NONE,
        "Enter",
        " Details",
    ),
    (
        Command::Focus,
        KeyCode::Tab,
        KeyModifiers::NONE,
        "Tab",
        " Pane",
    ),
    (
        Command::Refresh,
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
        "Ctrl+R",
        " Refresh",
    ),
    (
        Command::CopyTarget,
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
        "Ctrl+B",
        " Copy target",
    ),
    (
        Command::Policy,
        KeyCode::Char('p'),
        KeyModifiers::NONE,
        "p",
        " Policy",
    ),
    (
        Command::Approve,
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        "y",
        " Approve once",
    ),
    (
        Command::Reject,
        KeyCode::Char('n'),
        KeyModifiers::NONE,
        "n",
        " Reject",
    ),
    (
        Command::Apply,
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        "a",
        " Apply",
    ),
    (
        Command::Back,
        KeyCode::Esc,
        KeyModifiers::NONE,
        "Esc",
        " Back",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerView {
    Sessions,
    Held,
}

#[derive(Debug)]
pub enum PeerManagerAction {
    Consumed,
    Close,
    Refresh,
    Review(String),
    Decide {
        token: PeerReviewToken,
        decision: PeerDecision,
    },
    SetInbound(InboundPolicy),
    Copy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Sessions,
    Held,
    Filter,
    Activate,
    Focus,
    Refresh,
    CopyTarget,
    Policy,
    Approve,
    Reject,
    Apply,
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pane {
    List,
    Detail,
}

struct Browser {
    filter: TextField,
    selected: Option<String>,
    offset: usize,
    pane: Pane,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            filter: TextField::new(FieldKind::Line).limited_to(FILTER_LIMIT),
            selected: None,
            offset: 0,
            pane: Pane::List,
        }
    }
}

#[derive(Default)]
struct Reader {
    text: String,
    revision: u64,
    document: DocumentView<(u64, u64)>,
}

impl Reader {
    fn set(&mut self, text: String) {
        if self.text != text {
            self.document.reset();
            self.replace(text);
        }
    }

    fn replace(&mut self, text: String) {
        if self.text != text {
            self.text = text;
            self.revision = self.revision.wrapping_add(1);
            let text = &self.text;
            self.document
                .ensure((self.revision, theme::generation()), 0, || {
                    paint_literal(text, theme::current().item)
                });
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let style = theme::current().item;
        let text = &self.text;
        self.document
            .restyle((self.revision, theme::generation()), || {
                paint_literal(text, style)
            });
        let body = Rect {
            width: area.width.saturating_sub(1),
            height: area.height.saturating_sub(1),
            ..area
        };
        self.document.draw(frame, area, body);
    }
}

struct ReviewPanel {
    summary: HeldMessageSummary,
    token: Option<PeerReviewToken>,
    notice: Option<String>,
    resolved: bool,
}

struct PolicyDraft {
    value: InboundPolicy,
    pending: bool,
}

enum Confirmation {
    Reject {
        message_id: String,
        epoch: u64,
    },
    Relax {
        policy: InboundPolicy,
        previous: InboundPolicy,
        floor: InboundPolicy,
    },
}

#[derive(Default)]
struct FreshInput {
    last: Option<KeyCode>,
    blocked: Option<KeyCode>,
}

impl FreshInput {
    fn barrier(&mut self) {
        self.blocked = self.blocked.or(self.last);
    }

    fn accept(&mut self, key: KeyEvent) -> bool {
        match key.kind {
            KeyEventKind::Release => {
                if self.last == Some(key.code) {
                    self.last = None;
                }
                if self.blocked == Some(key.code) {
                    self.blocked = None;
                }
                false
            }
            KeyEventKind::Repeat => {
                self.last = Some(key.code);
                self.barrier();
                false
            }
            KeyEventKind::Press => {
                self.last = Some(key.code);
                if self.blocked == Some(key.code) {
                    return false;
                }
                self.blocked = None;
                true
            }
        }
    }
}

pub struct PeerManager {
    open: bool,
    generation: u64,
    active: PeerView,
    sessions: Option<Vec<PeerSummary>>,
    discovering: bool,
    discovery_error: Option<String>,
    refreshed_at: Option<String>,
    inbox: Option<PeerInboxSnapshot>,
    session_browser: Browser,
    held_browser: Browser,
    session_reader: Reader,
    held_reader: Reader,
    policy_reader: Reader,
    filter_focused: bool,
    review: Option<ReviewPanel>,
    review_request: Option<(String, u64)>,
    decision_pending: bool,
    policy: Option<PolicyDraft>,
    confirmation: Option<Confirmation>,
    review_rendered: bool,
    confirmation_rendered: bool,
    freshness: FreshInput,
    feedback: Option<String>,
    popup: Rect,
    list_area: Rect,
    detail_area: Rect,
    row_hits: Vec<(Rect, String)>,
    policy_hits: Vec<(Rect, InboundPolicy)>,
    controls: Vec<(Rect, Command)>,
    footer_hits: FooterHits,
    pointer: Option<Position>,
}

impl PeerManager {
    pub fn new() -> Self {
        Self {
            open: false,
            generation: 0,
            active: PeerView::Sessions,
            sessions: None,
            discovering: false,
            discovery_error: None,
            refreshed_at: None,
            inbox: None,
            session_browser: Browser::default(),
            held_browser: Browser::default(),
            session_reader: Reader::default(),
            held_reader: Reader::default(),
            policy_reader: Reader::default(),
            filter_focused: false,
            review: None,
            review_request: None,
            decision_pending: false,
            policy: None,
            confirmation: None,
            review_rendered: false,
            confirmation_rendered: false,
            freshness: FreshInput::default(),
            feedback: None,
            popup: Rect::default(),
            list_area: Rect::default(),
            detail_area: Rect::default(),
            row_hits: Vec::new(),
            policy_hits: Vec::new(),
            controls: Vec::new(),
            footer_hits: FooterHits::default(),
            pointer: None,
        }
    }

    pub fn open(&mut self, view: PeerView) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::new();
        self.generation = generation;
        self.open = true;
        self.active = view;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        if self.open {
            let generation = self.generation.wrapping_add(1);
            *self = Self::new();
            self.generation = generation;
        }
    }

    pub fn contains(&self, position: Position) -> bool {
        self.open && self.popup.contains(position)
    }

    pub fn start_discovery(&mut self) {
        if self.open {
            self.discovering = true;
            self.discovery_error = None;
        }
    }

    pub fn set_sessions(&mut self, result: Result<Vec<PeerSummary>, String>) {
        if !self.open {
            return;
        }
        self.discovering = false;
        match result {
            Ok(sessions) => {
                if self.session_browser.selected.is_none() {
                    self.session_browser.selected =
                        sessions.first().map(|peer| peer.target.clone());
                }
                self.sessions = Some(sessions);
                self.refreshed_at = Some(Zoned::now().strftime(REFRESH_TIME_FORMAT).to_string());
                self.discovery_error = None;
            }
            Err(error) => self.discovery_error = Some(literal(&error, false)),
        }
        self.invalidate_layout();
    }

    pub fn update_inbox(&mut self, snapshot: PeerInboxSnapshot) -> bool {
        if !self.open || self.inbox.as_ref() == Some(&snapshot) {
            return false;
        }
        if self.held_browser.selected.is_none() {
            self.held_browser.selected = snapshot
                .messages
                .first()
                .map(|held| held.message_id.clone());
        }
        let policy_changed = self.inbox.as_ref().is_some_and(|old| {
            old.inbound != snapshot.inbound
                || old.inbound_override != snapshot.inbound_override
                || old.project_floor != snapshot.project_floor
        });
        if let Some(review) = &mut self.review
            && !review.resolved
            && !self.decision_pending
        {
            let current = snapshot
                .messages
                .iter()
                .find(|held| held.message_id == review.summary.message_id);
            match current {
                Some(current) => {
                    if policy_changed || current.epoch != review.summary.epoch {
                        review.token = None;
                        review.notice = Some(STALE_REVIEW.to_owned());
                        self.confirmation = None;
                    }
                    review.summary = current.clone();
                }
                None => {
                    review.token = None;
                    review.notice = Some(NO_LONGER_HELD.to_owned());
                    self.confirmation = None;
                }
            }
        }
        if policy_changed && matches!(self.confirmation, Some(Confirmation::Relax { .. })) {
            self.confirmation = None;
            self.feedback = Some(
                "Policy changed while confirmation was open. Apply the draft again.".to_owned(),
            );
        }
        if self.review_request.as_ref().is_some_and(|(id, epoch)| {
            policy_changed
                || !snapshot
                    .messages
                    .iter()
                    .any(|held| &held.message_id == id && held.epoch == *epoch)
        }) {
            self.review_request = None;
            self.feedback = Some(STALE_REVIEW.to_owned());
        }
        self.inbox = Some(snapshot);
        self.invalidate_layout();
        true
    }

    pub fn set_review(&mut self, result: Result<HeldReview, String>) {
        if !self.open {
            return;
        }
        let Some((message_id, epoch)) = self.review_request.as_ref() else {
            return;
        };
        if let Ok(review) = &result
            && (&review.summary.message_id != message_id || review.summary.epoch != *epoch)
        {
            return;
        }
        let message_id = message_id.clone();
        let epoch = *epoch;
        self.review_request = None;
        match result {
            Ok(review) => {
                if self.held_browser.selected.as_ref() != Some(&message_id)
                    || !self.inbox.as_ref().is_some_and(|inbox| {
                        inbox
                            .messages
                            .iter()
                            .any(|held| held.message_id == message_id && held.epoch == epoch)
                    })
                {
                    self.feedback = Some(STALE_REVIEW.to_owned());
                    return;
                }
                self.held_reader.set(format!(
                    "{}\n\n{}",
                    held_identity(&review.summary),
                    literal(&review.text, true)
                ));
                let summary = self.selected_held().cloned().unwrap_or(review.summary);
                self.review = Some(ReviewPanel {
                    summary,
                    token: Some(review.token),
                    notice: None,
                    resolved: false,
                });
                self.feedback = None;
            }
            Err(error) => self.feedback = Some(literal(&error, false)),
        }
        self.freshness.barrier();
        self.invalidate_layout();
    }

    pub fn finish_decision(&mut self, result: Result<PeerDecisionResult, String>) {
        if !self.open || !self.decision_pending {
            return;
        }
        self.decision_pending = false;
        if let Some(review) = &mut self.review {
            review.token = None;
            review.resolved = true;
            review.notice = Some(match result {
                Ok(PeerDecisionResult::Queued) => QUEUED.to_owned(),
                Ok(PeerDecisionResult::Held { reason }) => format!(
                    "Approved once, but still held: {}. Not queued or delivered.",
                    literal(&reason, false)
                ),
                Ok(PeerDecisionResult::Rejected) => REJECTED.to_owned(),
                Err(error) => format!(
                    "Decision failed: {}. Review again before retrying.",
                    literal(&error, false)
                ),
            });
        }
        self.confirmation = None;
        self.freshness.barrier();
        self.invalidate_layout();
    }

    pub fn finish_policy(&mut self, result: Result<(), String>) {
        if !self.open || !self.policy.as_ref().is_some_and(|policy| policy.pending) {
            return;
        }
        match result {
            Ok(()) => {
                self.policy = None;
                self.feedback = Some(POLICY_SAVED.to_owned());
            }
            Err(error) => {
                if let Some(policy) = &mut self.policy {
                    policy.pending = false;
                }
                self.feedback = Some(literal(&error, false));
            }
        }
        self.freshness.barrier();
        self.invalidate_layout();
    }

    pub fn set_error(&mut self, error: String) {
        if self.open {
            self.feedback = Some(literal(&error, false));
            self.invalidate_layout();
        }
    }

    fn browser(&self) -> &Browser {
        match self.active {
            PeerView::Sessions => &self.session_browser,
            PeerView::Held => &self.held_browser,
        }
    }

    fn browser_mut(&mut self) -> &mut Browser {
        match self.active {
            PeerView::Sessions => &mut self.session_browser,
            PeerView::Held => &mut self.held_browser,
        }
    }

    fn reader_mut(&mut self) -> &mut Reader {
        if self.policy.is_some() {
            return &mut self.policy_reader;
        }
        match self.active {
            PeerView::Sessions => &mut self.session_reader,
            PeerView::Held => &mut self.held_reader,
        }
    }

    pub(crate) fn invalidate_layout(&mut self) {
        self.review_rendered = false;
        self.confirmation_rendered = false;
        self.controls.clear();
        self.footer_hits.clear();
        self.row_hits.clear();
        self.policy_hits.clear();
        self.list_area = Rect::default();
        self.detail_area = Rect::default();
    }

    fn selected_peer(&self) -> Option<&PeerSummary> {
        let selected = self.session_browser.selected.as_ref()?;
        self.sessions
            .as_ref()?
            .iter()
            .find(|peer| &peer.target == selected)
    }

    fn selected_held(&self) -> Option<&HeldMessageSummary> {
        let selected = self.held_browser.selected.as_ref()?;
        self.inbox
            .as_ref()?
            .messages
            .iter()
            .find(|held| &held.message_id == selected)
    }

    fn entries(&self) -> Vec<(String, String, String)> {
        let query = self.browser().filter.text().to_lowercase();
        let entries = match self.active {
            PeerView::Sessions => self
                .sessions
                .iter()
                .flatten()
                .map(|peer| {
                    let detail = format!(
                        "{} · {} · {}",
                        activity(peer),
                        policy_label(&peer.inbound),
                        literal(&peer.cwd.to_string_lossy(), false)
                    );
                    (peer.target.clone(), literal(&peer.name, false), detail)
                })
                .collect::<Vec<_>>(),
            PeerView::Held => self
                .inbox
                .iter()
                .flat_map(|inbox| &inbox.messages)
                .map(|held| {
                    (
                        held.message_id.clone(),
                        literal(&held.sender_name, false),
                        format!(
                            "{} · {}",
                            literal(&held.message_id, false),
                            literal(&held.reason, false)
                        ),
                    )
                })
                .collect::<Vec<_>>(),
        };
        entries
            .into_iter()
            .filter(|(id, name, detail)| {
                query.is_empty()
                    || format!("{id} {name} {detail}")
                        .to_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    fn select(&mut self, selected: String) {
        if self.browser().selected.as_ref() == Some(&selected) {
            return;
        }
        self.browser_mut().selected = Some(selected);
        self.review_request = None;
        if self.active == PeerView::Held {
            if let Some(review) = &mut self.review {
                review.token = None;
            }
            if !self.review.as_ref().is_some_and(|review| review.resolved) && !self.decision_pending
            {
                self.review = None;
                self.held_reader = Reader::default();
            }
        } else {
            self.session_reader = Reader::default();
        }
        self.freshness.barrier();
        self.invalidate_layout();
    }

    fn step(&mut self, delta: isize) {
        let entries = self.entries();
        if entries.is_empty() {
            return;
        }
        let selected = self.browser().selected.as_ref();
        let current = entries.iter().position(|(id, _, _)| Some(id) == selected);
        let index = current.map_or(0, |index| {
            index.saturating_add_signed(delta).min(entries.len() - 1)
        });
        self.select(entries[index].0.clone());
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> PeerManagerAction {
        let fresh = self.freshness.accept(event);
        if !self.open || event.kind == KeyEventKind::Release {
            return PeerManagerAction::Consumed;
        }
        if self.filter_focused {
            if event.kind == KeyEventKind::Repeat
                && matches!(event.code, KeyCode::Esc | KeyCode::Enter)
            {
                return PeerManagerAction::Consumed;
            }
            return self.filter_key(event);
        }
        if !fresh
            && !matches!(
                event.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return PeerManagerAction::Consumed;
        }
        if let Some((command, _, _, _, _)) = BINDINGS
            .iter()
            .find(|(_, code, modifiers, _, _)| *code == event.code && *modifiers == event.modifiers)
        {
            return self.command(command.clone());
        }
        if self.confirmation.is_some() {
            return PeerManagerAction::Consumed;
        }
        if key::QUIT.matches(event) {
            return self
                .reader_mut()
                .document
                .selected_text()
                .map_or(PeerManagerAction::Consumed, PeerManagerAction::Copy);
        }
        if key::SELECT_ALL.matches(event)
            && (self.policy.is_some() || self.browser().pane == Pane::Detail)
        {
            self.reader_mut().document.select_all();
            return PeerManagerAction::Consumed;
        }
        if self.policy.is_some() {
            match event.code {
                KeyCode::Up => self.step_policy(-1),
                KeyCode::Down => self.step_policy(1),
                _ => {
                    self.policy_reader.document.handle_scroll_key(event);
                }
            }
        } else if event.code == KeyCode::BackTab {
            return self.command(Command::Focus);
        } else if self.browser().pane == Pane::List {
            let page = (self.list_area.height / LIST_ROW_HEIGHT).max(1) as isize;
            match event.code {
                KeyCode::Up => self.step(-1),
                KeyCode::Down => self.step(1),
                KeyCode::PageUp => self.step(-page),
                KeyCode::PageDown => self.step(page),
                KeyCode::Home => self.step(isize::MIN),
                KeyCode::End => self.step(isize::MAX),
                KeyCode::Right => {
                    return self.command(Command::Focus);
                }
                _ => {}
            }
        } else {
            self.reader_mut().document.handle_scroll_key(event);
        }
        PeerManagerAction::Consumed
    }

    fn filter_key(&mut self, event: KeyEvent) -> PeerManagerAction {
        match event.code {
            KeyCode::Esc => {
                self.browser_mut().filter.clear();
                self.filter_focused = false;
            }
            KeyCode::Enter => self.filter_focused = false,
            _ => {
                let event = match event.code {
                    KeyCode::Char(character) if deceptive(character) => {
                        return PeerManagerAction::Consumed;
                    }
                    _ => event,
                };
                if let TextKey::Copy(text) | TextKey::Cut(text) =
                    self.browser_mut().filter.handle_key(event)
                {
                    self.invalidate_layout();
                    return PeerManagerAction::Copy(text);
                }
            }
        }
        self.browser_mut().offset = 0;
        self.invalidate_layout();
        PeerManagerAction::Consumed
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open {
            return false;
        }
        if self.filter_focused {
            self.browser_mut().filter.paste(&literal(text, false));
            self.browser_mut().offset = 0;
            self.invalidate_layout();
        }
        true
    }

    fn command(&mut self, command: Command) -> PeerManagerAction {
        if self.filter_focused {
            match command {
                Command::Activate => self.filter_focused = false,
                Command::Back => {
                    self.browser_mut().filter.clear();
                    self.filter_focused = false;
                }
                _ => return PeerManagerAction::Consumed,
            }
            self.invalidate_layout();
            return PeerManagerAction::Consumed;
        }
        if self.confirmation.is_some() {
            return match command {
                Command::Back => {
                    self.confirmation = None;
                    self.freshness.barrier();
                    self.invalidate_layout();
                    PeerManagerAction::Consumed
                }
                Command::Activate => self.confirm(),
                _ => PeerManagerAction::Consumed,
            };
        }
        if self.policy.is_some() {
            return match command {
                Command::Back if !self.policy.as_ref().is_some_and(|policy| policy.pending) => {
                    self.policy = None;
                    self.invalidate_layout();
                    PeerManagerAction::Consumed
                }
                Command::Apply => self.apply_policy(),
                _ => PeerManagerAction::Consumed,
            };
        }
        match command {
            Command::Sessions | Command::Held => {
                self.active = if command == Command::Sessions {
                    PeerView::Sessions
                } else {
                    PeerView::Held
                };
                self.filter_focused = false;
                self.review_request = None;
                self.freshness.barrier();
                self.invalidate_layout();
                if self.active == PeerView::Sessions && self.sessions.is_none() && !self.discovering
                {
                    return PeerManagerAction::Refresh;
                }
            }
            Command::Filter => {
                self.filter_focused = true;
                self.browser_mut().pane = Pane::List;
                self.invalidate_layout();
            }
            Command::Focus => {
                self.browser_mut().pane = match self.browser().pane {
                    Pane::List => Pane::Detail,
                    Pane::Detail => Pane::List,
                };
                self.invalidate_layout();
            }
            Command::Activate => return self.activate(),
            Command::Refresh => return PeerManagerAction::Refresh,
            Command::CopyTarget => {
                let target = match self.active {
                    PeerView::Sessions => self.selected_peer().map(|peer| peer.target.clone()),
                    PeerView::Held => self
                        .review
                        .as_ref()
                        .map(|review| review.summary.reply_target.clone())
                        .or_else(|| self.selected_held().map(|held| held.reply_target.clone())),
                };
                return target.map_or(PeerManagerAction::Consumed, PeerManagerAction::Copy);
            }
            Command::Policy => {
                if let Some(inbox) = &self.inbox {
                    self.policy = Some(PolicyDraft {
                        value: inbox.inbound.clone(),
                        pending: false,
                    });
                    self.policy_reader = Reader::default();
                    self.feedback = None;
                    self.invalidate_layout();
                }
            }
            Command::Approve => {
                self.freshness.barrier();
                if self.can_decide(true) {
                    return self.decide(PeerDecision::Approve);
                }
            }
            Command::Reject => {
                self.freshness.barrier();
                if self.can_decide(false)
                    && let Some(review) = &self.review
                {
                    self.confirmation = Some(Confirmation::Reject {
                        message_id: review.summary.message_id.clone(),
                        epoch: review.summary.epoch,
                    });
                    self.invalidate_layout();
                }
            }
            Command::Back => {
                if self.browser().pane == Pane::Detail {
                    self.browser_mut().pane = Pane::List;
                    self.invalidate_layout();
                } else if !self.browser().filter.is_empty() {
                    self.browser_mut().filter.clear();
                    self.invalidate_layout();
                } else {
                    self.close();
                    return PeerManagerAction::Close;
                }
            }
            Command::Apply => {}
        }
        PeerManagerAction::Consumed
    }

    fn activate(&mut self) -> PeerManagerAction {
        if !self
            .entries()
            .iter()
            .any(|(id, _, _)| self.browser().selected.as_ref() == Some(id))
        {
            return PeerManagerAction::Consumed;
        }
        self.browser_mut().pane = Pane::Detail;
        if self.active == PeerView::Sessions {
            self.invalidate_layout();
            return PeerManagerAction::Consumed;
        }
        if self.decision_pending || self.review_request.is_some() {
            return PeerManagerAction::Consumed;
        }
        let Some(summary) = self.selected_held().cloned() else {
            return PeerManagerAction::Consumed;
        };
        self.review_request = Some((summary.message_id.clone(), summary.epoch));
        self.review = None;
        self.held_reader = Reader::default();
        self.feedback = None;
        self.freshness.barrier();
        self.invalidate_layout();
        PeerManagerAction::Review(summary.message_id)
    }

    fn review_current(&self) -> bool {
        let Some(review) = &self.review else {
            return false;
        };
        !review.resolved
            && self.inbox.as_ref().is_some_and(|inbox| {
                inbox.messages.iter().any(|held| {
                    held.message_id == review.summary.message_id
                        && held.epoch == review.summary.epoch
                })
            })
    }

    fn can_decide(&self, approve: bool) -> bool {
        self.open
            && self.active == PeerView::Held
            && self.browser().pane == Pane::Detail
            && !self.filter_focused
            && self.policy.is_none()
            && !self.decision_pending
            && self.review_rendered
            && self.review_current()
            && self.review.as_ref().is_some_and(|review| {
                review.token.is_some()
                    && self.held_browser.selected.as_ref() == Some(&review.summary.message_id)
                    && (!approve || review.summary.approval_blocker.is_none())
            })
    }

    pub(crate) fn reviewed_message(&self) -> Option<(&str, &PeerReviewToken)> {
        if !self.can_decide(false) {
            return None;
        }
        let review = self.review.as_ref()?;
        Some((&review.summary.message_id, review.token.as_ref()?))
    }

    fn decide(&mut self, decision: PeerDecision) -> PeerManagerAction {
        let Some(token) = self.review.as_mut().and_then(|review| review.token.take()) else {
            return PeerManagerAction::Consumed;
        };
        self.decision_pending = true;
        self.freshness.barrier();
        self.invalidate_layout();
        PeerManagerAction::Decide { token, decision }
    }

    fn confirm(&mut self) -> PeerManagerAction {
        self.freshness.barrier();
        if !self.confirmation_rendered {
            return PeerManagerAction::Consumed;
        }
        let Some(confirmation) = self.confirmation.take() else {
            return PeerManagerAction::Consumed;
        };
        match confirmation {
            Confirmation::Reject { message_id, epoch } => {
                if !self.review_current()
                    || !self.review.as_ref().is_some_and(|review| {
                        review.summary.message_id == message_id
                            && review.summary.epoch == epoch
                            && review.token.is_some()
                    })
                {
                    self.invalidate_layout();
                    return PeerManagerAction::Consumed;
                }
                self.decide(PeerDecision::Reject)
            }
            Confirmation::Relax {
                policy,
                previous,
                floor,
            } => {
                if !self.inbox.as_ref().is_some_and(|inbox| {
                    inbox.inbound == previous
                        && inbox.project_floor == floor
                        && policy_rank(&policy) >= policy_rank(&floor)
                }) {
                    self.feedback = Some("Policy changed. Apply the draft again.".to_owned());
                    self.invalidate_layout();
                    return PeerManagerAction::Consumed;
                }
                self.submit_policy(policy)
            }
        }
    }

    fn step_policy(&mut self, delta: isize) {
        let (Some(draft), Some(inbox)) = (&self.policy, &self.inbox) else {
            return;
        };
        if draft.pending {
            return;
        }
        let current = POLICIES
            .iter()
            .position(|policy| policy == &draft.value)
            .unwrap_or(0);
        let mut index = current;
        while let Some(next) = index
            .checked_add_signed(delta)
            .filter(|next| *next < POLICIES.len())
        {
            index = next;
            if policy_rank(&POLICIES[index]) >= policy_rank(&inbox.project_floor) {
                if let Some(draft) = &mut self.policy {
                    draft.value = POLICIES[index].clone();
                }
                self.policy_reader
                    .document
                    .reveal(POLICY_HEADER_ROWS + index * POLICY_OPTION_ROWS);
                self.invalidate_layout();
                break;
            }
        }
    }

    fn apply_policy(&mut self) -> PeerManagerAction {
        let (Some(draft), Some(inbox)) = (&self.policy, &self.inbox) else {
            return PeerManagerAction::Consumed;
        };
        if draft.pending {
            return PeerManagerAction::Consumed;
        }
        if policy_rank(&draft.value) < policy_rank(&inbox.project_floor) {
            self.feedback = Some(FLOOR_BLOCKED.to_owned());
            return PeerManagerAction::Consumed;
        }
        let policy = draft.value.clone();
        self.freshness.barrier();
        if policy_rank(&policy) < policy_rank(&inbox.inbound) {
            self.confirmation = Some(Confirmation::Relax {
                policy,
                previous: inbox.inbound.clone(),
                floor: inbox.project_floor.clone(),
            });
            self.invalidate_layout();
            return PeerManagerAction::Consumed;
        }
        self.submit_policy(policy)
    }

    fn submit_policy(&mut self, policy: InboundPolicy) -> PeerManagerAction {
        if let Some(draft) = &mut self.policy {
            draft.pending = true;
        }
        if let Some(review) = &mut self.review {
            review.token = None;
            if !review.resolved {
                review.notice = Some(STALE_REVIEW.to_owned());
            }
        }
        self.review_request = None;
        self.invalidate_layout();
        PeerManagerAction::SetInbound(policy)
    }

    pub fn scroll(&mut self, delta: i32) {
        if !self.open || self.confirmation.is_some() {
            return;
        }
        if self.policy.is_none() && self.browser().pane == Pane::List {
            self.step(-(delta.signum() as isize));
        } else {
            self.reader_mut().document.scroll(delta);
        }
    }

    pub fn pan(&mut self, delta: i32) {
        if self.open && self.confirmation.is_none() {
            self.reader_mut().document.pan(delta);
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> PeerManagerAction {
        if !self.open {
            return PeerManagerAction::Consumed;
        }
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        if let Some(index) = self.footer_hits.handle_mouse(event)
            && let Some((_, command)) = self.controls.get(index)
        {
            return self.command(command.clone());
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && !self.popup.contains(position) {
            if self.confirmation.is_some() || self.policy.is_some() {
                return self.command(Command::Back);
            }
            self.close();
            return PeerManagerAction::Close;
        }
        if self.confirmation.is_some() {
            return PeerManagerAction::Consumed;
        }
        if matches!(
            event.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            match event.kind {
                MouseEventKind::ScrollLeft => self.pan(-WHEEL_STEP),
                MouseEventKind::ScrollRight => self.pan(WHEEL_STEP),
                _ => {
                    let delta = if event.kind == MouseEventKind::ScrollUp {
                        WHEEL_STEP
                    } else {
                        -WHEEL_STEP
                    };
                    if self.list_area.contains(position) {
                        self.step(-(delta.signum() as isize));
                    } else {
                        self.reader_mut().document.scroll(delta);
                    }
                }
            }
            return PeerManagerAction::Consumed;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, policy)) = self
                .policy_hits
                .iter()
                .find(|(area, _)| area.contains(position))
            {
                let policy = policy.clone();
                if let (Some(draft), Some(inbox)) = (&mut self.policy, &self.inbox)
                    && !draft.pending
                    && policy_rank(&policy) >= policy_rank(&inbox.project_floor)
                {
                    draft.value = policy;
                    self.invalidate_layout();
                }
                return PeerManagerAction::Consumed;
            }
            if let Some((_, id)) = self
                .row_hits
                .iter()
                .find(|(area, _)| area.contains(position))
            {
                let id = id.clone();
                self.browser_mut().pane = Pane::List;
                self.select(id);
                return PeerManagerAction::Consumed;
            }
            if self.detail_area.contains(position) && self.policy.is_none() {
                self.browser_mut().pane = Pane::Detail;
                if self.active == PeerView::Held
                    && self.review.is_none()
                    && self.review_request.is_none()
                {
                    return self.activate();
                }
            }
        }
        if self.policy.is_some() || self.browser().pane == Pane::Detail {
            match self.reader_mut().document.handle_mouse(event) {
                DocumentMouse::Copy(text) => return PeerManagerAction::Copy(text),
                DocumentMouse::Consumed | DocumentMouse::Passthrough => {}
            }
        }
        PeerManagerAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("peer_manager", area);
        self.review_rendered = false;
        self.confirmation_rendered = false;
        self.row_hits.clear();
        self.policy_hits.clear();
        let (popup, inner) = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: HEIGHT_PERCENT,
        }
        .render(frame, area, area.height);
        self.popup = popup;
        let padding = H_PAD.min(inner.width / 2);
        let padded = Rect {
            x: inner.x.saturating_add(padding),
            width: inner.width.saturating_sub(padding * 2),
            ..inner
        };
        let mut content = padded;
        let mut controls = Vec::new();
        let footer_rows = if content.height >= OPTIONAL_CHROME_ROWS {
            FOOTER_ROWS
        } else {
            1
        };
        let footer = take_bottom(&mut content, footer_rows);
        if self.confirmation.is_none() && self.policy.is_none() && content.height > MIN_BODY_ROWS {
            let tabs = take_top(&mut content, 1);
            self.draw_commands(
                frame,
                tabs,
                &[Command::Sessions, Command::Held],
                &mut controls,
            );
        }
        if content.height >= OPTIONAL_CHROME_ROWS && self.confirmation.is_none() {
            let status = take_top(&mut content, 1);
            let [status, policy] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(POLICY_ACTION_COLS)])
                    .areas(status);
            let inbound = self
                .inbox
                .as_ref()
                .map_or(UNAVAILABLE, |inbox| policy_label(&inbox.inbound));
            frame.render_widget(
                Paragraph::new(format!("This session: inbound {inbound}"))
                    .style(theme::current().tool_dim),
                status,
            );
            if self.policy.is_none() && !self.filter_focused && self.inbox.is_some() {
                self.draw_commands(frame, policy, &[Command::Policy], &mut controls);
            }
        }
        let filter_visible = self.filter_focused || !self.browser().filter.is_empty();
        if filter_visible
            && self.policy.is_none()
            && self.confirmation.is_none()
            && content.height > MIN_BODY_ROWS
        {
            let filter = take_top(&mut content, 1);
            let mut spans = vec![Span::styled("/ ", theme::current().accent)];
            spans.extend(
                self.browser()
                    .filter
                    .paint(
                        usize::from(filter.width.saturating_sub(2)),
                        &field_styles(input_text_style()),
                        self.filter_focused,
                        "Filter",
                    )
                    .spans,
            );
            frame.render_widget(Paragraph::new(Line::from(spans)), filter);
        }
        let feedback = self.feedback.clone().or_else(|| {
            (self.active == PeerView::Sessions && self.policy.is_none())
                .then(|| self.discovery_status())
        });
        if let Some(feedback) = feedback
            && self.confirmation.is_none()
            && content.height > MIN_BODY_ROWS
        {
            let status = take_top(&mut content, 1);
            frame.render_widget(
                Paragraph::new(feedback).style(theme::current().tool_dim),
                status,
            );
        }
        self.list_area = Rect::default();
        self.detail_area = Rect::default();
        if self.confirmation.is_some() {
            self.draw_confirmation(frame, content);
        } else if self.policy.is_some() {
            self.detail_area = content;
            self.draw_policy(frame, content);
        } else {
            let (list, detail) = panes(content, &self.browser().pane);
            self.list_area = list;
            self.detail_area = detail;
            if list.width > 0 && list.height > 0 {
                self.draw_list(frame, list);
            }
            if detail.width > 0 && detail.height > 0 {
                self.draw_detail(frame, detail);
            }
        }
        let commands = self.footer_commands();
        self.draw_commands(frame, footer, &commands, &mut controls);
        if self.controls != controls {
            self.footer_hits.clear();
        }
        self.footer_hits
            .set(controls.iter().map(|(area, _)| *area).collect());
        self.controls = controls;
        popup
    }

    fn discovery_status(&self) -> String {
        if let Some(error) = &self.discovery_error {
            return format!(
                "Discovery failed: {error} · Ctrl+R Retry{}",
                if self.sessions.is_some() {
                    " · showing previous snapshot"
                } else {
                    ""
                }
            );
        }
        if self.discovering {
            return if self.sessions.is_some() {
                "Refreshing discovery snapshot…"
            } else {
                "Loading discovery snapshot…"
            }
            .to_owned();
        }
        self.refreshed_at.as_ref().map_or_else(
            || "Discovery not loaded · Ctrl+R Retry".to_owned(),
            |time| format!("Discovery snapshot · refreshed {time} local · Ctrl+R Refresh"),
        )
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("peer_manager_list", area);
        let entries = self.entries();
        if entries.is_empty() {
            let text = match self.active {
                PeerView::Sessions if self.sessions.is_none() => self.discovery_status(),
                PeerView::Sessions if self.sessions.as_ref().is_some_and(Vec::is_empty) => {
                    NO_PEERS.to_owned()
                }
                PeerView::Held if self.inbox.is_none() => "Loading held messages…".to_owned(),
                PeerView::Held
                    if self
                        .inbox
                        .as_ref()
                        .is_some_and(|inbox| inbox.messages.is_empty()) =>
                {
                    NO_HELD.to_owned()
                }
                _ => NO_MATCHES.to_owned(),
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(theme::current().tool_dim)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let visible = usize::from(area.height / LIST_ROW_HEIGHT).max(1);
        let selected = self.browser().selected.clone();
        let selected_index = entries
            .iter()
            .position(|(id, _, _)| Some(id) == selected.as_ref());
        let browser = self.browser_mut();
        if let Some(index) = selected_index {
            if index < browser.offset {
                browser.offset = index;
            }
            if index >= browser.offset + visible {
                browser.offset = index.saturating_sub(visible - 1);
            }
        }
        browser.offset = browser.offset.min(entries.len().saturating_sub(visible));
        let offset = browser.offset;
        let theme = theme::current();
        for (index, (id, name, detail)) in
            entries.into_iter().skip(offset).take(visible).enumerate()
        {
            let row = Rect {
                y: area.y.saturating_add(index as u16 * LIST_ROW_HEIGHT),
                height: LIST_ROW_HEIGHT
                    .min(area.height.saturating_sub(index as u16 * LIST_ROW_HEIGHT)),
                ..area
            };
            let selected = selected.as_ref() == Some(&id);
            let style = hover_style(
                if selected {
                    theme.item_selected
                } else {
                    theme.item
                },
                self.pointer.is_some_and(|position| row.contains(position)),
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(name, style),
                    Line::styled(detail, if selected { style } else { theme.item_desc }),
                ]),
                row,
            );
            self.row_hits.push((row, id));
        }
    }

    fn draw_detail(&mut self, frame: &mut Frame, mut area: Rect) {
        grab_scope!("peer_manager_detail", area);
        if self.active == PeerView::Sessions {
            if let Some(peer) = self.selected_peer() {
                let status = format!(
                    "Activity: {} · Remote inbound: {}\n{}",
                    activity(peer),
                    policy_label(&peer.inbound),
                    policy_description(&peer.inbound),
                );
                let rows = wrapped_height(&status, area.width)
                    .min(area.height.saturating_sub(MIN_BODY_ROWS));
                let status_area = take_top(&mut area, rows);
                frame.render_widget(
                    Paragraph::new(status)
                        .style(theme::current().tool_dim)
                        .wrap(Wrap { trim: false }),
                    status_area,
                );
            }
            let text = self.selected_peer().map_or_else(
                || {
                    self.session_browser.selected.as_deref().map_or_else(
                        || SELECT_PEER.to_owned(),
                        |target| format!("{PEER_GONE}\n\nExact target: {}", literal(target, false)),
                    )
                },
                |peer| {
                    format!(
                        "{}\n\nExact target: {}\nWorkspace: {}\n\n{}",
                        literal(&peer.name, false),
                        literal(&peer.target, false),
                        literal(&peer.cwd.to_string_lossy(), false),
                        ALIAS_SCOPE,
                    )
                },
            );
            self.session_reader.replace(text);
            self.session_reader.draw(frame, area);
            return;
        }
        if self.review.is_none() {
            let text = self.selected_held().map_or_else(
                || {
                    if self.held_browser.selected.is_some() {
                        NO_LONGER_HELD
                    } else {
                        SELECT_HELD
                    }
                    .to_owned()
                },
                |held| {
                    format!(
                        "{}\nReason: {}\n\n{}",
                        held_identity(held),
                        literal(&held.reason, false),
                        if self.review_request.is_some() {
                            "Loading explicit review…"
                        } else {
                            PREVIEW_HINT
                        }
                    )
                },
            );
            self.held_reader.set(text);
            self.held_reader.draw(frame, area);
            return;
        }
        let Some(review) = &self.review else {
            return;
        };
        let identity = format!("Message: {}", literal(&review.summary.message_id, false));
        let status = if self.decision_pending {
            "Decision pending…".to_owned()
        } else {
            review
                .notice
                .clone()
                .unwrap_or_else(|| format!("Held: {}", literal(&review.summary.reason, false)))
        };
        let header = format!("{identity}\n{status}");
        let header_rows = wrapped_height(&header, area.width);
        if review.resolved || review.notice.is_some() || self.decision_pending {
            let height = header_rows.min(area.height.saturating_sub(MIN_BODY_ROWS));
            let identity_area = take_top(&mut area, height);
            frame.render_widget(
                Paragraph::new(header)
                    .style(theme::current().bold)
                    .wrap(Wrap { trim: false }),
                identity_area,
            );
            self.held_reader.draw(frame, area);
            return;
        }
        let warning_rows = wrapped_height(APPROVAL_WARNING, area.width);
        let enough = area.width > PANE_GAP
            && area.height
                >= header_rows
                    .saturating_add(warning_rows)
                    .saturating_add(MIN_BODY_ROWS + 1);
        if enough {
            let identity_area = take_top(&mut area, header_rows);
            frame.render_widget(
                Paragraph::new(header)
                    .style(theme::current().bold)
                    .wrap(Wrap { trim: false }),
                identity_area,
            );
            let warning_area = take_bottom(&mut area, warning_rows);
            frame.render_widget(
                Paragraph::new(APPROVAL_WARNING)
                    .style(theme::current().tool_warning)
                    .wrap(Wrap { trim: false }),
                warning_area,
            );
            if let Some(blocker) = &review.summary.approval_blocker {
                let blocker_area = take_top(&mut area, 1);
                frame.render_widget(
                    Paragraph::new(format!("Approve disabled: {}", literal(blocker, false)))
                        .style(theme::current().tool_warning),
                    blocker_area,
                );
            }
            self.review_rendered = area.height > MIN_BODY_ROWS && !self.filter_focused;
        } else {
            let guidance = take_top(&mut area, 1);
            frame.render_widget(
                Paragraph::new(RESIZE_GUIDANCE).style(theme::current().tool_warning),
                guidance,
            );
        }
        self.held_reader.draw(frame, area);
    }

    fn draw_policy(&mut self, frame: &mut Frame, area: Rect) {
        let (Some(draft), Some(inbox)) = (&self.policy, &self.inbox) else {
            return;
        };
        let override_label = inbox.inbound_override.as_ref().map_or("None", policy_label);
        let mut lines = vec![
            format!("This session · effective {}", policy_label(&inbox.inbound)),
            format!("Session override: {override_label}"),
            format!(
                "Project floor: {} (options below it are disabled)",
                policy_label(&inbox.project_floor)
            ),
            String::new(),
        ];
        let mut option_rows = Vec::new();
        for policy in POLICIES {
            let disabled = policy_rank(&policy) < policy_rank(&inbox.project_floor);
            option_rows.push((lines.len(), policy.clone()));
            lines.push(format!(
                "{} {}{}",
                if draft.value == policy { ">" } else { " " },
                policy_label(&policy),
                if disabled {
                    " · disabled by project"
                } else {
                    ""
                }
            ));
            lines.push(format!("  {}", policy_description(&policy)));
            lines.push(String::new());
        }
        lines.push(
            if draft.pending {
                "Applying policy…"
            } else {
                "Up/Down select a draft; a Apply changes this session. Esc cancels."
            }
            .to_owned(),
        );
        lines.push("Refuse rejects new arrivals; it does not delete held messages.".to_owned());
        self.policy_reader.replace(lines.join("\n"));
        self.policy_reader.draw(frame, area);
        let visible = self.policy_reader.document.visible();
        for (row, policy) in option_rows {
            if visible.contains(&row) {
                self.policy_hits.push((
                    Rect::new(
                        area.x,
                        area.y.saturating_add((row - visible.start) as u16),
                        area.width.saturating_sub(1),
                        1,
                    ),
                    policy,
                ));
            }
        }
    }

    fn draw_confirmation(&mut self, frame: &mut Frame, area: Rect) {
        let text = match &self.confirmation {
            Some(Confirmation::Reject { message_id, .. }) => format!(
                "Reject message: {}\n\n{REJECT_WARNING}\n\nEnter confirms this message only. Esc cancels.",
                literal(message_id, false)
            ),
            Some(Confirmation::Relax {
                policy,
                previous,
                floor,
            }) => format!(
                "This session: {} → {}\nProject floor: {}\n\n{RELAX_WARNING}\n\nEnter applies this policy. Esc cancels.",
                policy_label(previous),
                policy_label(policy),
                policy_label(floor)
            ),
            None => return,
        };
        self.confirmation_rendered =
            area.width > 0 && wrapped_height(&text, area.width) <= area.height;
        let text = if self.confirmation_rendered {
            text
        } else {
            format!("Resize to show the full confirmation. Esc cancels.\n\n{text}")
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(theme::current().tool_warning)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn footer_commands(&self) -> Vec<Command> {
        if self.confirmation.is_some() {
            return if self.confirmation_rendered {
                vec![Command::Activate, Command::Back]
            } else {
                vec![Command::Back]
            };
        }
        if self.policy.is_some() {
            return vec![Command::Apply, Command::Back];
        }
        if self.filter_focused {
            return vec![Command::Activate, Command::Back];
        }
        let mut commands = Vec::new();
        if self.can_decide(true) {
            commands.push(Command::Approve);
        }
        if self.can_decide(false) {
            commands.push(Command::Reject);
        }
        if self.active == PeerView::Sessions {
            commands.extend([Command::CopyTarget, Command::Refresh]);
        }
        commands.extend([
            Command::Activate,
            Command::Back,
            Command::Focus,
            Command::Filter,
            Command::Policy,
        ]);
        commands
    }

    fn draw_commands(
        &self,
        frame: &mut Frame,
        area: Rect,
        commands: &[Command],
        controls: &mut Vec<(Rect, Command)>,
    ) {
        let theme = theme::current();
        let mut remaining = commands.to_vec();
        for row in 0..area.height {
            if remaining.is_empty() {
                break;
            }
            let row_area = Rect::new(area.x, area.y + row, area.width, 1);
            let mut accepted = Vec::new();
            for command in &remaining {
                let mut trial = accepted.clone();
                trial.push(command.clone());
                if self.command_line(&trial).fits(area.width) {
                    accepted = trial;
                }
            }
            let footer = self.command_line(&accepted);
            let hits = footer.hits(row_area, 0, 1);
            let hovered = self
                .pointer
                .and_then(|position| hits.iter().position(|hit| hit.contains(position)));
            frame.render_widget(
                Paragraph::new(footer.line(hovered)).style(theme.tool_dim),
                row_area,
            );
            remaining.retain(|command| !accepted.contains(command));
            controls.extend(hits.into_iter().zip(accepted));
        }
    }

    fn command_line(&self, commands: &[Command]) -> FooterLine {
        let theme = theme::current();
        let mut footer = FooterLine::default();
        for command in commands {
            let Some((_, _, _, label, description)) = BINDINGS
                .iter()
                .find(|(candidate, _, _, _, _)| candidate == command)
            else {
                continue;
            };
            if command != &commands[0] {
                footer.text("  ", theme.tool_dim);
            }
            let active = matches!(
                (command, &self.active),
                (Command::Sessions, PeerView::Sessions) | (Command::Held, PeerView::Held)
            );
            footer.command(
                label,
                if active {
                    theme.item_selected
                } else {
                    theme.keybind_key
                },
            );
            let description = match command {
                Command::Held => format!(
                    " Held messages ({})",
                    self.inbox.as_ref().map_or(0, |inbox| inbox.messages.len())
                ),
                Command::Activate if self.confirmation.is_some() => " Confirm".to_owned(),
                Command::Activate if self.filter_focused => " Done".to_owned(),
                Command::Activate if self.active == PeerView::Held => " Review".to_owned(),
                Command::Back if self.filter_focused => " Clear".to_owned(),
                _ => (*description).to_owned(),
            };
            footer.describe(
                description,
                if active {
                    theme.item_selected
                } else {
                    theme.tool_dim
                },
            );
        }
        footer
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for PeerManager {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(self.open && self.discovering, Cadence::PENDING)
    }
}

fn policy_label(policy: &InboundPolicy) -> &'static str {
    match policy {
        InboundPolicy::Auto => "Auto",
        InboundPolicy::Accept => "Accept",
        InboundPolicy::Hold => "Hold",
        InboundPolicy::Refuse => "Refuse",
    }
}

fn policy_rank(policy: &InboundPolicy) -> u8 {
    match policy {
        InboundPolicy::Accept => 0,
        InboundPolicy::Auto => 1,
        InboundPolicy::Hold => 2,
        InboundPolicy::Refuse => 3,
    }
}

fn policy_description(policy: &InboundPolicy) -> &'static str {
    match policy {
        InboundPolicy::Auto => {
            "Automatic eligibility requires the same canonical workspace, matching Build/Plan mode, and Ask permissions; it does not establish equivalent authority."
        }
        InboundPolicy::Accept => {
            "Allows wider inbound delivery, subject to existing delivery budgets and safety boundaries; messages may start billable turns."
        }
        InboundPolicy::Hold => "Holds arrivals for explicit local approval.",
        InboundPolicy::Refuse => "Rejects new arrivals; existing held messages are not deleted.",
    }
}

fn activity(peer: &PeerSummary) -> &'static str {
    if peer.blocked {
        "Blocked"
    } else if peer.busy {
        "Busy"
    } else {
        "Idle"
    }
}

fn held_identity(held: &HeldMessageSummary) -> String {
    format!(
        "Sender: {}\nExact reply target: {}\nWorkspace: {}\nMode: {}\nMessage: {}",
        literal(&held.sender_name, false),
        literal(&held.reply_target, false),
        held.workspace.as_ref().map_or_else(
            || UNAVAILABLE.to_owned(),
            |path| literal(&path.to_string_lossy(), false)
        ),
        if held.mode.is_empty() {
            UNAVAILABLE.to_owned()
        } else {
            literal(&held.mode, false)
        },
        literal(&held.message_id, false)
    )
}

fn deceptive(character: char) -> bool {
    matches!(character, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn literal(text: &str, newlines: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' && newlines {
            output.push(character);
        } else if character.is_control() || deceptive(character) {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

fn paint_literal(text: &str, style: Style) -> Painted {
    let mut lines = Vec::new();
    let mut continuations = Vec::new();
    for line in text.split('\n') {
        let mut start = 0;
        let mut columns = 0;
        let mut continuation = false;
        for (index, grapheme) in line.grapheme_indices(true) {
            let width = grapheme.width();
            if columns + width > MAX_LITERAL_COLS {
                lines.push(Line::styled(line[start..index].to_owned(), style));
                continuations.push(continuation);
                continuation = true;
                start = index;
                columns = 0;
            }
            columns += width;
        }
        lines.push(Line::styled(line[start..].to_owned(), style));
        continuations.push(continuation);
    }
    Painted::new(lines, Vec::new(), Vec::new()).with_continuations(continuations)
}

fn wrapped_height(text: &str, width: u16) -> u16 {
    u16::try_from(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width.max(1)),
    )
    .unwrap_or(u16::MAX)
}

fn take_top(area: &mut Rect, rows: u16) -> Rect {
    let height = rows.min(area.height);
    let taken = Rect { height, ..*area };
    area.y = area.y.saturating_add(height);
    area.height = area.height.saturating_sub(height);
    taken
}

fn take_bottom(area: &mut Rect, rows: u16) -> Rect {
    let height = rows.min(area.height);
    area.height = area.height.saturating_sub(height);
    Rect {
        y: area.bottom(),
        height,
        ..*area
    }
}

fn panes(area: Rect, pane: &Pane) -> (Rect, Rect) {
    if area.width < SPLIT_MIN_COLS {
        return match pane {
            Pane::List => (area, Rect::default()),
            Pane::Detail => (Rect::default(), area),
        };
    }
    let [list, _, detail] = Layout::horizontal([
        Constraint::Length(LIST_COLS),
        Constraint::Length(PANE_GAP),
        Constraint::Fill(1),
    ])
    .areas(area);
    (list, detail)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_agent::peers::{
        HeldMessageSummary, PeerDecisionResult, PeerInboxSnapshot, PeerSummary,
    };
    use caudra_config::InboundPolicy;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::{Color, Style};
    use test_case::test_case;

    use super::{
        Command, Confirmation, FLOOR_BLOCKED, FreshInput, MAX_LITERAL_COLS, NO_LONGER_HELD,
        NO_MATCHES, NO_PEERS, Pane, PeerManager, PeerManagerAction, PeerView, QUEUED, REJECTED,
        Reader, ReviewPanel, STALE_REVIEW, literal, paint_literal, policy_rank,
    };
    use crate::components::buffer_text;

    const FIRST_TARGET: &str = "calm-fox-brings-dawn";
    const SECOND_TARGET: &str = "calm-fox-brings-rain";
    const FIRST_MESSAGE: &str = "bright-blue-brook";
    const SECOND_MESSAGE: &str = "still-green-pine";
    const TITLE: &str = "Same readable title";
    const WORKSPACE: &str = "/workspace/one";
    const HOLD_REASON: &str = "Local approval required";
    const BUDGET_REASON: &str = "Delivery budget exhausted";
    const FAILURE: &str = "Discovery is unavailable";
    const BODY: &str =
        "# Literal **not bold**\n[not a link](peer-target)\n/messages approve other\n";
    const FIRST_EPOCH: u64 = 7;
    const WIDE: u16 = 120;
    const NARROW: u16 = 80;
    const HEIGHT: u16 = 40;
    const SHORT: u16 = 24;
    const MANY_ROWS: usize = 100;
    const TEST_RENDER: &str = "test backend renders";
    const SELECTED_REVIEW: &str = "review panel exists";
    const POLICY_DRAFT: &str = "policy draft exists";

    fn peer(target: &str) -> PeerSummary {
        PeerSummary {
            target: target.to_owned(),
            name: TITLE.to_owned(),
            cwd: PathBuf::from(WORKSPACE),
            busy: false,
            blocked: false,
            inbound: InboundPolicy::Auto,
        }
    }

    fn held(id: &str) -> HeldMessageSummary {
        HeldMessageSummary {
            message_id: id.to_owned(),
            sender_name: TITLE.to_owned(),
            reply_target: FIRST_TARGET.to_owned(),
            workspace: Some(PathBuf::from(WORKSPACE)),
            mode: "Build".to_owned(),
            reason: HOLD_REASON.to_owned(),
            epoch: FIRST_EPOCH,
            approval_blocker: None,
        }
    }

    fn snapshot(messages: Vec<HeldMessageSummary>) -> PeerInboxSnapshot {
        PeerInboxSnapshot {
            messages,
            inbound: InboundPolicy::Hold,
            inbound_override: None,
            project_floor: InboundPolicy::Accept,
        }
    }

    fn manager(view: PeerView) -> PeerManager {
        let mut manager = PeerManager::new();
        manager.open(view);
        manager.update_inbox(snapshot(vec![held(FIRST_MESSAGE), held(SECOND_MESSAGE)]));
        manager
    }

    fn reviewing() -> PeerManager {
        let mut manager = manager(PeerView::Held);
        manager.held_browser.pane = Pane::Detail;
        manager.review = Some(ReviewPanel {
            summary: held(FIRST_MESSAGE),
            token: None,
            notice: None,
            resolved: false,
        });
        manager.held_reader.set(BODY.to_owned());
        manager
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw(manager: &mut PeerManager, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect(TEST_RENDER);
        terminal
            .draw(|frame| {
                let area = frame.area();
                manager.view(frame, area);
            })
            .expect(TEST_RENDER);
        buffer_text(terminal.backend().buffer())
    }

    #[test_case(PeerView::Sessions ; "sessions")]
    #[test_case(PeerView::Held ; "held")]
    fn opening_and_closing_fence_results_and_capture_paste(view: PeerView) {
        let mut manager = manager(view.clone());
        let opening = manager.generation();
        assert_eq!(manager.active, view);
        assert!(manager.handle_paste(BODY));
        assert!(manager.browser().filter.is_empty());
        manager.close();
        assert!(manager.generation() > opening);
        assert!(!manager.handle_paste(BODY));
        assert!(!manager.update_inbox(snapshot(vec![held(FIRST_MESSAGE)])));
        let closing = manager.generation();
        manager.open(PeerView::Held);
        assert!(manager.generation() > closing);
        assert!(manager.inbox.is_none());
        assert!(manager.sessions.is_none());
    }

    #[test_case('1')]
    #[test_case('2')]
    #[test_case('y')]
    #[test_case('n')]
    #[test_case('p')]
    fn printable_shortcuts_are_literal_while_filtering(character: char) {
        let mut manager = manager(PeerView::Held);
        manager.handle_key(press(KeyCode::Char('/')));
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char(character))),
            PeerManagerAction::Consumed
        ));
        assert_eq!(manager.held_browser.filter.text(), character.to_string());
        assert_eq!(manager.active, PeerView::Held);
        assert!(manager.policy.is_none());
        assert!(manager.review_request.is_none());
    }

    #[test]
    fn independent_filters_and_exact_selections_survive_view_switches() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET), peer(SECOND_TARGET)]));
        manager.select(SECOND_TARGET.to_owned());
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(TITLE);
        manager.handle_key(press(KeyCode::Enter));
        manager.handle_key(press(KeyCode::Char('2')));
        manager.select(SECOND_MESSAGE.to_owned());
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(SECOND_MESSAGE);
        manager.handle_key(press(KeyCode::Enter));
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('1'))),
            PeerManagerAction::Consumed
        ));
        assert_eq!(manager.session_browser.filter.text(), TITLE);
        assert_eq!(
            manager.session_browser.selected.as_deref(),
            Some(SECOND_TARGET)
        );
        manager.handle_key(press(KeyCode::Char('2')));
        assert_eq!(manager.held_browser.filter.text(), SECOND_MESSAGE);
        assert_eq!(
            manager.held_browser.selected.as_deref(),
            Some(SECOND_MESSAGE)
        );
    }

    #[test]
    fn sessions_without_snapshot_request_discovery_and_errors_preserve_snapshot() {
        let mut manager = manager(PeerView::Held);
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('1'))),
            PeerManagerAction::Refresh
        ));
        manager.start_discovery();
        assert!(manager.discovery_status().contains("Loading"));
        manager.set_sessions(Err(FAILURE.to_owned()));
        assert!(manager.discovery_status().contains(FAILURE));
        assert!(!draw(&mut manager, NARROW, SHORT).contains(NO_PEERS));
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET), peer(SECOND_TARGET)]));
        manager.select(SECOND_TARGET.to_owned());
        manager.start_discovery();
        assert!(manager.discovery_status().contains("Refreshing"));
        manager.set_sessions(Err(FAILURE.to_owned()));
        assert_eq!(manager.sessions.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            manager.session_browser.selected.as_deref(),
            Some(SECOND_TARGET)
        );
        assert!(manager.discovery_status().contains("previous snapshot"));
    }

    #[test]
    fn reordered_or_disappeared_duplicate_titles_never_retarget_selection() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET), peer(SECOND_TARGET)]));
        manager.set_sessions(Ok(vec![peer(SECOND_TARGET), peer(FIRST_TARGET)]));
        assert_eq!(
            manager.selected_peer().map(|peer| peer.target.as_str()),
            Some(FIRST_TARGET)
        );
        assert!(
            matches!(manager.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)), PeerManagerAction::Copy(target) if target == FIRST_TARGET)
        );
        manager.set_sessions(Ok(vec![peer(SECOND_TARGET)]));
        assert_eq!(
            manager.session_browser.selected.as_deref(),
            Some(FIRST_TARGET)
        );
        assert!(manager.selected_peer().is_none());
        assert!(matches!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            PeerManagerAction::Consumed
        ));
    }

    #[test]
    fn empty_snapshot_is_not_a_filter_miss() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(Vec::new()));
        assert!(draw(&mut manager, WIDE, HEIGHT).contains("No eligible live peers"));
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET)]));
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(SECOND_TARGET);
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(NO_MATCHES));
        manager.handle_key(press(KeyCode::Enter));
        assert!(matches!(
            manager.handle_key(press(KeyCode::Enter)),
            PeerManagerAction::Consumed
        ));
    }

    #[test]
    fn previews_hover_refresh_and_pane_focus_do_not_request_review() {
        let mut manager = manager(PeerView::Held);
        draw(&mut manager, WIDE, HEIGHT);
        let second = manager.row_hits[1].0;
        manager.handle_mouse(mouse(MouseEventKind::Moved, second));
        assert_eq!(
            manager.held_browser.selected.as_deref(),
            Some(FIRST_MESSAGE)
        );
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), second));
        assert_eq!(
            manager.held_browser.selected.as_deref(),
            Some(SECOND_MESSAGE)
        );
        manager.handle_key(press(KeyCode::Tab));
        draw(&mut manager, WIDE, HEIGHT);
        assert!(manager.review_request.is_none());
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('y'))),
            PeerManagerAction::Consumed
        ));
        assert!(
            matches!(manager.handle_key(press(KeyCode::Enter)), PeerManagerAction::Review(id) if id == SECOND_MESSAGE)
        );
        assert!(matches!(
            manager.handle_key(press(KeyCode::Enter)),
            PeerManagerAction::Consumed
        ));
    }

    #[test_case(KeyCode::Char('y'))]
    #[test_case(KeyCode::Char('n'))]
    #[test_case(KeyCode::Enter)]
    fn decision_barrier_rejects_repeat_and_unreleased_repeated_press(code: KeyCode) {
        let mut input = FreshInput::default();
        assert!(input.accept(press(code)));
        input.barrier();
        assert!(!input.accept(press(code)));
        assert!(!input.accept(KeyEvent {
            kind: KeyEventKind::Repeat,
            ..press(code)
        }));
        assert!(!input.accept(KeyEvent {
            kind: KeyEventKind::Release,
            ..press(code)
        }));
        assert!(input.accept(press(code)));
        input.barrier();
        assert!(input.accept(press(KeyCode::Tab)));
        assert!(input.accept(press(code)));
    }

    #[test_case(WIDE, HEIGHT, true ; "wide")]
    #[test_case(NARROW, SHORT, true ; "narrow")]
    #[test_case(NARROW, 6, false ; "very_short")]
    #[test_case(8, SHORT, false ; "very_narrow")]
    #[test_case(1, 1, false ; "one_cell")]
    #[test_case(0, 0, false ; "zero")]
    fn review_actions_require_safely_rendered_identity_warning_and_body(
        width: u16,
        height: u16,
        ready: bool,
    ) {
        let mut manager = reviewing();
        assert!(!manager.review_rendered);
        let rendered = draw(&mut manager, width, height);
        assert_eq!(manager.review_rendered, ready);
        if ready {
            assert!(rendered.contains("Approving exposes this message"));
        }
        let bounds = Rect::new(0, 0, width, height);
        assert_eq!(manager.popup.intersection(bounds), manager.popup);
        for (hit, _) in &manager.controls {
            assert_eq!(hit.intersection(bounds), *hit);
        }
        assert!(!manager.can_decide(true));
        assert!(!manager.can_decide(false));
    }

    #[test]
    fn metadata_changes_preserve_complete_literal_body_scroll_and_selection() {
        let mut manager = reviewing();
        let body = (0..MANY_ROWS)
            .map(|index| format!("{index}: {BODY}"))
            .collect::<Vec<_>>()
            .join("\n");
        manager.held_reader.set(body.clone());
        draw(&mut manager, WIDE, HEIGHT);
        manager.held_reader.document.select_all();
        manager.scroll(-20);
        let top = manager.held_reader.document.top();
        let mut current = held(FIRST_MESSAGE);
        current.reason = BUDGET_REASON.to_owned();
        assert!(manager.update_inbox(snapshot(vec![current.clone(), held(SECOND_MESSAGE)])));
        assert!(!manager.update_inbox(snapshot(vec![current, held(SECOND_MESSAGE)])));
        draw(&mut manager, WIDE, HEIGHT);
        assert_eq!(manager.held_reader.document.top(), top);
        assert_eq!(
            manager.held_reader.document.selected_text().as_deref(),
            Some(body.as_str())
        );
        assert_eq!(manager.held_reader.text, body);
    }

    #[test]
    fn epoch_changes_and_disappearance_invalidate_review_without_replacing_body() {
        let mut manager = reviewing();
        let mut changed = held(FIRST_MESSAGE);
        changed.epoch += 1;
        manager.update_inbox(snapshot(vec![changed]));
        assert_eq!(
            manager
                .review
                .as_ref()
                .expect(SELECTED_REVIEW)
                .notice
                .as_deref(),
            Some(STALE_REVIEW)
        );
        assert_eq!(manager.held_reader.text, BODY);
        manager.update_inbox(snapshot(vec![held(SECOND_MESSAGE)]));
        assert_eq!(
            manager
                .review
                .as_ref()
                .expect(SELECTED_REVIEW)
                .notice
                .as_deref(),
            Some(NO_LONGER_HELD)
        );
        assert_eq!(
            manager.held_browser.selected.as_deref(),
            Some(FIRST_MESSAGE)
        );
        assert_eq!(manager.held_reader.text, BODY);
    }

    #[test_case(PeerDecisionResult::Queued, QUEUED ; "queued_not_delivered")]
    #[test_case(PeerDecisionResult::Rejected, REJECTED ; "rejected")]
    fn resolution_stays_on_the_exact_message_until_an_explicit_new_review(
        result: PeerDecisionResult,
        notice: &str,
    ) {
        let mut manager = reviewing();
        manager.decision_pending = true;
        manager.finish_decision(Ok(result));
        manager.update_inbox(snapshot(vec![held(SECOND_MESSAGE)]));
        manager.held_browser.pane = Pane::List;
        manager.step(1);
        draw(&mut manager, WIDE, HEIGHT);
        let review = manager.review.as_ref().expect(SELECTED_REVIEW);
        assert_eq!(review.summary.message_id, FIRST_MESSAGE);
        assert_eq!(review.notice.as_deref(), Some(notice));
        assert!(!manager.can_decide(true));
        assert!(
            matches!(manager.handle_key(press(KeyCode::Enter)), PeerManagerAction::Review(id) if id == SECOND_MESSAGE)
        );
        assert!(manager.review.is_none());
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('y'))),
            PeerManagerAction::Consumed
        ));
    }

    #[test]
    fn budget_held_approval_is_not_reported_as_queued() {
        let mut manager = reviewing();
        manager.decision_pending = true;
        manager.finish_decision(Ok(PeerDecisionResult::Held {
            reason: BUDGET_REASON.to_owned(),
        }));
        let notice = manager
            .review
            .as_ref()
            .expect(SELECTED_REVIEW)
            .notice
            .as_deref()
            .expect(SELECTED_REVIEW);
        assert!(notice.contains(BUDGET_REASON));
        assert!(notice.contains("Not queued or delivered"));
    }

    #[test]
    fn reject_confirmation_freezes_selection_and_cannot_decide_before_rendering() {
        let mut manager = reviewing();
        manager.confirmation = Some(Confirmation::Reject {
            message_id: FIRST_MESSAGE.to_owned(),
            epoch: FIRST_EPOCH,
        });
        assert!(matches!(
            manager.handle_key(press(KeyCode::Enter)),
            PeerManagerAction::Consumed
        ));
        manager.handle_key(press(KeyCode::Down));
        manager.handle_key(press(KeyCode::Char('1')));
        manager.handle_paste(SECOND_MESSAGE);
        assert_eq!(manager.active, PeerView::Held);
        assert_eq!(
            manager.held_browser.selected.as_deref(),
            Some(FIRST_MESSAGE)
        );
        assert!(
            matches!(&manager.confirmation, Some(Confirmation::Reject { message_id, .. }) if message_id == FIRST_MESSAGE)
        );
        manager.handle_key(press(KeyCode::Esc));
        assert!(manager.confirmation.is_none());
        assert!(manager.is_open());
    }

    #[test_case(InboundPolicy::Accept, InboundPolicy::Auto, true)]
    #[test_case(InboundPolicy::Auto, InboundPolicy::Hold, true)]
    #[test_case(InboundPolicy::Hold, InboundPolicy::Refuse, true)]
    #[test_case(InboundPolicy::Refuse, InboundPolicy::Auto, false)]
    fn every_numeric_policy_relaxation_requires_confirmation(
        draft: InboundPolicy,
        effective: InboundPolicy,
        relaxation: bool,
    ) {
        let mut manager = manager(PeerView::Held);
        manager.inbox.as_mut().expect(POLICY_DRAFT).inbound = effective;
        manager.handle_key(press(KeyCode::Char('p')));
        manager.policy.as_mut().expect(POLICY_DRAFT).value = draft.clone();
        let action = manager.handle_key(press(KeyCode::Char('a')));
        assert_eq!(manager.confirmation.is_some(), relaxation);
        if relaxation {
            assert!(matches!(action, PeerManagerAction::Consumed));
            draw(&mut manager, WIDE, HEIGHT);
            assert!(
                matches!(manager.handle_key(press(KeyCode::Enter)), PeerManagerAction::SetInbound(policy) if policy == draft)
            );
        } else {
            assert!(matches!(action, PeerManagerAction::SetInbound(policy) if policy == draft));
        }
    }

    #[test]
    fn policy_floor_disables_drafts_and_external_changes_cancel_relaxation() {
        let mut manager = manager(PeerView::Held);
        manager.inbox.as_mut().expect(POLICY_DRAFT).project_floor = InboundPolicy::Hold;
        manager.handle_key(press(KeyCode::Char('p')));
        manager.handle_key(press(KeyCode::Up));
        assert_eq!(
            manager.policy.as_ref().expect(POLICY_DRAFT).value,
            InboundPolicy::Hold
        );
        manager.policy.as_mut().expect(POLICY_DRAFT).value = InboundPolicy::Accept;
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('a'))),
            PeerManagerAction::Consumed
        ));
        assert_eq!(manager.feedback.as_deref(), Some(FLOOR_BLOCKED));
        assert!(policy_rank(&InboundPolicy::Accept) < policy_rank(&InboundPolicy::Hold));
        manager.inbox.as_mut().expect(POLICY_DRAFT).project_floor = InboundPolicy::Accept;
        manager.handle_key(press(KeyCode::Char('a')));
        assert!(manager.confirmation.is_some());
        let mut changed = snapshot(vec![held(FIRST_MESSAGE)]);
        changed.inbound = InboundPolicy::Refuse;
        manager.update_inbox(changed);
        assert!(manager.confirmation.is_none());
    }

    #[test]
    fn stale_mouse_release_does_not_activate_a_replaced_layout() {
        let mut manager = manager(PeerView::Held);
        draw(&mut manager, NARROW, SHORT);
        let control = manager
            .controls
            .iter()
            .find(|(_, command)| command == &Command::Activate)
            .expect(TEST_RENDER)
            .0;
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), control));
        manager.handle_key(press(KeyCode::Tab));
        draw(&mut manager, NARROW, SHORT);
        assert!(matches!(
            manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), control)),
            PeerManagerAction::Consumed
        ));
        assert!(manager.review_request.is_none());
    }

    #[test_case(Command::CopyTarget; "copy_target")]
    #[test_case(Command::Refresh; "refresh")]
    fn narrow_sessions_actions_are_visible_and_clickable(command: Command) {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET)]));
        let screen = draw(&mut manager, NARROW, SHORT);
        assert!(screen.contains("Ctrl+B"));
        assert!(screen.contains("Ctrl+R"));
        let area = manager
            .controls
            .iter()
            .find(|(_, action)| action == &command)
            .unwrap()
            .0;
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        let action = manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area));
        match command {
            Command::CopyTarget => {
                assert!(matches!(action, PeerManagerAction::Copy(target) if target == FIRST_TARGET))
            }
            Command::Refresh => assert!(matches!(action, PeerManagerAction::Refresh)),
            _ => unreachable!(),
        }
    }

    #[test]
    fn resize_invalidation_cancels_pressed_controls_before_another_draw() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET)]));
        draw(&mut manager, NARROW, SHORT);
        let area = manager
            .controls
            .iter()
            .find(|(_, action)| action == &Command::Refresh)
            .unwrap()
            .0;
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
        manager.invalidate_layout();
        assert!(matches!(
            manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)),
            PeerManagerAction::Consumed
        ));
        assert!(manager.controls.is_empty());
        let mut manager = reviewing();
        draw(&mut manager, NARROW, SHORT);
        assert!(manager.review_rendered);
        manager.invalidate_layout();
        assert!(!manager.review_rendered);
        assert!(manager.reviewed_message().is_none());
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('y'))),
            PeerManagerAction::Consumed
        ));
    }

    #[test]
    fn mouse_filter_done_does_not_activate_a_review() {
        let mut manager = manager(PeerView::Held);
        manager.handle_key(press(KeyCode::Char('/')));
        draw(&mut manager, NARROW, SHORT);
        let control = manager
            .controls
            .iter()
            .find(|(_, command)| command == &Command::Activate)
            .expect(TEST_RENDER)
            .0;
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), control));
        manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), control));
        assert!(!manager.filter_focused);
        assert!(manager.review_request.is_none());
    }

    #[test]
    fn outside_click_cancels_policy_without_closing_manager() {
        let mut manager = manager(PeerView::Held);
        manager.handle_key(press(KeyCode::Char('p')));
        draw(&mut manager, WIDE, HEIGHT);
        assert!(!manager.contains(Position::new(0, 0)));
        assert!(matches!(
            manager.handle_mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                Rect::default()
            )),
            PeerManagerAction::Consumed
        ));
        assert!(manager.policy.is_none());
        assert!(manager.is_open());
    }

    #[test_case(false, "line\\n\\r\\t\\u{1b}\\u{85}\\u{202e}\\u{2066}\\u{200f}終" ; "metadata")]
    #[test_case(true, "line\n\\r\\t\\u{1b}\\u{85}\\u{202e}\\u{2066}\\u{200f}終" ; "body")]
    fn terminal_controls_and_bidi_are_literal_but_body_newlines_survive(
        newlines: bool,
        expected: &str,
    ) {
        assert_eq!(
            literal("line\n\r\t\u{1b}\u{85}\u{202e}\u{2066}\u{200f}終", newlines),
            expected
        );
    }

    #[test_case(Color::Black, Color::White ; "light")]
    #[test_case(Color::White, Color::Black ; "dark")]
    fn literal_reader_keeps_markdown_and_theme_roles(foreground: Color, background: Color) {
        let style = Style::default().fg(foreground).bg(background);
        let painted = paint_literal(BODY, style);
        assert_eq!(
            painted
                .lines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            BODY
        );
        assert!(painted.lines().iter().all(|line| line.style == style));
    }

    #[test_case("x"; "long_plain_line")]
    #[test_case("\u{1b}"; "expanded_controls")]
    fn copying_display_continuations_preserves_only_source_newlines(character: &str) {
        let source = format!("{}\n{BODY}", character.repeat(MAX_LITERAL_COLS + 1));
        let expected = literal(&source, true);
        let mut reader = Reader::default();
        reader.set(expected.clone());
        reader.document.select_all();
        assert_eq!(reader.document.selected_text(), Some(expected));
    }

    #[test_case("e\u{301}", 1; "combining_mark")]
    #[test_case("\u{1f44d}\u{1f3fb}", 2; "emoji_modifier")]
    #[test_case("\u{1f1eb}\u{1f1f7}", 2; "flag")]
    fn display_chunks_preserve_boundary_graphemes(grapheme: &str, width: usize) {
        let first = format!("{}{grapheme}", "x".repeat(MAX_LITERAL_COLS - width));
        let painted = paint_literal(&format!("{first}Z"), Style::default());
        assert_eq!(
            painted
                .lines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![first, "Z".to_owned()]
        );
    }

    #[test]
    fn peer_status_refresh_preserves_selected_identity_text() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET)]));
        manager.handle_key(press(KeyCode::Enter));
        draw(&mut manager, NARROW, SHORT);
        manager.session_reader.document.select_all();
        let selection = manager.session_reader.document.selected_text();
        assert!(selection.as_ref().unwrap().contains(FIRST_TARGET));
        let top = manager.session_reader.document.top();
        let mut changed = peer(FIRST_TARGET);
        changed.busy = true;
        changed.inbound = InboundPolicy::Hold;
        manager.set_sessions(Ok(vec![changed]));
        draw(&mut manager, NARROW, SHORT);
        assert_eq!(manager.session_reader.document.selected_text(), selection);
        assert_eq!(manager.session_reader.document.top(), top);
    }

    #[cfg(unix)]
    mod live {
        use std::{
            fs::{self, Permissions},
            os::unix::fs::PermissionsExt,
            sync::{Arc, atomic::AtomicUsize},
        };

        use caudra_agent::{
            AgentMode,
            peers::{PeerDecision, PeerDescriptor, PeerHost, PeerSession},
        };
        use caudra_config::InboundPolicy;
        use caudra_storage::{id::CaudraId, sessions::PermissionMode};
        use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEventKind};
        use tempfile::TempDir;
        use test_case::test_case;

        use super::{
            BODY, Command, NARROW, PeerManager, PeerManagerAction, PeerView, SHORT, TITLE, draw,
            mouse, press,
        };

        const PRIVATE_MODE: u32 = 0o700;
        const REQUEST: &str = "peer-manager-review-test";

        fn reviewing() -> (TempDir, PeerHost, PeerSession, PeerManager) {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), Permissions::from_mode(PRIVATE_MODE)).unwrap();
            let host =
                PeerHost::start_in(directory.path().to_owned(), Arc::new(AtomicUsize::new(0)))
                    .unwrap();
            let descriptor = PeerDescriptor {
                session_id: CaudraId::generate(),
                name: TITLE.into(),
                cwd: directory.path().to_owned(),
                mode: AgentMode::Build,
                permission_mode: PermissionMode::Ask,
                inbound: InboundPolicy::Hold,
                blocked: false,
                busy: false,
            };
            let receiver = host.register(descriptor.clone()).unwrap();
            let sender = host
                .register(PeerDescriptor {
                    session_id: CaudraId::generate(),
                    ..descriptor
                })
                .unwrap();
            let peers = smol::block_on(sender.list()).unwrap();
            let target = &peers
                .iter()
                .find(|peer| peer.session_id == receiver.session_id())
                .unwrap()
                .target;
            smol::block_on(sender.send(target, BODY, None, REQUEST)).unwrap();
            let snapshot = receiver.inbox_snapshot().unwrap();
            let id = snapshot.messages[0].message_id.clone();
            let mut manager = PeerManager::new();
            manager.open(PeerView::Held);
            manager.update_inbox(snapshot);
            assert!(
                matches!(manager.handle_key(press(KeyCode::Enter)), PeerManagerAction::Review(selected) if selected == id)
            );
            manager.set_review(Ok(receiver.review_held(&id).unwrap()));
            assert!(manager.reviewed_message().is_none());
            draw(&mut manager, NARROW, SHORT);
            assert!(manager.reviewed_message().is_some());
            assert!(manager.can_decide(true));
            (directory, host, receiver, manager)
        }

        #[test_case(PeerDecision::Approve; "approve")]
        #[test_case(PeerDecision::Reject; "reject")]
        fn rendered_review_decides_with_real_authority_and_then_disarms(decision: PeerDecision) {
            let (_directory, _host, receiver, mut manager) = reviewing();
            let action = if decision == PeerDecision::Approve {
                manager.handle_key(press(KeyCode::Char('y')))
            } else {
                manager.handle_key(press(KeyCode::Char('n')));
                draw(&mut manager, NARROW, SHORT);
                manager.handle_key(press(KeyCode::Enter))
            };
            let PeerManagerAction::Decide {
                token,
                decision: actual,
            } = action
            else {
                panic!("expected an exact peer decision");
            };
            assert_eq!(actual, decision);
            manager.finish_decision(receiver.decide_held(&token, actual));
            manager.update_inbox(receiver.inbox_snapshot().unwrap());
            draw(&mut manager, NARROW, SHORT);
            assert!(manager.reviewed_message().is_none());
            assert!(!manager.can_decide(true));
            let mut repeat = press(KeyCode::Char('y'));
            repeat.kind = KeyEventKind::Repeat;
            assert!(matches!(
                manager.handle_key(repeat),
                PeerManagerAction::Consumed
            ));
        }

        #[test]
        fn resized_and_stale_reviews_cannot_spend_real_authority() {
            let (_directory, _host, receiver, mut manager) = reviewing();
            let area = manager
                .controls
                .iter()
                .find(|(_, command)| command == &Command::Approve)
                .unwrap()
                .0;
            manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area));
            manager.invalidate_layout();
            assert!(matches!(
                manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), area)),
                PeerManagerAction::Consumed
            ));
            assert!(matches!(
                manager.handle_key(press(KeyCode::Char('y'))),
                PeerManagerAction::Consumed
            ));
            assert!(!receiver.has_pending());
            draw(&mut manager, NARROW, SHORT);
            assert!(manager.can_decide(true));
            receiver.set_inbound(InboundPolicy::Refuse).unwrap();
            manager.update_inbox(receiver.inbox_snapshot().unwrap());
            draw(&mut manager, NARROW, SHORT);
            assert!(!manager.can_decide(false));
            assert!(manager.reviewed_message().is_none());
            assert!(matches!(
                manager.handle_key(press(KeyCode::Char('y'))),
                PeerManagerAction::Consumed
            ));
            assert!(matches!(
                manager.handle_key(press(KeyCode::Char('n'))),
                PeerManagerAction::Consumed
            ));
        }
    }

    #[test]
    fn expanded_control_lines_remain_reachable_beyond_document_pan_limits() {
        let source = literal(&"\u{1b}".repeat(MAX_LITERAL_COLS * 4), true);
        let painted = paint_literal(&source, Style::default());
        assert!(
            painted
                .lines()
                .iter()
                .all(|line| line.width() <= MAX_LITERAL_COLS)
        );
        assert_eq!(
            painted
                .lines()
                .iter()
                .map(ToString::to_string)
                .collect::<String>(),
            source
        );
    }
}
