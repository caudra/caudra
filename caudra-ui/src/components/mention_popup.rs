//! Path completion for `@` mentions.
//!
//! Opens while the cursor sits inside an `@query` the user is typing, matches
//! against the same project walk the file picker uses, and splices the choice
//! back over the query in place. Unlike the command palette, which replaces the
//! whole buffer to complete, this must edit a range: the composer may hold
//! paste tokens that a whole-buffer replacement would silently drop.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use caudra_grab::grab_scope;
use caudra_workbench::{
    BackendDriver, BackendEvent, ResourceEntry, WorkbenchBackend, WorkbenchPath,
};
use caudra_workspace::{ResourceKind, WorkspacePath, WorkspaceSession};

use crate::components::file_walk::{self, Walk};
use crate::repaint::Dirty;
use crate::theme;

const SIGIL: char = '@';
const SEPARATOR: char = std::path::MAIN_SEPARATOR;
const MAX_ROWS: usize = 10;
/// Matching a whole project on every keystroke is wasted work when the reader
/// can only see ten rows; nucleo is asked for a little more so scrolling has
/// somewhere to go.
const MAX_MATCHES: usize = 64;
const PAD: u16 = 1;
/// How long a settling tick waits on the matcher before looking again.
#[cfg(test)]
const POLL_MS: u64 = 10;

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
    nucleo: Nucleo<()>,
    matches: Vec<String>,
    selected: usize,
    scroll_offset: usize,
    cancel: Arc<AtomicBool>,
    done_rx: Option<flume::Receiver<Walk>>,
    backend: Option<BackendDriver>,
    remote_resources: HashMap<String, ResourceEntry>,
    walk: Walk,
    query: String,
    /// Where the rows were last drawn, so the pointer can find them. Cleared
    /// on a frame that draws nothing, because a stale rectangle would keep
    /// answering clicks for rows that are no longer on screen.
    area: Rect,
    /// The path the button went down on. A release only takes a row when it is
    /// the row the press started on.
    pressed: Option<String>,
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
            .is_some_and(|session| !session.matches.is_empty())
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
            .is_some_and(|session| session.area.contains(position))
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
        let Some((range, query)) = trigger_at(text, cursor) else {
            self.close();
            return;
        };
        self.trigger = Some(range);
        match &mut self.session {
            Some(session) if session.query == query => {}
            Some(session) => {
                session.query = query;
                reparse(session);
            }
            None => match workspace {
                Some(workspace) => self.start_workspace(workspace, query),
                None => self.start(cwd, query),
            },
        }
    }

    fn start(&mut self, cwd: &str, query: String) {
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let Some(done_rx) =
            file_walk::spawn(PathBuf::from(cwd), nucleo.injector(), Arc::clone(&cancel))
        else {
            return;
        };
        let mut session = Session {
            nucleo,
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            cancel,
            done_rx: Some(done_rx),
            backend: None,
            remote_resources: HashMap::new(),
            walk: Walk::Running,
            query,
            area: Rect::default(),
            pressed: None,
        };
        reparse(&mut session);
        self.session = Some(session);
    }

    fn start_workspace(&mut self, workspace: WorkspaceSession, query: String) {
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let root = WorkbenchPath::Remote(WorkspacePath::root());
        let Ok(backend) = WorkbenchBackend::workspace(workspace) else {
            return;
        };
        let mut backend = BackendDriver::new(backend, root.clone());
        backend.list(root, true);
        let mut session = Session {
            nucleo,
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            cancel,
            done_rx: None,
            backend: Some(backend),
            remote_resources: HashMap::new(),
            walk: Walk::Running,
            query,
            area: Rect::default(),
            pressed: None,
        };
        reparse(&mut session);
        self.session = Some(session);
    }

    /// Collects whatever the walk has produced so far. The popup is fed by a
    /// background thread, so it grows over the first few frames after opening.
    pub fn tick(&mut self) -> Dirty {
        let Some(session) = &mut self.session else {
            return Dirty::NO;
        };
        if let Some(done_rx) = &session.done_rx
            && let Ok(walk) = done_rx.try_recv()
        {
            session.walk = walk;
        }
        if let Some(backend) = &mut session.backend {
            for event in backend.drain() {
                if let BackendEvent::Listed {
                    result: Ok(result), ..
                } = event
                {
                    let injector = session.nucleo.injector();
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
                    session.walk = Walk::Listed;
                }
            }
        }
        let before = session.matches.len();
        session.nucleo.tick(0);
        refresh(session);
        match session.matches.len() == before {
            true => Dirty::NO,
            false => Dirty::YES,
        }
    }

    /// Waits for the walk to finish and the matcher to drain, so a test can
    /// assert on what the popup found without racing the walker thread.
    #[cfg(test)]
    pub fn settle(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        if let Some(done_rx) = &session.done_rx
            && let Ok(walk) = done_rx.recv()
        {
            session.walk = walk;
        }
        while session.nucleo.tick(POLL_MS).running {}
        refresh(session);
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
        let position = Position::new(event.column, event.row);
        let Some(session) = &mut self.session else {
            return MentionAction::Passthrough;
        };
        if !session.area.contains(position) {
            session.pressed = None;
            return MentionAction::Passthrough;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                session.pressed = None;
                if let Some(index) = row_at(session, position) {
                    session.selected = index;
                    session.pressed = Some(session.matches[index].clone());
                }
                MentionAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                session.pressed = None;
                MentionAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(index) = row_at(session, position) {
                    session.selected = index;
                }
                MentionAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = session.pressed.take();
                let landed = row_at(session, position);
                let Some(index) =
                    landed.filter(|&index| pressed == Some(session.matches[index].clone()))
                else {
                    return MentionAction::Consumed;
                };
                session.selected = index;
                // One click carries no second intent, so a directory always
                // drills in and only a file finishes the mention.
                self.choose(true)
            }
            _ => MentionAction::Consumed,
        }
    }

    fn step(&mut self, delta: isize) -> MentionAction {
        if let Some(session) = &mut self.session {
            move_selection(session, delta);
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
        let Some(chosen) = session.matches.get(session.selected).cloned() else {
            return MentionAction::Passthrough;
        };
        if session.backend.is_some() && !session.remote_resources.contains_key(&chosen) {
            return MentionAction::Consumed;
        }
        let path = format!("{SIGIL}{chosen}");
        match drill && chosen.ends_with(SEPARATOR) {
            true => {
                session.query = chosen;
                reparse(session);
                self.trigger = Some(range.start..range.start + path.chars().count());
            }
            false => self.close(),
        }
        MentionAction::Insert { range, path }
    }

    pub fn view(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        let session = self.session.as_mut()?;
        session.area = Rect::default();
        if session.matches.is_empty() {
            return None;
        }
        let height = (session.matches.len().min(MAX_ROWS) as u16).min(input_area.y);
        if height == 0 {
            return None;
        }
        session.scroll_offset = session
            .scroll_offset
            .min(session.selected)
            .max((session.selected + 1).saturating_sub(height as usize));

        let width = session
            .matches
            .iter()
            .map(|path| path.chars().count() as u16 + PAD * 2)
            .max()
            .unwrap_or(0)
            .min(input_area.width);
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(height),
            width,
            height,
        };
        session.area = area;

        grab_scope!("mention_popup", area);
        let theme = theme::current();
        let rows: Vec<Line> = session
            .matches
            .iter()
            .enumerate()
            .skip(session.scroll_offset)
            .take(height as usize)
            .map(|(index, path)| {
                let style = match index == session.selected {
                    true => theme.item_selected,
                    false => theme.item,
                };
                Line::from(Span::styled(format!(" {path} "), style))
            })
            .collect();

        Clear.render(area, frame.buffer_mut());
        frame.render_widget(
            Paragraph::new(rows).style(theme.item.add_modifier(Modifier::empty())),
            area,
        );
        Some(area)
    }
}

