//! Shows the conversation as the provider receives it: the prompt the run
//! bound, the tools it offered, and the history after the projection the live
//! turn applies. `r` switches to the Wire view, the body the provider adapter
//! would send for that same request.
//!
//! Everything is taken when the modal opens, so the view never shifts under
//! the reader. Reopening it is how to see a newer request.

mod projection;
mod wire;

use std::sync::Arc;

use caudra_grab::grab_scope;
use caudra_providers::{CacheKey, Message, RequestOptions};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use self::projection::GUTTER;
use self::wire::Wire;
use crate::agent::BtwPrompt;
use crate::components::document_view::{
    COPIED_SELECTION, COPY_HINT, COPY_LABEL, DocumentMouse, DocumentView, Jump, body_width,
};
use crate::components::keybindings::key;
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, SEPARATOR};
use crate::components::{Overlay, escape_terminal_controls, plain_char};
use crate::theme::{self, Theme};

#[cfg(test)]
pub(crate) use self::projection::UNPREPARED;

const TITLE_PREFIX: &str = " Projection - ";
const UNPREPARED_TITLE: &str = " Projection ";
const TITLE_END: &str = " ";
const MESSAGE_NOUN: &str = "message";
/// The Wire view draws nothing beside its rows.
const NO_GUTTER: u16 = 0;
/// What keys the Wire view's rows in place of a wrap width: they are never
/// wrapped, so no resize repaints them.
const UNWRAPPED: u16 = u16::MAX;
const SWITCH_LABEL: &str = "r";
const WIRE_HINT: &str = " wire";
const PROJECTION_HINT: &str = " projection";
const NEXT_LABEL: &str = "n";
const NEXT_HINT: &str = " next";
const PREVIOUS_LABEL: &str = "p";
const PREVIOUS_HINT: &str = " previous";
const COPIED: &str = "Copied the projection";
const COPIED_WIRE: &str = "Copied the wire body";

/// What the host has to carry out. Jumping, selecting, switching view and
/// closing are the modal's own business and never reach here.
pub enum ProjectionAction {
    Consumed,
    Copy { text: String, label: &'static str },
}

/// The request `/projection` shows, as it stood when the modal opened: the
/// route and prefix the run bound, and the history the next request carries.
pub(crate) struct Projection {
    pub(crate) prompt: Arc<BtwPrompt>,
    pub(crate) messages: Vec<Message>,
    /// Calls the trailing turn made that are still running. Their results
    /// join the request after this one, so the view leaves them open.
    pub(crate) running_calls: usize,
    /// The conversation's key, so the Wire view's body names the cache the
    /// live request would.
    pub(crate) cache_key: CacheKey,
    /// The thinking and fast settings the next message will carry. The bound
    /// prompt holds the last run's, or defaults before any run.
    pub(crate) opts: RequestOptions,
}

/// Which picture of the request is on screen. Each paints a document of its
/// own, so the view is part of what the painted rows are keyed by.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum View {
    #[default]
    Projection,
    /// The body the provider adapter would send for the same request.
    Wire,
}

impl View {
    fn switched(self) -> Self {
        match self {
            Self::Projection => Self::Wire,
            Self::Wire => Self::Projection,
        }
    }

    /// The footer's controls in the order drawn. The Wire view has no
    /// sections to jump between.
    fn controls(self) -> &'static [Control] {
        match self {
            Self::Projection => &[
                Control::Switch,
                Control::Next,
                Control::Previous,
                Control::Copy,
                Control::Close,
            ],
            Self::Wire => &[Control::Switch, Control::Copy, Control::Close],
        }
    }

    fn gutter(self) -> u16 {
        match self {
            Self::Projection => GUTTER,
            Self::Wire => NO_GUTTER,
        }
    }

    /// The width the view's rows are wrapped to, given the body's. The Wire
    /// view pans instead, so the body's width is nothing to it.
    fn wrap_width(self, body: u16) -> u16 {
        match self {
            Self::Projection => body,
            Self::Wire => UNWRAPPED,
        }
    }
}

/// A footer control, reported by a click as its index among the controls the
/// view on screen draws.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Switch,
    Next,
    Previous,
    Copy,
    Close,
}

impl Control {
    /// The key the footer names in `view`, and the words glossing it. `r` is
    /// glossed by the view it switches to.
    fn legend(self, view: View) -> (&'static str, &'static str) {
        match self {
            Self::Switch => match view.switched() {
                View::Projection => (SWITCH_LABEL, PROJECTION_HINT),
                View::Wire => (SWITCH_LABEL, WIRE_HINT),
            },
            Self::Next => (NEXT_LABEL, NEXT_HINT),
            Self::Previous => (PREVIOUS_LABEL, PREVIOUS_HINT),
            Self::Copy => (COPY_LABEL, COPY_HINT),
            Self::Close => (ESC_LABEL, CLOSE_HINT),
        }
    }
}

