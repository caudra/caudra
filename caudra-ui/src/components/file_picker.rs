use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tracing::warn;
use unicode_width::UnicodeWidthChar;

use crate::animation::spinner_frame;
use crate::components::Overlay;
use crate::components::file_walk::{self, Walk};
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::repaint::{Cadence, Dirty};
use crate::text_buffer::{EditResult, TextBuffer};
use crate::theme;

const TITLE: &str = " Files ";
const TITLE_WALKING: &str = " Files (scanning…) ";
const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 80;
const SEARCH_ROW: u16 = 1;
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
/// Not "empty": a directory full of ignored files walks up just as short.
const WALKER_CRASHED_MSG: &str = "File scanner crashed";
const PENDING_DEBOUNCE_MS: u128 = 100;
const MAX_MATERIALIZED: u32 = 640;

/// The walker answers once, and its answer only matters when the list came up
/// empty: an empty directory, a fully ignored one and one we could not open
/// look identical from the injector's side.
pub enum FilePickerModalAction {
    Consumed,
    Select(String),
    Close,
}

struct Match {
    path: String,
    indices: Vec<u32>,
}

#[derive(Clone, Copy)]
struct FileRowHit {
    area: Rect,
    match_index: usize,
}

struct Session {
    nucleo: Nucleo<()>,
    matcher: Matcher,
    matches: Vec<Match>,
    total_matches: u32,

    search: TextBuffer,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    popup_area: Rect,
    row_hits: Vec<FileRowHit>,
    mouse_down: Option<String>,

    cancel: Arc<AtomicBool>,
    done_rx: flume::Receiver<Walk>,
    started_at: Instant,

    walk: Walk,
    /// The matcher owes an answer. Nothing delivers it, so `tick` has to look.
    matching: bool,
    visible: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct FilePickerModal {
    session: Option<Session>,
}

impl FilePickerModal {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn open(&mut self, cwd: &str) {
        self.close();

        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let Some(done_rx) =
            file_walk::spawn(PathBuf::from(cwd), nucleo.injector(), Arc::clone(&cancel))
        else {
            return;
        };

        self.session = Some(Session {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            search: TextBuffer::new(String::new()),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            popup_area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            cancel,
            done_rx,
            started_at: Instant::now(),
            walk: Walk::Running,
            matching: false,
            visible: false,
        });
    }

    pub fn close(&mut self) {
        self.session = None;
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// The whole popup, border included: the frame is part of the picker, so a
    /// press on it is a near miss rather than a press outside.
    pub fn contains(&self, pos: Position) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.visible && s.popup_area.contains(pos))
    }

