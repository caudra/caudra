//! `/docs`: the manual this build shipped with, read and searched inside the
//! TUI.
//!
//! The reader shows one page and follows its links from the keyboard or the
//! pointer, and a link out of the docs opens in the browser. The contents list
//! docks beside the reader when both fit, and the search view takes the
//! reader's place while it has focus. Everything is kept across closes, so
//! `/docs` reopens where it was left.

mod contents;
mod reader;
mod search;

use caudra_docs::{DocsLibrary, Library, Target};
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::document_view::{COPIED_SELECTION, Jump};
use crate::components::keybindings::key;
use crate::components::list_picker::truncate_label;
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::{Overlay, PAN_STEP, escape_terminal_controls, plain_char};
use crate::theme::{self, Theme};
use contents::Contents;
use reader::{Reader, ReaderMouse};
use search::SearchView;

const TITLE: &str = "Docs";
/// Either side of the title, clear of the border it sits on.
const TITLE_PAD: &str = " ";
const CRUMB: &str = " › ";
const WIDTH_PERCENT: u16 = 92;
const MAX_HEIGHT_PERCENT: u16 = 88;
const H_PAD: u16 = 1;
const FOOTER_ROWS: u16 = 1;
/// The workbench's default sidebar width.
const SIDEBAR_WIDTH: u16 = 30;
const PANE_GAP: u16 = 1;
/// The narrowest the reader gets before the sidebar gives way to it.
const READER_MIN_COLS: u16 = 64;
const PAGE_TITLE_LEVEL: u8 = 1;
const SECTION_LEVEL: u8 = 2;
const KEY_GAP: &str = " ";
/// How the footer draws itself, widest first: glossed, then keys alone, then
/// keys packed. Every key is on every rung, so a narrow terminal loses words
/// and never a key.
const FOOTER_RUNGS: [(bool, &str); 3] = [(true, SEPARATOR), (false, SEPARATOR), (false, KEY_GAP)];
const READER_FOOTER: [FooterKey; 9] = [
    ("/", " search", KeyCode::Char('/')),
    ("c", " contents", KeyCode::Char('c')),
    ("Tab", " link", KeyCode::Tab),
    ("Enter", " follow", KeyCode::Enter),
    ("Backspace", " back", KeyCode::Backspace),
    ("]", " forward", KeyCode::Char(']')),
    ("n", " next heading", KeyCode::Char('n')),
    (key::FIND_NEXT.label, " next match", key::FIND_NEXT.code),
    (ESC_LABEL, CLOSE_HINT, KeyCode::Esc),
];
const CONTENTS_FOOTER: [FooterKey; 3] = [
    ("Enter", " open", KeyCode::Enter),
    ("Tab", " reader", KeyCode::Tab),
    (ESC_LABEL, " clear", KeyCode::Esc),
];
const SEARCH_FOOTER: [FooterKey; 2] = [
    ("Enter", " open", KeyCode::Enter),
    (ESC_LABEL, " reader", KeyCode::Esc),
];
const NO_LINKS: &str = "This page has no links";
const NO_HIGHLIGHTS: &str = "Nothing is highlighted: / searches every page";

/// A footer key: its label, the words glossing it, and the key a click presses.
type FooterKey = (&'static str, &'static str, KeyCode);

/// What the host has to carry out. Moving around the docs is the modal's own
/// business and never reaches here.
#[derive(Debug, PartialEq, Eq)]
pub enum DocsAction {
    Consumed,
    Close,
    Flash(&'static str),
    Copy {
        text: String,
        label: &'static str,
    },
    /// A link that leads out of the docs.
    OpenUrl(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// The home page: every page as a link, grouped as on the site.
    Contents,
    Page(usize),
}

/// A place in the docs. `line` is the source line at the top of the reader,
/// which unlike a row does not move when the page is rewrapped.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Location {
    shown: Shown,
    line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Contents,
    Reader,
    Search,
}

/// The page being read and its `##` section at the top of the reader.
#[derive(Clone, Default)]
struct Current {
    page: Option<usize>,
    section: Option<usize>,
}

/// What a key did to a list: nothing the modal sees, a way back to the
/// reader, or a place to open.
enum Pick {
    Stay,
    Leave,
    Open(Target),
}

/// What every pane is drawn with.
struct PaneContext<'a> {
    library: &'a Library,
    theme: &'a Theme,
    /// Where the pointer is: a pane marks what a press there would act on.
    pointer: Option<Position>,
}

pub struct DocsModal {
    library: DocsLibrary,
    open: bool,
    shown: Shown,
    back: Vec<Location>,
    forward: Vec<Location>,
    pane: Pane,
    reader: Reader,
    contents: Contents,
    search: SearchView,
    footer: FooterHits,
    /// Where the pointer last was, kept rather than resolved, so each pane
    /// answers for its own geometry on the frame it is drawn.
    pointer: Option<Position>,
    popup: Rect,
    /// Where the contents list was drawn, empty while it is hidden.
    sidebar: Rect,
    /// Whether the last frame had room for the contents list beside the
    /// reader rather than in its place.
    docked: bool,
}

impl DocsModal {
    pub fn new(library: DocsLibrary) -> Self {
        Self {
            library,
            open: false,
            shown: Shown::Contents,
            back: Vec::new(),
            forward: Vec::new(),
            pane: Pane::Reader,
            reader: Reader::default(),
            contents: Contents::default(),
            search: SearchView::default(),
            footer: FooterHits::default(),
            pointer: None,
            popup: Rect::default(),
            sidebar: Rect::default(),
            docked: false,
        }
    }

