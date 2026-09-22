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
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use nucleo::{Config, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::completion::{self, Completion, MouseOutcome, PAD};
use crate::repaint::Dirty;
use crate::theme;

const SIGIL: char = '#';
const SCOPE: &str = "commit_popup";
/// Columns a subject is allowed in a row. Past this the hash and the author
/// would be pushed off a narrow composer.
const SUBJECT_WIDTH: usize = 60;
const GAP: &str = "  ";

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
/// Cheap to clone: every surface that needs it shares one load.
#[derive(Clone, Default)]
pub(crate) enum CommitIndex {
    /// No repository, or none read yet. Nothing lists and nothing resolves,
    /// which is what leaves `#` as prose in a project without git.
    #[default]
    Absent,
    /// The local log window. Lists rows, and resolves exactly what it lists.
    Local(Arc<[CommitSummary]>),
    /// A remote workspace, whose log this process cannot walk.
    ///
    /// Nothing lists, so there is no popup. A hash the reader spells out in
    /// full still resolves, because the workspace can be asked about it at send
    /// time and will say so if it does not know it. Admitting an unknown hash
    /// here costs one error note; refusing every hash would make a remote
    /// session unable to reference a commit at all.
    Remote,
}

impl CommitIndex {
    /// Reads the recent log of the repository `root` sits in. A directory that
    /// is not inside one yields [`Self::Absent`], which is not an error: most
    /// prose containing a `#` is not about a commit either way.
    pub fn load(root: &std::path::Path) -> Self {
        match repo::log(root, repo::LOG_LIMIT) {
            Ok(commits) if !commits.is_empty() => Self::Local(commits.into()),
            _ => Self::Absent,
        }
    }

    fn commits(&self) -> &[CommitSummary] {
        match self {
            Self::Local(commits) => commits,
            Self::Absent | Self::Remote => &[],
        }
    }

    /// Whether there is nothing to list, which is what keeps the popup shut.
    pub fn is_empty(&self) -> bool {
        self.commits().is_empty()
    }

    /// Whether `id` names a commit worth resolving, which is the predicate the
    /// composer's scanner runs on every edit.
    pub fn resolves(&self, id: &str) -> bool {
        match self {
            Self::Absent => false,
            Self::Remote => true,
            Self::Local(commits) => commits.iter().any(|commit| commit.id.starts_with(id)),
        }
    }

    /// The subject of the commit `id` abbreviates, for the status bar. A remote
    /// index knows no subjects, so a hovered hash there shows itself.
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

    pub fn is_open(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.completion.is_empty())
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
    pub fn sync(&mut self, text: &str, cursor: usize, index: &CommitIndex) {
        let Some((range, query)) = completion::trigger_at(text, cursor, SIGIL) else {
            self.close();
            return;
        };
        // A heading, a list marker or an issue number all start `#` too. Only a
        // run that could still grow into a hash is worth a popup.
        if index.is_empty() || !query.chars().all(|c| c.is_ascii_hexdigit()) {
            self.close();
            return;
        }
        self.trigger = Some(range);
        match &mut self.session {
            Some(session) if session.completion.query() == query => {}
            Some(session) => session.completion.set_query(query),
            None => self.start(query, index),
        }
    }

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
        let mut session = Session { completion, hashes };
        session.completion.set_query(query);
        self.session = Some(session);
    }

    /// The log is loaded up front rather than walked in the background, so a
    /// tick only has to drain the matcher.
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

    fn summary(id: &str, subject: &str) -> CommitSummary {
        CommitSummary {
            id: id.to_owned(),
            subject: subject.to_owned(),
            author: AUTHOR.to_owned(),
            committed_unix_seconds: 1_700_000_000,
        }
    }

    fn index() -> CommitIndex {
        CommitIndex::Local(vec![summary(FIRST, SUBJECT), summary(SECOND, "Earlier work")].into())
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

    /// Without a repository there is nothing to complete, so `#` stays prose.
    #[test_case(CommitIndex::Absent ; "no_repository")]
    #[test_case(CommitIndex::Remote ; "remote_workspace")]
    fn an_index_with_nothing_to_list_never_opens_the_popup(index: CommitIndex) {
        let mut popup = CommitPopup::new();
        popup.sync("#a1b2", 5, &index);
        assert!(!popup.is_open(), "{EXPECT_CLOSED}");
    }

    /// A remote session cannot list commits, but a hash spelled out in full
    /// must still reach the workspace, which is the only thing that can answer.
    #[test_case(CommitIndex::Absent, false ; "no_repository_resolves_nothing")]
    #[test_case(CommitIndex::Remote, true ; "remote_admits_any_hash")]
    fn an_unlistable_index_still_decides_what_resolves(index: CommitIndex, expected: bool) {
        assert_eq!(index.resolves("deadbeef"), expected);
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