    /// The walk is debounced onto the screen, so the picker can be open with
    /// nothing drawn for it yet.
    pub fn is_drawn(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.visible)
    }

    pub fn scroll(&mut self, delta: i32) {
        let Some(s) = &mut self.session else { return };
        invalidate_mouse_geometry(s);
        if delta > 0 {
            move_selection(s, -(delta as isize));
        } else {
            move_selection(s, delta.unsigned_abs() as isize);
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Some(s) = &mut self.session else {
            return false;
        };
        s.search.insert_text(text);
        reparse_pattern(s);
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FilePickerModalAction {
        let Some(s) = &mut self.session else {
            return FilePickerModalAction::Close;
        };

        match key.code {
            KeyCode::Esc => return FilePickerModalAction::Close,
            KeyCode::Enter => {
                if !s.visible {
                    return FilePickerModalAction::Consumed;
                }
                if let Some(m) = s.matches.get(s.selected) {
                    return FilePickerModalAction::Select(m.path.clone());
                }
                return FilePickerModalAction::Close;
            }
            KeyCode::Up => move_selection(s, -1),
            KeyCode::Down => move_selection(s, 1),
            _ if key::SCROLL_HALF_UP.matches(key) => {
                move_selection(s, -((s.viewport_height / 2).max(1) as isize))
            }
            _ if key::PAGE_DOWN.matches(key) => {
                move_selection(s, (s.viewport_height / 2).max(1) as isize)
            }
            _ if key::SCROLL_LINE_UP.matches(key) => move_selection(s, -1),
            _ if key::SCROLL_LINE_DOWN.matches(key) => move_selection(s, 1),
            // Everything the list itself does not claim edits the search
            // line, which owns the whole editing keymap.
            _ => {
                if s.search.handle_key(key) == EditResult::Changed {
                    reparse_pattern(s);
                }
            }
        }
        FilePickerModalAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> FilePickerModalAction {
        let Some(s) = &mut self.session else {
            return FilePickerModalAction::Close;
        };
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                s.mouse_down = None;
                if let Some(hit) = s
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .copied()
                {
                    s.selected = hit.match_index;
                    s.mouse_down = s.matches.get(hit.match_index).map(|m| m.path.clone());
                }
                FilePickerModalAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                s.mouse_down = None;
                FilePickerModalAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(hit) = s.row_hits.iter().find(|hit| hit.area.contains(position)) {
                    s.selected = hit.match_index;
                }
                FilePickerModalAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed_path) = s.mouse_down.take() else {
                    return FilePickerModalAction::Consumed;
                };
                let released = s
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .and_then(|hit| s.matches.get(hit.match_index));
                match released {
                    Some(m) if m.path == pressed_path => {
                        FilePickerModalAction::Select(m.path.clone())
                    }
                    _ => FilePickerModalAction::Consumed,
                }
            }
            _ => FilePickerModalAction::Consumed,
        }
    }

    pub fn cadence(&self) -> Cadence {
        let Some(s) = self.session.as_ref() else {
            return Cadence::IDLE;
        };
        Cadence::any([
            Cadence::when(s.visible && s.walk == Walk::Running, Cadence::SPINNER),
            // Results stream in all through the walk, and the spinner above is
            // already bringing the loop back for them. Once it ends, every
            // keystroke leaves one last answer in flight, and the list sits on
            // the old query until someone looks.
            Cadence::when(s.matching && s.walk != Walk::Running, Cadence::PENDING),
        ])
    }

    /// Returns the frame owed plus a message to flash if the picker gave up.
    pub fn tick(&mut self) -> (Dirty, Option<String>) {
        let Some(s) = self.session.as_mut() else {
            return (Dirty::NO, None);
        };

        let status = s.nucleo.tick(0);
        s.matching = status.running;
        // The title says "scanning…" while walking, so finishing redraws too.
        let mut dirty = Dirty::from(status.changed);

        if s.walk == Walk::Running {
            match s.done_rx.try_recv() {
                Ok(end) => {
                    s.walk = end;
                    dirty = Dirty::YES;
                }
                Err(flume::TryRecvError::Disconnected) => {
                    warn!("{WALKER_CRASHED_MSG}: walker thread panicked");
                    self.session = None;
                    return (Dirty::YES, Some(WALKER_CRASHED_MSG.into()));
                }
                Err(flume::TryRecvError::Empty) => {}
            }
        }

        let has_files = s.nucleo.injector().injected_items() > 0;

        // A walk slow enough to cross the debounce is already on screen when it
        // answers, so the close cannot sit behind the visibility gate below.
        if !has_files && let Some(msg) = s.walk.nothing_found_msg() {
            self.session = None;
            return (Dirty::YES, Some(msg.into()));
        }

        if !s.visible && (has_files || s.started_at.elapsed().as_millis() >= PENDING_DEBOUNCE_MS) {
            s.visible = true;
            dirty = Dirty::YES;
        }

        if status.changed {
            refresh_matches(s);
            clamp_selection(s);
        }

        (dirty, None)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let s = match &mut self.session {
            Some(s) if s.visible => s,
            _ => return Rect::default(),
        };

        let match_count = s.matches.len() as u16;
        let title = if s.walk == Walk::Running {
            TITLE_WALKING
        } else {
            TITLE
        };

        let has_query_without_matches = s.matches.is_empty() && !s.search.value().is_empty();
        let max_visible = area.height.saturating_sub(SEARCH_ROW + 2);
        let content_rows = if has_query_without_matches {
            1
        } else {
            match_count.min(max_visible)
        };

        let modal = Modal {
            title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, content_rows + SEARCH_ROW);
        s.popup_area = popup;
        s.viewport_height = inner.height.saturating_sub(SEARCH_ROW) as usize;
        ensure_visible(s);

        let [list_area, search_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        s.row_hits.clear();
        render_list(frame, list_area, s);
        render_search(frame, search_area, s);

        if match_count > s.viewport_height as u16 {
            render_vertical_scrollbar(frame, list_area, match_count, s.scroll_offset as u16);
        }

        popup
    }
}

