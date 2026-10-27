//! The card shown while a session loads. Opening a large session blocks the
//! event loop for seconds, so the loop cannot animate anything itself: a
//! thread of its own paints the card over what the terminal last showed, and
//! puts those cells back when the load is over, leaving the screen exactly as
//! ratatui believes it to be.

use std::io::{Write, stdout};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use caudra_storage::sessions::SessionOpenEvent;
use flume::{Receiver, RecvTimeoutError, Sender};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::animation::{SPINNER_FRAME, spinner_frame};
use crate::color_compat;
use crate::components::list_picker::truncate_label;
use crate::theme;

const CARD_TITLE: &str = " Loading session ";
const CARD_WIDTH: u16 = 60;
const CARD_HEIGHT: u16 = 6;
const HORIZONTAL_PADDING: u16 = 2;
const SPINNER_GAP: &str = " ";
const DETAIL_SEPARATOR: &str = " · ";
/// A load quicker than this never shows the card, so a small session opens
/// without a flash.
const GRACE: Duration = Duration::from_millis(200);
const ELAPSED_SHOWN_FROM: Duration = Duration::from_secs(1);
const THREAD_NAME: &str = "loading-screen";
const DATABASE_BUSY: &str = "Session database busy; retrying";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadPhase {
    Reading,
    History,
    Permissions,
    Subagents,
    Agent,
    Transcript,
}

impl LoadPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Reading => "Reading the session",
            Self::History => "Restoring history",
            Self::Permissions => "Checking permissions",
            Self::Subagents => "Restoring subagents",
            Self::Agent => "Starting the agent",
            Self::Transcript => "Drawing the transcript",
        }
    }
}

enum Update {
    Phase(LoadPhase),
    Database(SessionOpenEvent),
    Session {
        index: usize,
        count: usize,
        title: String,
    },
}

/// Where a load reports what it is doing. [`LoadProgress::SILENT`] goes
/// nowhere, for the callers that open a session nobody waits on.
#[derive(Clone, Copy)]
pub(crate) struct LoadProgress<'a>(Option<&'a Sender<Update>>);

impl LoadProgress<'_> {
    pub(crate) const SILENT: Self = Self(None);

    pub(crate) fn phase(self, phase: LoadPhase) {
        self.send(Update::Phase(phase));
    }

    pub(crate) fn session_open(self, event: SessionOpenEvent) {
        self.send(Update::Database(event));
    }

    pub(crate) fn session(self, index: usize, count: usize, title: &str) {
        self.send(Update::Session {
            index,
            count,
            title: title.to_owned(),
        });
    }

    fn send(self, update: Update) {
        if let Some(updates) = self.0 {
            let _ = updates.send(update);
        }
    }
}

/// While this is up, its thread paints cells: the main thread must not draw a
/// frame until [`Self::close`], or the two writers' escape sequences
/// interleave and the restore puts back cells that are no longer there.
/// Sequences that touch no cells and go out in one locked write, such as a
/// window title or an image upload, are safe.
pub(crate) struct LoadingScreen {
    updates: Option<Sender<Update>>,
    thread: Option<JoinHandle<()>>,
}

impl LoadingScreen {
    /// A guard that shows nothing, for a load with nothing worth waiting for.
    pub(crate) const fn idle() -> Self {
        Self {
            updates: None,
            thread: None,
        }
    }

    /// `screen` is what the terminal shows now, and so what the card covers
    /// and must give back.
    pub(crate) fn show(screen: &Buffer, title: &str) -> Self {
        let animator = Animator::new(
            screen.clone(),
            title,
            Instant::now(),
            color_compat::downgrade_if_needed,
        );
        let (updates, received) = flume::unbounded();
        match thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || animate(animator, &received))
        {
            Ok(thread) => Self {
                updates: Some(updates),
                thread: Some(thread),
            },
            Err(error) => {
                tracing::warn!(%error, "loading screen thread failed to start");
                Self::idle()
            }
        }
    }

    pub(crate) fn progress(&self) -> LoadProgress<'_> {
        LoadProgress(self.updates.as_ref())
    }

    /// Takes the card down, leaving the terminal as the main thread last drew
    /// it. Returns once the thread has written its last frame.
    pub(crate) fn close(&mut self) {
        self.updates.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("loading screen thread panicked");
        }
    }
}