#[derive(Default)]
pub struct ProjectionModal {
    open: bool,
    /// `None` until the agent has bound a prompt, when there is no request to
    /// show yet.
    projection: Option<Projection>,
    /// The Projection view's title. The Wire view's comes with its body.
    title: String,
    view: View,
    /// Painted per view, wrap width and theme generation.
    document: DocumentView<(View, u16, u64)>,
    /// The Projection view's text before it was wrapped or escaped: what `y`
    /// hands over there when nothing is swept. Every paint leaves it here, and
    /// a copy that comes before any builds it.
    source: Option<String>,
    /// What the provider answered when first asked for the body it would
    /// send, kept for the rest of the modal's life.
    wire: Option<Wire>,
    popup: Rect,
    footer: FooterHits,
}

impl ProjectionModal {
    pub(crate) fn open(&mut self, projection: Option<Projection>) {
        self.title = title(projection.as_ref());
        self.projection = projection;
        self.open = true;
        self.view = View::default();
        self.source = None;
        self.wire = None;
        self.document.reset();
        self.footer.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.projection = None;
        self.source = None;
        self.wire = None;
        self.document.reset();
        self.footer.reset();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub(crate) fn projection(&self) -> Option<&Projection> {
        self.projection.as_ref()
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.document.scroll(delta);
    }

    /// A sideways wheel over the modal, which reaches anything only while a
    /// row runs past the body.
    pub fn pan(&mut self, delta: i32) {
        self.document.pan(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> ProjectionAction {
        // Before the close chord, because a sweep the reader can see is what
        // the chord is for. With nothing selected it is the way out, as it is
        // everywhere else.
        if key::QUIT.matches(key_event)
            && let Some(copy) = self.copy_selection()
        {
            return copy;
        }
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
            return ProjectionAction::Consumed;
        }
        if key::SELECT_ALL.matches(key_event) {
            self.document.select_all();
            return ProjectionAction::Consumed;
        }
        match plain_char(&key_event) {
            Some('r') => self.switch_view(),
            Some('n') => self.document.jump(Jump::Next),
            Some('p') => self.document.jump(Jump::Previous),
            Some('y') => return self.copy(),
            _ => {
                self.document.handle_scroll_key(key_event);
            }
        }
        ProjectionAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ProjectionAction {
        match self.document.handle_mouse(event) {
            DocumentMouse::Consumed => return ProjectionAction::Consumed,
            DocumentMouse::Copy(text) => return copy_swept(text),
            DocumentMouse::Passthrough => {}
        }
        let control = self
            .footer
            .handle_mouse(event)
            .and_then(|index| self.view.controls().get(index));
        match control {
            Some(Control::Switch) => self.switch_view(),
            Some(Control::Next) => self.document.jump(Jump::Next),
            Some(Control::Previous) => self.document.jump(Jump::Previous),
            Some(Control::Copy) => return self.copy(),
            Some(Control::Close) => self.close(),
            None => {}
        }
        ProjectionAction::Consumed
    }

    /// Neither offset nor sweep survives the switch: the two views share no
    /// rows.
    fn switch_view(&mut self) {
        self.view = self.view.switched();
        self.document.reset();
        self.footer.clear();
    }

    /// The sweep if there is one, the whole view otherwise, as the text it was
    /// painted from: unwrapped, with every character the escaping drew in its
    /// place handed over as it was sent, and in the Wire view with nothing
    /// elided. Neither waits on a frame to have painted the view.
    fn copy(&mut self) -> ProjectionAction {
        if let Some(copy) = self.copy_selection() {
            return copy;
        }
        let (text, label) = match self.view {
            View::Projection => {
                let source = self.source.get_or_insert_with(|| {
                    projection::source(self.projection.as_ref(), &theme::current())
                });
                (source.clone(), COPIED)
            }
            View::Wire => (
                built_wire(&mut self.wire, self.projection.as_ref()).source(),
                COPIED_WIRE,
            ),
        };
        ProjectionAction::Copy { text, label }
    }

    fn copy_selection(&self) -> Option<ProjectionAction> {
        self.document.selected_text().map(copy_swept)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("projection_modal", area);

        let theme = theme::current();
        let gutter = self.view.gutter();
        let width = self.view.wrap_width(body_width(area.width, gutter));
        self.document.ensure(
            (self.view, width, theme::generation()),
            gutter,
            || match self.view {
                View::Projection => {
                    let (painted, source) =
                        projection::paint(self.projection.as_ref(), width, &theme);
                    self.source = Some(source);
                    painted
                }
                View::Wire => built_wire(&mut self.wire, self.projection.as_ref()).paint(&theme),
            },
        );
        let title = match self.view {
            View::Projection => self.title.as_str(),
            View::Wire => built_wire(&mut self.wire, self.projection.as_ref())
                .title
                .as_str(),
        };
        let footer = footer(self.view, &theme);
        self.popup = self
            .document
            .render(frame, area, title, &footer, &mut self.footer);
        self.popup
    }
}

impl Overlay for ProjectionModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

/// Hands over the text a sweep covers, under the label every sweep copies
/// with, whether a key asked or the pointer let go.
fn copy_swept(text: String) -> ProjectionAction {
    ProjectionAction::Copy {
        text,
        label: COPIED_SELECTION,
    }
}

/// The Wire view's body, asked of the provider the first time the view needs
/// it, so a reader who never switches never costs a dry run.
fn built_wire<'a>(wire: &'a mut Option<Wire>, projection: Option<&Projection>) -> &'a Wire {
    wire.get_or_insert_with(|| Wire::build(projection))
}

fn title(projection: Option<&Projection>) -> String {
    let Some(projection) = projection else {
        return UNPREPARED_TITLE.to_owned();
    };
    let model = escape_terminal_controls(&projection.prompt.model.spec());
    let messages = counted(projection.messages.len(), MESSAGE_NOUN);
    format!("{TITLE_PREFIX}{model}{SEPARATOR}{messages}{TITLE_END}")
}

fn footer(view: View, theme: &Theme) -> FooterLine {
    let mut footer = FooterLine::default();
    for (index, control) in view.controls().iter().enumerate() {
        if index > 0 {
            footer.text(SEPARATOR, theme.tool_dim);
        }
        let (label, hint) = control.legend(view);
        footer.command(label, theme.keybind_key);
        footer.describe(hint, theme.tool_dim);
    }
    footer
}

/// `1 message`, `3 messages`.
fn counted(count: usize, noun: &str) -> String {
    match count {
        1 => format!("{count} {noun}"),
        _ => format!("{count} {noun}s"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        AgentError, ContentBlock, ImageMediaType, ImageSource, Model, ModelInfo, ProviderEvent,
        RequestOptions, Role, StreamResponse, WireRequest,
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use flume::Sender;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::projection::{
        BAR, SYSTEM_HEADER, TOOL_DESCRIPTION_KEY, TOOL_NAME_KEY, TOOLS_HEADER, USER_HEADER,
    };
    use super::wire::{ELIDED_OPEN, UNBUILT, UNBUILT_TITLE, WIRE_TITLE_PREFIX};
    use super::*;
    use crate::components::key as key_ev;

    pub(super) const SYSTEM: &str = "# Rules\n\nBe **brief**.";
    pub(super) const TOOLS: [&str; 2] = ["bash", "file_read"];
    /// Every tool's, markdown across more than one line.
    pub(super) const TOOL_DESCRIPTION: &str = "Runs **one** command.\n\n- `cwd` is the project";
    pub(super) const SCHEMA_KEY: &str = "input_schema";
    pub(super) const PARAMETER: &str = "command";
    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const TEXT: &str = "hello";
    const WIRE_METHOD: &str = "POST";
    const WIRE_URL: &str = "https://api.example.test/v1/messages";
    /// [`WIRE_URL`] as a base URL from the user's config could spell it.
    const CREDENTIALED_WIRE_URL: &str = "https://user:hunter2@api.example.test/v1/messages";
    const PASSWORD: &str = "hunter2";
    /// Fixed, so two modals opened on one conversation build one body.
    const CONVERSATION_KEY: &str = "conversation";
    const SYSTEM_KEY: &str = "system";
    const MESSAGES_KEY: &str = "messages";
    const CACHE_KEY: &str = "cache_key";
    /// Long enough to be shortened on screen, and run together on no row.
    const IMAGE_DIGITS: usize = 1024;
    const IMAGE_DIGIT: &str = "Q";
    /// A run of [`IMAGE_DIGIT`]s any row drawing the payload would show.
    const PAYLOAD_GLIMPSE: usize = 16;
    const TERMINAL_WIDTH: u16 = 100;
    /// Wide enough to widen the popup past its size at [`TERMINAL_WIDTH`].
    const RESIZED_WIDTH: u16 = 120;
    /// Short enough that the fixture runs past the body, so every section can
    /// be scrolled to the top.
    const SHORT_HEIGHT: u16 = 24;
    const TALL_HEIGHT: u16 = 120;
    /// Rows each message spends on its text, so the conversation outgrows a
    /// [`SHORT_HEIGHT`] body.
    const MESSAGE_LINES: usize = 20;
    const MESSAGES: usize = 2;
    /// Rows the reader scrolls down before switching view.
    const SCROLLED: i32 = 5;
    const ROUND_TRIPS: usize = 2;
    const CALL_ID: &str = "toolu_01";
    /// A tool result's row, repeated [`PAST_THE_CAP`] times: more rows than a
    /// `u16` counts.
    const RESULT_ROW: &str = "result";
    const PAST_THE_CAP: usize = u16::MAX as usize + 1;
    const RUNNING_CALLS: usize = 2;
    const RUNNING_NOTICE: &str = "The results of 2 running calls join the next request.";
    const JUMP_WRONG: &str = "n and p must bring a section header to the top of the body";
    const COPY_IS_SOURCE: &str = "y with nothing swept must hand over the source of the view";
    const BARS_MISSING: &str = "the sections must wear their bars";
    const GUTTER_COPIED: &str = "the gutter must never be copied";
    const SWEEP_NOT_COPIED: &str = "letting go of a sweep must copy it";
    /// Where a sweep from the body's first cell ends: a few characters into
    /// its third row, so it runs across the system prompt's header.
    const SWEEP_END: (u16, u16) = (3, 2);
    const TITLE_WRONG: &str = "the title must name the model and count the messages";
    const WIRE_TITLE_WRONG: &str = "the Wire view must be titled by the request line";
    const CREDENTIALS_SHOWN: &str = "the title must never show a URL's credentials";
    const REFUSAL_HIDDEN: &str = "a provider that cannot build its body must be heard saying why";
    const SWITCH_WRONG: &str = "two switches must land at the top of the projection";
    const FOOTER_WRONG: &str = "the footer must offer what the view on screen does";
    const SWITCH_UNHEARD: &str = "the footer's r must switch the view";
    const BUILT_AGAIN: &str = "the wire body must be asked for once, and only when needed";
    const WIRE_DRIFTED: &str = "the wire body must be built from what the projection shows";
    const CLAMP_IDLE: &str = "the fixture must ask for an option its model does not serve";
    const NOT_ELIDED: &str = "a long base64 payload must be shown by its size";
    const COPY_ELIDED: &str = "y must copy the body whole, with nothing elided";
    const EARLY_COPY_WRONG: &str =
        "a copy before the first frame must hand over what one after does";
    const CONTROL_MISSING: &str = "the view on screen must draw the control";
    const NOT_SCROLLED: &str =
        "the fixture must start a row down a conversation taller than the body";
    const CHORD_TAKEN: &str = "Ctrl+Y must scroll a row up, never copy";
    const BODY_MISSING: &str = "a document past the row cap must still draw its body";
    const RESIZE_WRONG: &str = "a resize must keep a sweep over rows it does not rewrap";
    const RUNNING_WRONG: &str = "running calls are announced under the wire body, never copied";

    /// Never asked for anything but the dry run it cannot answer: the modal
    /// only reads the prompt it bound.
    struct Unused;

    impl Provider for Unused {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Answers a dry run with a body made of what it was asked with, and keeps
    /// the tools and options of every run.
    #[derive(Default)]
    struct Echo {
        asked: Mutex<Vec<(Value, RequestOptions)>>,
    }

    impl Echo {
        fn asked(&self) -> Vec<(Value, RequestOptions)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Provider for Echo {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn wire_request(
            &self,
            _: &Model,
            messages: &[Message],
            system: &str,
            tools: &Value,
            opts: &RequestOptions,
            cache_key: Option<&CacheKey>,
        ) -> Result<WireRequest, AgentError> {
            self.asked
                .lock()
                .unwrap()
                .push((tools.clone(), opts.clone()));
            Ok(WireRequest {
                method: WIRE_METHOD,
                url: CREDENTIALED_WIRE_URL.into(),
                body: json!({
                    SYSTEM_KEY: system,
                    CACHE_KEY: cache_key.map(CacheKey::as_str),
                    MESSAGES_KEY: messages,
                }),
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    pub(super) fn projection(messages: Vec<Message>, running_calls: usize) -> Projection {
        routed(Arc::new(Unused), messages, running_calls)
    }

    /// The input schema every fixture tool declares.
    pub(super) fn tool_schema() -> Value {
        json!({ "type": "object", "properties": { PARAMETER: { "type": "string" } } })
    }

    fn routed(
        provider: Arc<dyn Provider>,
        messages: Vec<Message>,
        running_calls: usize,
    ) -> Projection {
        let tools = TOOLS
            .iter()
            .map(|name| {
                json!({
                    TOOL_NAME_KEY: name,
                    TOOL_DESCRIPTION_KEY: TOOL_DESCRIPTION,
                    SCHEMA_KEY: tool_schema(),
                })
            })
            .collect();
        Projection {
            prompt: Arc::new(BtwPrompt {
                provider,
                model: Model::from_spec(MODEL).unwrap(),
                system: SYSTEM.into(),
                tools: Value::Array(tools),
                opts: RequestOptions::default(),
            }),
            messages,
            running_calls,
            cache_key: CacheKey::task(None, CONVERSATION_KEY),
            // A mode MODEL does not serve, so clamping changes these.
            opts: RequestOptions {
                fast: true,
                ..RequestOptions::default()
            },
        }
    }

    /// Messages long enough between them to scroll every header to the top.
    fn conversation() -> Vec<Message> {
        let text = vec![TEXT; MESSAGE_LINES].join("\n");
        vec![Message::user(text); MESSAGES]
    }

    fn opened() -> ProjectionModal {
        let mut modal = ProjectionModal::default();
        modal.open(Some(projection(conversation(), 0)));
        modal
    }

    fn echoed(echo: &Arc<Echo>, messages: Vec<Message>) -> ProjectionModal {
        let mut modal = ProjectionModal::default();
        modal.open(Some(routed(echo.clone(), messages, 0)));
        modal
    }

    fn switch(modal: &mut ProjectionModal) {
        modal.handle_key(key_ev(KeyCode::Char('r')));
    }

    fn render(modal: &mut ProjectionModal, height: u16) -> Buffer {
        render_sized(modal, TERMINAL_WIDTH, height)
    }

    fn render_sized(modal: &mut ProjectionModal, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(buffer: &Buffer) -> String {
        crate::components::buffer_text(buffer)
    }

    /// The body's first row, as drawn.
    fn top_row(modal: &mut ProjectionModal) -> String {
        let buffer = render(modal, SHORT_HEIGHT);
        let content = modal.document.content();
        (content.x..content.right())
            .map(|x| buffer[(x, content.y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn copied(modal: &mut ProjectionModal) -> (String, &'static str) {
        let ProjectionAction::Copy { text, label } = modal.handle_key(key_ev(KeyCode::Char('y')))
        else {
            panic!("{COPY_IS_SOURCE}");
        };
        (text, label)
    }

    fn click(modal: &mut ProjectionModal, control: Control) -> ProjectionAction {
        let index = modal
            .view
            .controls()
            .iter()
            .position(|&drawn| drawn == control)
            .expect(CONTROL_MISSING);
        let hit = modal.footer.hit(index);
        let press = |kind| MouseEvent {
            kind,
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        };
        modal.handle_mouse(press(MouseEventKind::Down(MouseButton::Left)));
        modal.handle_mouse(press(MouseEventKind::Up(MouseButton::Left)))
    }

    /// Drags the pointer across the body between two positions counted from
    /// its first cell and lets go. Answers with what the release asked of the
    /// host.
    fn sweep(modal: &mut ProjectionModal, from: (u16, u16), to: (u16, u16)) -> ProjectionAction {
        let content = modal.document.content();
        let at = |kind, (column, row): (u16, u16)| MouseEvent {
            kind,
            column: content.x + column,
            row: content.y + row,
            modifiers: KeyModifiers::NONE,
        };
        modal.handle_mouse(at(MouseEventKind::Down(MouseButton::Left), from));
        modal.handle_mouse(at(MouseEventKind::Drag(MouseButton::Left), to));
        modal.handle_mouse(at(MouseEventKind::Up(MouseButton::Left), to))
    }

    /// What a provider that never describes its body answers a dry run with.
    fn refusal() -> String {
        let model = Model::from_spec(MODEL).unwrap();
        Unused
            .wire_request(
                &model,
                &[],
                SYSTEM,
                &Value::Null,
                &RequestOptions::default(),
                None,
            )
            .unwrap_err()
            .user_message()
    }

    #[test_case(1, "1 message"  ; "one_message")]
    #[test_case(3, "3 messages" ; "several_messages")]
    fn the_title_names_the_model_and_counts_the_messages(count: usize, messages: &str) {
        let mut modal = ProjectionModal::default();
        modal.open(Some(projection(vec![Message::user(TEXT.into()); count], 0)));

        let expected = format!("{TITLE_PREFIX}{MODEL}{SEPARATOR}{messages}{TITLE_END}");
        assert!(
            screen(&render(&mut modal, TALL_HEIGHT)).contains(&expected),
            "{TITLE_WRONG}"
        );
    }

    #[test]
    fn an_unprepared_request_says_so() {
        let mut modal = ProjectionModal::default();
        modal.open(None);

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));
        assert!(drawn.contains(UNPREPARED_TITLE));
        assert!(drawn.contains(UNPREPARED));
    }

    #[test_case(&['n'],           TOOLS_HEADER  ; "next_from_the_top")]
    #[test_case(&['n', 'n'],      USER_HEADER   ; "next_twice")]
    #[test_case(&['n', 'n', 'p'], TOOLS_HEADER  ; "back_again")]
    #[test_case(&['p'],           SYSTEM_HEADER ; "previous_from_the_top_stays")]
    fn jumps_bring_a_section_header_to_the_top(keys: &[char], header: &str) {
        let mut modal = opened();
        render(&mut modal, SHORT_HEIGHT);

        for &key in keys {
            modal.handle_key(key_ev(KeyCode::Char(key)));
        }

        let top = top_row(&mut modal);
        assert!(top.contains(header), "{JUMP_WRONG}: {top:?}");
    }

    #[test]
    fn copy_without_a_sweep_hands_over_the_source_and_no_gutter() {
        let mut modal = opened();
        let buffer = render(&mut modal, TALL_HEIGHT);
        assert!(screen(&buffer).contains(BAR), "{BARS_MISSING}");

        let (text, label) = copied(&mut modal);

        assert_eq!(label, COPIED);
        assert!(
            text.starts_with(&format!("{SYSTEM_HEADER}\n{SYSTEM}\n")),
            "{COPY_IS_SOURCE}"
        );
        assert!(text.ends_with(TEXT), "{COPY_IS_SOURCE}");
        assert!(!text.contains(BAR), "{GUTTER_COPIED}");
    }

    #[test]
    fn select_all_copies_the_rows_and_none_of_the_gutter() {
        let mut modal = opened();
        render(&mut modal, TALL_HEIGHT);
        modal.handle_key(key::SELECT_ALL.to_key_event());

        let (text, label) = copied(&mut modal);

        assert_eq!(label, COPIED_SELECTION);
        assert!(text.starts_with(SYSTEM_HEADER), "{GUTTER_COPIED}: {text:?}");
        assert!(!text.contains(BAR), "{GUTTER_COPIED}");
    }

    #[test]
    fn a_released_sweep_copies_its_rows_and_none_of_the_gutter() {
        let mut modal = opened();
        render(&mut modal, TALL_HEIGHT);

        let ProjectionAction::Copy { text, label } = sweep(&mut modal, (0, 0), SWEEP_END) else {
            panic!("{SWEEP_NOT_COPIED}");
        };

        assert_eq!(label, COPIED_SELECTION);
        assert!(text.starts_with(SYSTEM_HEADER), "{GUTTER_COPIED}: {text:?}");
        assert!(!text.contains(BAR), "{GUTTER_COPIED}");
    }

    /// Nothing has painted the view yet, so the copy cannot lean on a paint
    /// having left its text behind.
    #[test_case(0 ; "projection")]
    #[test_case(1 ; "wire")]
    fn a_copy_before_the_first_frame_hands_over_what_a_later_one_does(switches: usize) {
        let echo = Arc::new(Echo::default());
        let (mut early, mut painted) =
            (echoed(&echo, conversation()), echoed(&echo, conversation()));
        for modal in [&mut early, &mut painted] {
            for _ in 0..switches {
                switch(modal);
            }
        }
        render(&mut painted, TALL_HEIGHT);

        let copy = copied(&mut early);

        assert!(!copy.0.is_empty(), "{EARLY_COPY_WRONG}");
        assert_eq!(copy, copied(&mut painted), "{EARLY_COPY_WRONG}");
    }

    #[test_case(key_ev(KeyCode::Esc)     ; "esc")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c")]
    fn the_modal_closes(key_event: KeyEvent) {
        let mut modal = opened();
        modal.handle_key(key_event);
        assert!(!modal.is_open());
    }

    /// `y` copies, and the chord spelled with it scrolls like it does in every
    /// other modal.
    #[test]
    fn ctrl_y_scrolls_a_row_up_rather_than_copying() {
        let mut modal = opened();
        render(&mut modal, SHORT_HEIGHT);
        modal.scroll(-1);
        assert!(
            !top_row(&mut modal).contains(SYSTEM_HEADER),
            "{NOT_SCROLLED}"
        );

        let action = modal.handle_key(key::SCROLL_LINE_UP.to_key_event());

        assert!(
            matches!(action, ProjectionAction::Consumed),
            "{CHORD_TAKEN}"
        );
        assert!(top_row(&mut modal).contains(SYSTEM_HEADER), "{CHORD_TAKEN}");
    }

    #[test]
    fn the_footer_answers_the_pointer() {
        let mut modal = opened();
        render(&mut modal, SHORT_HEIGHT);

        click(&mut modal, Control::Next);
        assert!(top_row(&mut modal).contains(TOOLS_HEADER), "{JUMP_WRONG}");
        click(&mut modal, Control::Previous);
        assert!(top_row(&mut modal).contains(SYSTEM_HEADER), "{JUMP_WRONG}");
        assert!(matches!(
            click(&mut modal, Control::Copy),
            ProjectionAction::Copy { label: COPIED, .. }
        ));

        click(&mut modal, Control::Switch);
        let drawn = screen(&render(&mut modal, SHORT_HEIGHT));
        assert!(drawn.contains(UNBUILT_TITLE), "{SWITCH_UNHEARD}");

        click(&mut modal, Control::Close);
        assert!(!modal.is_open());
    }

    #[test_case(0, PROJECTION_HINT, WIRE_HINT       ; "projection_offers_the_wire")]
    #[test_case(1, WIRE_HINT,       PROJECTION_HINT ; "wire_offers_the_projection")]
    fn the_footer_offers_the_other_view(switches: usize, hidden: &str, offered: &str) {
        let mut modal = opened();
        for _ in 0..switches {
            switch(&mut modal);
        }

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));

        assert!(
            drawn.contains(&format!("{SWITCH_LABEL}{offered}")),
            "{FOOTER_WRONG}"
        );
        assert!(
            !drawn.contains(&format!("{SWITCH_LABEL}{hidden}")),
            "{FOOTER_WRONG}"
        );
        assert_eq!(
            drawn.contains(&format!("{NEXT_LABEL}{NEXT_HINT}")),
            switches == 0,
            "{FOOTER_WRONG}"
        );
    }

    /// The URL can come from the user's config, and the title is on screen for
    /// anyone to read.
    #[test]
    fn the_wire_view_is_titled_by_the_request_line_without_credentials() {
        let mut modal = echoed(&Arc::new(Echo::default()), conversation());
        switch(&mut modal);

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));

        let expected = format!("{WIRE_TITLE_PREFIX}{WIRE_METHOD} {WIRE_URL}{TITLE_END}");
        assert!(drawn.contains(&expected), "{WIRE_TITLE_WRONG}");
        assert!(!drawn.contains(PASSWORD), "{CREDENTIALS_SHOWN}");
    }

    #[test]
    fn a_provider_that_cannot_describe_its_body_says_why() {
        let mut modal = opened();
        switch(&mut modal);

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));

        assert!(drawn.contains(UNBUILT_TITLE), "{REFUSAL_HIDDEN}");
        assert!(drawn.contains(UNBUILT), "{REFUSAL_HIDDEN}");
        assert!(drawn.contains(&refusal()), "{REFUSAL_HIDDEN}");
    }

    #[test]
    fn switching_there_and_back_lands_at_the_top() {
        let mut modal = opened();
        render(&mut modal, SHORT_HEIGHT);
        modal.scroll(-SCROLLED);
        assert!(
            !top_row(&mut modal).contains(SYSTEM_HEADER),
            "{SWITCH_WRONG}"
        );

        switch(&mut modal);
        render(&mut modal, SHORT_HEIGHT);
        switch(&mut modal);

        assert!(
            top_row(&mut modal).contains(SYSTEM_HEADER),
            "{SWITCH_WRONG}"
        );
    }

    /// The Projection view's rows are wrapped to the body, so a new width
    /// rewraps them and the sweep's rows are gone. The Wire view's never are.
    #[test_case(0, false ; "the_projection_rewraps")]
    #[test_case(1, true  ; "the_wire_pans")]
    fn a_resize_keeps_a_sweep_only_over_rows_it_leaves_alone(switches: usize, kept: bool) {
        let mut modal = echoed(&Arc::new(Echo::default()), conversation());
        for _ in 0..switches {
            switch(&mut modal);
        }
        render(&mut modal, TALL_HEIGHT);
        modal.handle_key(key::SELECT_ALL.to_key_event());

        render_sized(&mut modal, RESIZED_WIDTH, TALL_HEIGHT);

        assert_eq!(
            modal.document.selected_text().is_some(),
            kept,
            "{RESIZE_WRONG}"
        );
    }

    #[test]
    fn the_wire_body_is_asked_for_once_and_only_when_shown() {
        let echo = Arc::new(Echo::default());
        let mut modal = echoed(&echo, conversation());
        render(&mut modal, TALL_HEIGHT);
        copied(&mut modal);
        assert!(echo.asked().is_empty(), "{BUILT_AGAIN}");

        for _ in 0..ROUND_TRIPS {
            switch(&mut modal);
            render(&mut modal, TALL_HEIGHT);
            copied(&mut modal);
            switch(&mut modal);
            render(&mut modal, TALL_HEIGHT);
        }

        assert_eq!(echo.asked().len(), 1, "{BUILT_AGAIN}");
    }

    #[test]
    fn the_wire_body_is_asked_for_with_the_bound_tools_and_clamped_options() {
        let echo = Arc::new(Echo::default());
        let mut modal = echoed(&echo, conversation());
        switch(&mut modal);

        render(&mut modal, TALL_HEIGHT);

        let projection = modal.projection().unwrap();
        let prompt = &projection.prompt;
        let clamped = projection.opts.clamped(&prompt.model);
        assert_ne!(clamped, projection.opts, "{CLAMP_IDLE}");
        assert_eq!(
            echo.asked(),
            [(prompt.tools.clone(), clamped)],
            "{WIRE_DRIFTED}"
        );
    }

    #[test]
    fn the_wire_body_is_built_from_what_the_projection_shows() {
        let mut modal = echoed(&Arc::new(Echo::default()), conversation());
        switch(&mut modal);

        let (text, label) = copied(&mut modal);

        let body: Value = serde_json::from_str(&text).unwrap();
        let projection = modal.projection().unwrap();
        assert_eq!(label, COPIED_WIRE);
        assert_eq!(body[SYSTEM_KEY], SYSTEM, "{WIRE_DRIFTED}");
        assert_eq!(
            body[CACHE_KEY],
            projection.cache_key.as_str(),
            "{WIRE_DRIFTED}"
        );
        assert_eq!(
            body[MESSAGES_KEY],
            serde_json::to_value(&projection.messages).unwrap(),
            "{WIRE_DRIFTED}"
        );
    }

    #[test]
    fn base64_is_shortened_on_screen_and_copied_whole() {
        let payload = IMAGE_DIGIT.repeat(IMAGE_DIGITS);
        let image = Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, payload.as_str().into()),
            }],
            ..Default::default()
        };
        let mut modal = echoed(&Arc::new(Echo::default()), vec![image]);
        switch(&mut modal);

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));
        let (text, _) = copied(&mut modal);

        assert!(drawn.contains(ELIDED_OPEN), "{NOT_ELIDED}");
        assert!(
            !drawn.contains(&IMAGE_DIGIT.repeat(PAYLOAD_GLIMPSE)),
            "{NOT_ELIDED}"
        );
        assert!(text.contains(&payload), "{COPY_ELIDED}");
        assert!(!text.contains(ELIDED_OPEN), "{COPY_ELIDED}");
    }