impl Overlay for FilePickerModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

fn reparse_pattern(s: &mut Session) {
    invalidate_mouse_geometry(s);
    let query = s.search.value();
    s.nucleo
        .pattern
        .reparse(0, &query, CaseMatching::Smart, Normalization::Smart, false);
    s.selected = 0;
    s.scroll_offset = 0;
}

fn refresh_matches(s: &mut Session) {
    invalidate_mouse_geometry(s);
    let snapshot = s.nucleo.snapshot();
    s.total_matches = snapshot.matched_item_count();
    let count = s.total_matches.min(MAX_MATERIALIZED);

    s.matches.clear();

    let pattern = snapshot.pattern();
    let has_pattern = !pattern.column_pattern(0).atoms.is_empty();
    let mut indices_buf = Vec::new();

    for item in snapshot.matched_items(0..count) {
        let col = &item.matcher_columns[0];
        let path = col.to_string();

        let indices = if has_pattern {
            indices_buf.clear();
            pattern
                .column_pattern(0)
                .indices(col.slice(..), &mut s.matcher, &mut indices_buf);
            mem::take(&mut indices_buf)
        } else {
            Vec::new()
        };

        s.matches.push(Match { path, indices });
    }
}

fn move_selection(s: &mut Session, delta: isize) {
    if s.matches.is_empty() {
        return;
    }
    let new = (s.selected as isize + delta).clamp(0, s.matches.len() as isize - 1);
    s.selected = new as usize;
    ensure_visible(s);
}

fn clamp_selection(s: &mut Session) {
    if s.matches.is_empty() {
        s.selected = 0;
        s.scroll_offset = 0;
    } else {
        s.selected = s.selected.min(s.matches.len() - 1);
        ensure_visible(s);
    }
}

fn ensure_visible(s: &mut Session) {
    let previous_offset = s.scroll_offset;
    let len = s.matches.len();
    if len > s.viewport_height {
        s.scroll_offset = s.scroll_offset.min(len - s.viewport_height);
    } else {
        s.scroll_offset = 0;
    }

    if s.selected < s.scroll_offset {
        s.scroll_offset = s.selected;
    } else if s.selected >= s.scroll_offset + s.viewport_height {
        s.scroll_offset = s.selected + 1 - s.viewport_height;
    }
    if s.scroll_offset != previous_offset {
        invalidate_mouse_geometry(s);
    }
}

fn invalidate_mouse_geometry(s: &mut Session) {
    s.row_hits.clear();
    s.mouse_down = None;
}