    /// Opens where it was left, or where `args` points: a page or section in
    /// any form the site spells it, or failing that a search for the words.
    pub fn open(&mut self, args: &str) {
        self.open = true;
        self.pane = Pane::Reader;
        let args = args.trim();
        if args.is_empty() {
            return;
        }
        let library = self.library();
        match library.locate(args) {
            Some(target) => {
                self.visit(target);
            }
            None => {
                self.search.set_query(args, library);
                self.pane = Pane::Search;
            }
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.footer.reset();
        self.pointer = None;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, position: Position) -> bool {
        self.open && self.popup.contains(position)
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.open && self.pane != Pane::Reader
    }

    /// The wheel moves whichever pane is under the pointer. It never passes
    /// through [`handle_mouse`], so it reports where the pointer is itself.
    ///
    /// [`handle_mouse`]: Self::handle_mouse
    pub fn scroll_at(&mut self, position: Position, delta: i32) {
        self.pointer = Some(position);
        if self.sidebar.contains(position) {
            self.contents.scroll(delta);
        } else if self.pane == Pane::Search {
            self.search.scroll(delta);
        } else {
            self.reader.scroll(delta);
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        match self.pane {
            Pane::Contents => self.contents.paste(&text),
            Pane::Search => self.search.paste(&text, self.library()),
            Pane::Reader => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DocsAction {
        // Before the panes, because a sweep the reader can see is what the
        // chord is for. With nothing swept, or the sweep out of sight with the
        // reader, it is the way out.
        if key::QUIT.matches(key) {
            return self
                .reader_shown()
                .then(|| self.reader.selected_text())
                .flatten()
                .map_or(DocsAction::Close, |text| DocsAction::Copy {
                    text,
                    label: COPIED_SELECTION,
                });
        }
        match self.pane {
            Pane::Reader => self.reader_key(key),
            Pane::Contents => self.contents_key(key),
            Pane::Search => self.search_key(key),
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> DocsAction {
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        if let Some(index) = self.footer.handle_mouse(event) {
            return match self.footer_keys().get(index) {
                Some(&(_, _, code)) => self.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
                None => DocsAction::Consumed,
            };
        }
        let library = self.library();
        match event.kind {
            MouseEventKind::ScrollLeft => self.reader.pan(-PAN_STEP),
            MouseEventKind::ScrollRight => self.reader.pan(PAN_STEP),
            _ if self.sidebar.contains(position) => {
                if let Some(target) =
                    self.contents
                        .handle_mouse(event, library, self.current(library))
                {
                    self.open_entry(target);
                }
            }
            _ if self.pane == Pane::Search => {
                if let Some(target) = self.search.handle_mouse(event) {
                    self.open_hit(target);
                }
            }
            _ => {
                // A press on the page reads it, so the keys follow it there
                // from a contents list docked beside it.
                if event.kind == MouseEventKind::Down(MouseButton::Left)
                    && self.reader.contains(position)
                {
                    self.pane = Pane::Reader;
                }
                return match self.reader.handle_mouse(event) {
                    ReaderMouse::Consumed => DocsAction::Consumed,
                    ReaderMouse::Copy(text) => DocsAction::Copy {
                        text,
                        label: COPIED_SELECTION,
                    },
                    ReaderMouse::Follow(target) => self.follow(&target),
                };
            }
        }
        DocsAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("docs_modal", area);
        let theme = theme::current();
        let library = self.library();
        let title = self.title(library);
        let modal = Modal {
            title: &title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        let body = Rect {
            height: padded.height.saturating_sub(FOOTER_ROWS),
            ..padded
        };
        let footer_area = Rect {
            y: body.bottom(),
            height: padded.height.min(FOOTER_ROWS),
            ..padded
        };
        let (sidebar, main) = self.panes(body);
        let context = PaneContext {
            library,
            theme: &theme,
            pointer: self.pointer,
        };
        let current = self.current(library);
        let focused = self.pane == Pane::Contents;
        self.contents
            .draw(frame, sidebar, &context, current, focused);
        let link = match self.pane {
            Pane::Search => {
                self.search.draw(frame, main, &context);
                None
            }
            _ if main.is_empty() => None,
            // The bars run down the popup's edge and along its bottom border,
            // as they do in every other document modal.
            _ => {
                let bars = Rect {
                    x: main.x,
                    width: inner.right().saturating_sub(main.x),
                    ..inner
                };
                self.reader.draw(frame, bars, main, &context, self.shown)
            }
        };
        let line = match link {
            // The row speaks for the link, so it holds no key to press.
            Some(target) => {
                self.footer.set(Vec::new());
                link_line(library, &target, footer_area.width, &theme)
            }
            None => {
                let footer = self.footer_line(footer_area.width, &theme);
                self.footer.set(footer.hits(footer_area, 0, FOOTER_ROWS));
                footer.line(self.footer.hovered())
            }
        };
        frame.render_widget(Paragraph::new(line), footer_area);
        self.sidebar = sidebar;
        self.docked = !sidebar.is_empty() && !main.is_empty();
        self.popup = popup;
        popup
    }

    fn library(&self) -> &'static Library {
        (self.library)()
    }

    fn reader_key(&mut self, key: KeyEvent) -> DocsAction {
        let plain = key.modifiers == KeyModifiers::NONE;
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) | (_, Some('q')) => return DocsAction::Close,
            (_, Some('/')) => self.pane = Pane::Search,
            (_, Some('c')) => self.focus_contents(),
            (_, Some('n')) => self.reader.jump(Jump::Next),
            (_, Some('p')) => self.reader.jump(Jump::Previous),
            (KeyCode::Backspace, _) | (_, Some('[')) => self.back(),
            (_, Some(']')) => self.forward(),
            (KeyCode::Tab, _) => return self.step_link(true),
            (KeyCode::BackTab, _) => return self.step_link(false),
            (KeyCode::Enter, _) => return self.follow_selected(),
            (KeyCode::Left, _) if plain => self.reader.pan(-PAN_STEP),
            (KeyCode::Right, _) if plain => self.reader.pan(PAN_STEP),
            _ if key::FIND_NEXT.matches(key) => return self.step_match(true),
            _ if key::FIND_PREV.matches(key) => return self.step_match(false),
            _ => self.reader.handle_scroll_key(key),
        }
        DocsAction::Consumed
    }

    fn contents_key(&mut self, key: KeyEvent) -> DocsAction {
        let library = self.library();
        match self
            .contents
            .handle_key(key, library, self.current(library))
        {
            Pick::Stay => {}
            Pick::Leave => self.pane = Pane::Reader,
            Pick::Open(target) => self.open_entry(target),
        }
        DocsAction::Consumed
    }

    fn search_key(&mut self, key: KeyEvent) -> DocsAction {
        match self.search.handle_key(key, self.library()) {
            Pick::Stay => {}
            Pick::Leave => self.pane = Pane::Reader,
            Pick::Open(target) => self.open_hit(target),
        }
        DocsAction::Consumed
    }

    fn step_link(&mut self, forward: bool) -> DocsAction {
        match self.reader.step_link(forward) {
            true => DocsAction::Consumed,
            false => DocsAction::Flash(NO_LINKS),
        }
    }

    fn step_match(&mut self, forward: bool) -> DocsAction {
        match self.reader.step_match(forward) {
            true => DocsAction::Consumed,
            false => DocsAction::Flash(NO_HIGHLIGHTS),
        }
    }

    fn follow_selected(&mut self) -> DocsAction {
        match self.reader.selected_target() {
            Some(target) => self.follow(&target),
            None => DocsAction::Consumed,
        }
    }

    /// A target inside the docs opens here; anything else goes to the browser.
    fn follow(&mut self, target: &str) -> DocsAction {
        match self.library().locate(target) {
            Some(found) => {
                self.visit(found);
                DocsAction::Consumed
            }
            None => DocsAction::OpenUrl(target.to_owned()),
        }
    }

    fn focus_contents(&mut self) {
        self.pane = Pane::Contents;
        let library = self.library();
        self.contents.focus(library, self.current(library));
    }

    fn open_entry(&mut self, target: Target) {
        self.contents.clear();
        self.visit(target);
        self.pane = Pane::Reader;
    }

    /// Opens a result with its terms highlighted, and puts the first of them
    /// on screen.
    fn open_hit(&mut self, target: Target) {
        let terms = self.search.terms();
        let line = self.visit(target);
        self.reader.highlight(terms, line);
        self.pane = Pane::Reader;
    }

    /// Goes to `target` the way a link does, leaving the way back behind.
    /// Returns the line it lands on.
    fn visit(&mut self, target: Target) -> usize {
        let line = target
            .heading
            .and_then(|heading| self.library().pages()[target.page].headings.get(heading))
            .map_or(0, |heading| heading.line);
        self.back.push(self.location());
        self.forward.clear();
        self.show(Location {
            shown: Shown::Page(target.page),
            line,
        });
        line
    }

    fn back(&mut self) {
        if let Some(location) = self.back.pop() {
            self.forward.push(self.location());
            self.show(location);
        }
    }

    fn forward(&mut self) {
        if let Some(location) = self.forward.pop() {
            self.back.push(self.location());
            self.show(location);
        }
    }

    fn show(&mut self, location: Location) {
        let leaving = location.shown != self.shown;
        self.shown = location.shown;
        self.reader.land(location.line, leaving);
    }

    fn location(&self) -> Location {
        Location {
            shown: self.shown,
            line: self.reader.top_line(),
        }
    }

    fn current(&self, library: &Library) -> Current {
        let Shown::Page(page) = self.shown else {
            return Current::default();
        };
        let top = self.reader.top_line();
        let section = library.pages()[page]
            .headings
            .iter()
            .rposition(|heading| heading.level == SECTION_LEVEL && heading.line <= top);
        Current {
            page: Some(page),
            section,
        }
    }

    /// `Docs › Page › Section`, naming the section at the top of the reader.
    fn title(&self, library: &Library) -> String {
        let Current { page, section } = self.current(library);
        let Some(page) = page else {
            return format!("{TITLE_PAD}{TITLE}{TITLE_PAD}");
        };
        let here = Target {
            page,
            heading: section,
        };
        format!(
            "{TITLE_PAD}{TITLE}{CRUMB}{}{TITLE_PAD}",
            crumb(library, here)
        )
    }

    /// Whether the reader is on screen: the search view takes its place, and
    /// so does the contents list where there is no room to dock it beside.
    fn reader_shown(&self) -> bool {
        match self.pane {
            Pane::Reader => true,
            Pane::Contents => self.docked,
            Pane::Search => false,
        }
    }

    /// The contents list and the reader or search view. Too narrow for both,
    /// only the focused one draws and the other keeps an empty area.
    fn panes(&self, body: Rect) -> (Rect, Rect) {
        if body.width >= SIDEBAR_WIDTH + PANE_GAP + READER_MIN_COLS {
            let [sidebar, _, main] = Layout::horizontal([
                Constraint::Length(SIDEBAR_WIDTH),
                Constraint::Length(PANE_GAP),
                Constraint::Fill(1),
            ])
            .areas(body);
            return (sidebar, main);
        }
        match self.pane {
            Pane::Contents => (body, Rect::default()),
            Pane::Reader | Pane::Search => (Rect::default(), body),
        }
    }

    fn footer_keys(&self) -> &'static [FooterKey] {
        match self.pane {
            Pane::Reader => &READER_FOOTER,
            Pane::Contents => &CONTENTS_FOOTER,
            Pane::Search => &SEARCH_FOOTER,
        }
    }

    /// Whether a footer key would do anything now, which is what dims it.
    fn enabled(&self, code: KeyCode) -> bool {
        if self.pane != Pane::Reader {
            return true;
        }
        match code {
            KeyCode::Tab => self.reader.has_links(),
            KeyCode::Enter => self.reader.has_link_selected(),
            KeyCode::Backspace => !self.back.is_empty(),
            KeyCode::Char(']') => !self.forward.is_empty(),
            KeyCode::F(_) => self.reader.has_highlights(),
            _ => true,
        }
    }

    fn footer_line(&self, width: u16, theme: &Theme) -> FooterLine {
        let mut footer = self.footer_rung(FOOTER_RUNGS[0], theme);
        for rung in FOOTER_RUNGS.into_iter().skip(1) {
            if footer.fits(width) {
                break;
            }
            footer = self.footer_rung(rung, theme);
        }
        footer
    }

    fn footer_rung(&self, (glossed, gap): (bool, &'static str), theme: &Theme) -> FooterLine {
        let mut footer = FooterLine::default();
        for (index, &(label, gloss, code)) in self.footer_keys().iter().enumerate() {
            if index > 0 {
                footer.text(gap, theme.tool_dim);
            }
            let style = match self.enabled(code) {
                true => theme.keybind_key,
                false => theme.tool_dim,
            };
            footer.command(label, style);
            if glossed {
                footer.describe(gloss, theme.tool_dim);
            }
        }
        footer
    }
}

impl Overlay for DocsModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

/// `Page › Section`, or the page alone for a target at its top.
fn crumb(library: &Library, target: Target) -> String {
    let page = &library.pages()[target.page];
    match target.heading.and_then(|index| page.headings.get(index)) {
        Some(heading) if heading.level != PAGE_TITLE_LEVEL => {
            format!("{}{CRUMB}{}", page.title, heading.title)
        }
        _ => page.title.clone(),
    }
}

/// The footer while the pointer is on a link: the place in the docs it opens,
/// or the address it hands the browser.
fn link_line(library: &Library, target: &str, width: u16, theme: &Theme) -> Line<'static> {
    let place = library
        .locate(target)
        .map_or_else(|| target.to_owned(), |found| crumb(library, found));
    let place = truncate_label(&escape_terminal_controls(&place), usize::from(width));
    Line::from(Span::styled(place, theme.status_notice)).alignment(Alignment::Center)
}

/// The row a press and its release both landed on, which is what opens a list
/// entry or follows a link. A release anywhere else opens nothing.
fn clicked(pressed: &mut Option<usize>, kind: MouseEventKind, row: Option<usize>) -> Option<usize> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            *pressed = row;
            None
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let pressed = pressed.take();
            row.filter(|&row| pressed == Some(row))
        }
        _ => None,
    }
}

