//! `/changelog`: what changed in Caudra, from the release notes this build
//! shipped with. After an update it opens once on its own with every release
//! since the one last run, and `g` widens it to every release.
//!
//! Each release opens on a heading and a link to its GitHub page. Links in the
//! notes follow as they do in `/docs`, which is where a link into the docs
//! leads.

use std::borrow::Cow;
use std::fmt::Write;

use caudra_docs::{Release, ReleaseNotesSource};
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::document_view::{COPIED_SELECTION, Jump, MAX_HEIGHT_PERCENT, WIDTH_PERCENT};
use crate::components::keybindings::key;
use crate::components::list_picker::truncate_label;
use crate::components::markdown_reader::{Reader, ReaderMouse};
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::{Overlay, PAN_STEP, escape_terminal_controls, plain_char};
use crate::theme::{self, Theme};

const SINCE_TITLE: &str = " What's new since ";
const CURRENT_TITLE: &str = " What's new in ";
const ALL_TITLE: &str = " Release notes - all ";
const TITLE_END: &str = " ";
const RELEASE_HEADING: &str = "## ";
const GITHUB_LINK: &str = " on GitHub";
const NO_NOTES: &str = "This build ships without release notes.";
const H_PAD: u16 = 1;
const FOOTER_ROWS: u16 = 1;
const KEY_GAP: &str = " ";
/// Glossed, then keys alone, then keys packed: a narrow terminal loses words
/// and never a key.
const FOOTER_RUNGS: [(bool, &str); 3] = [(true, SEPARATOR), (false, SEPARATOR), (false, KEY_GAP)];
const GITHUB_LABEL: &str = "o";
const LINK_LABEL: &str = "Tab";
const NEXT_LABEL: &str = "n";
const ALL_GLOSS: &str = " all releases";
const SINCE_GLOSS: &str = " what's new";
const GITHUB_GLOSS: &str = " GitHub";
const LINK_GLOSS: &str = " link";
const NEXT_GLOSS: &str = " next release";
pub(crate) const REOPEN_HINT: &str = "Run /changelog to read these notes again";

/// Which releases are shown: the ones since the release last run, or all of
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Since,
    All,
}

/// What the host has to carry out. Moving around the notes is the modal's own
/// business and never reaches here.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChangelogAction {
    Consumed,
    /// `auto` when the modal opened on its own after an update.
    Close {
        auto: bool,
    },
    Copy {
        text: String,
        label: &'static str,
    },
    /// A link: into the docs, or out to the browser.
    Follow(String),
}

/// The notes shown, as one Markdown document, and the line each release's
/// heading is on.
#[derive(Default)]
struct Composed {
    text: String,
    headings: Vec<usize>,
    releases: &'static [Release],
}

pub(crate) struct ChangelogModal {
    notes: ReleaseNotesSource,
    current: &'static str,
    open: bool,
    view: View,
    since: Option<String>,
    auto: bool,
    composed: Composed,
    reader: Reader<View>,
    footer: FooterHits,
    pointer: Option<Position>,
    popup: Rect,
}

impl ChangelogModal {
    /// `current` is the release this build is, which the notes run up to.
    pub(crate) fn new(notes: ReleaseNotesSource, current: &'static str) -> Self {
        Self {
            notes,
            current,
            open: false,
            view: View::Since,
            since: None,
            auto: false,
            composed: Composed::default(),
            reader: Reader::default(),
            footer: FooterHits::default(),
            pointer: None,
            popup: Rect::default(),
        }
    }

    /// Opens on the releases after `since`, or on every release when there are
    /// none to show, as in a build without notes for itself.
    pub(crate) fn open(&mut self, view: View, since: Option<String>) {
        self.since = since;
        let view = match view == View::Since && self.releases(View::Since).is_empty() {
            true => View::All,
            false => view,
        };
        self.show(view, false);
    }

    /// Opens on its own after an update, unless no notes cover it. Returns
    /// whether it opened.
    pub(crate) fn open_after_update(&mut self, since: Option<String>) -> bool {
        self.since = since;
        if self.releases(View::Since).is_empty() {
            return false;
        }
        self.show(View::Since, true);
        true
    }

