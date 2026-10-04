use std::collections::BTreeMap;
use std::mem;
use std::time::Duration;

use caudra_agent::peers::topics::{MAX_PATTERNS, add_patterns, pattern_matches, remove_patterns};
use caudra_agent::peers::{
    ChannelMessage, ChannelPage, ChannelSummary, HeldMessageSummary, HeldReview, HistoryVersion,
    MessageChannel, PeerDecision, PeerDecisionResult, PeerInboxSnapshot, PeerReviewToken,
    PeerSummary, deceptive, handle_address, literal,
};
use caudra_config::InboundPolicy;
use caudra_grab::grab_scope;
use caudra_providers::PEER_SCRIPT_SENDER;
use caudra_storage::sessions::StoredPeerControls;
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::components::document_view::{DocumentMouse, DocumentView, MAX_ROWS, Painted};
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
const PATTERN_FIELD_LIMIT: usize = 1024;
const WHEEL_STEP: i32 = 3;
const SESSION_ACTION_COLS: u16 = 16;
const MAX_LITERAL_COLS: usize = 4096;
pub(crate) const HISTORY_POLL: Duration = Duration::from_secs(1);
const REFRESH_TIME_FORMAT: &str = "%H:%M:%S";
const HISTORY_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
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
const NO_REPLY_TARGET: &str = "None; a script takes no replies";
const NO_MATCHES: &str = "No search matches.";
const NO_PEERS: &str = "No eligible live peers. Opt in to messaging in another eligible local Caudra session, then refresh.";
const NO_HELD: &str = "No held messages. Press 3 to browse the stored message history.";
const SELECT_PEER: &str = "Select a peer to inspect its exact target.";
const SELECT_HELD: &str = "Select a held message, then press Enter to review it.";
const PEER_GONE: &str = "This exact peer target is unavailable in the latest discovery snapshot. Select another peer explicitly.";
const ALIAS_SCOPE: &str = "This exact alias belongs to this session's current live registration. It is not a globally shareable address.";
const NAME_LABEL: &str = "Name: ";
const EXACT_TARGET_LABEL: &str = "Exact target: ";
const NAME_IN_USE: &str = " was in use at launch; resuming while it is free reclaims it";
const AUDIENCE_LABEL: &str = "Audience: ";
const TOPICS_LABEL: &str = "Topics: ";
const BROADCASTS_LABEL: &str = "Broadcasts: ";
const GROUPS_LABEL: &str = "Consumer groups: ";
const GROUPS_HINT: &str = " · /groups joins or leaves one";
const NO_TOPICS: &str = "none";
const ON: &str = "on";
const OFF: &str = "off";
const PREVIEW_HINT: &str = "Passive preview. Enter or click these details to review the literal message. Selecting and refreshing do not authorize decisions.";
const TOPIC_KEY: &str = "topic:";
const BROADCAST_KEY: &str = "broadcast";
const DIRECT_KEY: &str = "direct:";
const BROADCASTS_TITLE: &str = "Broadcasts";
const UNKNOWN_SESSION: &str = "Unknown session";
const THIS_SESSION: &str = "this session";
const SUBSCRIBED: &str = "subscribed";
const VIA: &str = "via ";
const RECEIVING: &str = "receiving";
const NOT_RECEIVING: &str = "not receiving";
const MESSAGE_NOUN: &str = "message";
const MESSAGES_NOUN: &str = "messages";
const RECIPIENT_PREFIX: &str = "  ";
const OLDER_HINT: &str = "Older messages are stored. Press o to load them.";
const START_HINT: &str = "Start of this channel's stored messages.";
const LEFT_OUT_HINT: &str = "Earlier stored messages are left out of this view.";
const HINT_ROWS: usize = 1;
const GAP_ROWS: usize = 1;
const NO_HISTORY: &str = "No stored messages yet. Messages this session sends or receives, and every topic and broadcast message, appear here.";
const NO_TOPIC_HISTORY: &str = "No stored topic messages yet. Press 3 to show every channel.";
const SELECT_CHANNEL: &str = "Select a channel to read its stored messages.";
const LOADING_HISTORY: &str = "Loading message history…";
const LOADING_MESSAGES: &str = "Loading messages…";
const NOT_LOADED: &str = "Messages are not loaded. Press Ctrl+R to retry.";
const TOPICS_SCOPE: &str = "Topics only (3 shows every channel) · ";
const NO_NAME: &str =
    "No messaging name. Other sessions reach this one by a word target from their own discovery.";
const NO_SUBSCRIPTIONS: &str = "No topic subscriptions.";
const PATTERN_PROMPT: &str = "+ ";
const PATTERN_PLACEHOLDER: &str = "Add a topic or pattern, such as ci.*";
const SEPARATOR: &str = " · ";
const HISTORY_STATUS: &str = "Stored message history · Ctrl+R Refresh";
const HISTORY_FAILED: &str = "History failed: ";
const RETRY_HINT: &str = " · Ctrl+R Retry";
const PANEL_TITLE: &str = "This session";
const SUBSCRIPTIONS_HINT: &str = "Tab moves between the policy, topics, broadcasts, and the field below. Enter removes the selected topic or turns broadcasts on or off.";
const SUBSCRIBE_LABEL: &str = " Subscribe";
const UNSUBSCRIBE_LABEL: &str = " Unsubscribe";
const BROADCASTS_ON_LABEL: &str = " Receive broadcasts";
const BROADCASTS_OFF_LABEL: &str = " Stop broadcasts";
const NEXT_LABEL: &str = " Next";
const DIRECT_LABEL: &str = "direct";
const LIST_SEPARATOR: &str = ", ";
const POLICIES: [InboundPolicy; 4] = [
    InboundPolicy::Auto,
    InboundPolicy::Accept,
    InboundPolicy::Hold,
    InboundPolicy::Refuse,
];
const BINDINGS: [(Command, KeyCode, KeyModifiers, &str, &str); 15] = [
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
        Command::Messages,
        KeyCode::Char('3'),
        KeyModifiers::NONE,
        "3",
        " Messages",
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
        " This session",
    ),
    (
        Command::Older,
        KeyCode::Char('o'),
        KeyModifiers::NONE,
        "o",
        " Older",
    ),
    (
        Command::Subscribe,
        KeyCode::Char('s'),
        KeyModifiers::NONE,
        "s",
        SUBSCRIBE_LABEL,
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
    Messages,
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
    Subscribe(SubscriptionChange),
    Copy(String),
}

/// One edit to this session's subscriptions, applied to whichever set it
/// holds when the edit lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionChange {
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
    Broadcasts(bool),
}

impl SubscriptionChange {
    /// The whole subscription set after this change, topics then broadcasts.
    pub fn apply(
        &self,
        topics: &[String],
        broadcasts: bool,
    ) -> Result<(Vec<String>, bool), String> {
        match self {
            Self::Subscribe(patterns) => {
                add_patterns(topics, patterns).map(|topics| (topics, broadcasts))
            }
            Self::Unsubscribe(patterns) => {
                remove_patterns(topics, patterns).map(|topics| (topics, broadcasts))
            }
            Self::Broadcasts(on) => Ok((topics.to_vec(), *on)),
        }
    }
}

/// A page of one channel the Messages view waits for: the newest messages,
/// or those before a sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub channel: MessageChannel,
    pub before: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Sessions,
    Held,
    Messages,
    Filter,
    Activate,
    Focus,
    Refresh,
    CopyTarget,
    Policy,
    Older,
    Subscribe,
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
    highlight: Option<usize>,
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
            self.repaint();
        }
    }

    /// Marks source line `row` as the focused control.
    fn highlight(&mut self, row: Option<usize>) {
        if self.highlight != row {
            self.highlight = row;
            self.repaint();
        }
    }

    fn repaint(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let (text, highlight) = (&self.text, self.highlight);
        self.document
            .ensure((self.revision, theme::generation()), 0, || {
                paint_reader(text, highlight)
            });
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let (text, highlight) = (&self.text, self.highlight);
        self.document
            .restyle((self.revision, theme::generation()), || {
                paint_reader(text, highlight)
            });
        let inner = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        let body = Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        };
        self.document.draw(frame, inner, body);
    }
}

struct ReviewPanel {
    summary: HeldMessageSummary,
    token: Option<PeerReviewToken>,
    notice: Option<String>,
    resolved: bool,
}

/// The This session panel: a draft inbound policy, and the controls that
/// edit subscriptions at once.
struct SessionPanel {
    policy: InboundPolicy,
    pending: bool,
    focus: PanelFocus,
    field: TextField,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PanelFocus {
    Policy,
    Pattern(usize),
    Broadcasts,
    Field,
}

/// The panel's document, and the rows its controls landed on.
struct PanelDocument {
    text: String,
    policies: Vec<(usize, InboundPolicy)>,
    targets: Vec<(usize, PanelFocus)>,
}

impl PanelDocument {
    fn focus_row(&self, panel: &SessionPanel) -> Option<usize> {
        match &panel.focus {
            PanelFocus::Policy => self
                .policies
                .iter()
                .find(|(_, policy)| policy == &panel.policy)
                .map(|(row, _)| *row),
            focus => self
                .targets
                .iter()
                .find(|(_, target)| target == focus)
                .map(|(row, _)| *row),
        }
    }
}

/// The Messages view: the channel list, the selected channel's loaded
/// messages, and the loads it waits for. A change to the history while a
/// load is under way lets that load land, then asks once more. A load whose
/// answer was an error is asked again at each version poll until one lands.
#[derive(Default)]
struct History {
    channels: Option<Vec<ChannelRow>>,
    page: Option<LoadedPage>,
    version: Option<HistoryVersion>,
    polling: bool,
    reload: bool,
    reload_again: bool,
    request: Option<PageRequest>,
    refresh: bool,
    version_error: Option<String>,
    channels_error: Option<String>,
    page_error: Option<(PageRequest, String)>,
    topics_only: bool,
}

impl History {
    fn error(&self) -> Option<&str> {
        self.channels_error
            .as_deref()
            .or(self.page_error.as_ref().map(|(_, error)| error.as_str()))
            .or(self.version_error.as_deref())
    }

    /// Asks again for each load whose answer was an error, and reports
    /// whether any was not asked already.
    fn retry(&mut self) -> bool {
        let channels = self.channels_error.is_some() && !self.reload;
        self.reload |= channels;
        let page = self.request.is_none() && self.page_error.is_some();
        if page {
            self.request = self.page_error.as_ref().map(|(request, _)| request.clone());
        }
        channels || page
    }
}

/// A channel as its row shows it, worked out once per channel list or
/// change to this session's subscriptions rather than once per frame.
struct ChannelRow {
    summary: ChannelSummary,
    key: String,
    name: String,
    detail: String,
}

impl ChannelRow {
    fn new(summary: ChannelSummary, controls: Option<&StoredPeerControls>) -> Self {
        Self {
            key: channel_key(&summary.channel),
            name: channel_name(&summary),
            detail: channel_detail(&summary, controls),
            summary,
        }
    }
}

struct LoadedPage {
    channel: MessageChannel,
    messages: BTreeMap<i64, ChannelMessage>,
    older: Option<i64>,
    capped: bool,
}

impl LoadedPage {
    /// Takes a page, and reports whether that changed anything. An older page
    /// adds what lies before the loaded messages. A newest page replaces the
    /// ones it covers, and all of them once it reaches the channel's oldest
    /// message or no longer reaches the loaded ones.
    fn merge(
        &mut self,
        older: bool,
        mut incoming: BTreeMap<i64, ChannelMessage>,
        before: Option<i64>,
    ) -> bool {
        if older {
            let changed = !incoming.is_empty() || self.older != before;
            self.messages.append(&mut incoming);
            self.older = before;
            return changed;
        }
        let newest = self.messages.keys().next_back().copied();
        let cutoff = before
            .filter(|oldest| newest.is_some_and(|newest| newest >= *oldest))
            .unwrap_or(i64::MIN);
        let replaced = self.messages.split_off(&cutoff);
        let older = if self.messages.is_empty() {
            before
        } else {
            self.older
        };
        let changed = replaced != incoming || older != self.older;
        self.messages.append(&mut incoming);
        self.older = older;
        changed
    }

    /// The messages as the reader shows them, oldest first under a line
    /// saying what lies before them. Only the newest that fit in the rows a
    /// document keeps are shown, so the document never cuts them itself.
    fn text(&mut self) -> String {
        let mut remaining = MAX_ROWS - HINT_ROWS;
        let mut blocks = Vec::new();
        self.capped = false;
        for message in self.messages.values().rev() {
            let block = message_block(message);
            let rows = painted_rows(&block) + GAP_ROWS;
            if rows > remaining {
                self.capped = true;
                break;
            }
            remaining -= rows;
            blocks.push(block);
        }
        let hint = if self.capped {
            LEFT_OUT_HINT
        } else if self.older.is_some() {
            OLDER_HINT
        } else {
            START_HINT
        };
        blocks.push(hint.to_owned());
        blocks.reverse();
        blocks.join("\n\n")
    }
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
    peer_controls: Option<StoredPeerControls>,
    messaging_name: Option<String>,
    history: History,
    session_browser: Browser,
    held_browser: Browser,
    message_browser: Browser,
    session_reader: Reader,
    held_reader: Reader,
    message_reader: Reader,
    panel_reader: Reader,
    filter_focused: bool,
    review: Option<ReviewPanel>,
    review_request: Option<(String, u64)>,
    decision_pending: bool,
    panel: Option<SessionPanel>,
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
    panel_hits: Vec<(Rect, PanelFocus)>,
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
            peer_controls: None,
            messaging_name: None,
            history: History::default(),
            session_browser: Browser::default(),
            held_browser: Browser::default(),
            message_browser: Browser::default(),
            session_reader: Reader::default(),
            held_reader: Reader::default(),
            message_reader: Reader::default(),
            panel_reader: Reader::default(),
            filter_focused: false,
            review: None,
            review_request: None,
            decision_pending: false,
            panel: None,
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
            panel_hits: Vec::new(),
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