impl Drop for LoadingScreen {
    fn drop(&mut self) {
        self.close();
    }
}

fn animate(mut animator: Animator, updates: &Receiver<Update>) {
    let drawn_on = animator.screen.area;
    let current_area = || terminal_area().unwrap_or(drawn_on);
    loop {
        match updates.recv_timeout(SPINNER_FRAME) {
            Ok(update) => animator.update(update),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let mut bytes = Vec::new();
        if animator.tick(
            Instant::now(),
            current_area(),
            &mut CrosstermBackend::new(&mut bytes),
        ) {
            write_out(&bytes);
        }
    }
    let mut bytes = Vec::new();
    if animator.finish(current_area(), &mut CrosstermBackend::new(&mut bytes)) {
        write_out(&bytes);
    }
}

/// Asked of the terminal rather than read from input events, which belong to
/// the main thread's reader.
fn terminal_area() -> Option<Rect> {
    crossterm::terminal::size()
        .ok()
        .map(|(width, height)| Rect::new(0, 0, width, height))
}

/// One locked write per frame, so nothing else's output lands mid-sequence.
fn write_out(bytes: &[u8]) {
    let mut out = stdout().lock();
    if let Err(error) = out.write_all(bytes).and_then(|()| out.flush()) {
        tracing::debug!(%error, "loading screen write failed");
    }
}

struct DatabaseWait {
    operation: &'static str,
    waited: Duration,
    budget: Duration,
}

struct LoadingCard {
    title: Option<String>,
    phase: LoadPhase,
    position: Option<(usize, usize)>,
    database_wait: Option<DatabaseWait>,
}

impl LoadingCard {
    fn new(title: &str) -> Self {
        Self {
            title: shown_title(title),
            phase: LoadPhase::Reading,
            position: None,
            database_wait: None,
        }
    }

    /// Without a title the phase moves up to the headline, so the card never
    /// has an empty row.
    fn lines(&self, elapsed: Duration) -> (&str, String) {
        if let Some(wait) = &self.database_wait {
            return (
                DATABASE_BUSY,
                format!(
                    "{:.1}s / {:.0}s{DETAIL_SEPARATOR}{}",
                    wait.waited.as_secs_f64(),
                    wait.budget.as_secs_f64(),
                    wait.operation,
                ),
            );
        }
        let mut detail = Vec::new();
        let headline = match &self.title {
            Some(title) => {
                detail.push(self.phase.label().to_owned());
                title.as_str()
            }
            None => self.phase.label(),
        };
        if let Some((number, count)) = self.position {
            detail.push(format!("session {number} of {count}"));
        }
        if elapsed >= ELAPSED_SHOWN_FROM {
            detail.push(format!("{}s", elapsed.as_secs()));
        }
        (headline, detail.join(DETAIL_SEPARATOR))
    }
}

fn shown_title(title: &str) -> Option<String> {
    Some(title.trim())
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

fn render_card(buf: &mut Buffer, rect: Rect, card: &LoadingCard, elapsed: Duration, spinner: char) {
    let theme = theme::current();
    Clear.render(rect, buf);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.panel_border)
        .title(CARD_TITLE)
        .title_style(theme.panel_title)
        .style(theme.surface_style());
    let content = block.inner(rect).inner(Margin::new(HORIZONTAL_PADDING, 1));
    block.render(rect, buf);
    let width = content.width as usize;
    let spinner = format!("{spinner}{SPINNER_GAP}");
    let (headline, detail) = card.lines(elapsed);
    let lines = vec![
        Line::from(vec![
            Span::styled(spinner.clone(), theme.spinner),
            Span::styled(
                truncate_label(headline, width.saturating_sub(spinner.width())),
                theme.item,
            ),
        ]),
        Line::styled(truncate_label(&detail, width), theme.item_desc),
    ];
    Paragraph::new(lines).render(content, buf);
}

/// Centred and sized only by the screen, so it is the same rect for the
/// whole load and what the card covered is exactly what it gives back.
fn card_rect(area: Rect) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(CARD_HEIGHT)])
        .flex(Flex::Center)
        .areas(area);
    let [card] = Layout::horizontal([Constraint::Length(CARD_WIDTH)])
        .flex(Flex::Center)
        .areas(row);
    card
}