/// The `@query` under `cursor`, if there is one. The sigil has to start a word
/// and the query has to reach the cursor without whitespace, which is what
/// stops an old mention earlier in the line from reopening the popup.
fn trigger_at(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return None;
    }
    let start = chars[..cursor].iter().rposition(|&c| c == SIGIL)?;
    if start > 0 && !chars[start - 1].is_whitespace() {
        return None;
    }
    let query: String = chars[start + 1..cursor].iter().collect();
    match query.chars().any(char::is_whitespace) {
        true => None,
        false => Some((start..cursor, query)),
    }
}

fn reparse(session: &mut Session) {
    session.nucleo.pattern.reparse(
        0,
        &session.query,
        CaseMatching::Smart,
        Normalization::Smart,
        false,
    );
    session.selected = 0;
    session.scroll_offset = 0;
    session.nucleo.tick(0);
    refresh(session);
}

fn refresh(session: &mut Session) {
    let snapshot = session.nucleo.snapshot();
    let count = snapshot.matched_item_count().min(MAX_MATCHES as u32);
    session.matches = snapshot
        .matched_items(0..count)
        .map(|item| item.matcher_columns[0].to_string())
        .collect();
    session.selected = session
        .selected
        .min(session.matches.len().saturating_sub(1));
}

/// Which match the pointer is over. Rows are one line tall and drawn from
/// `scroll_offset`, so the row is arithmetic rather than a stored hit list.
/// A frame that draws fewer rows than the area is tall leaves blank rows,
/// which belong to no match.
fn row_at(session: &Session, position: Position) -> Option<usize> {
    let row = position.y.checked_sub(session.area.y)? as usize;
    let index = session.scroll_offset + row;
    (index < session.matches.len()).then_some(index)
}

fn move_selection(session: &mut Session, delta: isize) {
    if session.matches.is_empty() {
        return;
    }
    let len = session.matches.len() as isize;
    session.selected = (session.selected as isize + delta).rem_euclid(len) as usize;
}

#[cfg(test)]
mod tests {
    use super::*;
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
                nucleo: Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1),
                matches: rows.iter().map(|row| (*row).to_owned()).collect(),
                selected: 0,
                scroll_offset: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                done_rx: Some(done_rx),
                backend: None,
                remote_resources: HashMap::new(),
                walk: Walk::Listed,
                query: String::new(),
                area,
                pressed: None,
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
        popup.session.as_ref().expect(NO_SESSION).selected
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
        let found = trigger_at(text, cursor);
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