    /// Narrows the Messages view to topic channels, as `/topics` opens it.
    pub fn show_only_topics(&mut self) {
        if self.open {
            self.active = PeerView::Messages;
            self.history.topics_only = true;
            self.feedback = None;
            self.invalidate_layout();
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn update_controls(&mut self, controls: StoredPeerControls) -> bool {
        if !self.open || self.peer_controls.as_ref() == Some(&controls) {
            return false;
        }
        if let Some(panel) = &mut self.panel
            && let PanelFocus::Pattern(index) = panel.focus
            && index >= controls.topics.len()
        {
            panel.focus = controls
                .topics
                .len()
                .checked_sub(1)
                .map_or(PanelFocus::Broadcasts, PanelFocus::Pattern);
        }
        for row in self.history.channels.iter_mut().flatten() {
            row.detail = channel_detail(&row.summary, Some(&controls));
        }
        self.peer_controls = Some(controls);
        self.invalidate_layout();
        true
    }

    /// The name this session answers to this run, which differs from the
    /// stored one in its controls while another session holds that.
    pub fn update_messaging_name(&mut self, name: Option<String>) -> bool {
        if !self.open || self.messaging_name == name {
            return false;
        }
        self.messaging_name = name;
        self.invalidate_layout();
        true
    }

    pub fn history_visible(&self) -> bool {
        self.open && self.active == PeerView::Messages && self.panel.is_none()
    }

    /// A load the Messages view waits on: a version poll, a channel list, or
    /// a page.
    fn history_loading(&self) -> bool {
        self.history_visible()
            && (self.history.polling || self.history.reload || self.history.request.is_some())
    }

    pub fn wanted_channels(&self) -> bool {
        self.history_visible() && self.history.reload
    }

    pub fn wanted_page(&self) -> Option<&PageRequest> {
        self.history
            .request
            .as_ref()
            .filter(|_| self.history_visible())
    }

    pub fn set_history_polling(&mut self, polling: bool) {
        self.history.polling = polling;
    }

    /// Takes the history's current version. A change, the first version seen
    /// included, reloads the channel list and the selected channel's newest
    /// page; any other answer asks again for each load that failed.
    pub fn set_history_version(&mut self, result: Result<HistoryVersion, String>) -> bool {
        if !self.history_visible() {
            return false;
        }
        let error = result.as_ref().err().map(|error| literal(error, false));
        let mut changed = self.history.version_error != error;
        self.history.version_error = error;
        match result {
            Ok(version) if self.history.version.as_ref() != Some(&version) => {
                self.history.version = Some(version);
                self.mark_stale();
                changed = true;
            }
            _ => changed |= self.history.retry(),
        }
        if changed {
            self.invalidate_layout();
        }
        changed
    }

    /// Takes the channel list asked for, then asks once more if the history
    /// changed while it loaded. A selected channel the list no longer holds
    /// gives way to the first one.
    pub fn set_channels(&mut self, result: Result<Vec<ChannelSummary>, String>) {
        if !self.wanted_channels() {
            return;
        }
        let again = mem::take(&mut self.history.reload_again);
        match result {
            Ok(channels) => {
                let controls = self.peer_controls.as_ref();
                self.history.channels = Some(
                    channels
                        .into_iter()
                        .map(|summary| ChannelRow::new(summary, controls))
                        .collect(),
                );
                self.history.reload = again;
                self.history.channels_error = None;
                if self.selected_row().is_none() {
                    self.message_browser.selected = None;
                    self.clear_page();
                    self.select_first_channel();
                }
            }
            Err(error) => {
                self.history.reload = false;
                self.history.channels_error = Some(literal(&error, false));
            }
        }
        self.invalidate_layout();
    }

    /// Takes the page asked for, then asks for the newest page if the history
    /// changed while it loaded. That waits on an error until the page lands.
    pub fn set_page(&mut self, request: PageRequest, result: Result<ChannelPage, String>) {
        if self.wanted_page() != Some(&request) {
            return;
        }
        self.history.request = None;
        match result {
            Ok(page) => {
                self.history.page_error = None;
                self.merge_page(&request, page.messages, page.before);
                if mem::take(&mut self.history.refresh) {
                    self.history.request = self.newest_page();
                }
            }
            Err(error) => self.history.page_error = Some((request, literal(&error, false))),
        }
        self.invalidate_layout();
    }

    pub fn finish_subscriptions(&mut self, result: Result<String, String>) {
        if !self.open {
            return;
        }
        match (result, &mut self.panel) {
            (Ok(summary), panel) => {
                if let Some(panel) = panel {
                    panel.error = None;
                }
                self.feedback = Some(literal(&summary, false));
            }
            (Err(error), Some(panel)) => panel.error = Some(literal(&error, false)),
            (Err(error), None) => self.feedback = Some(literal(&error, false)),
        }
        self.invalidate_layout();
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
        let Some(panel) = self
            .panel
            .as_mut()
            .filter(|panel| self.open && panel.pending)
        else {
            return;
        };
        panel.pending = false;
        self.feedback = Some(match result {
            Ok(()) => POLICY_SAVED.to_owned(),
            Err(error) => literal(&error, false),
        });
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
            PeerView::Messages => &self.message_browser,
        }
    }

    fn browser_mut(&mut self) -> &mut Browser {
        match self.active {
            PeerView::Sessions => &mut self.session_browser,
            PeerView::Held => &mut self.held_browser,
            PeerView::Messages => &mut self.message_browser,
        }
    }

    fn reader_mut(&mut self) -> &mut Reader {
        if self.panel.is_some() {
            return &mut self.panel_reader;
        }
        match self.active {
            PeerView::Sessions => &mut self.session_reader,
            PeerView::Held => &mut self.held_reader,
            PeerView::Messages => &mut self.message_reader,
        }
    }

    pub(crate) fn invalidate_layout(&mut self) {
        self.review_rendered = false;
        self.confirmation_rendered = false;
        self.controls.clear();
        self.footer_hits.clear();
        self.row_hits.clear();
        self.policy_hits.clear();
        self.panel_hits.clear();
        self.list_area = Rect::default();
        self.detail_area = Rect::default();
    }

    fn selected_row(&self) -> Option<&ChannelRow> {
        let selected = self.message_browser.selected.as_ref()?;
        self.history
            .channels
            .as_ref()?
            .iter()
            .find(|row| &row.key == selected)
    }

    fn selected_channel(&self) -> Option<MessageChannel> {
        self.selected_row().map(|row| row.summary.channel.clone())
    }

    fn select_first_channel(&mut self) {
        if self.active == PeerView::Messages
            && self.message_browser.selected.is_none()
            && let Some((id, _, _)) = self.entries().into_iter().next()
        {
            self.select(id);
        }
    }

    fn newest_page(&self) -> Option<PageRequest> {
        self.selected_channel().map(|channel| PageRequest {
            channel,
            before: None,
        })
    }

    /// The page `o` asks for: none while a page loads, once the oldest
    /// message is loaded, or once the reader holds all the rows it can.
    fn older_page(&self) -> Option<PageRequest> {
        let page = self
            .history
            .page
            .as_ref()
            .filter(|page| !page.capped && self.history.request.is_none())?;
        Some(PageRequest {
            channel: page.channel.clone(),
            before: Some(page.older?),
        })
    }

    /// Reloads what a history change may have touched: the channel list, and
    /// the selected channel's newest page. A load under way lands first, then
    /// is followed by one more. Feedback shown until now goes with the change.
    fn mark_stale(&mut self) {
        self.feedback = None;
        let newest = self.newest_page();
        let history = &mut self.history;
        history.reload_again |= history.reload;
        history.reload = true;
        history.refresh = history.request.is_some();
        if !history.refresh {
            history.request = newest;
        }
    }

    /// Forgets the loaded messages and any page asked for.
    fn clear_page(&mut self) {
        self.history.page = None;
        self.history.request = None;
        self.history.refresh = false;
        self.history.page_error = None;
        self.message_reader = Reader::default();
    }

    fn load_selected(&mut self) {
        let channel = self.selected_channel();
        if self.history.page.as_ref().map(|page| &page.channel) == channel.as_ref() {
            return;
        }
        self.clear_page();
        self.history.request = self.newest_page();
    }

    /// Adds a page of `request`'s channel to what the reader shows, unless it
    /// changes nothing. The first page lands at the newest message and
    /// follows it. An older page adds rows only above the loaded messages, so
    /// they stay where they were on screen.
    fn merge_page(
        &mut self,
        request: &PageRequest,
        messages: Vec<ChannelMessage>,
        before: Option<i64>,
    ) {
        let incoming = messages
            .into_iter()
            .map(|message| (message.seq, message))
            .collect();
        let rows = self.message_reader.document.rows();
        let (page, first) = match &mut self.history.page {
            Some(page) if page.channel == request.channel => {
                if !page.merge(request.before.is_some(), incoming, before) {
                    return;
                }
                (page, false)
            }
            slot => (
                slot.insert(LoadedPage {
                    channel: request.channel.clone(),
                    messages: incoming,
                    older: before,
                    capped: false,
                }),
                true,
            ),
        };
        let text = page.text();
        if first {
            self.message_reader.set(text);
            self.message_reader.document.follow();
        } else {
            self.message_reader.replace(text);
            if request.before.is_some() {
                let added = self.message_reader.document.rows() - rows;
                self.message_reader.document.insert_above(added);
            }
        }
    }

    /// What `s` does to the selected channel: a topic is subscribed to or
    /// unsubscribed from exactly, and broadcasts are turned on or off.
    fn subscription_toggle(&self) -> Option<SubscriptionChange> {
        let controls = self
            .peer_controls
            .as_ref()
            .filter(|_| self.active == PeerView::Messages && self.panel.is_none())?;
        match self.selected_channel()? {
            MessageChannel::Topic(topic) if controls.topics.contains(&topic) => {
                Some(SubscriptionChange::Unsubscribe(vec![topic]))
            }
            MessageChannel::Topic(topic) => Some(SubscriptionChange::Subscribe(vec![topic])),
            MessageChannel::Broadcast => Some(SubscriptionChange::Broadcasts(!controls.broadcasts)),
            MessageChannel::Direct(_) => None,
        }
    }

    /// What Enter does from the panel's focus: removes the focused topic,
    /// turns broadcasts over, or subscribes to the field's patterns.
    fn panel_change(&self) -> Option<SubscriptionChange> {
        let (panel, controls) = (self.panel.as_ref()?, self.peer_controls.as_ref()?);
        match panel.focus {
            PanelFocus::Policy => None,
            PanelFocus::Pattern(index) => controls
                .topics
                .get(index)
                .map(|pattern| SubscriptionChange::Unsubscribe(vec![pattern.clone()])),
            PanelFocus::Broadcasts => Some(SubscriptionChange::Broadcasts(!controls.broadcasts)),
            PanelFocus::Field => Some(SubscriptionChange::Subscribe(
                panel
                    .field
                    .text()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            )),
        }
    }

    fn activate_panel(&mut self) -> PeerManagerAction {
        let (Some(change), Some(controls)) = (self.panel_change(), &self.peer_controls) else {
            return PeerManagerAction::Consumed;
        };
        let Some(panel) = &mut self.panel else {
            return PeerManagerAction::Consumed;
        };
        if let SubscriptionChange::Subscribe(patterns) = &change {
            if patterns.is_empty() {
                return PeerManagerAction::Consumed;
            }
            if let Err(error) = change.apply(&controls.topics, controls.broadcasts) {
                panel.error = Some(literal(&error, false));
                self.invalidate_layout();
                return PeerManagerAction::Consumed;
            }
            panel.field.clear();
        }
        panel.error = None;
        self.freshness.barrier();
        self.invalidate_layout();
        PeerManagerAction::Subscribe(change)
    }

    /// Moves the panel's focus through the policy, each topic, broadcasts,
    /// and the field, in that order, around the ends only when `wrap`.
    fn move_panel_focus(&mut self, delta: isize, wrap: bool) {
        let topics = self
            .peer_controls
            .as_ref()
            .map(|controls| controls.topics.len());
        let Some(panel) = &mut self.panel else {
            return;
        };
        let mut targets = vec![PanelFocus::Policy];
        if let Some(topics) = topics {
            targets.extend((0..topics).map(PanelFocus::Pattern));
            targets.extend([PanelFocus::Broadcasts, PanelFocus::Field]);
        }
        let count = targets.len();
        let current = targets
            .iter()
            .position(|target| target == &panel.focus)
            .unwrap_or(0);
        let next = if wrap {
            Some((current + count).saturating_add_signed(delta) % count)
        } else {
            current
                .checked_add_signed(delta)
                .filter(|next| *next < count)
        };
        if let Some(next) = next {
            panel.focus = targets.swap_remove(next);
            self.reveal_panel_focus();
            self.invalidate_layout();
        }
    }

    fn reveal_panel_focus(&mut self) {
        let row = self.panel_document().and_then(|document| {
            self.panel
                .as_ref()
                .and_then(|panel| document.focus_row(panel))
        });
        if let Some(row) = row {
            self.panel_reader.document.reveal(row);
        }
    }

    /// The panel as text, with the rows its policies and controls landed on.
    fn panel_document(&self) -> Option<PanelDocument> {
        let (panel, inbox) = (self.panel.as_ref()?, self.inbox.as_ref()?);
        let override_label = inbox.inbound_override.as_ref().map_or("None", policy_label);
        let name = match (&self.peer_controls, &self.messaging_name) {
            (None, _) => format!("{NAME_LABEL}{UNAVAILABLE}"),
            (Some(_), None) => NO_NAME.to_owned(),
            (Some(controls), Some(name)) => {
                let mut line = format!("{NAME_LABEL}{}", literal(&handle_address(name), false));
                if let Some(stored) = controls.handle.as_ref().filter(|stored| *stored != name) {
                    line.push_str(SEPARATOR);
                    line.push_str(&literal(&handle_address(stored), false));
                    line.push_str(NAME_IN_USE);
                }
                line
            }
        };
        let mut lines = vec![
            PANEL_TITLE.to_owned(),
            name,
            String::new(),
            format!(
                "Inbound policy · effective {}",
                policy_label(&inbox.inbound)
            ),
            format!("Session override: {override_label}"),
            format!(
                "Project floor: {} (options below it are disabled)",
                policy_label(&inbox.project_floor)
            ),
            String::new(),
        ];
        let mut policies = Vec::new();
        for policy in POLICIES {
            let disabled = policy_rank(&policy) < policy_rank(&inbox.project_floor);
            policies.push((lines.len(), policy.clone()));
            lines.push(format!(
                "{} {}{}",
                if panel.policy == policy { ">" } else { " " },
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
            if panel.pending {
                "Applying policy…"
            } else {
                "Up/Down select a draft; a Apply changes this session. Esc cancels."
            }
            .to_owned(),
        );
        lines.push("Refuse rejects new arrivals; it does not delete held messages.".to_owned());
        let mut targets = Vec::new();
        if let Some(controls) = &self.peer_controls {
            lines.push(String::new());
            lines.push(format!(
                "{TOPICS_LABEL}{} of {MAX_PATTERNS}",
                controls.topics.len()
            ));
            if controls.topics.is_empty() {
                lines.push(format!("{RECIPIENT_PREFIX}{NO_SUBSCRIPTIONS}"));
            }
            for (index, pattern) in controls.topics.iter().enumerate() {
                targets.push((lines.len(), PanelFocus::Pattern(index)));
                lines.push(format!("{RECIPIENT_PREFIX}{}", literal(pattern, false)));
            }
            targets.push((lines.len(), PanelFocus::Broadcasts));
            lines.push(format!(
                "{BROADCASTS_LABEL}{}",
                if controls.broadcasts { ON } else { OFF }
            ));
            lines.push(format!(
                "{GROUPS_LABEL}{}{GROUPS_HINT}",
                literal_list(&controls.groups)
            ));
            lines.push(String::new());
            lines.push(SUBSCRIPTIONS_HINT.to_owned());
        }
        Some(PanelDocument {
            text: lines.join("\n"),
            policies,
            targets,
        })
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

    /// The rows of the active view as id, name, and detail, matched against
    /// the filter. A channel's id names its direct party's session, so only
    /// what a row shows is searched.
    fn entries(&self) -> Vec<(String, String, String)> {
        let query = self.browser().filter.text().to_lowercase();
        let entries = match self.active {
            PeerView::Sessions => self
                .sessions
                .iter()
                .flatten()
                .map(|peer| {
                    let status = format!(
                        "{} · {} · {}",
                        activity(peer),
                        policy_label(&peer.inbound),
                        literal(&peer.cwd.to_string_lossy(), false)
                    );
                    let detail = match peer.handle_address() {
                        Some(address) => format!("{}{SEPARATOR}{status}", literal(&address, false)),
                        None => status,
                    };
                    let name = literal(&peer.title, false);
                    let search = format!(
                        "{} {name} {detail} {} {}",
                        peer.target,
                        peer.topics.join(" "),
                        if peer.broadcasts {
                            BROADCASTS_TITLE
                        } else {
                            ""
                        }
                    );
                    (peer.target.clone(), name, detail, search)
                })
                .collect::<Vec<_>>(),
            PeerView::Held => self
                .inbox
                .iter()
                .flat_map(|inbox| &inbox.messages)
                .map(|held| {
                    let name = held_sender(held);
                    let detail = format!(
                        "{}{SEPARATOR}{}{SEPARATOR}{}",
                        literal(&held.audience.to_string(), false),
                        literal(&held.message_id, false),
                        literal(&held.reason, false)
                    );
                    let search = format!("{} {name} {detail}", held.message_id);
                    (held.message_id.clone(), name, detail, search)
                })
                .collect::<Vec<_>>(),
            PeerView::Messages => self
                .history
                .channels
                .iter()
                .flatten()
                .filter(|row| {
                    !self.history.topics_only
                        || matches!(row.summary.channel, MessageChannel::Topic(_))
                })
                .map(|row| {
                    let search = format!("{} {}", row.name, row.detail);
                    (
                        row.key.clone(),
                        row.name.clone(),
                        row.detail.clone(),
                        search,
                    )
                })
                .collect::<Vec<_>>(),
        };
        entries
            .into_iter()
            .filter(|(_, _, _, search)| query.is_empty() || search.to_lowercase().contains(&query))
            .map(|(id, name, detail, _)| (id, name, detail))
            .collect()
    }

    fn select(&mut self, selected: String) {
        if self.browser().selected.as_ref() == Some(&selected) {
            return;
        }
        self.browser_mut().selected = Some(selected);
        self.review_request = None;
        match self.active {
            PeerView::Held => {
                if let Some(review) = &mut self.review {
                    review.token = None;
                }
                if !self.review.as_ref().is_some_and(|review| review.resolved)
                    && !self.decision_pending
                {
                    self.review = None;
                    self.held_reader = Reader::default();
                }
            }
            PeerView::Sessions => self.session_reader = Reader::default(),
            PeerView::Messages => {
                self.feedback = None;
                self.load_selected();
            }
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
        if self.field_focused() {
            if event.kind == KeyEventKind::Repeat
                && matches!(event.code, KeyCode::Esc | KeyCode::Enter)
            {
                return PeerManagerAction::Consumed;
            }
            return self.field_key(event);
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
            && (self.panel.is_some() || self.browser().pane == Pane::Detail)
        {
            self.reader_mut().document.select_all();
            return PeerManagerAction::Consumed;
        }
        if let Some(panel) = &self.panel {
            match (event.code, &panel.focus) {
                (KeyCode::Up, PanelFocus::Policy) => {
                    self.step_policy(-1);
                }
                (KeyCode::Down, PanelFocus::Policy) => {
                    if !self.step_policy(1) {
                        self.move_panel_focus(1, false);
                    }
                }
                (KeyCode::Up, _) => self.move_panel_focus(-1, false),
                (KeyCode::Down, _) => self.move_panel_focus(1, false),
                (KeyCode::BackTab, _) => self.move_panel_focus(-1, true),
                _ => {
                    self.panel_reader.document.handle_scroll_key(event);
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

    fn field_focused(&self) -> bool {
        self.confirmation.is_none()
            && self
                .panel
                .as_ref()
                .is_some_and(|panel| panel.focus == PanelFocus::Field)
    }

    /// Keys for the pattern field, which takes printable shortcuts as text.
    fn field_key(&mut self, event: KeyEvent) -> PeerManagerAction {
        let Some(panel) = &mut self.panel else {
            return PeerManagerAction::Consumed;
        };
        match event.code {
            KeyCode::Enter => return self.activate_panel(),
            KeyCode::Esc if panel.field.is_empty() => return self.command(Command::Back),
            KeyCode::Esc => {
                panel.field.clear();
                panel.error = None;
            }
            KeyCode::Tab => self.move_panel_focus(1, true),
            KeyCode::BackTab => self.move_panel_focus(-1, true),
            KeyCode::Up => self.move_panel_focus(-1, false),
            KeyCode::Char(character) if deceptive(character) => {}
            _ => {
                panel.error = None;
                if let TextKey::Copy(text) | TextKey::Cut(text) = panel.field.handle_key(event) {
                    self.invalidate_layout();
                    return PeerManagerAction::Copy(text);
                }
            }
        }
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
        } else if self.field_focused()
            && let Some(panel) = &mut self.panel
        {
            panel.field.paste(&literal(text, false));
            panel.error = None;
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
        if self.panel.is_some() {
            return match command {
                Command::Back if !self.panel.as_ref().is_some_and(|panel| panel.pending) => {
                    self.panel = None;
                    self.invalidate_layout();
                    PeerManagerAction::Consumed
                }
                Command::Apply => self.apply_policy(),
                Command::Activate => self.activate_panel(),
                Command::Focus => {
                    self.move_panel_focus(1, true);
                    PeerManagerAction::Consumed
                }
                _ => PeerManagerAction::Consumed,
            };
        }
        match command {
            Command::Sessions | Command::Held | Command::Messages => {
                let view = match command {
                    Command::Sessions => PeerView::Sessions,
                    Command::Held => PeerView::Held,
                    _ => PeerView::Messages,
                };
                if view == PeerView::Messages && self.active == PeerView::Messages {
                    self.history.topics_only = false;
                }
                self.active = view;
                self.filter_focused = false;
                self.review_request = None;
                self.feedback = None;
                self.select_first_channel();
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
            Command::Refresh if self.active == PeerView::Messages => {
                self.history.channels_error = None;
                self.history.page_error = None;
                self.mark_stale();
                self.invalidate_layout();
            }
            Command::Refresh => return PeerManagerAction::Refresh,
            Command::CopyTarget => {
                let target = match self.active {
                    PeerView::Sessions => self.selected_peer().map(|peer| peer.target.clone()),
                    PeerView::Held => self
                        .review
                        .as_ref()
                        .map(|review| review.summary.reply_target.clone())
                        .or_else(|| self.selected_held().map(|held| held.reply_target.clone()))
                        .filter(|target| !target.is_empty()),
                    PeerView::Messages => None,
                };
                return target.map_or(PeerManagerAction::Consumed, PeerManagerAction::Copy);
            }
            Command::Policy => {
                if let Some(inbox) = &self.inbox {
                    self.panel = Some(SessionPanel {
                        policy: inbox.inbound.clone(),
                        pending: false,
                        focus: PanelFocus::Policy,
                        field: TextField::new(FieldKind::Line).limited_to(PATTERN_FIELD_LIMIT),
                        error: None,
                    });
                    self.panel_reader = Reader::default();
                    self.feedback = None;
                    self.invalidate_layout();
                }
            }
            Command::Older => {
                if let Some(request) = self.older_page().filter(|_| self.history_visible()) {
                    self.history.request = Some(request);
                    self.invalidate_layout();
                }
            }
            Command::Subscribe => {
                if let Some(change) = self.subscription_toggle() {
                    self.freshness.barrier();
                    return PeerManagerAction::Subscribe(change);
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
        if self.active != PeerView::Held {
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
            && self.panel.is_none()
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

    /// Moves the draft to the next policy the project floor allows, and
    /// whether there was one. A pending draft holds still but keeps the key.
    fn step_policy(&mut self, delta: isize) -> bool {
        let (Some(panel), Some(inbox)) = (&mut self.panel, &self.inbox) else {
            return false;
        };
        if panel.pending {
            return true;
        }
        let mut index = POLICIES
            .iter()
            .position(|policy| policy == &panel.policy)
            .unwrap_or(0);
        while let Some(next) = index
            .checked_add_signed(delta)
            .filter(|next| *next < POLICIES.len())
        {
            index = next;
            if policy_rank(&POLICIES[index]) >= policy_rank(&inbox.project_floor) {
                panel.policy = POLICIES[index].clone();
                self.reveal_panel_focus();
                self.invalidate_layout();
                return true;
            }
        }
        false
    }

    fn apply_policy(&mut self) -> PeerManagerAction {
        let (Some(panel), Some(inbox)) = (&self.panel, &self.inbox) else {
            return PeerManagerAction::Consumed;
        };
        if panel.pending {
            return PeerManagerAction::Consumed;
        }
        if policy_rank(&panel.policy) < policy_rank(&inbox.project_floor) {
            self.feedback = Some(FLOOR_BLOCKED.to_owned());
            return PeerManagerAction::Consumed;
        }
        let policy = panel.policy.clone();
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
        if let Some(panel) = &mut self.panel {
            panel.pending = true;
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
        if self.panel.is_none() && self.browser().pane == Pane::List {
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
            if self.confirmation.is_some() || self.panel.is_some() {
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
                if let (Some(panel), Some(inbox)) = (&mut self.panel, &self.inbox)
                    && !panel.pending
                    && policy_rank(&policy) >= policy_rank(&inbox.project_floor)
                {
                    panel.policy = policy;
                    panel.focus = PanelFocus::Policy;
                    self.invalidate_layout();
                }
                return PeerManagerAction::Consumed;
            }
            if let Some((_, focus)) = self
                .panel_hits
                .iter()
                .find(|(area, _)| area.contains(position))
            {
                let focus = focus.clone();
                if let Some(panel) = &mut self.panel {
                    panel.focus = focus;
                }
                self.invalidate_layout();
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
            if self.detail_area.contains(position) && self.panel.is_none() {
                self.browser_mut().pane = Pane::Detail;
                if self.active == PeerView::Held
                    && self.review.is_none()
                    && self.review_request.is_none()
                {
                    return self.activate();
                }
            }
        }
        if self.panel.is_some() || self.browser().pane == Pane::Detail {
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
        self.panel_hits.clear();
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
        if self.confirmation.is_none() && self.panel.is_none() && content.height > MIN_BODY_ROWS {
            let tabs = take_top(&mut content, 1);
            self.draw_commands(
                frame,
                tabs,
                &[Command::Sessions, Command::Held, Command::Messages],
                &mut controls,
            );
        }
        if content.height >= OPTIONAL_CHROME_ROWS && self.confirmation.is_none() {
            let status = take_top(&mut content, 1);
            let [status, action] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(SESSION_ACTION_COLS)])
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
            if self.panel.is_none() && !self.filter_focused && self.inbox.is_some() {
                self.draw_commands(frame, action, &[Command::Policy], &mut controls);
            }
        }
        let filter_visible = self.filter_focused || !self.browser().filter.is_empty();
        if filter_visible
            && self.panel.is_none()
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
        let status = match self.active {
            _ if self.panel.is_some() => None,
            PeerView::Sessions => Some(self.discovery_status()),
            PeerView::Held => None,
            PeerView::Messages => Some(self.history_status()),
        };
        let feedback = if self.history_visible() && self.history.error().is_some() {
            status
        } else {
            self.feedback.clone().or(status)
        };
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
        } else if self.panel.is_some() {
            self.detail_area = content;
            self.draw_panel(frame, content);
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

    fn history_status(&self) -> String {
        if let Some(error) = self.history.error() {
            return format!("{HISTORY_FAILED}{error}{RETRY_HINT}");
        }
        let scope = if self.history.topics_only {
            TOPICS_SCOPE
        } else {
            ""
        };
        let status = if self.history.channels.is_some() {
            HISTORY_STATUS
        } else {
            LOADING_HISTORY
        };
        format!("{scope}{status}")
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
                PeerView::Messages if self.history.channels.is_none() => self.history_status(),
                PeerView::Messages if self.message_browser.filter.is_empty() => {
                    if self.history.topics_only {
                        NO_TOPIC_HISTORY
                    } else {
                        NO_HISTORY
                    }
                    .to_owned()
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
                        |target| format!("{PEER_GONE}\n\n{EXACT_TARGET_LABEL}{}", literal(target, false)),
                    )
                },
                |peer| {
                    let address = match peer.handle_address() {
                        Some(address) => format!("{NAME_LABEL}{}", literal(&address, false)),
                        None => format!("{EXACT_TARGET_LABEL}{}", literal(&peer.target, false)),
                    };
                    let mut text = format!(
                        "{}\n\n{address}\n{TOPICS_LABEL}{}\n{BROADCASTS_LABEL}{}\n{GROUPS_LABEL}{}\nWorkspace: {}",
                        literal(&peer.title, false),
                        literal_list(&peer.topics),
                        if peer.broadcasts { ON } else { OFF },
                        literal_list(&peer.groups),
                        literal(&peer.cwd.to_string_lossy(), false),
                    );
                    if peer.handle.is_none() {
                        text.push_str("\n\n");
                        text.push_str(ALIAS_SCOPE);
                    }
                    text
                },
            );
            self.session_reader.replace(text);
            self.session_reader.draw(frame, area);
            return;
        }
        if self.active == PeerView::Messages {
            let header = self
                .selected_row()
                .map(|row| format!("{}\n{}", row.name, row.detail));
            if let Some(header) = &header {
                let rows = wrapped_height(header, area.width)
                    .min(area.height.saturating_sub(MIN_BODY_ROWS));
                let header_area = take_top(&mut area, rows);
                frame.render_widget(
                    Paragraph::new(header.as_str())
                        .style(theme::current().tool_dim)
                        .wrap(Wrap { trim: false }),
                    header_area,
                );
            }
            let loading = self.history.request.is_some() && self.history.page_error.is_none();
            let placeholder = match (&header, &self.history.page) {
                (None, _) => Some(SELECT_CHANNEL),
                (Some(_), Some(_)) => None,
                (Some(_), None) if loading => Some(LOADING_MESSAGES),
                (Some(_), None) => Some(NOT_LOADED),
            };
            if let Some(placeholder) = placeholder {
                self.message_reader.set(placeholder.to_owned());
            }
            self.message_reader.draw(frame, area);
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

    /// The panel's document above the pattern field, with an error from the
    /// last subscription change between them.
    fn draw_panel(&mut self, frame: &mut Frame, mut area: Rect) {
        let (Some(document), Some(panel)) = (self.panel_document(), &self.panel) else {
            return;
        };
        let focus_row = document.focus_row(panel);
        let field_area = if self.peer_controls.is_some() {
            take_bottom(&mut area, 1)
        } else {
            Rect::default()
        };
        if let Some(error) = &panel.error {
            let rows =
                wrapped_height(error, area.width).min(area.height.saturating_sub(MIN_BODY_ROWS));
            let error_area = take_bottom(&mut area, rows);
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .style(theme::current().tool_warning)
                    .wrap(Wrap { trim: false }),
                error_area,
            );
        }
        if field_area.height > 0 {
            let theme = theme::current();
            let mut spans = vec![Span::styled(PATTERN_PROMPT, theme.accent)];
            spans.extend(
                panel
                    .field
                    .paint(
                        usize::from(field_area.width).saturating_sub(PATTERN_PROMPT.width()),
                        &field_styles(input_text_style()),
                        panel.focus == PanelFocus::Field,
                        PATTERN_PLACEHOLDER,
                    )
                    .spans,
            );
            frame.render_widget(Paragraph::new(Line::from(spans)), field_area);
            self.panel_hits.push((field_area, PanelFocus::Field));
        }
        self.panel_reader.replace(document.text);
        self.panel_reader.highlight(focus_row);
        self.panel_reader.draw(frame, area);
        let visible = self.panel_reader.document.visible();
        let row_area = |row: usize| {
            Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row - visible.start).unwrap_or(u16::MAX)),
                area.width.saturating_sub(1),
                1,
            )
        };
        for (row, policy) in document.policies {
            if visible.contains(&row) {
                self.policy_hits.push((row_area(row), policy));
            }
        }
        for (row, target) in document.targets {
            if visible.contains(&row) {
                self.panel_hits.push((row_area(row), target));
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
        if self.panel.is_some() {
            let mut commands = Vec::new();
            if self.panel_change().is_some() {
                commands.push(Command::Activate);
            }
            if !self.field_focused() {
                commands.push(Command::Apply);
            }
            commands.extend([Command::Focus, Command::Back]);
            return commands;
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
        match self.active {
            PeerView::Sessions => commands.extend([Command::CopyTarget, Command::Refresh]),
            PeerView::Held => {}
            PeerView::Messages => {
                if self.subscription_toggle().is_some() {
                    commands.push(Command::Subscribe);
                }
                if self.older_page().is_some() {
                    commands.push(Command::Older);
                }
                commands.push(Command::Refresh);
            }
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
                (Command::Sessions, PeerView::Sessions)
                    | (Command::Held, PeerView::Held)
                    | (Command::Messages, PeerView::Messages)
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
                Command::Activate if self.panel.is_some() => self
                    .panel_change()
                    .map_or(*description, |change| change_label(&change))
                    .to_owned(),
                Command::Activate if self.active == PeerView::Held => " Review".to_owned(),
                Command::Focus if self.panel.is_some() => NEXT_LABEL.to_owned(),
                Command::Subscribe => self
                    .subscription_toggle()
                    .map_or(*description, |change| change_label(&change))
                    .to_owned(),
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
        Cadence::any([
            Cadence::when(self.open && self.discovering, Cadence::PENDING),
            Cadence::when(self.history_visible(), Cadence::polling(HISTORY_POLL)),
            Cadence::when(self.history_loading(), Cadence::PENDING),
        ])
    }
}

/// `values` escaped and joined, or `none`.
fn literal_list(values: &[String]) -> String {
    if values.is_empty() {
        return NO_TOPICS.to_owned();
    }
    values
        .iter()
        .map(|value| literal(value, false))
        .collect::<Vec<_>>()
        .join(LIST_SEPARATOR)
}

fn change_label(change: &SubscriptionChange) -> &'static str {
    match change {
        SubscriptionChange::Subscribe(_) => SUBSCRIBE_LABEL,
        SubscriptionChange::Unsubscribe(_) => UNSUBSCRIBE_LABEL,
        SubscriptionChange::Broadcasts(true) => BROADCASTS_ON_LABEL,
        SubscriptionChange::Broadcasts(false) => BROADCASTS_OFF_LABEL,
    }
}

/// A channel's row id. A direct channel's names the other party's session,
/// so it is never shown.
fn channel_key(channel: &MessageChannel) -> String {
    match channel {
        MessageChannel::Topic(topic) => format!("{TOPIC_KEY}{topic}"),
        MessageChannel::Broadcast => BROADCAST_KEY.to_owned(),
        MessageChannel::Direct(party) => format!("{DIRECT_KEY}{party}"),
    }
}

fn channel_name(summary: &ChannelSummary) -> String {
    match &summary.channel {
        MessageChannel::Topic(topic) => literal(topic, false),
        MessageChannel::Broadcast => BROADCASTS_TITLE.to_owned(),
        MessageChannel::Direct(_) => match (&summary.name, &summary.handle) {
            (Some(title), _) => literal(title, false),
            (None, Some(handle)) => literal(&handle_address(handle), false),
            (None, None) => UNKNOWN_SESSION.to_owned(),
        },
    }
}

/// A channel's message count and latest time, and how this session receives
/// it under `controls`. A direct party's messaging name leads, since a list
/// row clips its title line.
fn channel_detail(summary: &ChannelSummary, controls: Option<&StoredPeerControls>) -> String {
    let noun = if summary.count == 1 {
        MESSAGE_NOUN
    } else {
        MESSAGES_NOUN
    };
    let address = match (&summary.channel, &summary.name, &summary.handle) {
        (MessageChannel::Direct(_), Some(_), Some(handle)) => {
            format!("{}{SEPARATOR}", literal(&handle_address(handle), false))
        }
        _ => String::new(),
    };
    let mut detail = format!(
        "{address}{} {noun}{SEPARATOR}{}",
        summary.count,
        local_time(summary.last_ms)
    );
    let marker = match &summary.channel {
        MessageChannel::Topic(topic) => controls.and_then(|controls| {
            if controls.topics.contains(topic) {
                return Some(SUBSCRIBED.to_owned());
            }
            controls
                .topics
                .iter()
                .find(|pattern| pattern_matches(pattern, topic))
                .map(|pattern| format!("{VIA}{}", literal(pattern, false)))
        }),
        MessageChannel::Broadcast => controls.map(|controls| {
            if controls.broadcasts {
                RECEIVING
            } else {
                NOT_RECEIVING
            }
            .to_owned()
        }),
        MessageChannel::Direct(_) => Some(DIRECT_LABEL.to_owned()),
    };
    if let Some(marker) = marker {
        detail.push_str(SEPARATOR);
        detail.push_str(&marker);
    }
    detail
}

/// One stored message: who sent it when to which audience, its literal
/// text, and what became of it for each recipient.
fn message_block(message: &ChannelMessage) -> String {
    let mut sender = literal(&message.sender_name, false);
    if let Some(handle) = &message.sender_handle {
        sender.push(' ');
        sender.push_str(&literal(&handle_address(handle), false));
    }
    if message.external {
        sender.push_str(&format!(" ({PEER_SCRIPT_SENDER})"));
    }
    if message.own {
        sender = format!("{sender} ({THIS_SESSION})");
    }
    let mut block = format!(
        "{sender}{SEPARATOR}{}{SEPARATOR}{}\n{}",
        local_time(message.sent_ms),
        literal(&message.audience.to_string(), false),
        literal(&message.text, true)
    );
    for recipient in &message.recipients {
        let mut name = match &recipient.name {
            _ if recipient.own => THIS_SESSION.to_owned(),
            Some(name) => literal(name, false),
            None => UNKNOWN_SESSION.to_owned(),
        };
        if let Some(handle) = recipient.handle.as_ref().filter(|_| !recipient.own) {
            name.push(' ');
            name.push_str(&literal(&handle_address(handle), false));
        }
        block.push_str(&format!(
            "\n{RECIPIENT_PREFIX}{name}: {}",
            literal(&recipient.status, false)
        ));
        if let Some(reason) = &recipient.reason {
            block.push_str(SEPARATOR);
            block.push_str(&literal(reason, false));
        }
    }
    block
}

fn local_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map_or_else(
            || UNAVAILABLE.to_owned(),
            |timestamp| {
                timestamp
                    .to_zoned(TimeZone::system())
                    .strftime(HISTORY_TIME_FORMAT)
                    .to_string()
            },
        )
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
            "Delivers from any local session, subject to rate limits and safety boundaries; every accepted message may start a billable turn."
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

fn held_sender(held: &HeldMessageSummary) -> String {
    let name = literal(&held.sender_name, false);
    if held.external {
        format!("{name} ({PEER_SCRIPT_SENDER})")
    } else {
        name
    }
}

/// A script's mode is a placeholder, so its identity names the script
/// instead.
fn held_identity(held: &HeldMessageSummary) -> String {
    let (reply_target, mode) = if held.external {
        (NO_REPLY_TARGET.to_owned(), PEER_SCRIPT_SENDER.to_owned())
    } else if held.mode.is_empty() {
        (literal(&held.reply_target, false), UNAVAILABLE.to_owned())
    } else {
        (
            literal(&held.reply_target, false),
            literal(&held.mode, false),
        )
    };
    format!(
        "Sender: {}\nExact reply target: {reply_target}\n{AUDIENCE_LABEL}{}\nWorkspace: {}\nMode: {mode}\nMessage: {}",
        held_sender(held),
        literal(&held.audience.to_string(), false),
        held.workspace.as_ref().map_or_else(
            || UNAVAILABLE.to_owned(),
            |path| literal(&path.to_string_lossy(), false)
        ),
        literal(&held.message_id, false)
    )
}

/// A reader's text in the item style, with source line `highlight` in the
/// selected one.
fn paint_reader(text: &str, highlight: Option<usize>) -> Painted {
    let theme = theme::current();
    paint_literal(text, |row| {
        if highlight == Some(row) {
            theme.item_selected
        } else {
            theme.item
        }
    })
}

/// `text` line by line in `style` of each source line, cut into chunks a
/// document can pan across.
fn paint_literal(text: &str, style: impl Fn(usize) -> Style) -> Painted {
    let mut lines = Vec::new();
    let mut continuations = Vec::new();
    for (row, line) in text.split('\n').enumerate() {
        let style = style(row);
        let mut start = 0;
        for end in chunk_ends(line) {
            lines.push(Line::styled(line[start..end].to_owned(), style));
            continuations.push(start > 0);
            start = end;
        }
    }
    Painted::new(lines, Vec::new(), Vec::new()).with_continuations(continuations)
}

/// The rows [`paint_literal`] paints `text` as.
fn painted_rows(text: &str) -> usize {
    text.split('\n').map(|line| chunk_ends(line).count()).sum()
}

/// Where each chunk of `line` a document can pan across ends, as a byte
/// offset.
fn chunk_ends(line: &str) -> impl Iterator<Item = usize> + '_ {
    let mut columns = 0;
    line.grapheme_indices(true)
        .filter_map(move |(index, grapheme)| {
            let width = grapheme.width();
            let cut = columns + width > MAX_LITERAL_COLS;
            columns = if cut { width } else { columns + width };
            cut.then_some(index)
        })
        .chain([line.len()])
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

    use caudra_agent::peers::topics::{INVALID_PATTERN, MISSING_PATTERN};
    use caudra_agent::peers::{
        ChannelMessage, ChannelPage, ChannelSummary, HeldMessageSummary, HistoryVersion,
        MessageChannel, PeerDecisionResult, PeerInboxSnapshot, PeerSummary, RecipientStatus,
    };
    use caudra_config::InboundPolicy;
    use caudra_providers::{PEER_SCRIPT_SENDER, PeerAudience};
    use caudra_storage::StateDir;
    use caudra_storage::messages::{MessageLog, Retention};
    use caudra_storage::sessions::StoredPeerControls;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::{Color, Style};
    use ratatui::widgets::Paragraph;
    use test_case::test_case;

    use super::{
        ALIAS_SCOPE, AUDIENCE_LABEL, BINDINGS, BROADCASTS_LABEL, BROADCASTS_ON_LABEL,
        BROADCASTS_TITLE, Command, Confirmation, DIRECT_LABEL, EXACT_TARGET_LABEL, FLOOR_BLOCKED,
        FreshInput, GROUPS_HINT, GROUPS_LABEL, HISTORY_FAILED, HISTORY_POLL, HISTORY_STATUS,
        LEFT_OUT_HINT, LIST_SEPARATOR, LOADING_HISTORY, LoadedPage, MAX_LITERAL_COLS, MAX_ROWS,
        NAME_IN_USE, NAME_LABEL, NO_LONGER_HELD, NO_MATCHES, NO_NAME, NO_PEERS, NO_REPLY_TARGET,
        NO_TOPIC_HISTORY, NO_TOPICS, NOT_LOADED, NOT_RECEIVING, OLDER_HINT, ON, POLICY_SAVED, Pane,
        PanelFocus, PeerManager, PeerManagerAction, PeerView, QUEUED, RECEIVING, REJECTED,
        RETRY_HINT, Reader, ReviewPanel, SEPARATOR, STALE_REVIEW, START_HINT, SUBSCRIBE_LABEL,
        SUBSCRIBED, SessionPanel, SubscriptionChange, TOPICS_LABEL, TOPICS_SCOPE,
        UNSUBSCRIBE_LABEL, VIA, WHEEL_STEP, change_label, channel_key, handle_address, literal,
        message_block, paint_literal, painted_rows, policy_rank,
    };
    use crate::components::{Overlay, buffer_text};
    use crate::repaint::Cadence;

    const FIRST_TARGET: &str = "calm-fox-brings-dawn";
    const SECOND_TARGET: &str = "calm-fox-brings-rain";
    const GROUP: &str = "build-fixes";
    const OTHER_GROUP: &str = "release-triage";
    const HANDLE: &str = "parser-agent";
    const GENERATED: &str = "quiet-amber-heron";
    const LONGEST_HANDLE: &str = "abcdefghijklmnopqrstuvwxyz-12345";
    const LONG_TITLE: &str = "A session title far longer than the list column";
    const FIRST_MESSAGE: &str = "bright-blue-brook";
    const SECOND_MESSAGE: &str = "still-green-pine";
    const TITLE: &str = "Same readable title";
    const WORKSPACE: &str = "/workspace/one";
    const HOLD_REASON: &str = "Local approval required";
    const BLOCKED_REASON: &str = "Receiver is blocked; local input must resume it";
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
    const TOPIC: &str = "ci.failures";
    const PATTERN: &str = "ci.*";
    const ADDED: &str = "deploy.**";
    const INVALID: &str = "CI";
    const DIRECT_PARTY: &str = "5f0c9b1e-direct-party-session";
    const NEWEST_TEXT: &str = "quiet-silver-harbor";
    const OLDER_TEXT: &str = "early-copper-meadow";
    const NEWER_SEQ: i64 = 64;
    const NEWEST_SEQ: i64 = 42;
    const OLDER_SEQ: i64 = 17;
    const OLDEST_SEQ: i64 = 3;
    const SENT_MS: u64 = 1_700_000_000_000;
    const DELIVERED: &str = "delivered";
    const SUMMARY: &str = "Peer topics: ci.* · broadcasts off";
    const HISTORY_ERROR: &str = "Message history is unavailable";
    const CHANNELS_ERROR: &str = "Message channels are unavailable";
    const PAGE_ERROR: &str = "Channel messages are unavailable";
    const HOSTILE: &str = "\u{1b}[2J\u{202e}\u{200b}";
    const HOSTILE_TOPIC: &str = "\u{1b}[2J\u{202e}\u{200b}.failures";
    const HOSTILE_PATTERN: &str = "\u{1b}[2J\u{202e}\u{200b}.*";
    const HOSTILE_CHARACTERS: [char; 3] = ['\u{1b}', '\u{202e}', '\u{200b}'];
    const BOUND: &str = "every command has a key";
    const VERSION_SEEN: &str = "a history version was seen";
    const RETENTION: Retention = Retention {
        days: 1,
        max_messages: 1,
    };
    const HISTORY_OPENS: &str = "test message history opens";
    const CHANNELS_WANTED: &str = "a channel list is wanted";
    const PAGE_WANTED: &str = "a page is wanted";
    const PANEL_OPEN: &str = "This session panel is open";
    const PATTERN_FIELD_TABS: usize = 3;
    const READER_ROWS: u16 = 4;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    /// Ctrl+R pressed and let go, so the next press is fresh.
    fn refresh(manager: &mut PeerManager) {
        let press = ctrl('r');
        manager.handle_key(press);
        manager.handle_key(KeyEvent {
            kind: KeyEventKind::Release,
            ..press
        });
    }

    fn controls(topics: &[&str], broadcasts: bool) -> StoredPeerControls {
        StoredPeerControls {
            topics: words(topics),
            broadcasts,
            ..StoredPeerControls::default()
        }
    }

    fn topic() -> MessageChannel {
        MessageChannel::Topic(TOPIC.to_owned())
    }

    fn direct() -> MessageChannel {
        MessageChannel::Direct(DIRECT_PARTY.to_owned())
    }

    fn summary(channel: MessageChannel) -> ChannelSummary {
        ChannelSummary {
            channel,
            name: Some(TITLE.to_owned()),
            handle: Some(HANDLE.to_owned()),
            count: 1,
            last_seq: NEWEST_SEQ,
            last_ms: SENT_MS,
        }
    }

    fn message(seq: i64, text: &str) -> ChannelMessage {
        ChannelMessage {
            seq,
            audience: PeerAudience::Direct,
            sender_name: TITLE.to_owned(),
            sender_handle: Some(HANDLE.to_owned()),
            external: false,
            own: false,
            sent_ms: SENT_MS,
            text: text.to_owned(),
            recipients: vec![RecipientStatus {
                name: None,
                handle: None,
                own: true,
                status: DELIVERED.to_owned(),
                reason: None,
            }],
        }
    }

    fn page(channel: MessageChannel, message: ChannelMessage, before: Option<i64>) -> ChannelPage {
        ChannelPage {
            channel,
            messages: vec![message],
            before,
        }
    }

    fn history_version() -> HistoryVersion {
        let directory = tempfile::tempdir().expect(HISTORY_OPENS);
        MessageLog::open(
            &StateDir::from_path(directory.path().to_owned()),
            &RETENTION,
            0,
        )
        .and_then(|log| log.version())
        .expect(HISTORY_OPENS)
    }

    fn load_channels(manager: &mut PeerManager, channels: Vec<ChannelSummary>) {
        assert!(manager.set_history_version(Ok(history_version())));
        assert!(manager.wanted_channels(), "{CHANNELS_WANTED}");
        manager.set_channels(Ok(channels));
    }

    fn failed(error: &str) -> String {
        format!("{HISTORY_FAILED}{error}{RETRY_HINT}")
    }

    fn key_label(command: &Command) -> &'static str {
        BINDINGS
            .iter()
            .find(|(candidate, ..)| candidate == command)
            .map(|(_, _, _, label, _)| *label)
            .expect(BOUND)
    }

    fn browsing(channels: Vec<ChannelSummary>) -> PeerManager {
        let mut manager = manager(PeerView::Messages);
        load_channels(&mut manager, channels);
        manager
    }

    /// The source lines the message reader shows.
    fn shown(manager: &PeerManager) -> Vec<String> {
        let lines: Vec<&str> = manager.message_reader.text.split('\n').collect();
        manager
            .message_reader
            .document
            .visible()
            .filter_map(|row| lines.get(row).map(|line| (*line).to_owned()))
            .collect()
    }

    fn session_panel(topics: &[&str]) -> PeerManager {
        let mut manager = manager(PeerView::Held);
        manager.update_controls(controls(topics, false));
        manager.handle_key(press(KeyCode::Char('p')));
        manager
    }

    fn panel(manager: &PeerManager) -> &SessionPanel {
        manager.panel.as_ref().expect(PANEL_OPEN)
    }

    fn peer(target: &str) -> PeerSummary {
        PeerSummary {
            target: target.to_owned(),
            title: TITLE.to_owned(),
            handle: None,
            cwd: PathBuf::from(WORKSPACE),
            busy: false,
            blocked: false,
            inbound: InboundPolicy::Auto,
            topics: Vec::new(),
            broadcasts: false,
            groups: Vec::new(),
        }
    }

    fn held(id: &str) -> HeldMessageSummary {
        HeldMessageSummary {
            message_id: id.to_owned(),
            sender_name: TITLE.to_owned(),
            reply_target: FIRST_TARGET.to_owned(),
            external: false,
            workspace: Some(PathBuf::from(WORKSPACE)),
            mode: "Build".to_owned(),
            audience: PeerAudience::Direct,
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
    #[test_case(PeerView::Messages ; "messages")]
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
    #[test_case('3')]
    #[test_case('y')]
    #[test_case('n')]
    #[test_case('p')]
    #[test_case('s')]
    #[test_case('o')]
    fn printable_shortcuts_are_literal_while_filtering(character: char) {
        let mut manager = manager(PeerView::Held);
        manager.handle_key(press(KeyCode::Char('/')));
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char(character))),
            PeerManagerAction::Consumed
        ));
        assert_eq!(manager.held_browser.filter.text(), character.to_string());
        assert_eq!(manager.active, PeerView::Held);
        assert!(manager.panel.is_none());
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

    fn named_peer(handle: &str) -> PeerSummary {
        PeerSummary {
            handle: Some(handle.to_owned()),
            ..peer(&handle_address(handle))
        }
    }

    #[test]
    fn messaging_names_are_searchable() {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer(FIRST_TARGET), named_peer(HANDLE)]));
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(HANDLE);
        manager.handle_key(press(KeyCode::Enter));
        let entries = manager.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, handle_address(HANDLE));
    }

    #[test_case(named_peer(HANDLE), NAME_LABEL; "named")]
    #[test_case(peer(FIRST_TARGET), EXACT_TARGET_LABEL; "unnamed")]
    fn session_details_show_the_address_ctrl_b_copies(peer: PeerSummary, label: &str) {
        let target = peer.target.clone();
        let unnamed = peer.handle.is_none();
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![peer]));
        manager.select(target.clone());
        draw(&mut manager, WIDE, HEIGHT);
        let detail = &manager.session_reader.text;
        assert!(detail.contains(&format!("{label}{target}")), "{detail}");
        assert_eq!(detail.contains(EXACT_TARGET_LABEL), unnamed);
        assert_eq!(detail.contains(ALIAS_SCOPE), unnamed);
        assert!(
            matches!(manager.handle_key(ctrl('b')), PeerManagerAction::Copy(copied) if copied == target)
        );
    }

    /// The selected first row fills the detail pane, so the long name can
    /// only appear in the second row of the list.
    fn sessions_listing_a_long_name() -> PeerManager {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![
            peer(FIRST_TARGET),
            PeerSummary {
                title: LONG_TITLE.to_owned(),
                ..named_peer(LONGEST_HANDLE)
            },
        ]));
        manager
    }

    fn channels_listing_a_long_name() -> PeerManager {
        browsing(vec![
            summary(topic()),
            ChannelSummary {
                name: Some(LONG_TITLE.to_owned()),
                handle: Some(LONGEST_HANDLE.to_owned()),
                ..summary(direct())
            },
        ])
    }

    #[test_case(sessions_listing_a_long_name(); "session_rows")]
    #[test_case(channels_listing_a_long_name(); "direct_channel_rows")]
    fn list_rows_keep_the_longest_messaging_name_whole(mut manager: PeerManager) {
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(&handle_address(LONGEST_HANDLE)));
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
        current.reason = BLOCKED_REASON.to_owned();
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
        manager.panel.as_mut().expect(POLICY_DRAFT).policy = draft.clone();
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
            manager.panel.as_ref().expect(POLICY_DRAFT).policy,
            InboundPolicy::Hold
        );
        manager.panel.as_mut().expect(POLICY_DRAFT).policy = InboundPolicy::Accept;
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
        assert!(manager.panel.is_none());
        assert!(manager.is_open());
    }

    #[test_case(Color::Black, Color::White ; "light")]
    #[test_case(Color::White, Color::Black ; "dark")]
    fn literal_reader_keeps_markdown_and_theme_roles(foreground: Color, background: Color) {
        let style = Style::default().fg(foreground).bg(background);
        let painted = paint_literal(BODY, |_| style);
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
        let text = format!("{first}Z");
        let painted = paint_literal(&text, |_| Style::default());
        assert_eq!(painted_rows(&text), painted.lines().len());
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

    #[test_case(ADDED; "topic_pattern")]
    #[test_case(BROADCASTS_TITLE; "broadcasts")]
    fn subscriptions_are_searchable_and_shown_in_session_details(query: &str) {
        let mut manager = manager(PeerView::Sessions);
        manager.set_sessions(Ok(vec![
            peer(FIRST_TARGET),
            PeerSummary {
                topics: words(&[PATTERN, ADDED]),
                broadcasts: true,
                ..peer(SECOND_TARGET)
            },
        ]));
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(query);
        manager.handle_key(press(KeyCode::Enter));
        let entries = manager.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SECOND_TARGET);
        manager.select(SECOND_TARGET.to_owned());
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(&format!("{TOPICS_LABEL}{PATTERN}{LIST_SEPARATOR}{ADDED}")));
        assert!(screen.contains(&format!("{BROADCASTS_LABEL}{ON}")));
    }

    #[test_case(PeerAudience::Direct; "direct")]
    #[test_case(PeerAudience::Topic { topic: TOPIC.to_owned() }; "topic")]
    #[test_case(PeerAudience::Broadcast; "broadcast")]
    fn held_messages_name_their_audience(audience: PeerAudience) {
        let mut manager = manager(PeerView::Held);
        manager.update_inbox(snapshot(vec![HeldMessageSummary {
            audience: audience.clone(),
            ..held(FIRST_MESSAGE)
        }]));
        let audience = audience.to_string();
        assert!(manager.entries()[0].2.starts_with(&audience));
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(&format!("{AUDIENCE_LABEL}{audience}")));
    }

    #[test]
    fn held_script_messages_name_the_script_and_offer_no_reply_target() {
        let mut manager = manager(PeerView::Held);
        manager.update_inbox(snapshot(vec![HeldMessageSummary {
            external: true,
            reply_target: String::new(),
            ..held(FIRST_MESSAGE)
        }]));
        let script = format!("{TITLE} ({PEER_SCRIPT_SENDER})");
        assert_eq!(manager.entries()[0].1, script);
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(&script), "{screen}");
        assert!(screen.contains(NO_REPLY_TARGET), "{screen}");
        assert!(matches!(
            manager.handle_key(ctrl('b')),
            PeerManagerAction::Consumed
        ));
    }

