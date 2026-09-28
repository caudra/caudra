//! The `/sessions` picker: every session in this directory, live or stored,
//! then those of the repository's other checkouts, one section each.
//!
//! Live rows come from the event loop, which is the only thing that can see
//! sibling sessions; stored ones are read off disk when the picker opens. Row
//! order is frozen for the picker's lifetime, so a background agent finishing
//! a turn never moves a row out from under the cursor.

use std::collections::HashMap;
use std::path::PathBuf;

use caudra_grab::grab_scope;
use caudra_storage::id::CaudraId;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use super::status_bar::collapse_home;
use super::{Hint, Overlay};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;

const TITLE: &str = " Sessions ";
const MAX_VISIBLE: u16 = 15;
const WIDTH_PERCENT: u16 = 95;
const EMPTY_TEXT: &str = "No sessions yet. Press Ctrl+N to start one.";
const CURRENT_LABEL: &str = "current";
const DELETE_FOCUSED_HINT: &str = "Cannot delete the current session";
const RENAME_TITLE: &str = " Rename session ";
const UNTITLED: &str = "(untitled)";
const DETACHED_LABEL: &str = "detached";
const SECTION_SEPARATOR: &str = " · ";

/// Largest unit first, so an age reads as the coarsest one that fits.
const AGE_UNITS: &[(u64, &str)] = &[
    (31_536_000, "y"),
    (2_592_000, "mo"),
    (604_800, "w"),
    (86_400, "d"),
    (3_600, "h"),
    (60, "m"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActivity {
    Working,
    NeedsInput,
    Idle,
}

/// A session as the picker shows it. Live sessions carry an activity; stored
/// ones have none, which is what tells the two apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: CaudraId,
    pub title: String,
    pub updated_at: u64,
    pub activity: Option<SessionActivity>,
    pub focused: bool,
    /// Set for a session in another checkout of this repository.
    pub checkout: Option<OtherCheckout>,
}

/// Where a session in another checkout of this repository works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherCheckout {
    pub root: PathBuf,
    /// `None` when the checkout's HEAD is detached.
    pub branch: Option<String>,
    pub cwd: PathBuf,
}

pub enum SessionPickerAction {
    Consumed,
    Focus(CaudraId),
    /// Open a session that works in another checkout.
    FocusElsewhere {
        id: CaudraId,
        cwd: PathBuf,
    },
    Delete(CaudraId),
    Rename {
        id: CaudraId,
        title: String,
    },
    Generate(CaudraId),
    New,
    MoveCurrent,
    MigrateDirectory,
    Closed,
}

struct SessionItem {
    id: CaudraId,
    title: String,
    detail: String,
    focused: bool,
    working: bool,
    /// The checkout's section header and the directory the session works in,
    /// for a session in another checkout.
    elsewhere: Option<(String, PathBuf)>,
}

