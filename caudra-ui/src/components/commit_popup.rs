//! The `#hash` completion popup.
//!
//! Typing `#` in the composer opens the project's recent log, fuzzy-matched
//! over the hash, subject and author together, and splices the abbreviated hash
//! back over the query that opened it.
//!
//! The window it lists is also the window the composer validates against, which
//! is why the index is loaded once per session rather than per keystroke: the
//! scanner that decides whether `#a1b2c3d` is a revision or prose has to answer
//! on every edit and every mouse move, and it must never open a repository to
//! do it.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use caudra_agent::commits::repo::{self, CommitSummary};
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nucleo::{Config, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::completion::{self, Completion, MouseOutcome, PAD};
use crate::components::list_picker::truncate_label;
use crate::components::tooltip::{Anchor, Tip, TipKey};
use crate::repaint::{Cadence, Dirty};
use crate::theme;

const SIGIL: char = '#';
const SCOPE: &str = "commit_popup";
/// Columns a subject is allowed in a row. Past this the hash and the author
/// would be pushed off a narrow composer.
const SUBJECT_WIDTH: usize = 60;
const GAP: &str = "  ";
const DESCRIPTION_SEPARATOR: &str = " \u{b7} ";
const DATE_FORMAT: &str = "%Y-%m-%d %H:%M";
const UNKNOWN_DATE: &str = "unknown date";
/// Shown while the log is being read, so a `#` answers at once in a project
/// whose history is a round trip away.
const LOADING: &str = "reading the log";

pub(crate) enum CommitAction {
    Consumed,
    /// The chosen hash and the composer range it replaces.
    Insert {
        range: Range<usize>,
        hash: String,
    },
    Passthrough,
}

/// What the popup lists and the composer validates against.
///
/// Where the rows came from is the host's business, so the variants say only
/// what this index knows. Cheap to clone: every surface shares one load.
#[derive(Clone, Default)]
pub(crate) enum CommitIndex {
    /// Not asked yet, or a read is in flight. Nothing lists, so the popup says
    /// it is loading rather than showing an empty list.
    ///
    /// A hash spelled out in full still resolves. Forty hex characters in a row
    /// is not plausible prose, and whoever pasted one meant a commit, so the
    /// reference survives to send time where it can be looked up for real. An
    /// abbreviation waits for the window, because `#abcdefg` is also a word.
    #[default]
    Pending,
    /// Asked, and there is no history to offer. Nothing resolves, which is what
    /// leaves `#` as prose in a project without a repository.
    Absent,
    /// A log window, however it was read. Resolves exactly what it lists.
    Loaded(Arc<[CommitSummary]>),
}

impl CommitIndex {
    /// Adopts a window read from somewhere. An empty one collapses to
    /// [`Self::Absent`]: a repository with no commits offers nothing, and that
    /// is an answer rather than an error.
    pub fn loaded(commits: Vec<CommitSummary>) -> Self {
        match commits.is_empty() {
            true => Self::Absent,
            false => Self::Loaded(commits.into()),
        }
    }

    pub fn commits(&self) -> &[CommitSummary] {
        match self {
            Self::Loaded(commits) => commits,
            Self::Pending | Self::Absent => &[],
        }
    }

    /// Whether there is nothing to wait for and nothing to show, which is what
    /// keeps the popup shut in a project that has no history.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Whether `id` names a commit worth resolving, which is the predicate the
    /// composer's scanner runs on every edit. Never a repository read.
    pub fn resolves(&self, id: &str) -> bool {
        match self {
            Self::Pending => id.len() == repo::FULL_ID_LENGTH,
            Self::Absent => false,
            Self::Loaded(commits) => commits.iter().any(|commit| commit.id.starts_with(id)),
        }
    }

    /// The subject, author and date of the commit `id` abbreviates, for the
    /// status bar. A window that has not arrived knows none of them, so a
    /// hovered hash shows itself.
    pub fn describe(&self, id: &str) -> Option<String> {
        let commit = self
            .commits()
            .iter()
            .find(|commit| commit.id.starts_with(id))?;
        let date = datetime(commit.committed_unix_seconds, &TimeZone::system());
        Some(format!(
            "{}{DESCRIPTION_SEPARATOR}{}{DESCRIPTION_SEPARATOR}{date}",
            commit.subject, commit.author
        ))
    }
}

struct CommitRow {
    hash: String,
    text: String,
    /// The subject whole, kept only when the row had to cut it.
    cut_subject: Option<String>,
}

impl CommitRow {
    fn new(commit: &CommitSummary, zone: &TimeZone) -> Self {
        let hash = commit.short().to_owned();
        let date = datetime(commit.committed_unix_seconds, zone);
        let subject = truncate_label(&commit.subject, SUBJECT_WIDTH);
        Self {
            text: format!("{hash}{GAP}{date}{GAP}{subject}{GAP}{}", commit.author),
            hash,
            cut_subject: (commit.subject.width() > SUBJECT_WIDTH).then(|| commit.subject.clone()),
        }
    }
}

struct Session {
    completion: Completion,
    /// The row text each match came from, so choosing one recovers its hash
    /// without re-parsing the rendered row.
    rows: HashMap<String, CommitRow>,
    /// The window the rows were seeded from, compared by pointer. A refresh
    /// replaces the window wholesale, so this is how an open popup notices one
    /// landing behind it instead of listing a stale log.
    sourced: Option<Arc<[CommitSummary]>>,
}

impl Session {
    fn sourced_from(&self, index: &CommitIndex) -> bool {
        match (&self.sourced, index) {
            (Some(held), CommitIndex::Loaded(current)) => Arc::ptr_eq(held, current),
            (None, CommitIndex::Pending) => true,
            _ => false,
        }
    }
}

#[derive(Default)]
pub(crate) struct CommitPopup {
    session: Option<Session>,
    /// Char range of the `#query` the popup is completing, into the composer's
    /// display text.
    trigger: Option<Range<usize>>,
}

impl CommitPopup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the popup has rows, which is what makes it take keys and clicks.
    /// A loading popup draws but captures nothing, so rows arriving mid-keystroke
    /// cannot change what a key was about to do.
    pub fn is_open(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.completion.is_empty())
    }

    /// Whether the popup is drawing a spinner in place of a list.
    pub fn is_loading(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.sourced.is_none())
    }

    /// Whether the reader is inside a `#query` at all, listing or loading. The
    /// edge into this is what asks for a fresh log.
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// The spinner turns on the clock alone, so the popup has to ask for the
    /// frames that move it. A list moves only when the reader does.
    pub fn cadence(&self) -> Cadence {
        Cadence::when(self.is_loading(), Cadence::SPINNER)
    }

    /// The whole subject of the row under the pointer, when the row cut it.
    pub(crate) fn tooltip(&self) -> Option<Tip> {
        let session = self.session.as_ref()?;
        let (row, query) = session.completion.hovered()?;
        let subject = session.rows.get(query)?.cut_subject.clone()?;
        Some(Tip {
            key: TipKey::Row(row),
            anchor: Anchor::Area(row),
            text: subject,
        })
    }

    pub fn close(&mut self) {
        self.session = None;
        self.trigger = None;
    }

    pub fn contains(&self, position: Position) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.completion.contains(position))
    }

    pub fn scroll(&mut self, delta: i32) {
        self.step(-delta.signum() as isize);
    }

    /// A pending index opens the popup anyway, on a spinner: the reader asked a
    /// question and the answer is on its way.
    pub fn sync(&mut self, text: &str, cursor: usize, index: &CommitIndex) {
        let Some((range, query)) = completion::trigger_at(text, cursor, SIGIL) else {
            self.close();
            return;
        };
        if index.is_absent() {
            self.close();
            return;
        }
        self.trigger = Some(range);
        let restart = match &self.session {
            Some(session) => !session.sourced_from(index),
            None => true,
        };
        match (restart, &mut self.session) {
            (true, _) => self.start(query, index),
            (false, Some(session)) if session.completion.query() != query => {
                session.completion.set_query(query)
            }
            (false, _) => {}
        }
    }

    /// Seeds a list from whatever the index holds. A pending index seeds nothing
    /// and the session stands as the placeholder until a window lands.
    fn start(&mut self, query: String, index: &CommitIndex) {
        let completion = Completion::new(Config::DEFAULT, query.clone());
        let injector = completion.injector();
        let mut rows = HashMap::new();
        let zone = TimeZone::system();
        for commit in index.commits() {
            let row = haystack(commit);
            injector.push((), |_, columns| {
                columns[0] = Utf32String::from(row.as_str());
            });
            rows.insert(row, CommitRow::new(commit, &zone));
        }
        let sourced = match index {
            CommitIndex::Loaded(commits) => Some(Arc::clone(commits)),
            CommitIndex::Pending | CommitIndex::Absent => None,
        };
        let mut session = Session {
            completion,
            rows,
            sourced,
        };
        session.completion.set_query(query);
        self.session = Some(session);
    }

    /// The window is seeded up front rather than streamed, so a tick only has to
    /// drain the matcher.
    pub fn tick(&mut self) -> Dirty {
        match &mut self.session {
            Some(session) => session.completion.tick(),
            None => Dirty::NO,
        }
    }

    #[cfg(test)]
    pub fn settle(&mut self) {
        if let Some(session) = &mut self.session {
            session.completion.settle();
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CommitAction {
        if !self.is_open() {
            return CommitAction::Passthrough;
        }
        match key.code {
            KeyCode::Esc => {
                self.close();
                CommitAction::Consumed
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Enter | KeyCode::Tab if key.modifiers == KeyModifiers::NONE => self.choose(),
            _ => CommitAction::Passthrough,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> CommitAction {
        let Some(session) = &mut self.session else {
            return CommitAction::Passthrough;
        };
        match session.completion.handle_mouse(event) {
            MouseOutcome::Outside => CommitAction::Passthrough,
            MouseOutcome::Consumed => CommitAction::Consumed,
            MouseOutcome::Chosen => self.choose(),
        }
    }

    fn step(&mut self, delta: isize) -> CommitAction {
        if let Some(session) = &mut self.session {
            session.completion.step(delta);
        }
        CommitAction::Consumed
    }

    /// Splices the highlighted commit over the `#query` that opened the popup.
    /// A commit is never a step on the way somewhere, so this always finishes.
    fn choose(&mut self) -> CommitAction {
        let (Some(session), Some(range)) = (self.session.as_mut(), self.trigger.clone()) else {
            return CommitAction::Passthrough;
        };
        let Some(hash) = session
            .completion
            .selected()
            .and_then(|query| session.rows.get(query))
            .map(|row| row.hash.clone())
        else {
            return CommitAction::Passthrough;
        };
        self.close();
        CommitAction::Insert {
            range,
            hash: format!("{SIGIL}{hash}"),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        if self.is_loading() {
            return loading_view(frame, input_area);
        }
        let session = self.session.as_mut()?;
        let theme = theme::current();
        let rows = &session.rows;
        session.completion.view(
            frame,
            input_area,
            SCOPE,
            |query| {
                rows.get(query).map_or(0, |row| {
                    UnicodeWidthStr::width(row.text.as_str()) as u16 + PAD * 2
                })
            },
            move |query, selected| {
                let Some(row) = rows.get(query) else {
                    return Line::default();
                };
                let base = match selected {
                    true => theme.item_selected,
                    false => theme.item,
                };
                let (hash, rest) = row.text.split_once(' ').unwrap_or((&row.text, ""));
                Line::from(vec![
                    Span::styled(format!(" {hash}"), base.patch(theme.mention)),
                    Span::styled(format!(" {rest} "), base),
                ])
            },
        )
    }
}

/// One row where the list will be, so the `#` is answered on the frame it was
/// typed on. Drawn here rather than through [`Completion`], which has no rows to
/// lay out yet.
fn loading_view(frame: &mut Frame, input_area: Rect) -> Option<Rect> {
    let line = format!(" {} {LOADING} ", spinner_str(animation_elapsed_ms()));
    let width = (UnicodeWidthStr::width(line.as_str()) as u16).min(input_area.width);
    if input_area.y == 0 || width == 0 {
        return None;
    }
    let area = Rect {
        x: input_area.x,
        y: input_area.y - 1,
        width,
        height: 1,
    };
    grab_scope!(SCOPE, area);
    Clear.render(area, frame.buffer_mut());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(line, theme::current().spinner))),
        area,
    );
    Some(area)
}

fn haystack(commit: &CommitSummary) -> String {
    format!(
        "{} {}{GAP}{}",
        commit.short(),
        commit.subject,
        commit.author
    )
}

fn datetime(seconds: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(seconds)
        .map(|stamp| {
            stamp
                .to_zoned(zone.clone())
                .strftime(DATE_FORMAT)
                .to_string()
        })
        .unwrap_or_else(|_| UNKNOWN_DATE.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        CommitAction, CommitIndex, CommitPopup, CommitRow, GAP, SUBJECT_WIDTH, UNKNOWN_DATE,
        datetime, haystack,
    };
    use caudra_agent::commits::repo::CommitSummary;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use jiff::tz::{Offset, TimeZone};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    use crate::components::key;
    use crate::components::tooltip::{Anchor, Tip, TipKey};
    use crate::repaint::Cadence;

    const FIRST: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const SECOND: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f098765432";
    const SUBJECT: &str = "Fix login crash";
    const AUTHOR: &str = "Ada Lovelace";
    const OTHER_AUTHOR: &str = "Grace Hopper";
    const COMMITTED: i64 = 1_700_000_000;
    const UTC_DATETIME: &str = "2023-11-14 22:13";
    const OFFSET_SECONDS: i32 = 19_800;
    const OFFSET_DATETIME: &str = "2023-11-15 03:43";
    const FIRST_MENTION: &str = "#a1b2c3d";
    const SECOND_MENTION: &str = "#0f1e2d3";
    const TITLE_QUERY: &str = "#login";
    const HIDDEN_WORD: &str = "needle";
    const WIDE: u16 = 120;
    const NARROW: u16 = 32;
    const NO_FRAME: &str = "the commit popup must render in a test terminal";
    const INVALID_OFFSET: &str = "the fixed test timezone must be valid";
    const MISSING_ROW: &str = "the popup did not retain its display row";
    const AREA: Rect = Rect {
        x: 0,
        y: 4,
        width: 60,
        height: 2,
    };
    const NO_SESSION: &str = "the popup dropped the session it was given";
    const NOT_INSERTED: &str = "the click did not take the row it landed on";
    const EXPECT_CLOSED: &str = "the popup stayed open without a matching commit query";
    const THIRD: &str = "c3d4e5f60718293a4b5c6d7e8f90123456789abc";
    const EXPECT_LOADING: &str = "a `#` typed before the log arrives must still answer";
    const LOADING_CAPTURES: &str = "a popup with no rows must not capture keys or clicks";
    const EXPECT_LISTED: &str = "the window that arrived did not become rows";
    const LOADING_LINGERED: &str = "the spinner outlived the window it was waiting for";
    const STALE_WINDOW: &str = "the popup kept listing the log from before the refresh";
    const SPINNER_FROZEN: &str = "a spinning popup must ask for the frames that turn it";
    const TIP_ONLY_WHEN_CUT: &str =
        "a hovered row offers its whole subject exactly when it was cut";
    const WIDE_GLYPH: &str = "\u{4e2d}";
    const ELLIPSIS: char = '\u{2026}';
    const NOT_CUT_BY_COLUMNS: &str =
        "a subject must be cut by the columns it takes, not its characters";

    fn summary(id: &str, subject: &str) -> CommitSummary {
        CommitSummary {
            id: id.to_owned(),
            subject: subject.to_owned(),
            author: AUTHOR.to_owned(),
            committed_unix_seconds: COMMITTED,
        }
    }

    fn index() -> CommitIndex {
        let mut earlier = summary(SECOND, "Earlier work");
        earlier.author = OTHER_AUTHOR.to_owned();
        CommitIndex::loaded(vec![summary(FIRST, SUBJECT), earlier])
    }

    fn opened() -> CommitPopup {
        let mut popup = CommitPopup::new();
        popup.sync("#", 1, &index());
        popup.settle();
        popup
    }

    fn event(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: AREA.x,
            row: AREA.y + row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn inserted(action: CommitAction) -> Option<String> {
        match action {
            CommitAction::Insert { hash, .. } => Some(hash),
            _ => None,
        }
    }

    #[test]
    fn a_bare_sigil_lists_the_whole_window() {
        let popup = opened();
        assert!(popup.is_open());
        assert_eq!(
            popup.session.as_ref().expect(NO_SESSION).rows.len(),
            2,
            "both commits are listed"
        );
    }

    #[test_case("# heading", 2 ; "a_heading")]
    #[test_case("#login more", 11 ; "past_whitespace")]
    #[test_case("word#login", 10 ; "mid_word")]
    fn text_outside_a_query_does_not_open_the_popup(text: &str, cursor: usize) {
        let mut popup = CommitPopup::new();
        popup.sync(text, cursor, &index());
        popup.settle();
        assert!(!popup.is_open(), "{EXPECT_CLOSED}: {text}");
        assert!(!popup.is_active(), "{EXPECT_CLOSED}: {text}");
    }

    /// Without a repository there is nothing to complete and nothing to wait
    /// for, so `#` stays prose.
    #[test]
    fn an_absent_index_never_opens_the_popup() {
        let mut popup = CommitPopup::new();
        popup.sync("#a1b2", 5, &CommitIndex::Absent);
        assert!(!popup.is_active(), "{EXPECT_CLOSED}");
    }

    /// A project whose history is a round trip away still answers the `#` on the
    /// frame it was typed on, which is what the spinner is for.
    #[test_case("#a1b2" ; "hash")]
    #[test_case(TITLE_QUERY ; "title")]
    fn a_pending_index_opens_the_popup_on_a_spinner(query: &str) {
        let mut popup = CommitPopup::new();
        popup.sync(query, query.chars().count(), &CommitIndex::Pending);
        assert!(popup.is_loading(), "{EXPECT_LOADING}");
        assert!(!popup.is_open(), "{LOADING_CAPTURES}");
    }

    /// A window landing behind an open popup replaces its rows. Without this the
    /// spinner would never become a list.
    #[test_case("#a1b2" ; "hash")]
    #[test_case(TITLE_QUERY ; "title")]
    fn a_window_arriving_behind_a_loading_popup_becomes_its_rows(query: &str) {
        let mut popup = CommitPopup::new();
        popup.sync(query, query.chars().count(), &CommitIndex::Pending);
        popup.sync(query, query.chars().count(), &index());
        popup.settle();
        assert!(popup.is_open(), "{EXPECT_LISTED}");
        assert!(!popup.is_loading(), "{LOADING_LINGERED}");
        assert_eq!(
            inserted(popup.handle_key(key(KeyCode::Enter))).as_deref(),
            Some(FIRST_MENTION)
        );
    }

    /// A refresh replaces the window, and an open popup has to notice: listing
    /// the log from before the reader's own commit is the bug this guards.
    #[test]
    fn a_refreshed_window_reseeds_an_open_popup() {
        let mut popup = CommitPopup::new();
        popup.sync("#", 1, &index());
        popup.settle();
        let refreshed = CommitIndex::loaded(vec![
            summary(THIRD, "Newest work"),
            summary(FIRST, SUBJECT),
            summary(SECOND, "Earlier work"),
        ]);
        popup.sync("#", 1, &refreshed);
        popup.settle();
        assert_eq!(
            popup.session.as_ref().expect(NO_SESSION).rows.len(),
            3,
            "{STALE_WINDOW}"
        );
    }

    /// A key that arrives while the spinner is up belongs to the composer. Rows
    /// landing mid-keystroke must not change what it does.
    #[test]
    fn a_loading_popup_passes_keys_through() {
        let mut popup = CommitPopup::new();
        popup.sync("#a1b2", 5, &CommitIndex::Pending);
        assert!(matches!(
            popup.handle_key(crate::components::key(KeyCode::Enter)),
            CommitAction::Passthrough
        ));
    }

    /// Nothing is known yet, so a hash nobody could have typed by accident is
    /// admitted and an abbreviation waits for the window.
    #[test_case(CommitIndex::Pending, FIRST, true ; "pending_admits_a_full_hash")]
    #[test_case(CommitIndex::Pending, "a1b2c3d", false ; "pending_refuses_an_abbreviation")]
    #[test_case(CommitIndex::Absent, FIRST, false ; "absent_refuses_even_a_full_hash")]
    #[test_case(CommitIndex::Absent, "a1b2c3d", false ; "absent_refuses_an_abbreviation")]
    fn an_index_without_rows_still_decides_what_resolves(
        index: CommitIndex,
        id: &str,
        expected: bool,
    ) {
        assert_eq!(index.resolves(id), expected);
    }

    /// The spinner turns on the clock alone, so a loading popup has to ask for
    /// frames. A list does not: nothing about it moves until the reader moves.
    #[test]
    fn only_a_loading_popup_asks_for_frames() {
        let mut popup = CommitPopup::new();
        assert_eq!(popup.cadence(), Cadence::IDLE);

        popup.sync("#a1b2", 5, &CommitIndex::Pending);
        assert_eq!(popup.cadence(), Cadence::SPINNER, "{SPINNER_FROZEN}");

        popup.sync("#a1b2", 5, &index());
        popup.settle();
        assert_eq!(popup.cadence(), Cadence::IDLE, "{LOADING_LINGERED}");
    }

    /// A repository with no commits is an answer, not a wait: unlike a pending
    /// index it refuses even a full hash, because there is nothing coming.
    #[test]
    fn an_empty_window_is_absent_rather_than_pending() {
        let index = CommitIndex::loaded(Vec::new());
        assert!(index.is_absent());
        assert!(!index.resolves(FIRST));
    }

    #[test]
    fn choosing_inserts_the_abbreviated_hash() {
        let mut popup = opened();
        let action = popup.handle_key(crate::components::key(KeyCode::Enter));
        assert_eq!(
            inserted(action).as_deref(),
            Some("#a1b2c3d"),
            "{NOT_INSERTED}"
        );
        assert!(popup.session.is_none(), "a commit finishes the mention");
    }

    #[test]
    fn a_press_and_release_on_one_row_takes_that_commit() {
        let mut popup = opened();
        popup.sync(TITLE_QUERY, TITLE_QUERY.chars().count(), &index());
        popup.settle();
        popup
            .session
            .as_mut()
            .expect(NO_SESSION)
            .completion
            .set_area(AREA);
        popup.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 0));
        let action = popup.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 0));
        assert_eq!(
            inserted(action).as_deref(),
            Some(FIRST_MENTION),
            "{NOT_INSERTED}"
        );
    }

    #[test_case("#0f1e", Some(SECOND_MENTION) ; "hash")]
    #[test_case(TITLE_QUERY, Some(FIRST_MENTION) ; "title_word")]
    #[test_case("#fixlogin", Some(FIRST_MENTION) ; "fuzzy_title")]
    #[test_case("#Hopper", Some(SECOND_MENTION) ; "author")]
    #[test_case("#zzz", None ; "unmatched")]
    #[test_case("#2023", None ; "dates_are_not_searchable")]
    fn the_query_selects_the_matching_commit(query: &str, expected: Option<&str>) {
        let index = index();
        let mut popup = CommitPopup::new();
        popup.sync("#", 1, &index);
        popup.settle();
        popup.sync(query, query.chars().count(), &index);
        popup.settle();
        let action = popup.handle_key(key(KeyCode::Enter));
        assert_eq!(inserted(action).as_deref(), expected);
    }

    #[test]
    fn a_haystack_carries_the_hash_subject_and_author() {
        let row = haystack(&summary(FIRST, SUBJECT));
        assert!(row.starts_with("a1b2c3d "));
        assert!(row.contains(SUBJECT));
        assert!(row.ends_with(AUTHOR));
    }

    #[test_case(HIDDEN_WORD)]
    fn a_truncated_title_remains_fully_searchable(word: &str) {
        let subject = format!("{} {word}", "x".repeat(SUBJECT_WIDTH));
        let commit = summary(FIRST, &subject);
        let query = format!("#{word}");
        let search = haystack(&commit);
        let mut popup = CommitPopup::new();
        popup.sync(
            &query,
            query.chars().count(),
            &CommitIndex::loaded(vec![commit]),
        );
        popup.settle();
        let row = &popup
            .session
            .as_ref()
            .expect(NO_SESSION)
            .rows
            .get(&search)
            .expect(MISSING_ROW)
            .text;
        assert!(row.contains('\u{2026}'));
        assert!(row.ends_with(AUTHOR));
        assert!(!row.contains(word));
        assert_eq!(
            inserted(popup.handle_key(key(KeyCode::Tab))).as_deref(),
            Some(FIRST_MENTION)
        );
    }

    fn long_subject() -> String {
        format!("{} {HIDDEN_WORD}", "x".repeat(SUBJECT_WIDTH))
    }

    /// The tip a popup listing a cut subject and then a whole one offers with
    /// the pointer moved to `row` of its list.
    fn hovered(row: u16) -> Option<Tip> {
        let mut popup = CommitPopup::new();
        let index = CommitIndex::loaded(vec![
            summary(FIRST, &long_subject()),
            summary(SECOND, SUBJECT),
        ]);
        popup.sync("#", 1, &index);
        popup.settle();
        popup
            .session
            .as_mut()
            .expect(NO_SESSION)
            .completion
            .set_area(AREA);
        popup.handle_mouse(event(MouseEventKind::Moved, row));
        popup.tooltip()
    }

    #[test_case(0, true ; "a_cut_subject")]
    #[test_case(1, false ; "a_subject_that_fits")]
    #[test_case(AREA.height, false ; "off_the_rows")]
    fn a_hovered_row_offers_its_subject_only_when_cut(row: u16, offered: bool) {
        let drawn = Rect {
            y: AREA.y + row,
            height: 1,
            ..AREA
        };
        let expected = offered.then(|| Tip {
            key: TipKey::Row(drawn),
            anchor: Anchor::Area(drawn),
            text: long_subject(),
        });
        assert_eq!(hovered(row), expected, "{TIP_ONLY_WHEN_CUT}");
    }

    /// As many wide glyphs as the row has columns for narrow ones: the count
    /// fits and the width does not.
    #[test]
    fn a_wide_subject_is_cut_by_the_columns_it_takes() {
        let subject = WIDE_GLYPH.repeat(SUBJECT_WIDTH);
        let row = CommitRow::new(&summary(FIRST, &subject), &TimeZone::UTC);
        let drawn = row.text.split(GAP).nth(2).expect(MISSING_ROW);
        assert!(drawn.width() <= SUBJECT_WIDTH, "{NOT_CUT_BY_COLUMNS}");
        assert!(drawn.ends_with(ELLIPSIS), "{NOT_CUT_BY_COLUMNS}");
        assert_eq!(row.cut_subject, Some(subject), "{TIP_ONLY_WHEN_CUT}");
    }

    #[test_case(COMMITTED, 0, UTC_DATETIME ; "utc")]
    #[test_case(COMMITTED, OFFSET_SECONDS, OFFSET_DATETIME ; "offset_crosses_midnight")]
    #[test_case(i64::MAX, 0, UNKNOWN_DATE ; "unrepresentable")]
    fn timestamps_use_the_requested_timezone(seconds: i64, offset: i32, expected: &str) {
        let zone = TimeZone::fixed(Offset::from_seconds(offset).expect(INVALID_OFFSET));
        assert_eq!(datetime(seconds, &zone), expected);
    }

    #[test_case(WIDE ; "wide")]
    #[test_case(NARROW ; "narrow")]
    fn rendered_rows_place_the_datetime_between_hash_and_title(width: u16) {
        let mut popup = CommitPopup::new();
        let commit = summary(FIRST, SUBJECT);
        let date = datetime(COMMITTED, &TimeZone::system());
        let expected = format!(" {}  {date}  {SUBJECT}  {AUTHOR} ", commit.short());
        popup.sync("#", 1, &CommitIndex::loaded(vec![commit]));
        popup.settle();
        let mut terminal = Terminal::new(TestBackend::new(width, AREA.bottom())).expect(NO_FRAME);
        let mut drawn = None;
        terminal
            .draw(|frame| drawn = popup.view(frame, Rect { width, ..AREA }))
            .expect(NO_FRAME);
        let area = drawn.expect(NO_FRAME);
        assert_eq!(area.width as usize, expected.len().min(width as usize));
        let buffer = terminal.backend().buffer();
        let actual: String = (area.x..area.right())
            .map(|x| buffer[(x, area.y)].symbol())
            .collect();
        assert_eq!(
            actual,
            expected.chars().take(width as usize).collect::<String>()
        );
    }

    #[test_case(i64::MAX)]
    fn invalid_timestamps_keep_the_commit_row(seconds: i64) {
        let mut commit = summary(FIRST, SUBJECT);
        commit.committed_unix_seconds = seconds;
        let row = CommitRow::new(&commit, &TimeZone::UTC);
        assert_eq!(row.hash, commit.short());
        assert_eq!(
            row.text,
            format!("{}  {UNKNOWN_DATE}  {SUBJECT}  {AUTHOR}", commit.short())
        );
    }

    #[test_case("a1b2c3d", true ; "an_abbreviation_of_a_listed_commit")]
    #[test_case(FIRST, true ; "a_full_hash")]
    #[test_case("deadbee", false ; "a_hash_of_no_listed_commit")]
    fn the_index_resolves_what_it_lists(id: &str, expected: bool) {
        assert_eq!(index().resolves(id), expected);
    }

    #[test]
    fn the_index_describes_the_commit_behind_an_abbreviation() {
        let described = index().describe("a1b2c3d").expect("a listed commit");
        assert!(described.starts_with(SUBJECT), "{described}");
        assert!(described.contains(AUTHOR), "{described}");
    }
}