    #[test]
    fn stored_script_messages_are_marked() {
        let block = message_block(&ChannelMessage {
            external: true,
            sender_handle: None,
            ..message(NEWEST_SEQ, NEWEST_TEXT)
        });
        let heading = format!("{TITLE} ({PEER_SCRIPT_SENDER}){SEPARATOR}");
        assert!(block.starts_with(&heading), "{block}");
    }

    #[test_case(SubscriptionChange::Subscribe(words(&[ADDED, PATTERN])), Ok((words(&[PATTERN, ADDED]), false)); "subscribe_adds_new_patterns")]
    #[test_case(SubscriptionChange::Unsubscribe(words(&[PATTERN])), Ok((Vec::new(), false)); "unsubscribe_removes")]
    #[test_case(SubscriptionChange::Unsubscribe(words(&[ADDED])), Err(format!("{MISSING_PATTERN}: {ADDED:?}")); "unsubscribe_names_a_missing_pattern")]
    #[test_case(SubscriptionChange::Subscribe(words(&[INVALID])), Err(INVALID_PATTERN.into()); "subscribe_validates")]
    #[test_case(SubscriptionChange::Broadcasts(true), Ok((words(&[PATTERN]), true)); "broadcasts_on")]
    fn subscription_changes_describe_the_whole_set(
        change: SubscriptionChange,
        expected: Result<(Vec<String>, bool), String>,
    ) {
        assert_eq!(change.apply(&words(&[PATTERN]), false), expected);
    }

