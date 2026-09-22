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
use nucleo::{Config, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::completion::{self, Completion, MouseOutcome, PAD};
use crate::repaint::{Cadence, Dirty};
use crate::theme;

const SIGIL: char = '#';
const SCOPE: &str = "commit_popup";
/// Columns a subject is allowed in a row. Past this the hash and the author
/// would be pushed off a narrow composer.
const SUBJECT_WIDTH: usize = 60;
const GAP: &str = "  ";
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

    /// The subject of the commit `id` abbreviates, for the status bar. A window
    /// that has not arrived knows no subjects, so a hovered hash shows itself.
    pub fn subject(&self, id: &str) -> Option<&str> {
        self.commits()
            .iter()
            .find(|commit| commit.id.starts_with(id))
            .map(|commit| commit.subject.as_str())
    }
}

struct Session {
    completion: Completion,
    /// The row text each match came from, so choosing one recovers its hash
    /// without re-parsing the rendered row.
    hashes: HashMap<String, String>,
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

    /// Re-reads the composer after an edit. Opens when the cursor is inside a
    /// `#query` and the query could still become a hash, re-queries while it
    /// grows, and closes as soon as it is not.
    ///
    /// A pending index opens the popup anyway, on a spinner: the reader asked a
    /// question and the answer is on its way.
    pub fn sync(&mut self, text: &str, cursor: usize, index: &CommitIndex) {
        let Some((range, query)) = completion::trigger_at(text, cursor, SIGIL) else {
            self.close();
            return;
        };
        // A heading, a list marker or an issue number all start `#` too. Only a
        // run that could still grow into a hash is worth a popup.
        if index.is_absent() || !query.chars().all(|c| c.is_ascii_hexdigit()) {
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
        let mut hashes = HashMap::new();
        for commit in index.commits() {
            let row = haystack(commit);
            injector.push((), |_, columns| {
                columns[0] = Utf32String::from(row.as_str());
            });
            hashes.insert(row, commit.short().to_owned());
        }
        let sourced = match index {
            CommitIndex::Loaded(commits) => Some(Arc::clone(commits)),
            CommitIndex::Pending | CommitIndex::Absent => None,
        };
        let mut session = Session {
            completion,
            hashes,
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
            .and_then(|row| session.hashes.get(row))
            .cloned()
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
        session.completion.view(
            frame,
            input_area,
            SCOPE,
            |row| UnicodeWidthStr::width(row) as u16 + PAD * 2,
            move |row, selected| {
                let base = match selected {
                    true => theme.item_selected,
                    false => theme.item,
                };
                let (hash, rest) = row.split_once(' ').unwrap_or((row, ""));
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

/// What the matcher searches and the row displays: the abbreviated hash, the
/// subject, and the author. One string for both, so what a reader matched on is
/// exactly what they can see.
fn haystack(commit: &CommitSummary) -> String {
    let subject = truncate(&commit.subject, SUBJECT_WIDTH);
    format!("{} {subject}{GAP}{}", commit.short(), commit.author)
}

fn truncate(text: &str, width: usize) -> String {
    match text.chars().count() > width {
        true => text.chars().take(width - 1).collect::<String>() + "\u{2026}",
        false => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEventKind};
    use test_case::test_case;

    const FIRST: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const SECOND: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f098765432";
    const SUBJECT: &str = "Fix login crash";
    const AUTHOR: &str = "Ada Lovelace";
    const AREA: Rect = Rect {
        x: 0,
        y: 4,
        width: 60,
        height: 2,
    };
    const NO_SESSION: &str = "the popup dropped the session it was given";
    const NOT_INSERTED: &str = "the click did not take the row it landed on";
    const EXPECT_CLOSED: &str = "the popup stayed open over text that cannot be a hash";
    const THIRD: &str = "c3d4e5f60718293a4b5c6d7e8f90123456789abc";
    const EXPECT_LOADING: &str = "a `#` typed before the log arrives must still answer";
    const LOADING_CAPTURES: &str = "a popup with no rows must not capture keys or clicks";
    const EXPECT_LISTED: &str = "the window that arrived did not become rows";
    const LOADING_LINGERED: &str = "the spinner outlived the window it was waiting for";
    const STALE_WINDOW: &str = "the popup kept listing the log from before the refresh";
    const SPINNER_FROZEN: &str = "a spinning popup must ask for the frames that turn it";

    fn summary(id: &str, subject: &str) -> CommitSummary {
        CommitSummary {
            id: id.to_owned(),
            subject: subject.to_owned(),
            author: AUTHOR.to_owned(),
        }
    }

    fn index() -> CommitIndex {
        CommitIndex::loaded(vec![
            summary(FIRST, SUBJECT),
            summary(SECOND, "Earlier work"),
        ])
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
            popup.session.as_ref().expect(NO_SESSION).hashes.len(),
            2,
            "both commits are listed"
        );
    }

    #[test_case("# heading", 2 ; "a_heading")]
    #[test_case("#zzz", 4 ; "not_hexadecimal")]
    fn text_that_cannot_become_a_hash_does_not_open_the_popup(text: &str, cursor: usize) {
        let mut popup = CommitPopup::new();
        popup.sync(text, cursor, &index());
        assert!(!popup.is_open(), "{EXPECT_CLOSED}: {text}");
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
    #[test]
    fn a_pending_index_opens_the_popup_on_a_spinner() {
        let mut popup = CommitPopup::new();
        popup.sync("#a1b2", 5, &CommitIndex::Pending);
        assert!(popup.is_loading(), "{EXPECT_LOADING}");
        assert!(!popup.is_open(), "{LOADING_CAPTURES}");
    }

    /// A window landing behind an open popup replaces its rows. Without this the
    /// spinner would never become a list.
    #[test]
    fn a_window_arriving_behind_a_loading_popup_becomes_its_rows() {
        let mut popup = CommitPopup::new();
        popup.sync("#a1b2", 5, &CommitIndex::Pending);
        popup.sync("#a1b2", 5, &index());
        popup.settle();
        assert!(popup.is_open(), "{EXPECT_LISTED}");
        assert!(!popup.is_loading(), "{LOADING_LINGERED}");
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
            popup.session.as_ref().expect(NO_SESSION).hashes.len(),
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
        popup
            .session
            .as_mut()
            .expect(NO_SESSION)
            .completion
            .set_area(AREA);
        popup.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 0));
        let action = popup.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 0));
        assert!(inserted(action).is_some(), "{NOT_INSERTED}");
    }

    #[test]
    fn the_query_narrows_the_list_to_the_hash_it_names() {
        let mut popup = CommitPopup::new();
        popup.sync("#0f1e", 5, &index());
        popup.settle();
        let action = popup.handle_key(crate::components::key(KeyCode::Enter));
        assert_eq!(inserted(action).as_deref(), Some("#0f1e2d3"));
    }

    #[test]
    fn a_haystack_carries_the_hash_subject_and_author() {
        let row = haystack(&summary(FIRST, SUBJECT));
        assert!(row.starts_with("a1b2c3d "));
        assert!(row.contains(SUBJECT));
        assert!(row.ends_with(AUTHOR));
    }

    #[test]
    fn a_long_subject_is_cut_rather_than_pushing_the_author_off() {
        let row = haystack(&summary(FIRST, &"x".repeat(SUBJECT_WIDTH * 2)));
        assert!(row.contains('\u{2026}'));
        assert!(row.ends_with(AUTHOR));
    }

    #[test_case("a1b2c3d", true ; "an_abbreviation_of_a_listed_commit")]
    #[test_case(FIRST, true ; "a_full_hash")]
    #[test_case("deadbee", false ; "a_hash_of_no_listed_commit")]
    fn the_index_resolves_what_it_lists(id: &str, expected: bool) {
        assert_eq!(index().resolves(id), expected);
    }

    #[test]
    fn the_index_names_the_subject_behind_an_abbreviation() {
        assert_eq!(index().subject("a1b2c3d"), Some(SUBJECT));
    }
}