    /// Mid-run the body carries the trailing turn's calls with no results,
    /// which is worth saying under it as the Projection view does.
    #[test]
    fn running_calls_are_announced_under_the_wire_body_and_never_copied() {
        let mut modal = ProjectionModal::default();
        let messages = vec![Message::user(TEXT.into())];
        modal.open(Some(routed(
            Arc::new(Echo::default()),
            messages,
            RUNNING_CALLS,
        )));
        switch(&mut modal);

        let drawn = screen(&render(&mut modal, TALL_HEIGHT));
        let (text, _) = copied(&mut modal);

        assert!(drawn.contains(RUNNING_NOTICE), "{RUNNING_WRONG}");
        assert!(!text.contains(RUNNING_NOTICE), "{RUNNING_WRONG}");
    }

    /// The document is cut to the newest `u16::MAX` rows, and the popup it
    /// asks for must not overflow counting them with its footer and border.
    #[test]
    fn a_document_past_the_row_cap_still_draws_its_body() {
        let result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: CALL_ID.into(),
                content: format!("{RESULT_ROW}\n").repeat(PAST_THE_CAP),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        };
        let mut modal = ProjectionModal::default();
        modal.open(Some(projection(vec![result], 0)));

        let drawn = screen(&render(&mut modal, SHORT_HEIGHT));

        assert!(drawn.contains(RESULT_ROW), "{BODY_MISSING}");
    }
}