    #[test]
    fn the_first_channel_opens_at_its_newest_messages_without_its_session_id() {
        let mut manager = browsing(vec![summary(direct()), summary(topic())]);
        let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
        assert_eq!((&request.channel, request.before), (&direct(), None));
        manager.set_page(
            request,
            Ok(page(
                direct(),
                message(NEWEST_SEQ, NEWEST_TEXT),
                Some(NEWEST_SEQ),
            )),
        );
        assert!(manager.wanted_page().is_none());
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(NEWEST_TEXT));
        assert!(screen.contains(OLDER_HINT));
        let (_, title, detail) = &manager.entries()[0];
        assert_eq!(title, TITLE);
        assert!(detail.starts_with(&format!("{}{SEPARATOR}", handle_address(HANDLE))));
        assert!(!screen.contains(DIRECT_PARTY));
        manager.handle_key(press(KeyCode::Char('/')));
        manager.handle_paste(DIRECT_PARTY);
        assert!(manager.entries().is_empty());
    }

    #[test]
    fn answers_nothing_waits_for_are_dropped() {
        let mut manager = manager(PeerView::Messages);
        manager.set_channels(Ok(vec![summary(topic())]));
        assert!(manager.history.channels.is_none());
        load_channels(
            &mut manager,
            vec![summary(topic()), summary(MessageChannel::Broadcast)],
        );
        let first = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.handle_key(press(KeyCode::Down));
        let second = manager.wanted_page().cloned().expect(PAGE_WANTED);
        assert_eq!(second.channel, MessageChannel::Broadcast);
        manager.set_page(
            first,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        assert!(manager.history.page.is_none());
        assert_eq!(manager.wanted_page(), Some(&second));
    }

    #[test]
    fn a_change_during_a_load_lets_it_land_then_asks_once_more() {
        let mut manager = manager(PeerView::Messages);
        assert!(manager.set_history_version(Ok(history_version())));
        refresh(&mut manager);
        refresh(&mut manager);
        manager.set_channels(Ok(vec![summary(topic())]));
        assert_eq!(manager.entries().len(), 1);
        assert!(manager.wanted_channels(), "{CHANNELS_WANTED}");
        manager.set_channels(Ok(vec![summary(topic()), summary(direct())]));
        assert_eq!(manager.entries().len(), 2);
        assert!(!manager.wanted_channels());
        let newest = manager.wanted_page().cloned().expect(PAGE_WANTED);
        refresh(&mut manager);
        assert_eq!(manager.wanted_page(), Some(&newest));
        manager.set_page(
            newest.clone(),
            Ok(page(topic(), message(OLDER_SEQ, OLDER_TEXT), None)),
        );
        assert!(manager.message_reader.text.contains(OLDER_TEXT));
        assert_eq!(manager.wanted_page(), Some(&newest));
        manager.set_page(
            newest,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        assert!(manager.message_reader.text.contains(NEWEST_TEXT));
        assert!(manager.wanted_page().is_none());
    }

    #[test_case(false, &[(NEWEST_SEQ, NEWEST_TEXT)], Some(NEWEST_SEQ), false, &[OLDER_SEQ, NEWEST_SEQ], Some(OLDER_SEQ); "unchanged_newest_page")]
    #[test_case(false, &[(NEWEST_SEQ, OLDER_TEXT)], Some(NEWEST_SEQ), true, &[OLDER_SEQ, NEWEST_SEQ], Some(OLDER_SEQ); "changed_message")]
    #[test_case(false, &[(NEWER_SEQ, NEWEST_TEXT), (NEWEST_SEQ, NEWEST_TEXT)], Some(NEWEST_SEQ), true, &[OLDER_SEQ, NEWEST_SEQ, NEWER_SEQ], Some(OLDER_SEQ); "newer_message")]
    #[test_case(false, &[(NEWEST_SEQ, NEWEST_TEXT)], None, true, &[NEWEST_SEQ], None; "whole_channel_drops_pruned_messages")]
    #[test_case(false, &[(NEWER_SEQ, NEWEST_TEXT)], Some(NEWER_SEQ), true, &[NEWER_SEQ], Some(NEWER_SEQ); "gap_replaces_everything")]
    #[test_case(true, &[(OLDEST_SEQ, OLDER_TEXT)], None, true, &[OLDEST_SEQ, OLDER_SEQ, NEWEST_SEQ], None; "older_page_adds_below")]
    fn pages_merge_into_the_loaded_messages(
        older: bool,
        incoming: &[(i64, &str)],
        before: Option<i64>,
        changed: bool,
        loaded: &[i64],
        cursor: Option<i64>,
    ) {
        let mut page = LoadedPage {
            channel: topic(),
            messages: [
                message(OLDER_SEQ, OLDER_TEXT),
                message(NEWEST_SEQ, NEWEST_TEXT),
            ]
            .into_iter()
            .map(|message| (message.seq, message))
            .collect(),
            older: Some(OLDER_SEQ),
            capped: false,
        };
        let incoming = incoming
            .iter()
            .map(|(seq, text)| (*seq, message(*seq, text)))
            .collect();
        assert_eq!(page.merge(older, incoming, before), changed);
        assert_eq!(page.messages.keys().copied().collect::<Vec<_>>(), loaded);
        assert_eq!(page.older, cursor);
    }

    #[test]
    fn a_newest_page_reaching_the_oldest_message_offers_nothing_older() {
        let mut manager = browsing(vec![summary(topic())]);
        let newest = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            newest,
            Ok(page(
                topic(),
                message(NEWEST_SEQ, NEWEST_TEXT),
                Some(NEWEST_SEQ),
            )),
        );
        manager.handle_key(press(KeyCode::Char('o')));
        let older = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            older,
            Ok(page(
                topic(),
                message(OLDER_SEQ, OLDER_TEXT),
                Some(OLDER_SEQ),
            )),
        );
        assert!(manager.footer_commands().contains(&Command::Older));
        refresh(&mut manager);
        let refreshed = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            refreshed,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        let text = &manager.message_reader.text;
        assert!(text.starts_with(START_HINT));
        assert!(!text.contains(OLDER_TEXT) && !text.contains(OLDER_HINT));
        assert!(!manager.footer_commands().contains(&Command::Older));
    }