/// Where a list's selection goes for a movement key, or `None` for a key that
/// moves nothing. `page` is how far the paging keys move. A selection left past
/// the end of a list that lost rows moves from its last row.
fn list_move(key: KeyEvent, selected: usize, count: usize, page: usize) -> Option<usize> {
    let last = count.saturating_sub(1);
    let selected = selected.min(last);
    let page = page.max(1);
    let up = |rows: usize| selected.saturating_sub(rows);
    let down = |rows: usize| selected.saturating_add(rows).min(last);
    Some(match key.code {
        KeyCode::Up => up(1),
        KeyCode::Down => down(1),
        _ if key::PAGE_UP.matches(key) || key::SCROLL_HALF_UP.matches(key) => up(page),
        _ if key::PAGE_DOWN.matches(key) => down(page),
        _ if key::SCROLL_LINE_UP.matches(key) => up(1),
        _ if key::SCROLL_LINE_DOWN.matches(key) => down(1),
        _ if key::DOC_TOP.matches(key) || key::SCROLL_TOP.matches(key) => 0,
        _ if key::DOC_BOTTOM.matches(key) || key::SCROLL_BOTTOM.matches(key) => last,
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::sync::LazyLock;

    use caudra_docs::Library;

    pub(super) const GUIDE: &str = "guide";
    pub(super) const PERMISSIONS: &str = "permissions";
    pub(super) const MODES: &str = "modes";
    pub(super) const RULES: &str = "rules";
    pub(super) const LINKS: &str = "links";
    pub(super) const EXTERNAL: &str = "https://example.com/";
    pub(super) const TERM: &str = "timeout";
    pub(super) const MODES_LINK: &str = "the modes";
    pub(super) const RULES_LINK: &str = "the rules";
    /// The ends of a link to [`EXTERNAL`] too long for any reader the tests
    /// draw, so it wraps.
    pub(super) const SITE_HEAD: &str = "the site";
    pub(super) const SITE_TAIL: &str = "the install script";
    /// Rows enough that a section below them opens at the top of the reader.
    const FILLER_LINES: usize = 30;
    const LANDING: &str = "\
<span class=\"eyebrow\">Start</span>
<a class=\"card\" href=\"/docs/guide/\"><span class=\"card-title\">Guide</span><span class=\"card-desc\">Getting going.</span></a>
<span class=\"eyebrow\">Reference</span>
<a class=\"card\" href=\"/docs/permissions/\"><span class=\"card-title\">Permissions</span><span class=\"card-desc\">Who may do what.</span></a>
";

    /// Two pages, each long enough to scroll: a guide that links into the
    /// other page at its top, and out of the docs and into it again at its
    /// foot, and a reference with two hits for [`TERM`] a screen apart.
    pub(crate) fn library() -> &'static Library {
        static LIBRARY: LazyLock<Library> = LazyLock::new(|| {
            Library::parse([
                ("_index.md", LANDING),
                ("guide/_index.md", leak(guide())),
                ("permissions/_index.md", leak(permissions())),
            ])
        });
        &LIBRARY
    }

    fn guide() -> String {
        format!(
            "# Guide\n\nRead [{MODES_LINK}](/docs/{PERMISSIONS}/#{MODES}) first.\n\n## Install\n\n{}\n## Links\n\nSee [{SITE_HEAD}, where these pages are kept together with the release notes, the changelog and {SITE_TAIL}]({EXTERNAL}).\n\nThen read [{RULES_LINK}](/docs/{PERMISSIONS}/#{RULES}).\n",
            filler("Step")
        )
    }

    fn permissions() -> String {
        format!(
            "# Permissions\n\nWho may do what.\n\n## Modes\n\n{}\n## Rules\n\nThe shell {TERM} is thirty seconds.\n\n{}\nA {TERM} stops the command.\n\n{}",
            filler("Mode"),
            filler("Rule"),
            filler("Note")
        )
    }

    fn filler(word: &str) -> String {
        (1..=FILLER_LINES)
            .map(|line| format!("- {word} {line}.\n"))
            .collect()
    }

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::fixture::{
        EXTERNAL, GUIDE, LINKS, MODES, MODES_LINK, PERMISSIONS, RULES, RULES_LINK, SITE_HEAD,
        SITE_TAIL, TERM,
    };
    use super::search::{HIT_ROWS, SNIPPET_INDENT};
    use super::*;
    use crate::components::{buffer_text, key as press};

    const WIDE: u16 = 140;
    const NARROW: u16 = 80;
    /// Narrow enough that the reader's footer packs its keys.
    const PACKED: u16 = 50;
    const HEIGHT: u16 = 24;
    /// Short enough that the permissions page's contents list scrolls.
    const SHORT: u16 = 10;
    /// From the foot of the guide `Tab` selects the site, then the rules, then
    /// wraps round to the modes at the top.
    const TABS_TO_THE_TOP: usize = 3;
    /// The guide's title, the blank line under it, and the paragraph that
    /// links to the modes.
    const OPENING_LINES: usize = 3;
    const STALE_SELECTION: usize = 5;
    const SHRUNK_TO: usize = 3;
    const LIST_PAGE: usize = 2;
    const FILTER: &str = "prm";
    const QUERY: &str = "shell timeout";
    /// Found in the modes section and in the guide's link to it.
    const MODES_QUERY: &str = "mode";
    const PERMISSIONS_TITLE: &str = "Permissions";
    const GUIDE_TITLE: &str = "Guide";
    const MODES_TITLE: &str = "Modes";
    const RULES_TITLE: &str = "Rules";
    const REFERENCE_GROUP: &str = "Reference";
    const MISSING_KEY: &str = "a footer key the reader cannot click";
    const WRONG_PLACE: &str = "the reader is not where the address points";
    const OFF_SCREEN: &str = "the highlight moved to is not on screen";
    const NOT_DRAWN: &str = "not drawn where it was looked for";
    const UNMARKED: &str = "what the pointer is on is not marked";
    const STRAY_MARK: &str = "something the pointer is not on is marked";
    const SELECTION_MOVED: &str = "the pointer moved the selection";
    const FOCUS_MOVED: &str = "the pointer moved the focus";
    const UNWRAPPED: &str = "the link has to wrap for this to test anything";
    const ROWS_STILL: &str = "the scroll did not move the rows";
    const WRONG_HINT: &str = "the footer does not say where the hovered link leads";
    const HINT_PRESSABLE: &str = "a footer speaking for a link must hold no key to press";
    const SELECTED_AS_HOVERED: &str = "the selected link must not look hovered";
    const UNSELECTED_LOOK: &str = "the selected link is not drawn as a selection";
    const PAGE_TOO_LONG: &str = "the page has to end above the body's last row";
    const NOT_ABOVE: &str = "the link has to lie above the view End left for this to test anything";
    const LINK_HIDDEN: &str = "the link Tab moved to is not on screen";
    const ENTRY_HIDDEN: &str = "the entry the contents list marks is not on screen";
    const FOCUS_STAYED: &str = "a press on the page left the keys with the contents";
    const HIDDEN_FOLLOWED: &str = "Enter followed a link scrolled out of sight";
    const NOT_MARKDOWN: &str = "the copy is not the Markdown behind the rows swept";
    const WRONG_CTRL_C: &str = "Ctrl+C copies a sweep the reader shows, and closes otherwise";
    const PLACE_LOST: &str = "a resize moved the reading position";

    fn modal() -> DocsModal {
        let mut modal = DocsModal::new(fixture::library);
        modal.open("");
        modal
    }

    fn draw(modal: &mut DocsModal, width: u16) -> Terminal<TestBackend> {
        draw_sized(modal, width, HEIGHT)
    }

    fn draw_sized(modal: &mut DocsModal, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        terminal
    }

    fn region(terminal: &Terminal<TestBackend>, area: Rect) -> String {
        let buffer = terminal.backend().buffer();
        area.positions()
            .map(|position| buffer[position].symbol())
            .collect()
    }

    fn typed(modal: &mut DocsModal, text: &str) {
        for character in text.chars() {
            modal.handle_key(press(KeyCode::Char(character)));
        }
    }

    fn page(slug: &str) -> usize {
        fixture::library()
            .pages()
            .iter()
            .position(|page| page.slug == slug)
            .unwrap()
    }

    fn heading_line(slug: &str, anchor: &str) -> usize {
        fixture::library().pages()[page(slug)]
            .headings
            .iter()
            .find(|heading| heading.anchor == anchor)
            .unwrap()
            .line
    }

    fn mouse(modal: &mut DocsModal, kind: MouseEventKind, at: Position) -> DocsAction {
        modal.handle_mouse(MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn point(modal: &mut DocsModal, at: Position) {
        mouse(modal, MouseEventKind::Moved, at);
    }

    fn click(modal: &mut DocsModal, at: Position) -> DocsAction {
        mouse(modal, MouseEventKind::Down(MouseButton::Left), at);
        mouse(modal, MouseEventKind::Up(MouseButton::Left), at)
    }

    /// Opens the guide at `width` and sweeps its title and first paragraph,
    /// from the title row's first cell to the paragraph row's last, letting
    /// go there.
    fn sweep_opening(modal: &mut DocsModal, width: u16) -> DocsAction {
        modal.open(GUIDE);
        let terminal = draw(modal, width);
        let content = modal.reader.content();
        let title = locate(&terminal, content, GUIDE_TITLE);
        let paragraph = locate(&terminal, content, MODES_LINK);
        let from = Position::new(content.x, title.y);
        let to = Position::new(content.right() - 1, paragraph.y);
        mouse(modal, MouseEventKind::Down(MouseButton::Left), from);
        mouse(modal, MouseEventKind::Drag(MouseButton::Left), to);
        mouse(modal, MouseEventKind::Up(MouseButton::Left), to)
    }

    /// What [`sweep_opening`] covers, as the page's Markdown spells it.
    fn opening_markdown() -> String {
        let display = &fixture::library().pages()[page(GUIDE)].display;
        display
            .lines()
            .take(OPENING_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(terminal: &Terminal<TestBackend>) -> Rect {
        terminal.backend().buffer().area
    }

    /// Where `text` starts in `area`, looked for a row at a time from the top.
    fn locate(terminal: &Terminal<TestBackend>, area: Rect, text: &str) -> Position {
        let buffer = terminal.backend().buffer();
        area.rows()
            .find_map(|row| {
                let line: String = row.positions().map(|at| buffer[at].symbol()).collect();
                let column = line[..line.find(text)?].chars().count();
                Some(Position::new(row.x + u16::try_from(column).ok()?, row.y))
            })
            .unwrap_or_else(|| panic!("{NOT_DRAWN}: {text}"))
    }

    /// How many of the `cells` cells from `at` rightwards are drawn reversed.
    fn reversed(terminal: &Terminal<TestBackend>, at: Position, cells: usize) -> usize {
        let buffer = terminal.backend().buffer();
        (at.x..)
            .take(cells)
            .filter(|&x| buffer[(x, at.y)].modifier.contains(Modifier::REVERSED))
            .count()
    }

    fn searched(query: &str) -> DocsModal {
        let mut modal = modal();
        modal.handle_key(press(KeyCode::Char('/')));
        typed(&mut modal, query);
        modal
    }

    #[test_case(WIDE, true ; "wide")]
    #[test_case(NARROW, false ; "narrow")]
    fn the_sidebar_docks_only_beside_a_full_width_reader(width: u16, docked: bool) {
        let mut modal = modal();
        draw(&mut modal, width);
        assert_eq!(modal.sidebar.width == SIDEBAR_WIDTH, docked);
        assert_eq!(modal.sidebar.is_empty(), !docked);
    }

    /// Each key's label is drawn on the cells a click on it lands on.
    #[test]
    fn the_narrow_footer_keeps_every_key() {
        let mut modal = modal();
        let terminal = draw(&mut modal, NARROW);
        for (index, (label, _, _)) in READER_FOOTER.iter().enumerate() {
            assert_eq!(
                region(&terminal, modal.footer.hit(index)),
                *label,
                "{MISSING_KEY}: {label}"
            );
        }
    }

    #[test]
    fn tab_selects_a_link_that_enter_follows_and_backspace_returns_from() {
        let mut modal = modal();
        modal.open(GUIDE);
        draw(&mut modal, WIDE);
        let guide = modal.location();
        assert_eq!(modal.handle_key(press(KeyCode::Tab)), DocsAction::Consumed);
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            DocsAction::Consumed
        );
        draw(&mut modal, WIDE);
        assert_eq!(
            modal.location(),
            Location {
                shown: Shown::Page(page(PERMISSIONS)),
                line: heading_line(PERMISSIONS, MODES),
            },
            "{WRONG_PLACE}"
        );
        modal.handle_key(press(KeyCode::Backspace));
        draw(&mut modal, WIDE);
        assert_eq!(modal.location(), guide);
    }

    #[test]
    fn a_link_out_of_the_docs_opens_in_the_browser() {
        let mut modal = modal();
        modal.open(GUIDE);
        draw(&mut modal, WIDE);
        modal.handle_key(press(KeyCode::Tab));
        modal.handle_key(press(KeyCode::Tab));
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            DocsAction::OpenUrl(EXTERNAL.to_owned())
        );
    }

    #[test]
    fn a_result_opens_on_its_first_highlight_and_f3_moves_to_the_next() {
        let mut modal = modal();
        modal.handle_key(press(KeyCode::Char('/')));
        typed(&mut modal, TERM);
        let screen = buffer_text(draw(&mut modal, WIDE).backend().buffer());
        assert!(screen.contains(&format!("{PERMISSIONS_TITLE}{CRUMB}{RULES_TITLE}")));

        modal.handle_key(press(KeyCode::Enter));
        draw(&mut modal, WIDE);
        assert_eq!(modal.pane, Pane::Reader);
        let first = modal.reader.found().unwrap();
        assert!(modal.reader.visible().contains(&first), "{OFF_SCREEN}");
        assert!(modal.reader.row_text(first).unwrap().contains(TERM));
        assert!(modal.title(fixture::library()).contains(RULES_TITLE));

        assert_eq!(
            modal.handle_key(key::FIND_NEXT.to_key_event()),
            DocsAction::Consumed
        );
        let next = modal.reader.found().unwrap();
        assert!(next > first);
        assert!(modal.reader.visible().contains(&next), "{OFF_SCREEN}");
        assert!(modal.reader.row_text(next).unwrap().contains(TERM));
    }

    #[test_case("permissions#modes" ; "slug_and_anchor")]
    #[test_case("/docs/permissions/#modes" ; "site_path")]
    #[test_case("https://caudra.ai/docs/permissions/#modes" ; "site_url")]
    fn an_address_opens_its_section_under_its_title(address: &str) {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(address);
        let screen = buffer_text(draw(&mut modal, WIDE).backend().buffer());
        assert_eq!(
            modal.location(),
            Location {
                shown: Shown::Page(page(PERMISSIONS)),
                line: heading_line(PERMISSIONS, MODES),
            },
            "{WRONG_PLACE}"
        );
        assert!(screen.contains(&format!(
            "{TITLE}{CRUMB}{PERMISSIONS_TITLE}{CRUMB}{MODES_TITLE}"
        )));
    }

    #[test]
    fn other_words_open_the_search_for_them() {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(QUERY);
        assert_eq!(modal.pane, Pane::Search);
        assert_eq!(modal.search.query(), QUERY);
        assert!(modal.text_input_active());
    }

    #[test]
    fn the_contents_filter_narrows_the_pages_and_enter_opens_one() {
        let mut modal = modal();
        modal.handle_key(press(KeyCode::Char('c')));
        typed(&mut modal, FILTER);
        let terminal = draw(&mut modal, WIDE);
        let sidebar = region(&terminal, modal.sidebar);
        assert!(sidebar.contains(PERMISSIONS_TITLE));
        assert!(!sidebar.contains(GUIDE_TITLE));

        modal.handle_key(press(KeyCode::Enter));
        assert_eq!(modal.pane, Pane::Reader);
        assert_eq!(modal.shown, Shown::Page(page(PERMISSIONS)));
    }

    #[test]
    fn a_page_without_highlights_says_so_on_f3() {
        let mut modal = modal();
        draw(&mut modal, WIDE);
        assert_eq!(
            modal.handle_key(key::FIND_NEXT.to_key_event()),
            DocsAction::Flash(NO_HIGHLIGHTS)
        );
    }

    #[test]
    fn closing_keeps_the_place() {
        let mut modal = modal();
        modal.open(&format!("{PERMISSIONS}#{MODES}"));
        draw(&mut modal, WIDE);
        let place = modal.location();
        assert_eq!(modal.handle_key(press(KeyCode::Esc)), DocsAction::Close);
        modal.close();
        modal.open("");
        draw(&mut modal, WIDE);
        assert_eq!(modal.location(), place);
    }

    #[test]
    fn hovering_a_contents_entry_marks_its_title_and_moves_nothing() {
        let mut modal = modal();
        modal.handle_key(press(KeyCode::Char('c')));
        let terminal = draw(&mut modal, WIDE);
        let sidebar = modal.sidebar;
        let group = locate(&terminal, sidebar, REFERENCE_GROUP);
        let title = locate(&terminal, sidebar, PERMISSIONS_TITLE);
        let indent = Position::new(sidebar.x, title.y);

        point(&mut modal, group);
        let terminal = draw(&mut modal, WIDE);
        let row = usize::from(sidebar.width);
        assert_eq!(
            reversed(&terminal, Position::new(sidebar.x, group.y), row),
            0,
            "{STRAY_MARK}"
        );

        point(&mut modal, indent);
        let terminal = draw(&mut modal, WIDE);
        let glyphs = PERMISSIONS_TITLE.len();
        assert_eq!(reversed(&terminal, title, glyphs), glyphs, "{UNMARKED}");
        let before = usize::from(title.x - sidebar.x);
        assert_eq!(reversed(&terminal, indent, before), 0, "{STRAY_MARK}");
        assert_eq!(modal.pane, Pane::Contents, "{FOCUS_MOVED}");

        point(&mut modal, Position::ORIGIN);
        let terminal = draw(&mut modal, WIDE);
        assert_eq!(reversed(&terminal, title, glyphs), 0, "{STRAY_MARK}");
        modal.handle_key(press(KeyCode::Enter));
        assert_eq!(modal.shown, Shown::Page(page(GUIDE)), "{SELECTION_MOVED}");
    }

    #[test_case(0 ; "its_place_row")]
    #[test_case(1 ; "its_snippet_row")]
    fn hovering_either_row_of_a_result_marks_both_and_selects_neither(row: u16) {
        let mut modal = searched(MODES_QUERY);
        draw(&mut modal, WIDE);
        let results = modal.search.results();
        let [first, second] = [results.y, results.y + HIT_ROWS];

        point(&mut modal, Position::new(results.x, second + row));
        let terminal = draw(&mut modal, WIDE);
        let width = usize::from(results.width);
        let at = |y| Position::new(results.x, y);
        for y in [second, second + 1] {
            assert!(reversed(&terminal, at(y), width) > 0, "{UNMARKED}");
        }
        let indent = SNIPPET_INDENT.len();
        assert_eq!(
            reversed(&terminal, at(second + 1), indent),
            0,
            "{STRAY_MARK}"
        );
        for y in [first, first + 1] {
            assert_eq!(reversed(&terminal, at(y), width), 0, "{STRAY_MARK}");
        }

        let mut unhovered = searched(MODES_QUERY);
        modal.handle_key(press(KeyCode::Enter));
        unhovered.handle_key(press(KeyCode::Enter));
        assert_eq!(modal.location(), unhovered.location(), "{SELECTION_MOVED}");
    }

    #[test]
    fn hovering_a_wrapped_link_marks_every_row_of_it() {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(&format!("{GUIDE}#{LINKS}"));
        let terminal = draw(&mut modal, WIDE);
        let head = locate(&terminal, screen(&terminal), SITE_HEAD);
        let tail = locate(&terminal, screen(&terminal), SITE_TAIL);
        assert_ne!(head.y, tail.y, "{UNWRAPPED}");

        point(&mut modal, tail);
        let terminal = draw(&mut modal, WIDE);
        for (at, text) in [(head, SITE_HEAD), (tail, SITE_TAIL)] {
            assert_eq!(
                reversed(&terminal, at, text.len()),
                text.len(),
                "{UNMARKED}"
            );
        }
    }

    #[test_case(GUIDE, MODES_LINK, &[PERMISSIONS_TITLE, MODES_TITLE] ; "a_docs_link_names_its_place")]
    #[test_case(&format!("{GUIDE}#{LINKS}"), SITE_HEAD, &[EXTERNAL] ; "an_outside_link_shows_its_address")]
    fn the_footer_says_where_the_hovered_link_leads(address: &str, link: &str, place: &[&str]) {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(address);
        let terminal = draw(&mut modal, WIDE);
        point(&mut modal, locate(&terminal, screen(&terminal), link));
        let drawn = buffer_text(draw(&mut modal, WIDE).backend().buffer());
        assert!(drawn.contains(&place.join(CRUMB)), "{WRONG_HINT}");
        assert!(modal.footer.hit(0).is_empty(), "{HINT_PRESSABLE}");

        point(&mut modal, Position::ORIGIN);
        draw(&mut modal, WIDE);
        assert!(!modal.footer.hit(0).is_empty(), "{MISSING_KEY}");
    }

    #[test]
    fn the_selected_link_and_the_hovered_one_look_different() {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(&format!("{GUIDE}#{LINKS}"));
        draw(&mut modal, WIDE);
        modal.handle_key(press(KeyCode::Tab));
        let terminal = draw(&mut modal, WIDE);
        let selected = locate(&terminal, screen(&terminal), SITE_HEAD);
        let hovered = locate(&terminal, screen(&terminal), RULES_LINK);

        point(&mut modal, hovered);
        let terminal = draw(&mut modal, WIDE);
        let buffer = terminal.backend().buffer();
        let (selected, hovered) = (&buffer[selected], &buffer[hovered]);
        assert_eq!(
            Some(selected.bg),
            theme::current().item_selected.bg,
            "{UNSELECTED_LOOK}"
        );
        assert!(
            !selected.modifier.contains(Modifier::REVERSED),
            "{SELECTED_AS_HOVERED}"
        );
        assert!(hovered.modifier.contains(Modifier::REVERSED), "{UNMARKED}");
    }

    #[test]
    fn a_scroll_under_a_still_pointer_marks_what_it_brings_there() {
        let mut modal = modal();
        modal.open(GUIDE);
        let terminal = draw(&mut modal, WIDE);
        let link = locate(&terminal, screen(&terminal), MODES_LINK);
        let above = Position::new(link.x, link.y - 1);
        point(&mut modal, above);
        draw(&mut modal, WIDE);
        assert!(!modal.footer.hit(0).is_empty(), "{STRAY_MARK}");

        modal.handle_key(press(KeyCode::Down));
        let terminal = draw(&mut modal, WIDE);
        let glyphs = MODES_LINK.len();
        assert_eq!(
            locate(&terminal, screen(&terminal), MODES_LINK),
            above,
            "{ROWS_STILL}"
        );
        assert_eq!(reversed(&terminal, above, glyphs), glyphs, "{UNMARKED}");
    }

    #[test]
    fn a_reopened_modal_marks_nothing_from_before_it_closed() {
        let mut modal = modal();
        modal.open(GUIDE);
        let terminal = draw(&mut modal, WIDE);
        let link = locate(&terminal, screen(&terminal), MODES_LINK);
        let glyphs = MODES_LINK.len();
        point(&mut modal, link);
        let terminal = draw(&mut modal, WIDE);
        assert_eq!(reversed(&terminal, link, glyphs), glyphs, "{UNMARKED}");

        modal.close();
        modal.open("");
        let terminal = draw(&mut modal, WIDE);
        assert_eq!(reversed(&terminal, link, glyphs), 0, "{STRAY_MARK}");
        assert!(!modal.footer.hit(0).is_empty(), "{MISSING_KEY}");
    }

    /// Only the last row is there to be pointed at below a short page, and a
    /// link on it must not answer for the empty rows under it.
    #[test]
    fn below_a_short_page_the_pointer_is_on_no_link() {
        let mut modal = modal();
        let terminal = draw(&mut modal, NARROW);
        let link = locate(&terminal, screen(&terminal), PERMISSIONS_TITLE);
        let below = Position::new(link.x, modal.footer.hit(0).y - 1);
        assert!(below.y > link.y, "{PAGE_TOO_LONG}");

        point(&mut modal, below);
        let terminal = draw(&mut modal, NARROW);
        let glyphs = PERMISSIONS_TITLE.len();
        assert_eq!(reversed(&terminal, link, glyphs), 0, "{STRAY_MARK}");
        assert!(!modal.footer.hit(0).is_empty(), "{STRAY_MARK}");
    }

    #[test_case(&[], WIDE ; "reader_glossed")]
    #[test_case(&[], NARROW ; "reader_bare")]
    #[test_case(&[], PACKED ; "reader_packed")]
    #[test_case(&[KeyCode::Char('c')], WIDE ; "contents")]
    #[test_case(&[KeyCode::Char('/')], WIDE ; "search")]
    fn a_footer_key_is_marked_only_while_the_pointer_is_on_it(keys: &[KeyCode], width: u16) {
        let mut modal = modal();
        for &code in keys {
            modal.handle_key(press(code));
        }
        draw(&mut modal, width);
        let key = modal.footer.hit(0);
        let cells = usize::from(key.width);

        point(&mut modal, key.as_position());
        let terminal = draw(&mut modal, width);
        assert_eq!(
            reversed(&terminal, key.as_position(), cells),
            cells,
            "{UNMARKED}"
        );

        point(&mut modal, Position::ORIGIN);
        let terminal = draw(&mut modal, width);
        assert_eq!(
            reversed(&terminal, key.as_position(), cells),
            0,
            "{STRAY_MARK}"
        );
    }

    /// `End` leaves the reader following its last row. A link `Tab` wraps
    /// round to above it has to hold the view there, not be pulled back down
    /// on the next frame.
    #[test]
    fn a_link_tabbed_to_after_end_stays_on_screen() {
        let mut modal = modal();
        modal.open(GUIDE);
        draw(&mut modal, WIDE);
        modal.handle_key(key::DOC_BOTTOM.to_key_event());
        draw(&mut modal, WIDE);
        let bottom = modal.reader.visible();

        for _ in 0..TABS_TO_THE_TOP {
            modal.handle_key(press(KeyCode::Tab));
        }
        draw(&mut modal, WIDE);
        let row = modal.reader.link_row().unwrap();
        assert!(row < bottom.start, "{NOT_ABOVE}");
        assert!(modal.reader.visible().contains(&row), "{LINK_HIDDEN}");
    }

    /// The list is hidden while the reader has a narrow screen to itself, so
    /// it has no height to scroll by until `c` draws it.
    #[test]
    fn the_first_c_shows_the_entry_being_read_on_a_short_screen() {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(&format!("{PERMISSIONS}#{RULES}"));
        draw_sized(&mut modal, NARROW, SHORT);

        modal.handle_key(press(KeyCode::Char('c')));
        let terminal = draw_sized(&mut modal, NARROW, SHORT);
        assert!(
            region(&terminal, modal.sidebar).contains(RULES_TITLE),
            "{ENTRY_HIDDEN}"
        );
    }

    /// The list's rows follow the page being read, so after a link changes
    /// the page its selection names another entry, and `Enter` must not
    /// reach the list and open it.
    #[test]
    fn a_link_clicked_beside_the_focused_contents_takes_the_keys_to_the_reader() {
        let mut modal = modal();
        modal.open(GUIDE);
        draw(&mut modal, WIDE);
        modal.handle_key(press(KeyCode::Char('c')));
        let terminal = draw(&mut modal, WIDE);
        let link = locate(&terminal, modal.reader.content(), MODES_LINK);

        click(&mut modal, link);
        draw(&mut modal, WIDE);
        assert_eq!(modal.pane, Pane::Reader, "{FOCUS_STAYED}");
        let place = modal.location();
        modal.handle_key(press(KeyCode::Enter));
        draw(&mut modal, WIDE);
        assert_eq!(modal.location(), place, "{WRONG_PLACE}");
    }

    #[test_case(KeyCode::Up, SHRUNK_TO - 2 ; "up")]
    #[test_case(KeyCode::Down, SHRUNK_TO - 1 ; "down")]
    fn a_selection_past_a_shrunken_list_moves_from_its_last_row(code: KeyCode, expected: usize) {
        assert_eq!(
            list_move(press(code), STALE_SELECTION, SHRUNK_TO, LIST_PAGE),
            Some(expected)
        );
    }

    #[test]
    fn enter_leaves_a_selected_link_scrolled_out_of_sight_alone() {
        let mut modal = modal();
        modal.open(GUIDE);
        draw(&mut modal, WIDE);
        modal.handle_key(press(KeyCode::Tab));
        modal.handle_key(key::DOC_BOTTOM.to_key_event());
        draw(&mut modal, WIDE);
        let place = modal.location();

        modal.handle_key(press(KeyCode::Enter));
        draw(&mut modal, WIDE);
        assert_eq!(modal.location(), place, "{HIDDEN_FOLLOWED}");
    }

    #[test]
    fn a_resize_keeps_the_reading_position() {
        let mut modal = DocsModal::new(fixture::library);
        modal.open(&format!("{PERMISSIONS}#{RULES}"));
        draw(&mut modal, WIDE);
        let place = modal.location();

        for width in [NARROW, PACKED, WIDE] {
            draw(&mut modal, width);
            assert_eq!(modal.location(), place, "{PLACE_LOST}: {width}");
        }
    }

    /// Copied as a sweep over the transcript is: the heading keeps its `#`,
    /// and the link its address.
    #[test]
    fn a_sweep_copies_the_markdown_behind_the_rows() {
        let mut modal = modal();
        assert_eq!(
            sweep_opening(&mut modal, WIDE),
            DocsAction::Copy {
                text: opening_markdown(),
                label: COPIED_SELECTION,
            },
            "{NOT_MARKDOWN}"
        );
    }

    #[test]
    fn ctrl_c_copies_the_markdown_a_standing_sweep_covers() {
        let mut modal = modal();
        sweep_opening(&mut modal, WIDE);
        assert_eq!(
            modal.handle_key(key::QUIT.to_key_event()),
            DocsAction::Copy {
                text: opening_markdown(),
                label: COPIED_SELECTION,
            },
            "{NOT_MARKDOWN}"
        );
    }

    /// The search view takes the reader's place, and so does a contents list
    /// with no room to dock beside it: a sweep under either is out of sight.
    #[test_case(WIDE, 'c', true ; "beside_the_docked_contents")]
    #[test_case(NARROW, 'c', false ; "under_the_contents_in_its_place")]
    #[test_case(WIDE, '/', false ; "under_the_search_view")]
    fn ctrl_c_copies_a_sweep_only_while_the_reader_shows_it(width: u16, pane: char, copies: bool) {
        let mut modal = modal();
        sweep_opening(&mut modal, width);
        modal.handle_key(press(KeyCode::Char(pane)));
        draw(&mut modal, width);

        let action = modal.handle_key(key::QUIT.to_key_event());
        assert_eq!(action != DocsAction::Close, copies, "{WRONG_CTRL_C}");
    }
}