fn region(screen: &Buffer, rect: Rect) -> Buffer {
    let rect = rect.intersection(screen.area);
    let mut copy = Buffer::empty(rect);
    for position in rect.positions() {
        copy[position] = screen[position].clone();
    }
    copy
}

struct Animator {
    /// The whole screen as the main thread left it.
    screen: Buffer,
    /// The screen the card is centred on, which a resize moves.
    area: Rect,
    /// The cells under the card: copied from `screen` until a resize clears
    /// the terminal, blank after.
    beneath: Buffer,
    shown: Option<Buffer>,
    /// Set once a resize has cleared what `screen` remembers off the terminal.
    cleared: bool,
    card: LoadingCard,
    started: Instant,
    adapt: fn(&mut Buffer),
}

impl Animator {
    fn new(screen: Buffer, title: &str, started: Instant, adapt: fn(&mut Buffer)) -> Self {
        let area = screen.area;
        Self {
            beneath: region(&screen, card_rect(area)),
            screen,
            area,
            shown: None,
            cleared: false,
            card: LoadingCard::new(title),
            started,
            adapt,
        }
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Phase(phase) => self.card.phase = phase,
            Update::Database(event) => {
                self.card.database_wait = match event {
                    SessionOpenEvent::Waiting {
                        operation,
                        waited,
                        budget,
                    } => Some(DatabaseWait {
                        operation,
                        waited,
                        budget,
                    }),
                    SessionOpenEvent::Recovered | SessionOpenEvent::Finished => None,
                };
            }
            Update::Session {
                index,
                count,
                title,
            } => {
                self.card.title = shown_title(&title);
                self.card.position = (count > 1).then_some((index + 1, count));
                self.card.database_wait = None;
            }
        }
    }

    /// Paints the card's next frame on a screen of `area`, as a diff against
    /// the last one, into `out`. Says whether it wrote anything.
    fn tick<B: Backend>(&mut self, now: Instant, area: Rect, out: &mut B) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed < GRACE {
            return false;
        }
        let resized = area != self.area;
        if resized {
            self.relayout(area, out);
        }
        let mut next = Buffer::empty(self.beneath.area);
        render_card(
            &mut next,
            self.beneath.area,
            &self.card,
            elapsed,
            spinner_frame(elapsed.as_millis()),
        );
        (self.adapt)(&mut next);
        let wrote = emit(out, self.shown.as_ref().unwrap_or(&self.beneath), &next);
        self.shown = Some(next);
        wrote || resized
    }

    /// A resized terminal keeps whatever it likes of the old screen, so the
    /// card starts over, centred, on a cleared one.
    fn relayout<B: Backend>(&mut self, area: Rect, out: &mut B) {
        if let Err(error) = out.clear() {
            tracing::debug!(%error, "loading screen failed to clear after a resize");
        }
        self.area = area;
        self.beneath = Buffer::empty(card_rect(area));
        self.shown = None;
        self.cleared = true;
    }

    /// Leaves the terminal showing what the main thread believes it shows.
    /// Untouched by a resize, that is the cells the card covered. Cleared by
    /// one at the size it started, it is the whole screen. At any other size
    /// the main thread's next frame notices the resize and redraws it all.
    fn finish<B: Backend>(self, area: Rect, out: &mut B) -> bool {
        if !self.cleared {
            let Some(shown) = &self.shown else {
                return false;
            };
            emit(out, shown, &self.beneath);
            self.redraw_spilled_glyphs(out);
            return true;
        }
        if area != self.screen.area {
            return false;
        }
        if let Err(error) = out.clear() {
            tracing::debug!(%error, "loading screen failed to clear before restoring");
        }
        emit(out, &Buffer::empty(area), &self.screen);
        true
    }

    /// A wide glyph just left of the card spills into its first column, so
    /// painting the card erased the whole glyph from the terminal. The diff
    /// only puts back the half inside the card.
    fn redraw_spilled_glyphs<B: Backend>(&self, out: &mut B) {
        let card = self.beneath.area;
        let Some(left) = card.x.checked_sub(1) else {
            return;
        };
        let spilled: Vec<_> = (card.top()..card.bottom())
            .map(|y| (left, y, &self.screen[(left, y)]))
            .filter(|(_, _, cell)| cell.symbol().width() > 1)
            .collect();
        if !spilled.is_empty()
            && let Err(error) = out.draw(spilled.into_iter())
        {
            tracing::debug!(%error, "loading screen failed to redraw a spilled glyph");
        }
    }
}

