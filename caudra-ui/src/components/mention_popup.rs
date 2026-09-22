//! The `@path` completion popup.
//!
//! Typing `@` in the composer opens a list of project paths, fuzzy-matched
//! against the same project walk the file picker uses, and splices the choice
//! back over the query that opened it. A remote session lists the workspace
//! through the workbench backend instead of walking a local directory.
//!
//! Everything about being a list — matching, selection, scrolling, hit testing
//! and painting — lives in [`crate::components::completion`]. What is here is
//! what makes the list one of paths: where the rows come from, and the fact
//! that a directory drills in rather than finishing the mention.

use caudra_workbench::{
    BackendDriver, BackendEvent, ResourceEntry, WorkbenchBackend, WorkbenchPath,
};
use caudra_workspace::{ResourceKind, WorkspacePath, WorkspaceSession};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use nucleo::{Config, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::components::completion::{self, Completion, MouseOutcome, PAD};
use crate::components::file_walk::{self, Walk};
use crate::repaint::Dirty;
use crate::theme;

const SIGIL: char = '@';
const SEPARATOR: char = std::path::MAIN_SEPARATOR;
const SCOPE: &str = "mention_popup";

pub(crate) enum MentionAction {
    Consumed,
    /// The chosen path and the composer range it replaces. The range travels
    /// with the action because choosing also closes the popup, and a closed
    /// popup no longer remembers what it was completing.
    Insert {
        range: Range<usize>,
        path: String,
    },
    Passthrough,
}

struct Session {
    completion: Completion,
    cancel: Arc<AtomicBool>,
    done_rx: Option<flume::Receiver<Walk>>,
    backend: Option<BackendDriver>,
    remote_resources: HashMap<String, ResourceEntry>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(crate) struct MentionPopup {
    session: Option<Session>,
    /// Char range of the `@query` the popup is completing, into the composer's
    /// display text.
    trigger: Option<Range<usize>>,
}

impl MentionPopup {
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

    /// Where the rows are, so the host can hand the popup the wheel it drew
    /// under the pointer instead of scrolling the transcript behind it.
    pub fn contains(&self, position: Position) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.completion.contains(position))
    }

    /// The wheel walks the list. The viewport follows the selection on every
    /// frame, so scrolling the offset on its own would not survive the redraw.
    pub fn scroll(&mut self, delta: i32) {
        self.step(-delta.signum() as isize);
    }

    /// Re-reads the composer after an edit. Opens when the cursor is inside an
    /// `@query`, re-queries while it grows, and closes as soon as it is not.
    pub fn sync_workspace(
        &mut self,
        text: &str,
        cursor: usize,
        cwd: &str,
        workspace: Option<WorkspaceSession>,
    ) {
        let Some((range, query)) = completion::trigger_at(text, cursor, SIGIL) else {
            self.close();
            return;
        };
        self.trigger = Some(range);
        match &mut self.session {
            Some(session) if session.completion.query() == query => {}
            Some(session) => session.completion.set_query(query),
            None => match workspace {
                Some(workspace) => self.start_workspace(workspace, query),
                None => self.start(cwd, query),
            },
        }
    }

    fn start(&mut self, cwd: &str, query: String) {
        let completion = Completion::new(Config::DEFAULT.match_paths(), query);
        let cancel = Arc::new(AtomicBool::new(false));
        let Some(done_rx) = file_walk::spawn(
            PathBuf::from(cwd),
            completion.injector(),
            Arc::clone(&cancel),
        ) else {
            return;
        };
        self.open(Session {
            completion,
            cancel,
            done_rx: Some(done_rx),
            backend: None,
            remote_resources: HashMap::new(),
        });
    }

    fn start_workspace(&mut self, workspace: WorkspaceSession, query: String) {
        let root = WorkbenchPath::Remote(WorkspacePath::root());
        let Ok(backend) = WorkbenchBackend::workspace(workspace) else {
            return;
        };
        let mut backend = BackendDriver::new(backend, root.clone());
        backend.list(root, true);
        self.open(Session {
            completion: Completion::new(Config::DEFAULT.match_paths(), query),
            cancel: Arc::new(AtomicBool::new(false)),
            done_rx: None,
            backend: Some(backend),
            remote_resources: HashMap::new(),
        });
    }

    /// Runs the query the session was built with before the popup adopts it, so
    /// the first frame shows matches rather than the whole project.
    fn open(&mut self, mut session: Session) {
        let query = session.completion.query().to_owned();
        session.completion.set_query(query);
        self.session = Some(session);
    }

    /// Collects whatever the walk has produced so far. The popup is fed by a
    /// background thread, so it grows over the first few frames after opening.
    pub fn tick(&mut self) -> Dirty {
        let Some(session) = &mut self.session else {
            return Dirty::NO;
        };
        if let Some(done_rx) = &session.done_rx {
            let _ = done_rx.try_recv();
        }
        if let Some(backend) = &mut session.backend {
            for event in backend.drain() {
                if let BackendEvent::Listed {
                    result: Ok(result), ..
                } = event
                {
                    let injector = session.completion.injector();
                    for entry in result.entries {
                        let mut path = entry.path.display();
                        if entry.kind == ResourceKind::Directory {
                            path.push('/');
                        }
                        injector.push((), |_, columns| {
                            columns[0] = Utf32String::from(path.as_str());
                        });
                        session.remote_resources.insert(path, entry);
                    }
                }
            }
        }
        session.completion.tick()
    }

    /// Waits for the walk to finish and the matcher to drain, so a test can
    /// assert on what the popup found without racing the walker thread.
    #[cfg(test)]
    pub fn settle(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        if let Some(done_rx) = &session.done_rx {
            let _ = done_rx.recv();
        }
        session.completion.settle();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MentionAction {
        if !self.is_open() {
            return MentionAction::Passthrough;
        }
        match key.code {
            KeyCode::Esc => {
                self.close();
                MentionAction::Consumed
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Enter | KeyCode::Tab if key.modifiers == KeyModifiers::NONE => {
                self.choose(key.code == KeyCode::Tab)
            }
            _ => MentionAction::Passthrough,
        }
    }

    /// The rows answer the pointer the way they answer the arrow keys: moving
    /// over one highlights it, and a press and release on the same row takes
    /// it. Anything outside the drawn rows is left for the composer.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> MentionAction {
        let Some(session) = &mut self.session else {
            return MentionAction::Passthrough;
        };
        match session.completion.handle_mouse(event) {
            MouseOutcome::Outside => MentionAction::Passthrough,
            MouseOutcome::Consumed => MentionAction::Consumed,
            // One click carries no second intent, so a directory always drills
            // in and only a file finishes the mention.
            MouseOutcome::Chosen => self.choose(true),
        }
    }

    fn step(&mut self, delta: isize) -> MentionAction {
        if let Some(session) = &mut self.session {
            session.completion.step(delta);
        }
        MentionAction::Consumed
    }

    /// Splices the highlighted row over the `@query` that opened the popup.
    /// A directory is a step on the way somewhere, so `drill` leaves the popup
    /// open with the query extended instead of taking it as the answer.
    fn choose(&mut self, drill: bool) -> MentionAction {
        let (Some(session), Some(range)) = (self.session.as_mut(), self.trigger.clone()) else {
            return MentionAction::Passthrough;
        };
        let Some(chosen) = session.completion.selected().map(str::to_owned) else {
            return MentionAction::Passthrough;
        };
        if session.backend.is_some() && !session.remote_resources.contains_key(&chosen) {
            return MentionAction::Consumed;
        }
        let path = format!("{SIGIL}{chosen}");
        match drill && chosen.ends_with(SEPARATOR) {
            true => {
                session.completion.set_query(chosen);
                self.trigger = Some(range.start..range.start + path.chars().count());
            }
            false => self.close(),
        }
        MentionAction::Insert { range, path }
    }

    pub fn view(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        let session = self.session.as_mut()?;
        let theme = theme::current();
        session.completion.view(
            frame,
            input_area,
            SCOPE,
            |path| path.chars().count() as u16 + PAD * 2,
            move |path, selected| {
                let style = match selected {
                    true => theme.item_selected,
                    false => theme.item,
                };
                Line::from(Span::styled(format!(" {path} "), style))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEventKind};
    use test_case::test_case;

    const ROWS: [&str; 3] = ["src/lib.rs", "src/main.rs", "docs/"];
    const AREA: Rect = Rect {
        x: 0,
        y: 4,
        width: 20,
        height: 3,
    };
    const DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
    const UP: MouseEventKind = MouseEventKind::Up(MouseButton::Left);
    const NO_SESSION: &str = "the popup dropped the session it was given";
    const NOT_INSERTED: &str = "the click did not complete the row it landed on";
    const INSERTED: &str = "the pointer completed a row it should have left alone";
    const WRONG_ROW: &str = "the pointer marked a row other than the one under it";

    /// A popup with rows already matched, so a test can drive the pointer
    /// without waiting on a walker thread.
    fn popup(rows: &[&str], area: Rect) -> MentionPopup {
        let (_tx, done_rx) = flume::bounded(1);
        MentionPopup {
            session: Some(Session {
                completion: completion::seeded(rows, area),
                cancel: Arc::new(AtomicBool::new(false)),
                done_rx: Some(done_rx),
                backend: None,
                remote_resources: HashMap::new(),
            }),
            trigger: Some(0..1),
        }
    }

    fn event(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: AREA.x,
            row: AREA.y + row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn inserted(action: MentionAction) -> Option<String> {
        match action {
            MentionAction::Insert { path, .. } => Some(path),
            _ => None,
        }
    }

    fn selected(popup: &MentionPopup) -> usize {
        popup
            .session
            .as_ref()
            .expect(NO_SESSION)
            .completion
            .selected_index()
    }

    #[test_case(0 ; "first")]
    #[test_case(2 ; "last")]
    fn moving_over_a_row_marks_it(row: u16) {
        let mut popup = popup(&ROWS, AREA);

        popup.handle_mouse(event(MouseEventKind::Moved, row));

        assert_eq!(selected(&popup), row as usize, "{WRONG_ROW}");
    }

    #[test]
    fn pressing_and_releasing_one_row_completes_it() {
        let mut popup = popup(&ROWS, AREA);

        popup.handle_mouse(event(DOWN, 1));
        let action = popup.handle_mouse(event(UP, 1));

        assert_eq!(
            inserted(action).as_deref(),
            Some("@src/main.rs"),
            "{NOT_INSERTED}"
        );
        assert!(popup.session.is_none(), "a file finishes the mention");
    }

    /// Pressing one row and releasing on another is a slip rather than a
    /// choice, so nothing is completed.
    #[test]
    fn releasing_on_another_row_completes_nothing() {
        let mut popup = popup(&ROWS, AREA);

        popup.handle_mouse(event(DOWN, 0));
        let action = popup.handle_mouse(event(UP, 1));

        assert!(inserted(action).is_none(), "{INSERTED}");
        assert!(popup.session.is_some(), "{NO_SESSION}");
    }

    /// One click carries no second intent, so a directory drills in the way
    /// `Tab` does instead of ending the mention on a folder.
    #[test]
    fn clicking_a_directory_keeps_the_popup_open() {
        let mut popup = popup(&ROWS, AREA);

        popup.handle_mouse(event(DOWN, 2));
        let action = popup.handle_mouse(event(UP, 2));

        assert_eq!(
            inserted(action).as_deref(),
            Some("@docs/"),
            "{NOT_INSERTED}"
        );
        assert_eq!(
            popup.trigger,
            Some(0..6),
            "the query grew by what was inserted"
        );
    }

    #[test_case(-1, 1 ; "down")]
    #[test_case(1, ROWS.len() - 1 ; "up_wraps")]
    fn the_wheel_moves_the_selection(delta: i32, expected: usize) {
        let mut popup = popup(&ROWS, AREA);

        popup.scroll(delta);

        assert_eq!(selected(&popup), expected, "{WRONG_ROW}");
    }

    #[test]
    fn the_wheel_is_claimed_only_over_the_drawn_rows() {
        let popup = popup(&ROWS, AREA);

        assert!(popup.contains(Position::new(AREA.x, AREA.y)));
        assert!(!popup.contains(Position::new(AREA.x, AREA.bottom())));
    }

    #[test]
    fn a_press_outside_the_rows_is_left_for_the_composer() {
        let mut popup = popup(&ROWS, AREA);

        let action = popup.handle_mouse(MouseEvent {
            kind: DOWN,
            column: AREA.x,
            row: AREA.bottom(),
            modifiers: KeyModifiers::NONE,
        });

        assert!(matches!(action, MentionAction::Passthrough), "{INSERTED}");
    }

    /// An edit can shrink the list before the next frame redraws the popup, so
    /// the drawn area outlives the rows it was sized for.
    #[test]
    fn a_row_the_list_no_longer_has_completes_nothing() {
        let mut popup = popup(&ROWS[..1], AREA);

        popup.handle_mouse(event(DOWN, 2));
        let action = popup.handle_mouse(event(UP, 2));

        assert!(inserted(action).is_none(), "{INSERTED}");
        assert_eq!(selected(&popup), 0, "{WRONG_ROW}");
    }

    #[test_case("@src", 4, Some((0, "src")) ; "at_end_of_query")]
    #[test_case("see @src", 8, Some((4, "src")) ; "after_whitespace")]
    #[test_case("@", 1, Some((0, "")) ; "bare_sigil_lists_everything")]
    #[test_case("@src/main.rs", 4, Some((0, "src")) ; "cursor_inside_the_query")]
    #[test_case("user@host", 9, None ; "sigil_mid_word")]
    #[test_case("@src now", 8, None ; "whitespace_closes_it")]
    #[test_case("plain", 5, None ; "no_sigil")]
    fn trigger_follows_the_cursor(text: &str, cursor: usize, expected: Option<(usize, &str)>) {
        let found = completion::trigger_at(text, cursor, SIGIL);
        assert_eq!(
            found
                .as_ref()
                .map(|(range, query)| (range.start, query.as_str())),
            expected,
            "{text}"
        );
        if let Some((range, _)) = found {
            assert_eq!(range.end, cursor);
        }
    }
}