fn render_list(frame: &mut Frame, area: Rect, s: &mut Session) {
    let t = theme::current();

    if s.matches.is_empty() {
        if !s.search.value().is_empty() {
            frame.render_widget(
                Paragraph::new(vec![Line::from(Span::styled(NO_MATCHES, t.item_desc))]),
                area,
            );
        }
        return;
    }

    let more = s.total_matches > MAX_MATERIALIZED;
    let at_bottom = s.scroll_offset + s.viewport_height >= s.matches.len();
    let hint_row = usize::from(more && at_bottom);
    // A viewport one row tall spends it all on the hint, and `end` below would
    // then slice backwards from `scroll_offset`.
    let visible_rows = s.viewport_height.saturating_sub(hint_row);

    let max_label_width = area.width.saturating_sub(LABEL_INDENT.len() as u16) as usize;
    let end = (s.scroll_offset + visible_rows).min(s.matches.len());

    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows + hint_row);
    for (i, m) in s.matches[s.scroll_offset..end].iter().enumerate() {
        let match_index = s.scroll_offset + i;
        s.row_hits.push(FileRowHit {
            area: Rect::new(area.x, area.y + i as u16, area.width, 1),
            match_index,
        });
        lines.push(build_highlighted_line(
            &m.path,
            &m.indices,
            max_label_width,
            match_index == s.selected,
            &t,
        ));
    }

    if hint_row > 0 {
        let n = s.total_matches - MAX_MATERIALIZED;
        lines.push(Line::from(Span::styled(
            format!("{LABEL_INDENT}+{n} more files (not shown)"),
            t.item_desc,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_search(frame: &mut Frame, area: Rect, s: &Session) {
    let t = theme::current();
    let query = s.search.value();
    let cursor_byte = TextBuffer::char_to_byte(&query, s.search.x());
    let (before, rest) = query.split_at(cursor_byte);
    let mut chars = rest.chars();
    let cursor_char = chars.next().unwrap_or(' ');
    let after = chars.as_str();

    let mut spans = vec![super::chevron_span()];

    if s.walk == Walk::Running {
        let ch = spinner_frame(s.started_at.elapsed().as_millis());
        spans.push(Span::styled(format!("{ch} "), t.item_desc));
    }

    let text = super::input_text_style();
    spans.extend([
        Span::styled(before.to_owned(), text),
        Span::styled(cursor_char.to_string(), t.cursor),
        Span::styled(after.to_owned(), text),
    ]);

    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), area);
}

fn build_highlighted_line<'a>(
    text: &str,
    indices: &[u32],
    max_width: usize,
    selected: bool,
    t: &'a theme::Theme,
) -> Line<'a> {
    let base = if selected { t.item_selected } else { t.item };
    let highlight = base
        .fg(t.accent.fg.unwrap_or(t.foreground))
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(LABEL_INDENT, base)];
    let mut in_match = false;
    let mut run = String::new();
    let mut width = 0usize;

    for (i, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max_width {
            break;
        }
        width += cw;

        let is_match = indices.binary_search(&(i as u32)).is_ok();
        if is_match != in_match && !run.is_empty() {
            spans.push(Span::styled(
                mem::take(&mut run),
                if in_match { highlight } else { base },
            ));
        }
        in_match = is_match;
        run.push(ch);
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, if in_match { highlight } else { base }));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::file_walk::NOTHING_TO_PICK_MSG;
    use crate::components::keybindings::key as kb;
    use crate::repaint::expect::{OWED, QUIET};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use std::time::Duration;
    use tempfile::TempDir;
    use test_case::test_case;

    /// Waits on the matcher are bounded by wall clock, not by a tick budget:
    /// nucleo matches on a worker thread, and a tight loop can burn through N
    /// ticks before that thread is ever scheduled.
    const CONVERGE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Far enough from `PENDING_DEBOUNCE_MS` that no scheduling delay can
    /// cross it in either direction.
    const DEBOUNCE_HELD_OFF: Duration = Duration::from_secs(60);
    const NEVER_CONVERGED: &str = "picker never rebuilt its matches from later ticks";
    const NEVER_CLOSED: &str = "picker never closed on an empty walk";

    const MAIN_PATH: &str = "src/main.rs";
    const README_PATH: &str = "docs/readme.md";
    const README_QUERY: &str = "readme";
    const MAIN_FILE: &str = "main.rs";

    /// Ticks until `ready` holds, collecting the frames owed on the way, or
    /// `None` if the picker never got there.
    fn tick_until(picker: &mut FilePickerModal, ready: impl Fn(&Session) -> bool) -> Option<Dirty> {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let mut dirty = Dirty::NO;
        while Instant::now() < deadline {
            let (owed, _) = picker.tick();
            dirty |= owed;
            if picker.session.as_ref().is_some_and(&ready) {
                return Some(dirty);
            }
            std::thread::yield_now();
        }
        None
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render(picker: &mut FilePickerModal) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
    }

    fn pending_picker() -> (FilePickerModal, flume::Sender<Walk>) {
        let mut picker = FilePickerModal::new();
        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let (done_tx, done_rx) = flume::bounded(1);
        picker.session = Some(Session {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            search: TextBuffer::new(String::new()),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            popup_area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            cancel: Arc::new(AtomicBool::new(false)),
            done_rx,
            started_at: Instant::now(),
            walk: Walk::Running,
            matching: false,
            visible: false,
        });
        (picker, done_tx)
    }

    fn inject_file(picker: &FilePickerModal, path: &str) {
        let s = picker.session.as_ref().unwrap();
        s.nucleo.injector().push((), |_, cols| {
            cols[0] = nucleo::Utf32String::from(path);
        });
    }

    /// Files are counted straight off the injector, so one tick settles the
    /// question. `started_at` in the future keeps the debounce out of it,
    /// however long the test is descheduled for.
    fn tick_once_before_the_debounce(picker: &mut FilePickerModal) {
        picker.session.as_mut().unwrap().started_at = Instant::now() + DEBOUNCE_HELD_OFF;
        let _ = picker.tick();
    }

    /// Nothing else in the app knows the walk is running, so the picker is the
    /// one that has to claim the spinner. `view` draws nothing until files
    /// arrive, so a hidden walk claiming `SPINNER` would animate pixels that
    /// are not on screen.
    #[test_case(&[MAIN_PATH] => Cadence::SPINNER ; "on_screen_walk_spins")]
    #[test_case(&[]          => Cadence::IDLE    ; "hidden_walk_does_not")]
    fn walking_picker_spins_only_once_it_is_on_screen(files: &[&str]) -> Cadence {
        let (mut picker, _done_tx) = pending_picker();
        for path in files {
            inject_file(&picker, path);
        }
        tick_once_before_the_debounce(&mut picker);

        let s = picker.session.as_ref().unwrap();
        assert_eq!(s.walk, Walk::Running);
        assert_eq!(
            s.visible,
            !files.is_empty(),
            "the picker shows itself exactly when it has something"
        );
        picker.cadence()
    }

    /// Neither ending leaves anything to pick, so the picker closes itself and
    /// says why: one frame, one flash, and then quiet, or the loop never
    /// settles again. A walk slow enough to cross the debounce is already on
    /// screen when it comes back empty, and it still has to close, or the user
    /// is left staring at an empty list with no reason for it.
    #[test_case(Some(Walk::Listed), false => NOTHING_TO_PICK_MSG ; "walk_finished_with_nothing")]
    #[test_case(Some(Walk::Listed), true  => NOTHING_TO_PICK_MSG ; "shown_walk_finished_with_nothing")]
    #[test_case(None,               false => WALKER_CRASHED_MSG  ; "walker_died")]
    fn self_close_flashes_once_then_stays_quiet(end: Option<Walk>, on_screen: bool) -> String {
        let (mut picker, done_tx) = pending_picker();
        picker.session.as_mut().unwrap().visible = on_screen;
        match end {
            Some(end) => done_tx.send(end).unwrap(),
            None => drop(done_tx),
        }

        let (dirty, flash) = picker.tick();
        assert!(picker.session.is_none());
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        flash.unwrap()
    }

    /// Depth 0 is the root itself, which strips to an empty name: a bare
    /// separator at the top of the list, selected by default, one Enter away
    /// from picking the user's own directory. It also counts as an injected
    /// item, so every directory used to look non-empty.
    #[test]
    fn a_real_walk_offers_the_files_and_not_the_root_itself() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(MAIN_FILE), "").unwrap();

        let mut picker = FilePickerModal::new();
        picker.open(&tmp.path().to_string_lossy());
        let _ = tick_until(&mut picker, |s| !s.matches.is_empty()).expect(NEVER_CONVERGED);

        let s = picker.session.as_ref().unwrap();
        let paths: Vec<&str> = s.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, [MAIN_FILE]);
    }

    /// An empty directory only reads as empty once the root stops counting as
    /// a find, so this is the close that never used to happen.
    #[test]
    fn a_real_walk_of_an_empty_directory_closes_the_picker() {
        let tmp = TempDir::new().unwrap();
        let mut picker = FilePickerModal::new();
        picker.open(&tmp.path().to_string_lossy());

        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let flash = loop {
            if let (_, Some(flash)) = picker.tick() {
                break flash;
            }
            assert!(Instant::now() < deadline, "{NEVER_CLOSED}");
            std::thread::yield_now();
        };

        assert_eq!(flash, NOTHING_TO_PICK_MSG);
        assert!(!picker.is_open());
    }

    /// A walk with nothing to show yet still opens once it drags on, so the
    /// user is not left staring at an unchanged screen.
    #[test]
    fn pending_debounce_controls_visibility() {
        let (mut picker, _done_tx) = pending_picker();
        tick_once_before_the_debounce(&mut picker);
        assert!(!picker.session.as_ref().unwrap().visible, "hidden so far");

        picker.session.as_mut().unwrap().started_at = Instant::now() - DEBOUNCE_HELD_OFF;
        let _ = picker.tick();
        assert!(
            picker.session.as_ref().unwrap().visible,
            "shown once the walk drags on"
        );
    }

    /// A finished walk with an unchanged query draws the same pixels every
    /// frame, so the loop has to be free to settle.
    #[test]
    fn settled_picker_owes_no_frame_and_does_not_animate() {
        let (mut picker, done_tx) = pending_picker();
        inject_file(&picker, MAIN_PATH);
        done_tx.send(Walk::Listed).unwrap();

        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        while picker.tick() != (Dirty::NO, None) || picker.cadence() != Cadence::IDLE {
            assert!(Instant::now() < deadline, "the picker never stopped");
            std::thread::yield_now();
        }

        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        assert_eq!(picker.cadence(), Cadence::IDLE);
    }

    /// The matcher answers on a worker thread, long after the keypress was
    /// handled, so typing is only redrawn because a later `tick` reports the
    /// change. Without that the list freezes on the previous query.
    #[test]
    fn query_change_owes_a_frame_from_a_later_tick() {
        let (mut picker, done_tx) = pending_picker();
        inject_file(&picker, MAIN_PATH);
        inject_file(&picker, README_PATH);
        done_tx.send(Walk::Listed).unwrap();
        let _ = tick_until(&mut picker, |s| s.matches.len() == 2).expect(NEVER_CONVERGED);

        for c in README_QUERY.chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }

        let dirty = tick_until(&mut picker, |s| s.matches.len() == 1).expect(NEVER_CONVERGED);
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(
            picker.session.as_ref().unwrap().matches[0].path,
            README_PATH
        );
    }

    /// Nucleo matches on a worker thread and hands the answer to nobody, long
    /// after the keystroke that started it. Only looking again finds it, so an
    /// idle cadence here leaves the list on the previous query until some
    /// unrelated poll comes round. It is not motion either: nothing lands, so
    /// there is nothing to paint.
    #[test_case(true,  Walk::Listed  => Cadence::PENDING ; "matching_after_the_walk")]
    #[test_case(true,  Walk::Running => Cadence::SPINNER ; "walk_spinner_already_comes_back")]
    #[test_case(false, Walk::Listed  => Cadence::IDLE    ; "settled")]
    fn a_matcher_mid_answer_keeps_the_loop_coming_back(matching: bool, walk: Walk) -> Cadence {
        let (mut picker, _done_tx) = pending_picker();
        let s = picker.session.as_mut().unwrap();
        s.visible = true;
        s.matching = matching;
        s.walk = walk;

        picker.cadence()
    }

    #[test]
    fn esc_returns_close() {
        let (mut picker, _done_tx) = pending_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            FilePickerModalAction::Close
        ));
    }

    #[test]
    fn typing_during_pending_buffers_query() {
        let (mut picker, _done_tx) = pending_picker();
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "ma");
    }

    #[test]
    fn enter_during_pending_is_consumed() {
        let (mut picker, _done_tx) = pending_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerModalAction::Consumed
        ));
    }

    #[test]
    fn matches_capped_at_max_materialized() {
        let mut picker = picker_with_matches(MAX_MATERIALIZED as usize + 50);
        let s = picker.session.as_mut().unwrap();
        s.total_matches = MAX_MATERIALIZED + 50;
        s.matches.truncate(MAX_MATERIALIZED as usize);
        assert_eq!(s.total_matches, MAX_MATERIALIZED + 50);
        assert_eq!(s.matches.len(), MAX_MATERIALIZED as usize);
    }

    fn picker_with_matches(n: usize) -> FilePickerModal {
        let (mut picker, _done_tx) = pending_picker();
        let s = picker.session.as_mut().unwrap();
        s.walk = Walk::Listed;
        s.visible = true;
        s.matches = (0..n)
            .map(|i| Match {
                path: format!("file_{i:03}.rs"),
                indices: Vec::new(),
            })
            .collect();
        s.total_matches = n as u32;
        picker
    }

    #[test]
    fn resize_clamps_scroll_offset() {
        let mut picker = picker_with_matches(20);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = 19;
        s.scroll_offset = 15;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, 15);

        s.viewport_height = 20;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test_case(&[], 3 ; "empty_indices")]
    #[test_case(&[0, 2], 5 ; "sparse_match")]
    fn build_highlighted_line_no_panic(indices: &[u32], max_width: usize) {
        let t = theme::current();
        let _ = build_highlighted_line("hello", indices, max_width, false, &t);
    }

    #[test]
    fn build_highlighted_line_truncates_at_max_width() {
        let t = theme::current();
        let line = build_highlighted_line("verylongfilename.rs", &[], 5, false, &t);
        let text: String = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "veryl");
    }

    #[test]
    fn build_highlighted_line_unicode_width() {
        let t = theme::current();
        let line = build_highlighted_line("日本語.rs", &[], 6, false, &t);
        let text: String = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "日本語");
    }

    #[test_case(0, -10, 0 ; "clamps_at_start")]
    #[test_case(4, 10, 4 ; "clamps_at_end")]
    #[test_case(2, 1, 3 ; "moves_down")]
    #[test_case(2, -1, 1 ; "moves_up")]
    fn move_selection_behavior(start: usize, delta: isize, expected: usize) {
        let mut picker = picker_with_matches(5);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 10;
        s.selected = start;
        move_selection(s, delta);
        assert_eq!(s.selected, expected);
    }

    #[test]
    fn move_selection_empty_is_noop() {
        let mut picker = picker_with_matches(0);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 10;
        move_selection(s, 5);
        assert_eq!(s.selected, 0);
    }

    #[test_case(0, -3, 3 ; "negative_scrolls_down")]
    #[test_case(5, 2, 3 ; "positive_scrolls_up")]
    fn scroll_updates_selection(start: usize, delta: i32, expected: usize) {
        let mut picker = picker_with_matches(10);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = start;
        picker.scroll(delta);
        assert_eq!(picker.session.as_ref().unwrap().selected, expected);
    }

    #[test]
    fn handle_paste_appends_to_search() {
        let (mut picker, _done_tx) = pending_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        assert!(picker.handle_paste("bc"));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "abc");
    }

    #[test]
    fn handle_paste_returns_false_when_closed() {
        let mut picker = FilePickerModal::new();
        assert!(!picker.handle_paste("test"));
    }

    #[test]
    fn enter_with_selection_returns_path() {
        let mut picker = picker_with_matches(3);
        picker.session.as_mut().unwrap().selected = 1;
        match picker.handle_key(key(KeyCode::Enter)) {
            FilePickerModalAction::Select(path) => assert_eq!(path, "file_001.rs"),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn enter_with_no_matches_returns_close() {
        let mut picker = picker_with_matches(0);
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerModalAction::Close
        ));
    }

    #[test]
    fn backspace_clears_search_and_reparses() {
        let (mut picker, _done_tx) = pending_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Char('b')));
        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "a");
    }

    #[test]
    fn ctrl_w_deletes_the_search_word_and_reparses() {
        let (mut picker, _done_tx) = pending_picker();
        for c in "src main".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        picker.handle_key(kb::DELETE_WORD.to_key_event());
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "src ");
    }

    #[test_case(10, 0, 6 ; "scrolls_down_when_below")]
    #[test_case(2, 10, 2 ; "scrolls_up_when_above")]
    fn ensure_visible_adjusts_scroll(
        selected: usize,
        initial_scroll: usize,
        expected_scroll: usize,
    ) {
        let mut picker = picker_with_matches(20);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = selected;
        s.scroll_offset = initial_scroll;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, expected_scroll);
    }

    #[test]
    fn ensure_visible_zero_viewport_no_panic() {
        let mut picker = picker_with_matches(5);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 0;
        s.selected = 3;
        ensure_visible(s);
    }

    #[test]
    fn clamp_selection_reduces_when_matches_shrink() {
        let mut picker = picker_with_matches(10);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = 9;
        s.matches.truncate(3);
        clamp_selection(s);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn contains_returns_false_when_not_visible() {
        let (picker, _done_tx) = pending_picker();
        assert!(!picker.contains(Position::new(0, 0)));
    }

    #[test]
    fn hovering_file_moves_selection() {
        let mut picker = picker_with_matches(3);
        render(&mut picker);
        let hit = picker.session.as_ref().unwrap().row_hits[2];

        assert!(matches!(
            picker.handle_mouse(mouse(MouseEventKind::Moved, hit.area)),
            FilePickerModalAction::Consumed
        ));
        assert_eq!(picker.session.as_ref().unwrap().selected, 2);
    }

    #[test]
    fn clicking_file_returns_armed_path() {
        let mut picker = picker_with_matches(3);
        render(&mut picker);
        let hit = picker.session.as_ref().unwrap().row_hits[1];

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(
            action,
            FilePickerModalAction::Select(path) if path == "file_001.rs"
        ));
    }

    #[test]
    fn releasing_on_another_file_does_not_select() {
        let mut picker = picker_with_matches(2);
        render(&mut picker);
        let first = picker.session.as_ref().unwrap().row_hits[0];
        let second = picker.session.as_ref().unwrap().row_hits[1];

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.area));

        assert!(matches!(action, FilePickerModalAction::Consumed));
    }

    #[test]
    fn dragging_file_cancels_click() {
        let mut picker = picker_with_matches(2);
        render(&mut picker);
        let hit = picker.session.as_ref().unwrap().row_hits[0];

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        picker.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), hit.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(action, FilePickerModalAction::Consumed));
    }

    #[test]
    fn filtering_invalidates_rendered_file_rows() {
        let mut picker = picker_with_matches(2);
        render(&mut picker);
        let stale = picker.session.as_ref().unwrap().row_hits[0];

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));
        picker.handle_key(key(KeyCode::Char('z')));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

        assert!(matches!(action, FilePickerModalAction::Consumed));
        let session = picker.session.as_ref().unwrap();
        assert!(session.row_hits.is_empty());
        assert!(session.mouse_down.is_none());
    }

    #[test]
    fn async_match_refresh_invalidates_armed_file_row() {
        let mut picker = picker_with_matches(2);
        render(&mut picker);
        let stale = picker.session.as_ref().unwrap().row_hits[0];
        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));

        refresh_matches(picker.session.as_mut().unwrap());
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

        assert!(matches!(action, FilePickerModalAction::Consumed));
        let session = picker.session.as_ref().unwrap();
        assert!(session.row_hits.is_empty());
        assert!(session.mouse_down.is_none());
    }
}