    #[test]
    fn a_full_reader_offers_nothing_older_and_never_cuts_rows_itself() {
        let tall = "\n".repeat(MAX_ROWS / 2);
        let mut manager = browsing(vec![summary(topic())]);
        let newest = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            newest,
            Ok(ChannelPage {
                channel: topic(),
                messages: vec![message(NEWEST_SEQ, &tall), message(OLDER_SEQ, &tall)],
                before: Some(OLDER_SEQ),
            }),
        );
        let text = &manager.message_reader.text;
        assert!(text.starts_with(LEFT_OUT_HINT));
        assert_eq!(
            usize::from(manager.message_reader.document.rows()),
            painted_rows(text)
        );
        assert!(manager.older_page().is_none());
        assert!(!manager.footer_commands().contains(&Command::Older));
    }

    #[test]
    fn a_change_during_an_older_load_reloads_the_newest_page_after_it() {
        let mut manager = browsing(vec![summary(topic())]);
        let newest = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            newest,
            Ok(page(
                topic(),
                message(NEWEST_SEQ, NEWEST_TEXT),
                Some(NEWEST_SEQ),
            )),
        );
        manager.handle_key(press(KeyCode::Char('o')));
        let older = manager.wanted_page().cloned().expect(PAGE_WANTED);
        assert_eq!(older.before, Some(NEWEST_SEQ));
        manager.handle_key(ctrl('r'));
        assert_eq!(manager.wanted_page(), Some(&older));
        manager.set_page(
            older,
            Ok(page(topic(), message(OLDER_SEQ, OLDER_TEXT), None)),
        );
        let refreshed = manager.wanted_page().cloned().expect(PAGE_WANTED);
        assert_eq!((&refreshed.channel, refreshed.before), (&topic(), None));
        let text = &manager.message_reader.text;
        assert!(text.contains(OLDER_TEXT) && text.contains(NEWEST_TEXT));
        assert!(!text.contains(OLDER_HINT));
    }

    #[test_case(0; "following_the_newest")]
    #[test_case(WHEEL_STEP; "scrolled_back")]
    fn older_messages_load_above_without_moving_what_the_reader_shows(scrolled: i32) {
        let lines = (0..MANY_ROWS)
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut manager = browsing(vec![summary(topic())]);
        let newest = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            newest,
            Ok(page(topic(), message(NEWEST_SEQ, &lines), Some(NEWEST_SEQ))),
        );
        manager.handle_key(press(KeyCode::Tab));
        draw(&mut manager, WIDE, HEIGHT);
        manager.scroll(scrolled);
        draw(&mut manager, WIDE, HEIGHT);
        let before = shown(&manager);
        manager.handle_key(press(KeyCode::Char('o')));
        let older = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            older,
            Ok(page(
                topic(),
                message(OLDER_SEQ, OLDER_TEXT),
                Some(OLDER_SEQ),
            )),
        );
        draw(&mut manager, WIDE, HEIGHT);
        assert!(manager.message_reader.text.contains(OLDER_TEXT));
        assert!(before.len() > 1);
        assert_eq!(shown(&manager), before);
    }

    #[test_case(topic(), &[], false, None, Some(SubscriptionChange::Subscribe(words(&[TOPIC]))); "unsubscribed_topic")]
    #[test_case(topic(), &[PATTERN], false, Some(format!("{VIA}{PATTERN}").as_str()), Some(SubscriptionChange::Subscribe(words(&[TOPIC]))); "topic_matched_by_a_pattern")]
    #[test_case(topic(), &[TOPIC], false, Some(SUBSCRIBED), Some(SubscriptionChange::Unsubscribe(words(&[TOPIC]))); "subscribed_topic")]
    #[test_case(MessageChannel::Broadcast, &[], false, Some(NOT_RECEIVING), Some(SubscriptionChange::Broadcasts(true)); "ignored_broadcasts")]
    #[test_case(MessageChannel::Broadcast, &[], true, Some(RECEIVING), Some(SubscriptionChange::Broadcasts(false)); "received_broadcasts")]
    #[test_case(direct(), &[], false, Some(DIRECT_LABEL), None; "direct_conversation")]
    fn channel_rows_show_and_s_toggles_how_this_session_receives_them(
        channel: MessageChannel,
        topics: &[&str],
        broadcasts: bool,
        marker: Option<&str>,
        expected: Option<SubscriptionChange>,
    ) {
        let mut manager = browsing(vec![summary(channel)]);
        manager.update_controls(controls(topics, broadcasts));
        let detail = manager.entries().remove(0).2;
        match marker {
            Some(marker) => assert!(detail.ends_with(&format!("{SEPARATOR}{marker}"))),
            None => assert_eq!(detail.matches(SEPARATOR).count(), 1),
        }
        let screen = draw(&mut manager, WIDE, HEIGHT);
        let action = manager.handle_key(press(KeyCode::Char('s')));
        match expected {
            Some(expected) => {
                assert!(screen.contains(&format!(
                    "{}{}",
                    key_label(&Command::Subscribe),
                    change_label(&expected)
                )));
                assert!(
                    matches!(action, PeerManagerAction::Subscribe(change) if change == expected)
                );
            }
            None => assert!(matches!(action, PeerManagerAction::Consumed)),
        }
    }

    #[test]
    fn topics_view_lists_topic_channels_until_3_shows_every_channel() {
        let mut manager = manager(PeerView::Held);
        manager.show_only_topics();
        assert_eq!(manager.active, PeerView::Messages);
        assert!(draw(&mut manager, NARROW, SHORT).contains(TOPICS_SCOPE));
        load_channels(&mut manager, vec![summary(direct())]);
        assert!(draw(&mut manager, NARROW, SHORT).contains(NO_TOPIC_HISTORY));
        refresh(&mut manager);
        manager.set_channels(Ok(vec![
            summary(direct()),
            summary(MessageChannel::Broadcast),
            summary(topic()),
        ]));
        let ids: Vec<String> = manager.entries().into_iter().map(|(id, ..)| id).collect();
        assert_eq!(ids, [channel_key(&topic())]);
        assert_eq!(
            manager.wanted_page().map(|request| &request.channel),
            Some(&topic())
        );
        manager.handle_key(press(KeyCode::Char('3')));
        assert_eq!(manager.entries().len(), 3);
        assert!(!draw(&mut manager, WIDE, HEIGHT).contains(TOPICS_SCOPE));
    }

    #[test]
    fn showing_every_channel_selects_the_first_when_none_is() {
        let mut manager = manager(PeerView::Held);
        manager.show_only_topics();
        load_channels(&mut manager, vec![summary(direct())]);
        assert!(manager.message_browser.selected.is_none());
        assert!(manager.wanted_page().is_none());
        manager.handle_key(press(KeyCode::Char('3')));
        assert_eq!(
            manager.message_browser.selected,
            Some(channel_key(&direct()))
        );
        assert_eq!(
            manager.wanted_page().map(|request| &request.channel),
            Some(&direct())
        );
    }

    #[test]
    fn each_failed_load_says_so_and_alone_is_asked_again_each_poll() {
        let mut manager = manager(PeerView::Messages);
        let version = history_version();
        assert!(manager.set_history_version(Ok(version.clone())));
        manager.set_channels(Err(CHANNELS_ERROR.to_owned()));
        assert!(!manager.wanted_channels());
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(&failed(CHANNELS_ERROR)));
        assert!(manager.set_history_version(Ok(version.clone())));
        assert!(manager.wanted_channels(), "{CHANNELS_WANTED}");
        manager.set_channels(Ok(vec![summary(topic())]));
        let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(request.clone(), Err(PAGE_ERROR.to_owned()));
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(&failed(PAGE_ERROR)) && screen.contains(NOT_LOADED));
        assert!(manager.set_history_version(Ok(version.clone())));
        assert_eq!(manager.wanted_page(), Some(&request));
        assert!(!manager.wanted_channels());
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(NOT_LOADED));
        manager.set_page(
            request,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        assert!(manager.history.error().is_none());
        assert!(!manager.set_history_version(Ok(version)));
        assert!(manager.set_history_version(Err(HISTORY_ERROR.to_owned())));
        assert!(!manager.set_history_version(Err(HISTORY_ERROR.to_owned())));
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(&failed(HISTORY_ERROR)));
        refresh(&mut manager);
        assert!(manager.wanted_channels(), "{CHANNELS_WANTED}");
        assert!(manager.wanted_page().is_some());
    }

    #[test]
    fn a_load_that_lands_clears_only_its_own_error() {
        let mut manager = browsing(vec![summary(topic())]);
        let version = manager.history.version.clone().expect(VERSION_SEEN);
        let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
        refresh(&mut manager);
        manager.set_channels(Err(CHANNELS_ERROR.to_owned()));
        manager.set_page(request.clone(), Err(PAGE_ERROR.to_owned()));
        assert!(manager.set_history_version(Ok(version)));
        manager.set_page(
            request,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        assert_eq!(manager.history_status(), failed(CHANNELS_ERROR));
        manager.set_channels(Ok(vec![summary(topic())]));
        assert!(manager.history.error().is_none());
    }

    #[test]
    fn history_polls_only_while_the_messages_view_shows() {
        let polling = Cadence::polling(HISTORY_POLL);
        let loading = Cadence::any([polling, Cadence::PENDING]);
        let mut manager = manager(PeerView::Held);
        assert_eq!(manager.cadence(), Cadence::IDLE);
        manager.handle_key(press(KeyCode::Char('3')));
        assert_eq!(manager.cadence(), polling);
        manager.set_history_polling(true);
        assert_eq!(manager.cadence(), loading);
        manager.set_history_polling(false);
        load_channels(&mut manager, vec![summary(topic())]);
        assert_eq!(manager.cadence(), loading);
        let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            request,
            Ok(page(topic(), message(NEWEST_SEQ, NEWEST_TEXT), None)),
        );
        assert_eq!(manager.cadence(), polling);
        manager.handle_key(press(KeyCode::Char('p')));
        assert_eq!(manager.cadence(), Cadence::IDLE);
    }

    #[test_case(1, "", UNSUBSCRIBE_LABEL, Some(SubscriptionChange::Unsubscribe(words(&[PATTERN]))); "pattern")]
    #[test_case(2, "", BROADCASTS_ON_LABEL, Some(SubscriptionChange::Broadcasts(true)); "broadcasts")]
    #[test_case(PATTERN_FIELD_TABS, ADDED, SUBSCRIBE_LABEL, Some(SubscriptionChange::Subscribe(words(&[ADDED]))); "field")]
    #[test_case(PATTERN_FIELD_TABS, "", SUBSCRIBE_LABEL, None; "empty_field")]
    fn panel_enter_changes_what_its_focus_names(
        tabs: usize,
        typed: &str,
        label: &str,
        expected: Option<SubscriptionChange>,
    ) {
        let mut manager = session_panel(&[PATTERN]);
        for _ in 0..tabs {
            manager.handle_key(press(KeyCode::Tab));
        }
        manager.handle_paste(typed);
        assert!(
            draw(&mut manager, WIDE, HEIGHT)
                .contains(&format!("{}{label}", key_label(&Command::Activate)))
        );
        let action = manager.handle_key(press(KeyCode::Enter));
        match expected {
            Some(expected) => assert!(
                matches!(action, PeerManagerAction::Subscribe(change) if change == expected)
            ),
            None => assert!(matches!(action, PeerManagerAction::Consumed)),
        }
        assert!(panel(&manager).field.is_empty());
    }

    #[test]
    fn invalid_patterns_stay_in_the_field_with_the_reason() {
        let mut manager = session_panel(&[PATTERN]);
        for _ in 0..PATTERN_FIELD_TABS {
            manager.handle_key(press(KeyCode::Tab));
        }
        manager.handle_paste(INVALID);
        assert!(matches!(
            manager.handle_key(press(KeyCode::Enter)),
            PeerManagerAction::Consumed
        ));
        assert_eq!(panel(&manager).error.as_deref(), Some(INVALID_PATTERN));
        assert_eq!(panel(&manager).field.text(), INVALID);
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(INVALID_PATTERN));
        manager.handle_paste(ADDED);
        assert!(panel(&manager).error.is_none());
    }

    #[test_case('1')]
    #[test_case('3')]
    #[test_case('p')]
    #[test_case('a')]
    #[test_case('s')]
    #[test_case('y')]
    fn printable_shortcuts_are_literal_in_the_pattern_field(character: char) {
        let mut manager = session_panel(&[PATTERN]);
        for _ in 0..PATTERN_FIELD_TABS {
            manager.handle_key(press(KeyCode::Tab));
        }
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char(character))),
            PeerManagerAction::Consumed
        ));
        assert_eq!(panel(&manager).field.text(), character.to_string());
        assert_eq!(manager.active, PeerView::Held);
    }

    #[test]
    fn escape_clears_the_field_before_leaving_the_panel() {
        let mut manager = session_panel(&[PATTERN]);
        for _ in 0..PATTERN_FIELD_TABS {
            manager.handle_key(press(KeyCode::Tab));
        }
        manager.handle_paste(ADDED);
        manager.handle_key(press(KeyCode::Esc));
        assert!(panel(&manager).field.is_empty());
        manager.handle_key(press(KeyCode::Esc));
        assert!(manager.panel.is_none());
        assert!(manager.is_open());
    }

    #[test]
    fn focus_follows_patterns_removed_elsewhere() {
        let mut manager = session_panel(&[PATTERN, ADDED]);
        manager.handle_key(press(KeyCode::Tab));
        manager.handle_key(press(KeyCode::Tab));
        assert_eq!(panel(&manager).focus, PanelFocus::Pattern(1));
        manager.update_controls(controls(&[PATTERN], false));
        assert_eq!(panel(&manager).focus, PanelFocus::Pattern(0));
        manager.update_controls(controls(&[], false));
        assert_eq!(panel(&manager).focus, PanelFocus::Broadcasts);
    }

    #[test]
    fn clicking_a_panel_control_focuses_it() {
        let mut manager = session_panel(&[PATTERN]);
        draw(&mut manager, WIDE, HEIGHT);
        let broadcasts = manager
            .panel_hits
            .iter()
            .find(|(_, focus)| focus == &PanelFocus::Broadcasts)
            .expect(TEST_RENDER)
            .0;
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), broadcasts));
        assert_eq!(panel(&manager).focus, PanelFocus::Broadcasts);
    }

    #[test]
    fn an_applied_policy_keeps_the_panel_open() {
        let mut manager = manager(PeerView::Held);
        manager.handle_key(press(KeyCode::Char('p')));
        manager.panel.as_mut().expect(PANEL_OPEN).policy = InboundPolicy::Refuse;
        assert!(matches!(
            manager.handle_key(press(KeyCode::Char('a'))),
            PeerManagerAction::SetInbound(InboundPolicy::Refuse)
        ));
        assert!(panel(&manager).pending);
        manager.finish_policy(Ok(()));
        assert!(!panel(&manager).pending);
        assert_eq!(manager.feedback.as_deref(), Some(POLICY_SAVED));
    }

    #[test_case(true; "panel")]
    #[test_case(false; "messages_view")]
    fn subscription_results_land_where_the_change_was_made(from_panel: bool) {
        let mut manager = browsing(vec![summary(topic())]);
        manager.update_controls(controls(&[PATTERN], false));
        if from_panel {
            manager.handle_key(press(KeyCode::Char('p')));
        }
        manager.finish_subscriptions(Err(HISTORY_ERROR.to_owned()));
        let panel_error = manager
            .panel
            .as_ref()
            .and_then(|panel| panel.error.as_deref());
        let error = if from_panel {
            panel_error
        } else {
            manager.feedback.as_deref()
        };
        assert_eq!(error, Some(HISTORY_ERROR));
        manager.finish_subscriptions(Ok(SUMMARY.to_owned()));
        assert_eq!(manager.feedback.as_deref(), Some(SUMMARY));
        assert!(
            manager
                .panel
                .as_ref()
                .is_none_or(|panel| panel.error.is_none())
        );
    }

    #[test_case(None, Some(GENERATED), None; "generated")]
    #[test_case(Some(HANDLE), Some(HANDLE), None; "stored")]
    #[test_case(Some(HANDLE), Some(GENERATED), Some(HANDLE); "stored_name_in_use")]
    #[test_case(Some(HANDLE), None, None; "unnamed")]
    fn this_session_panel_shows_the_name_it_answers_to(
        stored: Option<&str>,
        live: Option<&str>,
        in_use: Option<&str>,
    ) {
        let mut manager = manager(PeerView::Held);
        manager.update_controls(StoredPeerControls {
            handle: stored.map(str::to_owned),
            ..StoredPeerControls::default()
        });
        manager.update_messaging_name(live.map(str::to_owned));
        manager.handle_key(press(KeyCode::Char('p')));
        let name = live.map_or_else(
            || NO_NAME.to_owned(),
            |live| format!("{NAME_LABEL}{}", handle_address(live)),
        );
        let expected = match in_use {
            Some(stored) => format!("{name}{SEPARATOR}{}{NAME_IN_USE}", handle_address(stored)),
            None => name,
        };
        let panel = manager.panel_document().expect(PANEL_OPEN).text;
        assert_eq!(panel.lines().nth(1), Some(expected.as_str()));
    }

    #[test_case(&[], NO_TOPICS; "no_groups")]
    #[test_case(&[GROUP, OTHER_GROUP], &format!("{GROUP}{LIST_SEPARATOR}{OTHER_GROUP}"); "groups")]
    fn group_memberships_show_in_session_details_and_this_session(groups: &[&str], listed: &str) {
        let shown = format!("{GROUPS_LABEL}{listed}");
        let mut sessions = manager(PeerView::Sessions);
        sessions.set_sessions(Ok(vec![PeerSummary {
            groups: words(groups),
            ..peer(FIRST_TARGET)
        }]));
        sessions.select(FIRST_TARGET.to_owned());
        assert!(draw(&mut sessions, WIDE, HEIGHT).contains(&shown));
        let mut this_session = manager(PeerView::Held);
        this_session.update_controls(StoredPeerControls {
            groups: words(groups),
            ..StoredPeerControls::default()
        });
        this_session.handle_key(press(KeyCode::Char('p')));
        let panel = this_session.panel_document().expect(PANEL_OPEN).text;
        assert!(panel.contains(&format!("{shown}{GROUPS_HINT}")), "{panel}");
    }

    #[test]
    fn the_footer_offers_apply_only_while_the_field_leaves_a_alone() {
        let mut manager = session_panel(&[PATTERN]);
        assert!(manager.footer_commands().contains(&Command::Apply));
        for _ in 0..PATTERN_FIELD_TABS {
            manager.handle_key(press(KeyCode::Tab));
        }
        assert!(manager.field_focused());
        assert!(!manager.footer_commands().contains(&Command::Apply));
    }

    #[test_case(ctrl('r'); "history_change")]
    #[test_case(press(KeyCode::Down); "selection_change")]
    #[test_case(press(KeyCode::Char('3')); "scope_change")]
    #[test_case(press(KeyCode::Char('2')); "view_switch")]
    fn messages_feedback_lasts_until_the_next_change(change: KeyEvent) {
        let mut manager = browsing(vec![summary(topic()), summary(MessageChannel::Broadcast)]);
        manager.finish_subscriptions(Ok(SUMMARY.to_owned()));
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(SUMMARY) && !screen.contains(HISTORY_STATUS));
        manager.handle_key(change);
        assert!(!draw(&mut manager, WIDE, HEIGHT).contains(SUMMARY));
    }

    #[test]
    fn feedback_from_another_view_gives_way_to_the_topics_status() {
        let mut manager = manager(PeerView::Held);
        manager.feedback = Some(STALE_REVIEW.to_owned());
        assert!(draw(&mut manager, WIDE, HEIGHT).contains(STALE_REVIEW));
        manager.show_only_topics();
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(!screen.contains(STALE_REVIEW));
        assert!(screen.contains(&format!("{TOPICS_SCOPE}{LOADING_HISTORY}")));
    }

    /// What the Messages view shows: one channel, its one message, and this
    /// session's controls, with the This session panel open or not.
    struct Shown {
        channel: ChannelSummary,
        message: ChannelMessage,
        controls: StoredPeerControls,
        name: Option<String>,
        panel: bool,
    }

    #[test_case(|shown| shown.message.sender_name = HOSTILE.to_owned(), &literal(HOSTILE, false); "sender_name")]
    #[test_case(|shown| shown.message.sender_handle = Some(HOSTILE.to_owned()), &literal(&handle_address(HOSTILE), false); "sender_handle")]
    #[test_case(|shown| shown.message.text = HOSTILE.to_owned(), &literal(HOSTILE, true); "message_text")]
    #[test_case(|shown| shown.message.recipients[0].name = Some(HOSTILE.to_owned()), &literal(HOSTILE, false); "recipient_name")]
    #[test_case(|shown| shown.message.recipients[0].status = HOSTILE.to_owned(), &literal(HOSTILE, false); "recipient_status")]
    #[test_case(|shown| shown.message.recipients[0].reason = Some(HOSTILE.to_owned()), &literal(HOSTILE, false); "recipient_reason")]
    #[test_case(|shown| shown.message.recipients[0].handle = Some(HOSTILE.to_owned()), &literal(&handle_address(HOSTILE), false); "recipient_handle")]
    #[test_case(|shown| shown.channel.channel = MessageChannel::Topic(HOSTILE.to_owned()), &literal(HOSTILE, false); "topic_name")]
    #[test_case(|shown| shown.channel.name = Some(HOSTILE.to_owned()), &literal(HOSTILE, false); "direct_name")]
    #[test_case(|shown| shown.channel.handle = Some(HOSTILE.to_owned()), &literal(&handle_address(HOSTILE), false); "direct_handle")]
    #[test_case(|shown| { shown.channel.channel = MessageChannel::Topic(HOSTILE_TOPIC.to_owned()); shown.controls.topics = words(&[HOSTILE_PATTERN]); }, &format!("{VIA}{}", literal(HOSTILE_PATTERN, false)); "via_pattern")]
    #[test_case(|shown| { shown.controls.topics = words(&[HOSTILE]); shown.panel = true; }, &literal(HOSTILE, false); "panel_pattern")]
    #[test_case(|shown| { shown.name = Some(HOSTILE.to_owned()); shown.panel = true; }, &format!("{NAME_LABEL}{}", literal(&handle_address(HOSTILE), false)); "panel_name")]
    #[test_case(|shown| { shown.name = Some(HANDLE.to_owned()); shown.controls.handle = Some(HOSTILE.to_owned()); shown.panel = true; }, &format!("{SEPARATOR}{}", literal(&handle_address(HOSTILE), false)); "panel_stored_name")]
    fn peer_strings_show_literally_wherever_they_appear(place: fn(&mut Shown), expected: &str) {
        let mut shown = Shown {
            channel: summary(direct()),
            message: ChannelMessage {
                recipients: vec![RecipientStatus {
                    name: Some(TITLE.to_owned()),
                    handle: None,
                    own: false,
                    status: DELIVERED.to_owned(),
                    reason: None,
                }],
                ..message(NEWEST_SEQ, NEWEST_TEXT)
            },
            controls: controls(&[], false),
            name: None,
            panel: false,
        };
        place(&mut shown);
        let mut manager = browsing(vec![shown.channel]);
        manager.update_controls(shown.controls);
        manager.update_messaging_name(shown.name);
        let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
        manager.set_page(
            request.clone(),
            Ok(ChannelPage {
                channel: request.channel,
                messages: vec![shown.message],
                before: None,
            }),
        );
        if shown.panel {
            manager.handle_key(press(KeyCode::Char('p')));
        }
        let screen = draw(&mut manager, WIDE, HEIGHT);
        assert!(screen.contains(expected), "{screen}");
        assert!(!screen.contains(HOSTILE_CHARACTERS), "{screen}");
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
            peers::{MAX_HISTORY_PAGE, PeerDecision, PeerDescriptor, PeerHost, PeerSession},
        };
        use caudra_config::InboundPolicy;
        use caudra_storage::{id::CaudraId, sessions::PermissionMode};
        use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEventKind};
        use tempfile::TempDir;
        use test_case::test_case;

        use super::{
            BODY, CHANNELS_WANTED, Command, HEIGHT, NARROW, PAGE_WANTED, PeerManager,
            PeerManagerAction, PeerView, SHORT, TITLE, WIDE, draw, mouse, press,
        };

        const PRIVATE_MODE: u32 = 0o700;
        const REQUEST: &str = "peer-manager-review-test";

        /// A receiving session holding what it is sent, and the sender's
        /// message to it.
        fn sent() -> (TempDir, PeerHost, PeerSession, PeerSession) {
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
            (directory, host, receiver, sender)
        }

        fn reviewing() -> (TempDir, PeerHost, PeerSession, PeerManager) {
            let (directory, host, receiver, _) = sent();
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

        #[test]
        fn stored_direct_messages_name_the_sender_never_its_session() {
            let (_directory, _host, receiver, sender) = sent();
            let mut manager = PeerManager::new();
            manager.open(PeerView::Messages);
            assert!(manager.set_history_version(smol::block_on(receiver.history_version())));
            assert!(manager.wanted_channels(), "{CHANNELS_WANTED}");
            manager.set_channels(smol::block_on(receiver.message_channels()));
            let request = manager.wanted_page().cloned().expect(PAGE_WANTED);
            let page = smol::block_on(receiver.channel_messages(
                request.channel.clone(),
                request.before,
                MAX_HISTORY_PAGE,
            ));
            manager.set_page(request, page);
            let screen = draw(&mut manager, WIDE, HEIGHT);
            assert!(BODY.lines().all(|line| screen.contains(line)));
            assert!(screen.contains(TITLE));
            assert!(!screen.contains(&sender.session_id().to_string()));
        }
    }

    #[test]
    fn expanded_control_lines_remain_reachable_beyond_document_pan_limits() {
        let source = literal(&"\u{1b}".repeat(MAX_LITERAL_COLS * 4), true);
        let painted = paint_literal(&source, |_| Style::default());
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

    #[test]
    fn a_reader_paints_its_bars_inside_the_area_it_is_given() {
        let mut reader = Reader::default();
        reader.set(FIRST_TARGET.repeat(usize::from(NARROW)));
        let mut terminal =
            Terminal::new(TestBackend::new(NARROW, READER_ROWS + 1)).expect(TEST_RENDER);
        terminal
            .draw(|frame| {
                let area = frame.area();
                let below = Rect {
                    y: READER_ROWS,
                    height: 1,
                    ..area
                };
                frame.render_widget(Paragraph::new(HOLD_REASON), below);
                reader.draw(
                    frame,
                    Rect {
                        height: READER_ROWS,
                        ..area
                    },
                );
            })
            .expect(TEST_RENDER);
        assert!(buffer_text(terminal.backend().buffer()).contains(HOLD_REASON));
    }
}