    fn show(&mut self, view: View, auto: bool) {
        self.open = true;
        self.auto = auto;
        self.view = view;
        self.compose();
        self.reader = Reader::default();
        self.footer.reset();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.auto = false;
        self.footer.reset();
        self.pointer = None;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        self.open && self.popup.contains(position)
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        self.reader.scroll(delta);
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ChangelogAction {
        // A sweep the reader can see is what the chord is for; with none it is
        // the way out, as everywhere else.
        if key::QUIT.matches(key) {
            return match self.reader.selected_text() {
                Some(text) => copied(text),
                None => self.close_action(),
            };
        }
        let plain = key.modifiers == KeyModifiers::NONE;
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) | (_, Some('q')) => return self.close_action(),
            _ if key::SCOPE.matches(key) => self.toggle_view(),
            (_, Some('o')) => return self.github(),
            (_, Some('n')) => self.reader.jump(Jump::Next),
            (_, Some('p')) => self.reader.jump(Jump::Previous),
            (KeyCode::Tab, _) => {
                self.reader.step_link(true);
            }
            (KeyCode::BackTab, _) => {
                self.reader.step_link(false);
            }
            (KeyCode::Enter, _) => {
                if let Some(target) = self.reader.selected_target() {
                    return ChangelogAction::Follow(target.to_string());
                }
            }
            (KeyCode::Left, _) if plain => self.reader.pan(-PAN_STEP),
            (KeyCode::Right, _) if plain => self.reader.pan(PAN_STEP),
            _ => self.reader.handle_scroll_key(key),
        }
        ChangelogAction::Consumed
    }

    /// A click on a footer entry presses its key.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> ChangelogAction {
        self.pointer = Some(Position::new(event.column, event.row));
        if let Some(index) = self.footer.handle_mouse(event) {
            return match FOOTER_KEYS.get(index) {
                Some(&code) => self.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
                None => ChangelogAction::Consumed,
            };
        }
        match event.kind {
            MouseEventKind::ScrollLeft => self.reader.pan(-PAN_STEP),
            MouseEventKind::ScrollRight => self.reader.pan(PAN_STEP),
            _ => {
                return match self.reader.handle_mouse(event) {
                    ReaderMouse::Consumed => ChangelogAction::Consumed,
                    ReaderMouse::Copy(text) => copied(text),
                    ReaderMouse::Follow(target) => ChangelogAction::Follow(target.to_string()),
                };
            }
        }
        ChangelogAction::Consumed
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("changelog_modal", area);
        let theme = theme::current();
        let title = self.title();
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
        let Composed { text, headings, .. } = &self.composed;
        // The bars run down the popup's edge and along its bottom border, as
        // they do in every other document modal.
        let hovered = self
            .reader
            .draw(frame, inner, body, self.pointer, self.view, || {
                (Cow::Borrowed(text.as_str()), headings.clone())
            });
        let line = match hovered {
            // The row speaks for the link, so it holds no key to press.
            Some(target) => {
                self.footer.set(Vec::new());
                link_line(&target, footer_area.width, &theme)
            }
            None => {
                let footer = self.footer_line(footer_area.width, &theme);
                self.footer.set(footer.hits(footer_area, 0, FOOTER_ROWS));
                footer.line(self.footer.hovered())
            }
        };
        frame.render_widget(Paragraph::new(line), footer_area);
        self.popup = popup;
        popup
    }

    fn releases(&self, view: View) -> &'static [Release] {
        let notes = (self.notes)();
        match view {
            View::Since => notes.since(self.since.as_deref(), self.current),
            View::All => notes.up_to(self.current),
        }
    }

    fn compose(&mut self) {
        let releases = self.releases(self.view);
        let mut text = String::new();
        let mut headings = Vec::with_capacity(releases.len());
        let mut line = 0;
        for release in releases {
            if !text.is_empty() {
                text.push('\n');
                line += 1;
            }
            headings.push(line);
            let start = text.len();
            let _ = write!(
                text,
                "{RELEASE_HEADING}{}\n\n[{}{GITHUB_LINK}]({})\n\n{}",
                release.title, release.tag, release.url, release.display
            );
            line += text[start..].matches('\n').count();
        }
        if releases.is_empty() {
            text.push_str(NO_NOTES);
        }
        self.composed = Composed {
            text,
            headings,
            releases,
        };
    }

    /// Shows the other view, on the release that was at the top where it is in
    /// both.
    fn toggle_view(&mut self) {
        let reading = self.release_at_top().map(|release| release.tag.as_str());
        self.view = match self.view {
            View::Since => View::All,
            View::All => View::Since,
        };
        self.compose();
        let landing = reading.and_then(|tag| {
            let index = self
                .composed
                .releases
                .iter()
                .position(|release| release.tag == tag)?;
            self.composed.headings.get(index).copied()
        });
        self.reader.land(landing.unwrap_or(0), landing.is_none());
        self.footer.clear();
    }

    fn release_at_top(&self) -> Option<&'static Release> {
        let top = self.reader.top_line();
        let index = self
            .composed
            .headings
            .iter()
            .rposition(|&line| line <= top)
            .unwrap_or(0);
        self.composed.releases.get(index)
    }

    fn github(&self) -> ChangelogAction {
        match self.release_at_top() {
            Some(release) => ChangelogAction::Follow(release.url.clone()),
            None => ChangelogAction::Consumed,
        }
    }

    /// Closing as `Esc` does, which an auto-opened modal answers with a hint.
    pub(crate) fn close_action(&self) -> ChangelogAction {
        ChangelogAction::Close { auto: self.auto }
    }

    fn title(&self) -> String {
        match (self.view, &self.since) {
            (View::All, _) => ALL_TITLE.to_owned(),
            (View::Since, Some(since)) => format!("{SINCE_TITLE}{since}{TITLE_END}"),
            (View::Since, None) => format!("{CURRENT_TITLE}{}{TITLE_END}", self.current),
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

    /// The entries in [`FOOTER_KEYS`] order, dimmed where the key would do
    /// nothing now.
    fn footer_rung(&self, (glossed, gap): (bool, &'static str), theme: &Theme) -> FooterLine {
        let entries = [
            (
                key::SCOPE.label,
                match self.view {
                    View::Since => ALL_GLOSS,
                    View::All => SINCE_GLOSS,
                },
                true,
            ),
            (
                GITHUB_LABEL,
                GITHUB_GLOSS,
                !self.composed.releases.is_empty(),
            ),
            (LINK_LABEL, LINK_GLOSS, self.reader.has_links()),
            (NEXT_LABEL, NEXT_GLOSS, self.composed.headings.len() > 1),
            (ESC_LABEL, CLOSE_HINT, true),
        ];
        let mut footer = FooterLine::default();
        for (index, (label, gloss, enabled)) in entries.into_iter().enumerate() {
            if index > 0 {
                footer.text(gap, theme.tool_dim);
            }
            footer.command(
                label,
                match enabled {
                    true => theme.keybind_key,
                    false => theme.tool_dim,
                },
            );
            if glossed {
                footer.describe(gloss, theme.tool_dim);
            }
        }
        footer
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, index: usize) -> Rect {
        self.footer.hit(index)
    }
}

/// The key each footer entry presses, in the order [`ChangelogModal::footer_rung`]
/// lays them out.
const FOOTER_KEYS: [KeyCode; 5] = [
    KeyCode::Char('g'),
    KeyCode::Char('o'),
    KeyCode::Tab,
    KeyCode::Char('n'),
    KeyCode::Esc,
];

impl Overlay for ChangelogModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

/// The footer while the pointer is on a link: the address it leads to.
fn link_line(target: &str, width: u16, theme: &Theme) -> Line<'static> {
    let target = truncate_label(&escape_terminal_controls(target), usize::from(width));
    Line::from(Span::styled(target, theme.status_notice)).alignment(Alignment::Center)
}

fn copied(text: String) -> ChangelogAction {
    ChangelogAction::Copy {
        text,
        label: COPIED_SELECTION,
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::sync::LazyLock;

    use caudra_docs::ReleaseNotes;

    pub(crate) const OLDEST: &str = "0.2.0-preview.2";
    pub(crate) const MIDDLE: &str = "0.2.0-preview.3";
    pub(crate) const CURRENT: &str = "0.2.0-preview.4";
    pub(crate) const DOCS_LINK: &str = "https://caudra.ai/docs/permissions/";
    pub(crate) const EXTERNAL_LINK: &str = "https://example.com/elsewhere";
    const BULLETS: usize = 12;

    fn source(version: &str) -> String {
        let bullets: String = (0..BULLETS)
            .map(|index| format!("- **Change {index} of {version}.** Details.\n"))
            .collect();
        format!(
            "Lead for {version}, see [permissions]({DOCS_LINK}) and [elsewhere]({EXTERNAL_LINK}).\n\n> [!IMPORTANT]\n> Restart.\n\n### Changes\n\n{bullets}"
        )
    }

    /// Three releases, each long enough to fill a small screen, linking into
    /// the docs and out of them.
    pub(crate) fn notes() -> &'static ReleaseNotes {
        static NOTES: LazyLock<ReleaseNotes> = LazyLock::new(|| {
            ReleaseNotes::parse([OLDEST, MIDDLE, CURRENT].map(|version| {
                (
                    &*Box::leak(format!("{version}.md").into_boxed_str()),
                    &*Box::leak(source(version).into_boxed_str()),
                )
            }))
            .expect("valid fixture notes")
        });
        &NOTES
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Modifier;

    use super::fixture::{CURRENT, DOCS_LINK, MIDDLE, OLDEST, notes};
    use super::{ALL_TITLE, ChangelogAction, ChangelogModal, View};
    use crate::components::keybindings::key;

    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    const SHORT: u16 = 16;
    const SINCE_FOOTER: usize = 0;
    const CLOSE_FOOTER: usize = 4;
    const PREVIEW_2_TITLE: &str = "Caudra 0.2 Preview 2";
    const PREVIEW_3_TITLE: &str = "Caudra 0.2 Preview 3";
    const PREVIEW_4_TITLE: &str = "Caudra 0.2 Preview 4";
    const SINCE_TITLE: &str = "What's new since 0.2.0-preview.2";
    const HOVER_MISSED: &str = "the footer command must reverse under the pointer";

    fn modal() -> ChangelogModal {
        ChangelogModal::new(notes, CURRENT)
    }

    fn draw(terminal: &mut Terminal<TestBackend>, modal: &mut ChangelogModal) -> String {
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn terminal(height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(WIDTH, height)).unwrap()
    }

    fn press(modal: &mut ChangelogModal, code: KeyCode) -> ChangelogAction {
        modal.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(modal: &mut ChangelogModal, at: Rect) -> ChangelogAction {
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at))
    }

    fn url(version: &str) -> String {
        format!("https://github.com/caudra/caudra/releases/tag/v{version}")
    }

    #[test]
    fn the_default_view_holds_only_the_releases_since_the_last_run() {
        let mut modal = modal();
        modal.open(View::Since, Some(OLDEST.to_owned()));
        let text = modal.composed.text.clone();
        assert!(text.contains(PREVIEW_4_TITLE) && text.contains(PREVIEW_3_TITLE));
        assert!(!text.contains(PREVIEW_2_TITLE));
        assert!(draw(&mut terminal(HEIGHT), &mut modal).contains(SINCE_TITLE));
        assert_eq!(
            modal
                .composed
                .headings
                .iter()
                .map(|&line| text.lines().nth(line).unwrap())
                .collect::<Vec<_>>(),
            [
                format!("## {PREVIEW_4_TITLE}"),
                format!("## {PREVIEW_3_TITLE}")
            ]
        );
    }

    #[test]
    fn scope_toggles_every_release_and_back() {
        let mut modal = modal();
        let mut terminal = terminal(HEIGHT);
        modal.open(View::Since, Some(MIDDLE.to_owned()));
        assert!(!modal.composed.text.contains(PREVIEW_3_TITLE));
        modal.handle_key(key::SCOPE.to_key_event());
        let screen = draw(&mut terminal, &mut modal);
        assert!(screen.contains(ALL_TITLE.trim()), "{screen}");
        assert!(modal.composed.text.contains(PREVIEW_2_TITLE));
        modal.handle_key(key::SCOPE.to_key_event());
        assert!(!modal.composed.text.contains(PREVIEW_3_TITLE));
    }

    #[test]
    fn nothing_to_show_since_opens_every_release() {
        let mut modal = modal();
        modal.open(View::Since, Some(CURRENT.to_owned()));
        assert_eq!(modal.view, View::All);
        assert!(!modal.open_after_update(Some(CURRENT.to_owned())));
    }

    #[test]
    fn the_footer_command_hovers_and_switches_views() {
        let mut modal = modal();
        let mut terminal = terminal(HEIGHT);
        modal.open(View::Since, Some(MIDDLE.to_owned()));
        draw(&mut terminal, &mut modal);
        let hit = modal.footer_hit(SINCE_FOOTER);
        assert!(!hit.is_empty());

        modal.handle_mouse(mouse(MouseEventKind::Moved, hit));
        draw(&mut terminal, &mut modal);
        let buffer = terminal.backend().buffer();
        let reversed: Vec<_> = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer[(x, y)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(
            reversed.len() == usize::from(hit.width)
                && reversed
                    .iter()
                    .all(|&(x, y)| hit.contains(Position::new(x, y))),
            "{HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        assert_eq!(click(&mut modal, hit), ChangelogAction::Consumed);
        assert_eq!(modal.view, View::All);

        draw(&mut terminal, &mut modal);
        let close = modal.footer_hit(CLOSE_FOOTER);
        assert_eq!(
            click(&mut modal, close),
            ChangelogAction::Close { auto: false }
        );
    }

    #[test]
    fn github_opens_the_release_being_read() {
        let mut modal = modal();
        let mut terminal = terminal(SHORT);
        modal.open(View::All, None);
        draw(&mut terminal, &mut modal);
        assert_eq!(
            press(&mut modal, KeyCode::Char('o')),
            ChangelogAction::Follow(url(CURRENT))
        );
        press(&mut modal, KeyCode::Char('n'));
        draw(&mut terminal, &mut modal);
        assert_eq!(
            press(&mut modal, KeyCode::Char('o')),
            ChangelogAction::Follow(url(MIDDLE))
        );
    }

    #[test]
    fn toggling_keeps_the_release_being_read() {
        let mut modal = modal();
        let mut terminal = terminal(SHORT);
        modal.open(View::Since, Some(OLDEST.to_owned()));
        draw(&mut terminal, &mut modal);
        press(&mut modal, KeyCode::Char('n'));
        draw(&mut terminal, &mut modal);
        modal.handle_key(key::SCOPE.to_key_event());
        draw(&mut terminal, &mut modal);
        assert_eq!(
            press(&mut modal, KeyCode::Char('o')),
            ChangelogAction::Follow(url(MIDDLE))
        );
    }

    #[test]
    fn links_follow_from_the_keyboard() {
        let mut modal = modal();
        let mut terminal = terminal(HEIGHT);
        modal.open(View::Since, None);
        draw(&mut terminal, &mut modal);
        press(&mut modal, KeyCode::Tab);
        assert_eq!(
            press(&mut modal, KeyCode::Enter),
            ChangelogAction::Follow(url(CURRENT))
        );
        press(&mut modal, KeyCode::Tab);
        assert_eq!(
            press(&mut modal, KeyCode::Enter),
            ChangelogAction::Follow(DOCS_LINK.to_owned())
        );
    }

    #[test]
    fn closing_says_whether_it_opened_on_its_own() {
        let mut modal = modal();
        assert!(modal.open_after_update(Some(MIDDLE.to_owned())));
        assert_eq!(
            press(&mut modal, KeyCode::Esc),
            ChangelogAction::Close { auto: true }
        );
        modal.open(View::Since, None);
        assert_eq!(
            modal.handle_key(key::QUIT.to_key_event()),
            ChangelogAction::Close { auto: false }
        );
    }

    #[test]
    fn the_github_line_names_the_tag() {
        let mut modal = modal();
        modal.open(View::Since, None);
        let screen = draw(&mut terminal(HEIGHT), &mut modal);
        assert!(
            screen.contains(&format!("v{CURRENT} on GitHub")),
            "{screen}"
        );
        assert!(screen.contains("Important"), "{screen}");
        assert!(!modal.composed.text.contains("[!IMPORTANT]"));
    }
}