fn emit<B: Backend>(out: &mut B, previous: &Buffer, next: &Buffer) -> bool {
    let changes = previous.diff(next);
    if changes.is_empty() {
        return false;
    }
    if let Err(error) = out.draw(changes.into_iter()) {
        tracing::debug!(%error, "loading screen frame failed to encode");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::layout::Position;
    use ratatui::style::{Color, Style};
    use std::iter;
    use test_case::test_case;

    const SCREEN_WIDTH: u16 = 80;
    const SCREEN_HEIGHT: u16 = 24;
    const TITLE: &str = "Add echo to allowed tool calls";
    const SPINNER: char = '⠋';
    const ELLIPSIS: char = '\u{2026}';
    const RESIZED_WIDTH: u16 = 100;
    const RESIZED_HEIGHT: u16 = 30;
    const ASCII_ROW: &str = "x";
    const KITTY_PLACEHOLDER_ROW: &str = "\u{10EEEE}\u{0305}\u{030D}";
    const WIDE_ROW: &str = "漢";
    const STALE_SCREEN: &str = "a resize must clear the screen the card no longer matches";
    const MISSING_TEXT: &str = "the card must show this text";
    const UNEXPECTED_TEXT: &str = "the card must not show this text";
    const RESTORE_MISMATCH: &str = "finishing must put back exactly what the card covered";
    const EARLY_PAINT: &str = "nothing may paint inside the grace period";
    const OPERATION: &str = "recording session open";
    const LONG_OPERATION: &str = "reading 漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字";
    const WAITED: Duration = Duration::from_millis(7_500);
    const WAIT_BUDGET: Duration = Duration::from_secs(30);
    const WAIT_LABEL: &str = "7.5s / 30s";
    const WAITING: SessionOpenEvent = SessionOpenEvent::Waiting {
        operation: OPERATION,
        waited: WAITED,
        budget: WAIT_BUDGET,
    };

    fn rendered(card: &LoadingCard, elapsed: Duration, area: Rect) -> String {
        let mut buf = Buffer::empty(area);
        render_card(&mut buf, card_rect(area), card, elapsed, SPINNER);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen_area() -> Rect {
        Rect::new(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT)
    }

    fn card(title: &str, phase: LoadPhase, position: Option<(usize, usize)>) -> LoadingCard {
        LoadingCard {
            title: shown_title(title),
            phase,
            position,
            database_wait: None,
        }
    }

    #[test_case(SessionOpenEvent::Recovered; "recovered")]
    #[test_case(SessionOpenEvent::Finished; "finished")]
    fn database_wait_replaces_the_phase_until_cleared(event: SessionOpenEvent) {
        let screen = beneath_screen(ASCII_ROW);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);
        animator.update(Update::Phase(LoadPhase::History));
        animator.update(Update::Database(WAITING));
        animator.tick(started + GRACE, screen.area, &mut out);

        let text = text_in(out.buffer(), screen.area);
        for part in [DATABASE_BUSY, OPERATION, WAIT_LABEL] {
            assert!(text.contains(part), "{MISSING_TEXT}: {part}");
        }
        animator.update(Update::Database(event));
        animator.tick(started + GRACE, screen.area, &mut out);

        let text = text_in(out.buffer(), screen.area);
        for part in [TITLE, LoadPhase::History.label()] {
            assert!(text.contains(part), "{MISSING_TEXT}: {part}");
        }
        for part in [DATABASE_BUSY, OPERATION, WAIT_LABEL] {
            assert!(!text.contains(part), "{UNEXPECTED_TEXT}: {part}");
        }
    }

    #[test]
    fn waiting_updates_use_contention_time_not_total_loading_time() {
        let screen = beneath_screen(ASCII_ROW);
        let mut animator = animator(&screen, Instant::now());
        animator.update(Update::Database(SessionOpenEvent::Waiting {
            operation: OPERATION,
            waited: Duration::ZERO,
            budget: WAIT_BUDGET,
        }));
        animator.update(Update::Database(WAITING));

        let text = rendered(&animator.card, WAIT_BUDGET * 2, screen.area);
        assert!(text.contains(WAIT_LABEL), "{MISSING_TEXT}: {WAIT_LABEL}");
        assert!(text.contains(SPINNER), "{MISSING_TEXT}: {SPINNER}");
    }

    #[test_case(Rect::new(0, 0, 45, 6); "narrow card")]
    #[test_case(Rect::new(0, 0, 1, 1); "single cell")]
    fn database_wait_clips_inside_the_card(area: Rect) {
        let screen = beneath_screen(ASCII_ROW);
        let mut animator = animator(&screen, Instant::now());
        animator.update(Update::Database(SessionOpenEvent::Waiting {
            operation: LONG_OPERATION,
            waited: WAITED,
            budget: WAIT_BUDGET,
        }));

        let text = rendered(&animator.card, Duration::ZERO, area);
        if area.height >= CARD_HEIGHT {
            assert!(text.contains(ELLIPSIS), "{MISSING_TEXT}: {ELLIPSIS}");
            assert!(text.contains(WAIT_LABEL), "{MISSING_TEXT}: {WAIT_LABEL}");
        }
        assert!(!text.contains(LONG_OPERATION), "{UNEXPECTED_TEXT}");
    }

    #[test]
    fn database_progress_is_scoped_to_its_loading_screen() {
        let (first_tx, first_rx) = flume::unbounded();
        let first = LoadingScreen {
            updates: Some(first_tx),
            thread: None,
        };
        let (second_tx, second_rx) = flume::unbounded();
        let second = LoadingScreen {
            updates: Some(second_tx),
            thread: None,
        };
        first.progress().session_open(WAITING);
        assert!(second_rx.is_empty());

        let screen = beneath_screen(ASCII_ROW);
        let mut first_animator = animator(&screen, Instant::now());
        first_animator.update(first_rx.try_recv().unwrap());
        assert_eq!(first_animator.card.lines(Duration::ZERO).0, DATABASE_BUSY);
        let mut second_animator = animator(&screen, Instant::now());
        second.progress().session(0, 1, TITLE);
        second_animator.update(second_rx.try_recv().unwrap());
        assert_eq!(second_animator.card.lines(Duration::ZERO).0, TITLE);
        assert!(second_animator.card.database_wait.is_none());

        first.progress().session_open(WAITING);
        drop(first);
        assert!(second_rx.is_empty());
        assert!(second_animator.card.database_wait.is_none());
    }

    #[test]
    fn moving_to_another_session_clears_the_previous_database_wait() {
        let screen = beneath_screen(ASCII_ROW);
        let mut animator = animator(&screen, Instant::now());
        animator.update(Update::Database(WAITING));
        animator.update(Update::Session {
            index: 1,
            count: 2,
            title: TITLE.to_owned(),
        });

        assert_eq!(animator.card.lines(Duration::ZERO).0, TITLE);
        assert!(animator.card.database_wait.is_none());
    }

    #[test_case(TITLE, LoadPhase::Permissions, &[TITLE, "Checking permissions"]; "title and phase")]
    #[test_case("  ", LoadPhase::Reading, &["Reading the session"]; "a blank title gives the headline to the phase")]
    fn the_card_names_the_session_and_what_it_is_doing(
        title: &str,
        phase: LoadPhase,
        expected: &[&str],
    ) {
        let text = rendered(&card(title, phase, None), Duration::ZERO, screen_area());

        for part in expected {
            assert!(text.contains(part), "{MISSING_TEXT}: {part}\n{text}");
        }
        assert!(text.contains(SPINNER), "{MISSING_TEXT}: spinner\n{text}");
    }

    #[test_case(&"long title ".repeat(10); "a long title")]
    #[test_case(&"漢字".repeat(30); "a wide glyph title")]
    fn a_title_too_wide_for_the_card_is_cut_with_an_ellipsis(title: &str) {
        let text = rendered(
            &card(title, LoadPhase::History, None),
            Duration::ZERO,
            screen_area(),
        );

        assert!(
            text.contains(ELLIPSIS),
            "{MISSING_TEXT}: {ELLIPSIS}\n{text}"
        );
    }

    #[test_case(None, None; "one session")]
    #[test_case(Some((2, 3)), Some("session 2 of 3"); "several sessions")]
    fn the_position_shows_only_among_several_sessions(
        position: Option<(usize, usize)>,
        expected: Option<&str>,
    ) {
        let text = rendered(
            &card(TITLE, LoadPhase::Agent, position),
            Duration::ZERO,
            screen_area(),
        );

        match expected {
            Some(part) => assert!(text.contains(part), "{MISSING_TEXT}: {part}\n{text}"),
            None => assert!(!text.contains("session 1"), "{UNEXPECTED_TEXT}\n{text}"),
        }
    }

    #[test_case(Duration::from_millis(999), false; "under a second")]
    #[test_case(Duration::from_millis(4_200), true; "several seconds")]
    fn elapsed_time_shows_from_one_second(elapsed: Duration, shown: bool) {
        let text = rendered(
            &card(TITLE, LoadPhase::Transcript, None),
            elapsed,
            screen_area(),
        );

        let seconds = format!("{DETAIL_SEPARATOR}{}s", elapsed.as_secs());
        assert_eq!(text.contains(&seconds), shown, "{text}");
    }

    #[test_case(Rect::new(0, 0, 20, 3); "smaller than the card")]
    #[test_case(Rect::new(0, 0, 1, 1); "a single cell")]
    fn the_card_stays_inside_a_small_screen(area: Rect) {
        let rect = card_rect(area);

        assert_eq!(rect.intersection(area), rect);
        rendered(&card(TITLE, LoadPhase::History, None), Duration::ZERO, area);
    }

    /// A screen covered in one row's text and colour, so a cell the restore
    /// misses cannot pass for one it got right.
    fn beneath_screen(row: &str) -> Buffer {
        let mut buf = Buffer::empty(screen_area());
        let style = Style::default().fg(Color::Magenta);
        for y in 0..SCREEN_HEIGHT {
            buf.set_string(0, y, row.repeat(SCREEN_WIDTH.into()), style);
        }
        buf
    }

    fn mirror(screen: &Buffer) -> TestBackend {
        let mut backend = TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT);
        let cells = screen
            .area
            .positions()
            .map(|position| (position.x, position.y, &screen[position]));
        backend.draw(cells).unwrap();
        backend
    }

    fn animator(screen: &Buffer, started: Instant) -> Animator {
        Animator::new(screen.clone(), TITLE, started, |_| {})
    }

    /// A terminal blanks the cell a wide glyph spills into, where a
    /// `TestBackend` keeps whatever was drawn there before, so restores are
    /// compared on the cells that show.
    fn visible(buffer: &Buffer) -> Vec<(Position, Cell)> {
        let mut spilled = 0;
        let mut shown = Vec::new();
        for position in buffer.area.positions() {
            if position.x == buffer.area.x {
                spilled = 0;
            }
            if spilled > 0 {
                spilled -= 1;
                continue;
            }
            let cell = &buffer[position];
            spilled = cell.symbol().width().saturating_sub(1);
            shown.push((position, cell.clone()));
        }
        shown
    }

    fn text_in(buffer: &Buffer, area: Rect) -> String {
        area.positions()
            .map(|position| buffer[position].symbol())
            .collect()
    }

    #[test]
    fn a_load_inside_the_grace_period_paints_nothing() {
        let screen = beneath_screen(ASCII_ROW);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);

        assert!(
            !animator.tick(started + GRACE / 2, screen.area, &mut out),
            "{EARLY_PAINT}"
        );
        assert!(!animator.finish(screen.area, &mut out), "{EARLY_PAINT}");
        assert_eq!(out.buffer(), &screen, "{EARLY_PAINT}");
    }

    #[test]
    fn the_first_tick_after_grace_paints_and_an_unchanged_one_does_not() {
        let screen = beneath_screen(ASCII_ROW);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);

        assert!(animator.tick(started + GRACE, screen.area, &mut out));
        assert!(!animator.tick(started + GRACE, screen.area, &mut out));
        let text = text_in(out.buffer(), screen.area);
        assert!(text.contains(TITLE), "{MISSING_TEXT}: {TITLE}");
    }

    #[test_case(ASCII_ROW; "plain text")]
    #[test_case(KITTY_PLACEHOLDER_ROW; "a kitty picture")]
    #[test_case(WIDE_ROW; "wide glyphs")]
    fn finishing_gives_back_every_cell_the_card_covered(row: &str) {
        let screen = beneath_screen(row);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);

        animator.tick(started + GRACE, screen.area, &mut out);
        animator.update(Update::Session {
            index: 1,
            count: 2,
            title: TITLE.to_owned(),
        });
        animator.update(Update::Phase(LoadPhase::Subagents));
        animator.tick(started + ELAPSED_SHOWN_FROM * 3, screen.area, &mut out);
        assert_ne!(out.buffer(), &screen);
        assert!(animator.finish(screen.area, &mut out));

        assert_eq!(
            visible(out.buffer()),
            visible(&screen),
            "{RESTORE_MISMATCH}"
        );
    }

    #[test]
    fn a_wide_glyph_the_card_cut_in_half_comes_back_whole() {
        let row = format!("{ASCII_ROW}{}", WIDE_ROW.repeat(SCREEN_WIDTH.into()));
        let mut screen = Buffer::empty(screen_area());
        for y in 0..SCREEN_HEIGHT {
            screen.set_string(0, y, &row, Style::default());
        }
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);
        let card = card_rect(screen.area);
        assert!(screen[(card.x - 1, card.y)].symbol().width() > 1);

        animator.tick(started + GRACE, screen.area, &mut out);
        for y in card.top()..card.bottom() {
            out.draw(iter::once((card.x - 1, y, &Cell::EMPTY))).unwrap();
        }
        animator.finish(screen.area, &mut out);

        assert_eq!(
            visible(out.buffer()),
            visible(&screen),
            "{RESTORE_MISMATCH}"
        );
    }

    #[test]
    fn a_resize_centres_the_card_on_a_cleared_screen_and_leaves_the_rest_to_the_next_frame() {
        let screen = beneath_screen(ASCII_ROW);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);
        animator.tick(started + GRACE, screen.area, &mut out);

        out.resize(RESIZED_WIDTH, RESIZED_HEIGHT);
        let resized = Rect::new(0, 0, RESIZED_WIDTH, RESIZED_HEIGHT);
        assert!(animator.tick(started + GRACE, resized, &mut out));

        let card = card_rect(resized);
        assert!(
            text_in(out.buffer(), card).contains(TITLE),
            "{MISSING_TEXT}: {TITLE}"
        );
        let stale = resized
            .positions()
            .filter(|position| !card.contains(*position))
            .any(|position| out.buffer()[position].symbol() == ASCII_ROW);
        assert!(!stale, "{STALE_SCREEN}");
        assert!(!animator.finish(resized, &mut out));
    }

    #[test]
    fn a_resize_and_back_gives_back_the_whole_screen() {
        let screen = beneath_screen(ASCII_ROW);
        let started = Instant::now();
        let mut animator = animator(&screen, started);
        let mut out = mirror(&screen);
        animator.tick(started + GRACE, screen.area, &mut out);

        out.resize(RESIZED_WIDTH, RESIZED_HEIGHT);
        animator.tick(
            started + GRACE,
            Rect::new(0, 0, RESIZED_WIDTH, RESIZED_HEIGHT),
            &mut out,
        );
        out.resize(SCREEN_WIDTH, SCREEN_HEIGHT);
        assert!(animator.finish(screen.area, &mut out));

        assert_eq!(
            visible(out.buffer()),
            visible(&screen),
            "{RESTORE_MISMATCH}"
        );
    }
}