impl PickerItem for SessionItem {
    fn label(&self) -> &str {
        &self.title
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn section(&self) -> Option<&str> {
        self.elsewhere.as_ref().map(|(section, _)| section.as_str())
    }

    fn is_spinning(&self) -> bool {
        self.working
    }

    fn is_highlighted(&self) -> bool {
        self.focused
    }
}

pub struct SessionPicker {
    picker: ListPicker<SessionItem>,
    /// Row order, frozen when a session is first seen. A session that shows up
    /// while the picker is open enters above the ones already on screen, so
    /// nothing the user is looking at moves.
    rank: HashMap<CaudraId, i64>,
    next_rank: i64,
    /// `Some` while the rename box is up. Renaming borrows the whole picker
    /// rather than opening a second overlay, so `Esc` cannot leave a form
    /// stranded over a list that has moved on.
    rename: Option<(CaudraId, TextBuffer)>,
    pending_delete: Option<CaudraId>,
    now: u64,
}

impl SessionPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_max_visible(MAX_VISIBLE)
            .with_width_percent(WIDTH_PERCENT)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            rank: HashMap::new(),
            next_rank: 0,
            rename: None,
            pending_delete: None,
            now: 0,
        }
    }

    pub fn open(&mut self, rows: Vec<SessionRow>, now: u64) {
        self.rank.clear();
        self.next_rank = 0;
        self.rename = None;
        self.pending_delete = None;
        self.now = now;
        self.picker.set_info_text(Some(editing_hints()));
        let items = self.rank_and_build(rows);
        self.picker.open(items, TITLE);
    }

    /// Refreshes the rows behind an open picker without disturbing the order,
    /// the cursor, or the query the user is typing.
    pub fn refresh(&mut self, rows: Vec<SessionRow>, now: u64) {
        if !self.picker.is_open() {
            return;
        }
        self.now = now;
        let selected = self.selected_id();
        let items = self.rank_and_build(rows);
        self.picker.replace_items(items);
        if let Some(id) = selected {
            self.picker.select_item_by(|item| item.id == id);
        }
    }

    /// This directory's rows keep their frozen rank; other checkouts' follow
    /// in the order given, which already groups them by checkout.
    fn rank_and_build(&mut self, rows: Vec<SessionRow>) -> Vec<SessionItem> {
        let (mut here, elsewhere): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|row| row.checkout.is_none());
        // Anything unseen is ranked as a batch, most recent first, and placed
        // above every row already on screen.
        let mut fresh: Vec<&SessionRow> = here
            .iter()
            .filter(|row| !self.rank.contains_key(&row.id))
            .collect();
        fresh.sort_by(|a, b| {
            b.focused
                .cmp(&a.focused)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        let base = self.next_rank - fresh.len() as i64;
        for (offset, row) in fresh.iter().enumerate() {
            self.rank.insert(row.id, base + offset as i64);
        }
        self.next_rank = base;

        here.sort_by_key(|row| self.rank.get(&row.id).copied().unwrap_or(0));
        let now = self.now;
        here.into_iter()
            .chain(elsewhere)
            .map(|row| SessionItem {
                detail: detail(&row, now),
                title: if row.title.is_empty() {
                    UNTITLED.to_owned()
                } else {
                    row.title
                },
                focused: row.focused,
                working: row.activity == Some(SessionActivity::Working),
                id: row.id,
                elsewhere: row
                    .checkout
                    .map(|checkout| (section(&checkout), checkout.cwd)),
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<CaudraId> {
        (0..)
            .map_while(|index| self.picker.item(index))
            .map(|item| item.id)
            .collect()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.rename = None;
        self.pending_delete = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SessionPickerAction {
        if self.rename.is_some() {
            return self.key_renaming(key);
        }
        // `ListPicker` swallows every unbound control key, so each binding has
        // to be claimed before the list sees it.
        if key::DELETE.matches(key) {
            return self.press_delete();
        }
        self.clear_pending();
        if key::MOVE_SESSION.matches(key) {
            self.close();
            return SessionPickerAction::MoveCurrent;
        }
        if key::MIGRATE_SESSIONS.matches(key) {
            self.close();
            return SessionPickerAction::MigrateDirectory;
        }
        if key::NEW_SESSION.matches(key) {
            self.close();
            return SessionPickerAction::New;
        }
        if key::RENAME_SESSION.matches(key) {
            return self.start_rename();
        }
        if key::GENERATE_TITLE.matches(key) {
            return self
                .selected_id()
                .map_or(SessionPickerAction::Consumed, SessionPickerAction::Generate);
        }
        let action = self.picker.handle_key(key);
        self.map_action(action)
    }

    /// A footer click is the key it names and takes the key path whole: a
    /// second click on the delete hint confirms rather than re-arms, and the
    /// rename footer still answers while the list itself is deaf.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> SessionPickerAction {
        if self.rename.is_some() {
            return match self.picker.handle_footer_mouse(event) {
                Some(key) => self.handle_key(key),
                None => SessionPickerAction::Consumed,
            };
        }
        let action = self.picker.handle_mouse(event);
        if !matches!(action, PickerAction::Consumed | PickerAction::Key(_)) {
            self.clear_pending();
        }
        self.map_action(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match &mut self.rename {
            Some((_, buffer)) => {
                buffer.insert_text(text);
                true
            }
            None => self.picker.handle_paste(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("session_picker", area);
        self.picker.view(frame, area)
    }

    fn selected_id(&self) -> Option<CaudraId> {
        self.picker.selected_item().map(|item| item.id)
    }

    #[cfg(test)]
    pub fn selected_index(&self) -> Option<usize> {
        self.picker.selected_index()
    }

    fn start_rename(&mut self) -> SessionPickerAction {
        let Some(item) = self.picker.selected_item() else {
            return SessionPickerAction::Consumed;
        };
        let mut buffer = TextBuffer::new(item.title.clone());
        let end = buffer.value().chars().count();
        buffer.set_cursor_offset(end);
        self.rename = Some((item.id, buffer));
        self.picker.set_title(RENAME_TITLE);
        self.picker.set_footer_builder(rename_footer);
        self.sync_rename_text();
        SessionPickerAction::Consumed
    }

    /// The rename box is the picker's own search line, so what the user types
    /// is echoed there rather than in a second widget. The caret is mirrored
    /// too, because `set_search_text` parks it at the end of the line.
    fn sync_rename_text(&mut self) {
        let Some((_, buffer)) = &self.rename else {
            return;
        };
        let (text, cursor) = (buffer.value(), buffer.cursor_offset());
        self.picker.set_search_text(&text);
        self.picker.set_search_cursor(cursor);
    }

    fn key_renaming(&mut self, key: KeyEvent) -> SessionPickerAction {
        let Some((id, buffer)) = &mut self.rename else {
            return SessionPickerAction::Consumed;
        };
        match key.code {
            KeyCode::Enter => {
                let title = buffer.value().trim().to_owned();
                let id = *id;
                self.end_rename();
                if title.is_empty() {
                    return SessionPickerAction::Consumed;
                }
                SessionPickerAction::Rename { id, title }
            }
            KeyCode::Esc => {
                self.end_rename();
                SessionPickerAction::Consumed
            }
            _ => {
                buffer.handle_key(key);
                self.sync_rename_text();
                SessionPickerAction::Consumed
            }
        }
    }

    fn end_rename(&mut self) {
        self.rename = None;
        self.picker.set_title(TITLE);
        self.picker.set_footer_builder(footer);
        self.picker.clear_search();
    }

    fn press_delete(&mut self) -> SessionPickerAction {
        let Some(item) = self.picker.selected_item() else {
            return SessionPickerAction::Consumed;
        };
        if item.focused {
            self.picker.set_info_text(Some(DELETE_FOCUSED_HINT.into()));
            return SessionPickerAction::Consumed;
        }
        let id = item.id;
        if self.pending_delete == Some(id) {
            self.clear_pending();
            return SessionPickerAction::Delete(id);
        }
        self.pending_delete = Some(id);
        self.picker.set_info_text(Some(confirm_hint()));
        SessionPickerAction::Consumed
    }

    /// Moving off the armed row cancels the delete, so a stray `Ctrl+D` later
    /// cannot drop a session the user is no longer looking at.
    fn clear_pending(&mut self) {
        if self.pending_delete.take().is_some() {
            self.picker.set_info_text(Some(editing_hints()));
        }
    }

    fn map_action(&mut self, action: PickerAction<SessionItem>) -> SessionPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => SessionPickerAction::Consumed,
            PickerAction::Select(item) => {
                self.close();
                match item.elsewhere {
                    Some((_, cwd)) => SessionPickerAction::FocusElsewhere { id: item.id, cwd },
                    None => SessionPickerAction::Focus(item.id),
                }
            }
            PickerAction::Close => {
                self.close();
                SessionPickerAction::Closed
            }
            PickerAction::Key(key) => self.handle_key(key),
        }
    }
}

impl Overlay for SessionPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

fn footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "open"),
        Hint::bind(key::MOVE_SESSION, "Move current session"),
        Hint::bind(key::MIGRATE_SESSIONS, "Migrate directory sessions"),
    ]
}

fn editing_hints() -> String {
    format!(
        "{} new · {} rename · {} name it · {} delete",
        key::NEW_SESSION.label,
        key::RENAME_SESSION.label,
        key::GENERATE_TITLE.label,
        key::DELETE.label,
    )
}

fn rename_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "save"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn confirm_hint() -> String {
    format!("Press {} again to delete this session.", key::DELETE.label)
}

/// Names another checkout by its branch and where it lives.
fn section(checkout: &OtherCheckout) -> String {
    format!(
        "{}{SECTION_SEPARATOR}{}",
        checkout.branch.as_deref().unwrap_or(DETACHED_LABEL),
        collapse_home(&checkout.root.to_string_lossy())
    )
}

/// The current session says so; every other row is described by how long ago
/// it was touched, which is what tells two similar titles apart.
fn detail(row: &SessionRow, now: u64) -> String {
    if row.focused {
        return CURRENT_LABEL.to_owned();
    }
    match row.activity {
        Some(SessionActivity::NeedsInput) => "needs input".to_owned(),
        Some(SessionActivity::Working) => "working".to_owned(),
        _ => age(now.saturating_sub(row.updated_at)),
    }
}

fn age(seconds: u64) -> String {
    AGE_UNITS
        .iter()
        .find(|(size, _)| seconds >= *size)
        .map_or_else(
            || format!("{seconds}s"),
            |(size, unit)| format!("{}{unit}", seconds / size),
        )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::components::key as key_event;
    use test_case::test_case;

    const FIRST: &str = "CNK1hV6GWoysH3KQMm5wv";
    const SECOND: &str = "CNK1hV6GWoysH3KQMm5ww";
    const THIRD: &str = "CNK1hV6GWoysH3KQMm5wx";
    const NOW: u64 = 1_000_000;
    const TITLE_A: &str = "refactor the parser";
    const TITLE_B: &str = "fix the tests";
    const OTHER_ROOT: &str = "/work/app-login";
    const OTHER_BRANCH: &str = "feature/login";
    const OTHER_SECTION: &str = "feature/login · /work/app-login";

    fn char_key(c: char) -> KeyEvent {
        key_event(KeyCode::Char(c))
    }

    fn id(raw: &str) -> CaudraId {
        raw.parse().expect("a valid session id")
    }

    fn row(raw: &str, title: &str, ago: u64, activity: Option<SessionActivity>) -> SessionRow {
        SessionRow {
            id: id(raw),
            title: title.into(),
            updated_at: NOW - ago,
            activity,
            focused: false,
            checkout: None,
        }
    }

    fn elsewhere(raw: &str, title: &str, ago: u64) -> SessionRow {
        SessionRow {
            checkout: Some(OtherCheckout {
                root: PathBuf::from(OTHER_ROOT),
                branch: Some(OTHER_BRANCH.into()),
                cwd: PathBuf::from(OTHER_ROOT),
            }),
            ..row(raw, title, ago, None)
        }
    }

    fn opened(rows: Vec<SessionRow>) -> SessionPicker {
        let mut picker = SessionPicker::new();
        picker.open(rows, NOW);
        picker
    }

    #[test_case(false; "move_current")]
    #[test_case(true; "migrate_directory")]
    fn relocation_actions_do_not_target_the_selected_session(bulk: bool) {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 0, None)]);
        let binding = if bulk {
            key::MIGRATE_SESSIONS
        } else {
            key::MOVE_SESSION
        };
        let action = picker.handle_key(binding.to_key_event());
        assert!(matches!(
            (bulk, action),
            (false, SessionPickerAction::MoveCurrent)
                | (true, SessionPickerAction::MigrateDirectory)
        ));
        assert!(!picker.is_open());
    }

    #[test]
    fn the_current_session_sorts_first_and_says_so() {
        let mut current = row(SECOND, TITLE_B, 500, Some(SessionActivity::Idle));
        current.focused = true;
        let picker = opened(vec![row(FIRST, TITLE_A, 10, None), current]);
        assert_eq!(picker.ids(), [id(SECOND), id(FIRST)]);
        assert_eq!(picker.picker.item(0).unwrap().detail(), Some(CURRENT_LABEL));
    }

    #[test]
    fn the_rest_sort_by_how_recently_they_were_touched() {
        let picker = opened(vec![
            row(FIRST, TITLE_A, 5_000, None),
            row(SECOND, TITLE_B, 10, None),
        ]);
        assert_eq!(picker.ids(), [id(SECOND), id(FIRST)]);
    }

    #[test]
    fn a_session_that_appears_while_the_picker_is_open_does_not_move_the_others() {
        let mut picker = opened(vec![
            row(FIRST, TITLE_A, 5_000, None),
            row(SECOND, TITLE_B, 10, None),
        ]);
        let before = picker.ids();
        picker.refresh(
            vec![
                row(FIRST, TITLE_A, 5_000, None),
                row(SECOND, TITLE_B, 10, None),
                row(THIRD, "new arrival", 0, Some(SessionActivity::Working)),
            ],
            NOW,
        );
        assert_eq!(
            picker.ids(),
            [id(THIRD)].into_iter().chain(before).collect::<Vec<_>>(),
            "the newcomer enters above rows already on screen"
        );
    }

    #[test]
    fn a_refresh_keeps_the_cursor_on_its_session() {
        let mut picker = opened(vec![
            row(FIRST, TITLE_A, 5_000, None),
            row(SECOND, TITLE_B, 10, None),
        ]);
        picker.handle_key(key_event(KeyCode::Down));
        assert_eq!(picker.selected_id(), Some(id(FIRST)));
        picker.refresh(
            vec![
                row(FIRST, TITLE_A, 5_000, Some(SessionActivity::Working)),
                row(SECOND, TITLE_B, 10, None),
            ],
            NOW,
        );
        assert_eq!(picker.selected_id(), Some(id(FIRST)));
    }

    #[test]
    fn other_checkouts_follow_this_directory_under_their_own_section() {
        let picker = opened(vec![
            elsewhere(FIRST, TITLE_A, 0),
            row(SECOND, TITLE_B, 5_000, None),
        ]);

        assert_eq!(picker.ids(), [id(SECOND), id(FIRST)]);
        assert_eq!(picker.picker.item(0).unwrap().section(), None);
        assert_eq!(
            picker.picker.item(1).unwrap().section(),
            Some(OTHER_SECTION)
        );
    }

    #[test]
    fn opening_another_checkouts_session_names_where_it_works() {
        let mut picker = opened(vec![elsewhere(FIRST, TITLE_A, 0)]);

        let action = picker.handle_key(key_event(KeyCode::Enter));

        assert!(matches!(
            action,
            SessionPickerAction::FocusElsewhere { id: got, cwd }
                if got == id(FIRST) && cwd == Path::new(OTHER_ROOT)
        ));
    }

    #[test]
    fn only_a_working_session_spins() {
        let picker = opened(vec![
            row(FIRST, TITLE_A, 10, Some(SessionActivity::Working)),
            row(SECOND, TITLE_B, 20, Some(SessionActivity::Idle)),
            row(THIRD, "stored", 30, None),
        ]);
        let spinning: Vec<bool> = (0..3)
            .map(|index| picker.picker.item(index).unwrap().is_spinning())
            .collect();
        assert_eq!(spinning, [true, false, false]);
    }

    #[test]
    fn deleting_takes_two_presses() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            SessionPickerAction::Consumed
        ));
        assert!(matches!(
            picker.handle_key(key::DELETE.to_key_event()),
            SessionPickerAction::Delete(_)
        ));
    }

    #[test]
    fn moving_off_the_armed_row_cancels_the_delete() {
        let mut picker = opened(vec![
            row(FIRST, TITLE_A, 5_000, None),
            row(SECOND, TITLE_B, 10, None),
        ]);
        picker.handle_key(key::DELETE.to_key_event());
        picker.handle_key(key_event(KeyCode::Down));
        assert!(
            matches!(
                picker.handle_key(key::DELETE.to_key_event()),
                SessionPickerAction::Consumed
            ),
            "the confirmation does not carry to another row"
        );
    }

    #[test]
    fn the_current_session_cannot_be_deleted() {
        let mut current = row(FIRST, TITLE_A, 10, Some(SessionActivity::Idle));
        current.focused = true;
        let mut picker = opened(vec![current]);
        picker.handle_key(key::DELETE.to_key_event());
        assert!(
            matches!(
                picker.handle_key(key::DELETE.to_key_event()),
                SessionPickerAction::Consumed
            ),
            "no amount of pressing deletes the session you are in"
        );
    }

    #[test]
    fn renaming_starts_from_the_existing_title_and_saves_the_edit() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        assert!(picker.rename.is_some());
        picker.handle_key(char_key('!'));
        let action = picker.handle_key(key_event(KeyCode::Enter));
        assert!(
            matches!(action, SessionPickerAction::Rename { id: got, title }
                if got == id(FIRST) && title == format!("{TITLE_A}!")),
        );
        assert!(picker.rename.is_none(), "the box closes on save");
    }

    #[test]
    fn escaping_a_rename_leaves_the_title_alone() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        picker.handle_key(char_key('x'));
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Esc)),
            SessionPickerAction::Consumed
        ));
        assert!(picker.rename.is_none());
        assert!(picker.is_open(), "cancelling a rename keeps the list up");
    }

    #[test]
    fn an_empty_rename_is_not_saved() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        for _ in 0..TITLE_A.len() {
            picker.handle_key(key_event(KeyCode::Backspace));
        }
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            SessionPickerAction::Consumed
        ));
    }

    /// The rename box delegates to the shared editing keymap, and the echo in
    /// the picker's search line has to follow the caret, not park at the end.
    #[test]
    fn ctrl_w_deletes_a_word_of_the_rename_and_moves_the_echoed_caret() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        picker.handle_key(key::DELETE_WORD.to_key_event());

        let kept = TITLE_A.rsplit_once(' ').expect("a multi-word title").0;
        assert_eq!(picker.picker.search_text(), format!("{kept} "));
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Enter)),
            SessionPickerAction::Rename { title, .. } if title == kept
        ));
    }

    #[test]
    fn naming_a_session_asks_for_the_selected_row() {
        let mut picker = opened(vec![row(FIRST, TITLE_A, 10, None)]);

        let action = picker.handle_key(key::GENERATE_TITLE.to_key_event());

        assert!(matches!(action, SessionPickerAction::Generate(got) if got == id(FIRST)));
        assert!(
            picker.is_open(),
            "the list stays up while the model answers"
        );
    }

    #[test]
    fn naming_nothing_asks_for_nothing() {
        let mut picker = opened(Vec::new());

        assert!(matches!(
            picker.handle_key(key::GENERATE_TITLE.to_key_event()),
            SessionPickerAction::Consumed
        ));
    }

    #[test]
    fn an_untitled_session_still_has_something_to_match_on() {
        let picker = opened(vec![row(FIRST, "", 10, None)]);
        assert_eq!(picker.picker.item(0).unwrap().label(), UNTITLED);
    }

    #[test_case(0, "0s"; "just_now")]
    #[test_case(59, "59s"; "under_a_minute")]
    #[test_case(60, "1m"; "one_minute")]
    #[test_case(3_600, "1h"; "one_hour")]
    #[test_case(86_400, "1d"; "one_day")]
    #[test_case(604_800, "1w"; "one_week")]
    #[test_case(2_592_000, "1mo"; "one_month")]
    #[test_case(31_536_000, "1y"; "one_year")]
    fn an_age_reads_as_the_coarsest_unit_that_fits(seconds: u64, expected: &str) {
        assert_eq!(age(seconds), expected);
    }
}
