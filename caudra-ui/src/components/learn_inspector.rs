//! `/learn`: the project's learning runs, newest first, with the selected
//! run opened under its row to show what it changed and what it dropped.
//! Reads and writes go through the app, which answers on a later tick.

use std::mem;
use std::ops::Range;

use caudra_agent::learning::apply::Dropped;
use caudra_agent::learning::pump::{CONFLICT, FAILED, LEARNED, PARTIAL};
use caudra_agent::learning::targets::bullets;
use caudra_config::LearningTarget;
use caudra_grab::grab_scope;
use caudra_providers::token_label;
use caudra_storage::learning::{LearningProject, LearningRun, StoredEdit};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::docs_link::DocsTopic;
use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    Hint, HintBar, ModalScroll, Overlay, escape_terminal_controls, format_elapsed, hover_style,
    visual_rows,
};
use crate::theme::{self, Theme};

const TITLE: &str = " Learning ";
const WIDTH_PERCENT: u16 = 80;
const MAX_HEIGHT_PERCENT: u16 = 82;
const FOOTER_ROWS: u16 = 1;
const H_PAD: u16 = 1;
const MILLIS_PER_SECOND: i64 = 1_000;
const RUN_TIME_FORMAT: &str = "%m-%d %H:%M";
const SELECTED_MARK: &str = "\u{25b8} ";
const UNSELECTED_MARK: &str = "  ";
const EDIT_INDENT: &str = "    ";
const DETAIL_INDENT: &str = "      ";
const SEPARATOR: &str = " \u{b7} ";
const STATE_LABEL: &str = "Learning  ";
const PROJECT_LABEL: &str = "Project   ";
const STATE_ON: &str = "on";
const STATE_PAUSED: &str = "paused";
const STATE_OFF_HERE: &str = "off in this session";
const LAST_RUN: &str = "last run ";
const AGO: &str = " ago";
const NEVER_RUN: &str = "never run";
const RUNNING: &str = "running";
const READING: &str = "Reading learning runs\u{2026}";
const READ_FAILED: &str = "Could not read learning runs: ";
pub(crate) const NO_RUNS: &str = "No learning runs yet.";
pub(crate) const NO_CHANGES: &str = "no changes";
const REVERTED_MARK: &str = "  reverted";
const REFS: &str = " refs";
const ONE_REF: &str = " ref";
const ADDED: &str = "+ ";
const REMOVED: &str = "- ";
pub(crate) const NEW_NOTE: &str = "+ new note";
pub(crate) const REWROTE_NOTE: &str = "~ note rewritten";
pub(crate) const REMOVED_NOTE: &str = "- note removed";
const DROPPED_SECTION: &str = "Dropped";
pub(crate) const NOTHING_SELECTED: &str = "This run changed nothing to revert";
pub(crate) const ALREADY_REVERTED: &str = "That change is already reverted";
pub(crate) const PAUSED_NO_RUN: &str = "Learning is paused for this project; press p to resume";
/// Fits the narrowest modal, so `Esc` is never the hint that is dropped.
const HINTS: &[Hint] = &[
    Hint::inert("\u{2191}\u{2193}\u{2190}\u{2192}", "select"),
    Hint::char("t", "transcript"),
    Hint::char("n", "run now"),
    Hint::char("u", "revert"),
    Hint::char("p", "pause"),
    Hint::key("Esc", KeyCode::Esc, "close"),
];

/// A run with what it changed and what it dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LearnRun {
    pub run: LearningRun,
    pub edits: Vec<StoredEdit>,
    pub dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LearnSnapshot {
    pub project: String,
    pub state: Option<LearningProject>,
    pub runs: Vec<LearnRun>,
}

impl LearnSnapshot {
    fn paused(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.paused)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LearnAction {
    Consumed,
    Close,
    RunNow,
    Revert(StoredEdit),
    SetPaused(bool),
    /// Open this run's conversation.
    Transcript(i64),
    Flash(&'static str),
}

/// What a line of the modal answers the pointer for: a run's row, or one of
/// the selected run's changes with the lines that show what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowTarget {
    Run(usize),
    Edit(usize),
}

/// The modal's lines, each with the row it belongs to.
#[derive(Default)]
struct Body {
    lines: Vec<Line<'static>>,
    targets: Vec<Option<RowTarget>>,
}

impl Body {
    fn push(&mut self, line: Line<'static>, target: Option<RowTarget>) {
        self.lines.push(line);
        self.targets.push(target);
    }

    /// The line the selection is on: the selected change, else its run.
    fn focus(&self, run: usize, edit: usize) -> Option<usize> {
        let line_of = |target| self.targets.iter().position(|at| *at == Some(target));
        line_of(RowTarget::Edit(edit)).or_else(|| line_of(RowTarget::Run(run)))
    }
}

pub(crate) struct LearnInspector {
    open: bool,
    /// A learner runs for the session in front.
    live: bool,
    snapshot: Option<Result<LearnSnapshot, String>>,
    run: usize,
    edit: usize,
    /// The selection moved, so the next frame scrolls it into view.
    follow: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    footer: HintBar,
    pointer: Option<Position>,
    /// Where the last frame drew the lines, and the rows each target took.
    content: Rect,
    hits: Vec<(Range<u16>, RowTarget)>,
    popup: Rect,
}

impl LearnInspector {
    pub fn new() -> Self {
        Self {
            open: false,
            live: false,
            snapshot: None,
            run: 0,
            edit: 0,
            follow: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            footer: HintBar::default(),
            pointer: None,
            content: Rect::default(),
            hits: Vec::new(),
            popup: Rect::default(),
        }
    }

    pub fn open(&mut self, live: bool) {
        *self = Self {
            open: true,
            live,
            ..Self::new()
        };
    }

    pub fn close(&mut self) {
        *self = Self::new();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    /// Lands a read, keeping the selected run and change where they still
    /// exist. A selection on the newest run follows a run that starts.
    pub fn fill(&mut self, snapshot: Result<LearnSnapshot, String>, live: bool) {
        let selected = self.selected_run().map(|run| run.run.id);
        let on_newest = self.run == 0;
        self.live = live;
        self.snapshot = Some(snapshot);
        let runs = self.runs();
        self.run = match on_newest {
            true => 0,
            false => selected
                .and_then(|id| runs.iter().position(|run| run.run.id == id))
                .unwrap_or(0),
        };
        if self.selected_run().map(|run| run.run.id) != selected {
            self.edit = 0;
        }
        self.edit = self.edit.min(
            self.selected_run()
                .map_or(0, |run| run.edits.len().saturating_sub(1)),
        );
    }

    fn runs(&self) -> &[LearnRun] {
        match &self.snapshot {
            Some(Ok(snapshot)) => &snapshot.runs,
            _ => &[],
        }
    }

    fn selected_run(&self) -> Option<&LearnRun> {
        self.runs().get(self.run)
    }

    fn selected_edit(&self) -> Option<&StoredEdit> {
        self.selected_run()?.edits.get(self.edit)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> LearnAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return LearnAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.select_run(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select_run(1),
            KeyCode::Left | KeyCode::Char('h') => self.select_edit(-1),
            KeyCode::Right | KeyCode::Char('l') => self.select_edit(1),
            KeyCode::Char('n') => {
                return match &self.snapshot {
                    Some(Ok(snapshot)) if snapshot.paused() => LearnAction::Flash(PAUSED_NO_RUN),
                    _ => LearnAction::RunNow,
                };
            }
            KeyCode::Char('u') => return self.revert(),
            KeyCode::Enter | KeyCode::Char('t') => return self.transcript(),
            KeyCode::Char('p') => {
                if let Some(Ok(snapshot)) = &self.snapshot {
                    return LearnAction::SetPaused(!snapshot.paused());
                }
            }
            _ => {
                self.scroll.handle_key(key);
            }
        }
        LearnAction::Consumed
    }

    fn revert(&self) -> LearnAction {
        match self.selected_edit() {
            None => LearnAction::Flash(NOTHING_SELECTED),
            Some(edit) if edit.reverted_ms.is_some() => LearnAction::Flash(ALREADY_REVERTED),
            Some(edit) => LearnAction::Revert(edit.clone()),
        }
    }

    fn transcript(&self) -> LearnAction {
        self.selected_run().map_or(LearnAction::Consumed, |run| {
            LearnAction::Transcript(run.run.id)
        })
    }

    fn select_run(&mut self, delta: isize) {
        self.pick_run(self.run.saturating_add_signed(delta));
    }

    fn pick_run(&mut self, run: usize) {
        let run = run.min(self.runs().len().saturating_sub(1));
        if run != self.run {
            self.run = run;
            self.edit = 0;
        }
        self.follow = true;
    }

    fn select_edit(&mut self, delta: isize) {
        self.pick_edit(self.edit.saturating_add_signed(delta));
    }

    fn pick_edit(&mut self, edit: usize) {
        let last = self
            .selected_run()
            .map_or(0, |run| run.edits.len().saturating_sub(1));
        self.edit = edit.min(last);
        self.follow = true;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// A click selects the run or change under it, and a click on the
    /// selected run opens its transcript, as `Enter` does. A footer hint
    /// presses its key.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> LearnAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return LearnAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return LearnAction::Consumed;
            }
        }
        let at = Position::new(event.column, event.row);
        self.pointer = Some(at);
        if let Some(key) = self.footer.handle_mouse(event) {
            return self.handle_key(key);
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::ScrollDown => self.scroll(1),
            MouseEventKind::Down(MouseButton::Left) => match self.target_at(at) {
                Some(RowTarget::Run(run)) if run == self.run => return self.transcript(),
                Some(RowTarget::Run(run)) => self.pick_run(run),
                Some(RowTarget::Edit(edit)) => self.pick_edit(edit),
                None => {}
            },
            _ => {}
        }
        LearnAction::Consumed
    }

    /// The row under `at`, measured against the frame last drawn.
    fn target_at(&self, at: Position) -> Option<RowTarget> {
        if !self.content.contains(at) {
            return None;
        }
        let row = (at.y - self.content.y).saturating_add(self.scroll.offset());
        self.hits
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, target)| *target)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("learn_inspector", area);

        let theme = theme::current();
        let hovered = self.pointer.and_then(|at| self.target_at(at));
        let mut body = self.body(&theme, Timestamp::now().as_millisecond());
        for (line, target) in body.lines.iter_mut().zip(&body.targets) {
            if hovered.is_some() && *target == hovered {
                line.style = hover_style(line.style, true);
            }
        }
        let width = Modal::inner_width(area.width, WIDTH_PERCENT)
            .saturating_sub(H_PAD * 2)
            .max(1);
        let rows = visual_rows(&body.lines, width);
        let total = rows.total.min(u16::MAX - CHROME_LINES - FOOTER_ROWS);
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total + FOOTER_ROWS);
        let content = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            height: inner.height.saturating_sub(FOOTER_ROWS),
            ..inner
        };
        self.scroll.update_dimensions(total, content.height);
        if mem::take(&mut self.follow)
            && let Some(focus) = body.focus(self.run, self.edit)
        {
            let focus = u16::try_from(focus).unwrap_or(u16::MAX);
            self.scroll
                .reveal_and_hold(rows.row_of(focus), rows.height_of(focus));
        }
        self.hits = body
            .targets
            .iter()
            .zip(0u16..)
            .filter_map(|(target, line)| {
                let top = rows.row_of(line);
                Some((top..top.saturating_add(rows.height_of(line)), (*target)?))
            })
            .collect();
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(body.lines)
                .style(Style::new().fg(theme.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            content,
        );
        self.scrollbar.draw(frame, inner, total, offset);
        let footer = Rect {
            y: inner.bottom().saturating_sub(FOOTER_ROWS),
            height: FOOTER_ROWS.min(inner.height),
            ..content
        };
        self.footer.draw(frame, footer, HINTS.to_vec());

        self.content = content;
        self.popup = popup;
        popup
    }

    fn body(&self, theme: &Theme, now_ms: i64) -> Body {
        let mut body = Body::default();
        let snapshot = match &self.snapshot {
            None => {
                body.push(Line::styled(READING, theme.status_dim), None);
                return body;
            }
            Some(Err(error)) => {
                let text = format!("{READ_FAILED}{}", escape_terminal_controls(error));
                body.push(Line::styled(text, theme.tool_error), None);
                return body;
            }
            Some(Ok(snapshot)) => snapshot,
        };
        for line in header_lines(snapshot, self.live, now_ms, theme) {
            body.push(line, None);
        }
        body.push(Line::default(), None);
        if snapshot.runs.is_empty() {
            body.push(Line::styled(NO_RUNS, theme.status_dim), None);
            return body;
        }
        for (index, run) in snapshot.runs.iter().enumerate() {
            let selected = index == self.run;
            body.push(run_line(run, selected, theme), Some(RowTarget::Run(index)));
            if selected {
                detail_lines(run, self.edit, theme, &mut body);
            }
        }
        body
    }
}

impl Default for LearnInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for LearnInspector {
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Learning)
    }

    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

fn header_lines(
    snapshot: &LearnSnapshot,
    live: bool,
    now_ms: i64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (state, style) = match (snapshot.paused(), live) {
        (true, _) => (STATE_PAUSED, theme.tool_warning),
        (false, true) => (STATE_ON, theme.tool_success),
        (false, false) => (STATE_OFF_HERE, theme.status_dim),
    };
    let last = snapshot
        .state
        .as_ref()
        .map(|state| state.last_run_ms)
        .filter(|at| *at > 0)
        .map_or_else(
            || NEVER_RUN.to_owned(),
            |at| format!("{LAST_RUN}{}{AGO}", age(now_ms - at)),
        );
    vec![
        Line::from(vec![
            Span::styled(STATE_LABEL, theme.keybind_desc),
            Span::styled(state, style),
            Span::styled(format!("{SEPARATOR}{last}"), theme.status_dim),
        ]),
        Line::from(vec![
            Span::styled(PROJECT_LABEL, theme.keybind_desc),
            Span::styled(escape_terminal_controls(&snapshot.project), theme.tool_path),
        ]),
    ]
}

fn age(millis: i64) -> String {
    format_elapsed(u64::try_from(millis.max(0) / MILLIS_PER_SECOND).unwrap_or_default())
}

fn run_line(run: &LearnRun, selected: bool, theme: &Theme) -> Line<'static> {
    let outcome = match run.run.finished_ms {
        None => RUNNING,
        Some(_) => run.run.outcome.as_str(),
    };
    let outcome_style = match outcome {
        LEARNED => theme.tool_success,
        PARTIAL | CONFLICT => theme.tool_warning,
        FAILED => theme.tool_error,
        _ => theme.status_dim,
    };
    let mut spans = vec![
        Span::styled(mark(selected), theme.accent),
        Span::styled(clock(run.run.started_ms), theme.timestamp),
        Span::raw("  "),
        Span::styled(escape_terminal_controls(outcome), outcome_style),
    ];
    if !run.run.model.is_empty() {
        spans.push(Span::styled(
            format!("{SEPARATOR}{}", escape_terminal_controls(&run.run.model)),
            theme.tool_dim,
        ));
    }
    if run.run.tokens > 0 {
        let tokens = u32::try_from(run.run.tokens).unwrap_or(u32::MAX);
        spans.push(Span::styled(
            format!("{SEPARATOR}{}", token_label(tokens)),
            theme.tool_dim,
        ));
    }
    spans.push(Span::raw(format!("{SEPARATOR}{}", edit_counts(&run.edits))));
    let line = Line::from(spans);
    if selected {
        line.style(theme.item_selected)
    } else {
        line
    }
}

/// `project 1 · memory 2`, in the order targets are listed everywhere else.
fn edit_counts(edits: &[StoredEdit]) -> String {
    let counts: Vec<String> = LearningTarget::ALL
        .into_iter()
        .filter_map(|target| {
            let count = edits
                .iter()
                .filter(|edit| edit.edit.target == target.as_str())
                .count();
            (count > 0).then(|| format!("{} {count}", target.as_str()))
        })
        .collect();
    match counts.is_empty() {
        true => NO_CHANGES.to_owned(),
        false => counts.join(SEPARATOR),
    }
}

/// Pushes the run's summary, changes and dropped items.
fn detail_lines(run: &LearnRun, selected: usize, theme: &Theme, body: &mut Body) {
    if !run.run.summary.is_empty() {
        body.push(
            Line::styled(
                format!(
                    "{EDIT_INDENT}{}",
                    escape_terminal_controls(&run.run.summary)
                ),
                theme.tool_dim,
            ),
            None,
        );
    }
    for (index, edit) in run.edits.iter().enumerate() {
        let target = Some(RowTarget::Edit(index));
        body.push(edit_line(edit, index == selected, theme), target);
        for line in diff_lines(edit, theme) {
            body.push(line, target);
        }
    }
    if !run.dropped.is_empty() {
        body.push(
            Line::styled(format!("{EDIT_INDENT}{DROPPED_SECTION}"), theme.keybind_key),
            None,
        );
        for dropped in &run.dropped {
            body.push(
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{DETAIL_INDENT}{}: ",
                            escape_terminal_controls(&dropped.target)
                        ),
                        theme.tool_dim,
                    ),
                    Span::raw(escape_terminal_controls(&dropped.item)),
                    Span::styled(
                        format!(" ({})", escape_terminal_controls(&dropped.reason)),
                        theme.status_dim,
                    ),
                ]),
                None,
            );
        }
    }
}

fn edit_line(edit: &StoredEdit, selected: bool, theme: &Theme) -> Line<'static> {
    let refs =
        serde_json::from_str::<Vec<String>>(&edit.edit.evidence).map_or(0, |refs| refs.len());
    let unit = if refs == 1 { ONE_REF } else { REFS };
    let mut spans = vec![
        Span::raw(EDIT_INDENT),
        Span::styled(mark(selected), theme.accent),
        Span::styled(
            escape_terminal_controls(&edit.edit.target),
            theme.keybind_key,
        ),
        Span::raw("  "),
        Span::styled(escape_terminal_controls(&edit.edit.path), theme.tool_path),
        Span::styled(format!("{SEPARATOR}{refs}{unit}"), theme.status_dim),
    ];
    if edit.reverted_ms.is_some() {
        spans.push(Span::styled(REVERTED_MARK, theme.tool_warning));
    }
    Line::from(spans)
}

/// What a change did: the bullets an instruction block lost and gained, or
/// what became of a note.
fn diff_lines(edit: &StoredEdit, theme: &Theme) -> Vec<Line<'static>> {
    let (before, after) = (edit.edit.before.as_deref(), edit.edit.after.as_deref());
    if edit.edit.target == LearningTarget::Memory.as_str() {
        let (text, style) = match (before, after) {
            (None, _) => (NEW_NOTE, theme.diff_new),
            (Some(_), Some(_)) => (REWROTE_NOTE, theme.tool_dim),
            (Some(_), None) => (REMOVED_NOTE, theme.diff_old),
        };
        return vec![Line::styled(format!("{DETAIL_INDENT}{text}"), style)];
    }
    let before = before.map(bullets).unwrap_or_default();
    let after = after.map(bullets).unwrap_or_default();
    let removed = before
        .iter()
        .filter(|bullet| !after.contains(bullet))
        .map(|bullet| (REMOVED, bullet, theme.diff_old));
    let added = after
        .iter()
        .filter(|bullet| !before.contains(bullet))
        .map(|bullet| (ADDED, bullet, theme.diff_new));
    removed
        .chain(added)
        .map(|(sign, bullet, style)| {
            Line::styled(
                format!("{DETAIL_INDENT}{sign}{}", escape_terminal_controls(bullet)),
                style,
            )
        })
        .collect()
}

fn mark(selected: bool) -> &'static str {
    if selected {
        SELECTED_MARK
    } else {
        UNSELECTED_MARK
    }
}

fn clock(at: i64) -> String {
    Timestamp::from_millisecond(at)
        .map(|at| {
            at.to_zoned(TimeZone::system())
                .strftime(RUN_TIME_FORMAT)
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use caudra_agent::learning::targets::{BLOCK_CLOSE, BLOCK_OPEN};
    use caudra_storage::learning::LearningEdit;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::{
        ADDED, ALREADY_REVERTED, DETAIL_INDENT, H_PAD, HINTS, KeyCode, KeyEvent, LEARNED,
        LearnAction, LearnInspector, LearnRun, LearnSnapshot, LearningProject, LearningRun,
        LearningTarget, Line, Modal, MouseButton, MouseEvent, MouseEventKind, NEW_NOTE, NO_CHANGES,
        NO_RUNS, NOTHING_SELECTED, PAUSED_NO_RUN, Position, REMOVED, REWROTE_NOTE, Rect, RowTarget,
        StoredEdit, WIDTH_PERCENT, diff_lines, run_line, theme,
    };
    use crate::components::hint_hits;

    const RUN_ID: i64 = 7;
    const PROJECT: &str = "/repo";
    const AGENTS: &str = "/repo/AGENTS.md";
    const NOTE: &str = "build.md";
    const KEPT: &str = "Use make check";
    const GAINED: &str = "Run nextest per crate";
    const LOST: &str = "Prefer cargo test";
    const REVERTED_AT: i64 = 5;
    const SCREEN_WIDTH: u16 = 100;
    const SCREEN_HEIGHT: u16 = 40;
    const TRANSCRIPT_HINT: usize = 1;
    const CLOSE_HINT: usize = 5;
    const NARROW_TERMINAL: u16 = 80;
    const GROUP_HINT: usize = 0;
    const NOT_DRAWN: &str = "the row was drawn";

    fn block(bullets: &[&str]) -> String {
        let body: String = bullets
            .iter()
            .map(|bullet| format!("- {bullet}\n"))
            .collect();
        format!("# Notes\n\n{BLOCK_OPEN}\n## Learned\n\n{body}{BLOCK_CLOSE}\n")
    }

    fn edit(seq: i64, target: LearningTarget, before: Option<String>, after: &str) -> StoredEdit {
        StoredEdit {
            run_id: RUN_ID,
            seq,
            edit: LearningEdit {
                target: target.as_str().to_owned(),
                path: match target {
                    LearningTarget::Memory => NOTE.to_owned(),
                    _ => AGENTS.to_owned(),
                },
                before,
                after: Some(after.to_owned()),
                evidence: r#"["s1#1","s2#4"]"#.to_owned(),
            },
            reverted_ms: None,
        }
    }

    fn run(edits: Vec<StoredEdit>) -> LearnRun {
        LearnRun {
            run: LearningRun {
                id: RUN_ID,
                started_ms: 0,
                finished_ms: Some(1),
                from_ms: 0,
                through_ms: 1,
                outcome: LEARNED.to_owned(),
                model: String::new(),
                tokens: 0,
                summary: String::new(),
                dropped: "[]".to_owned(),
            },
            edits,
            dropped: Vec::new(),
        }
    }

    fn inspector(runs: Vec<LearnRun>) -> LearnInspector {
        let mut inspector = LearnInspector::new();
        inspector.open(true);
        inspector.fill(
            Ok(LearnSnapshot {
                project: PROJECT.to_owned(),
                state: None,
                runs,
            }),
            true,
        );
        inspector
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(inspector: &mut LearnInspector, code: KeyCode) -> LearnAction {
        inspector.handle_key(KeyEvent::from(code))
    }

    #[test]
    fn a_run_row_counts_its_changes_per_target() {
        let edits = vec![
            edit(0, LearningTarget::Project, None, &block(&[GAINED])),
            edit(1, LearningTarget::Memory, None, KEPT),
            edit(2, LearningTarget::Memory, Some(KEPT.to_owned()), GAINED),
        ];
        let row = text(&[run_line(&run(edits), false, &theme::current())]);
        assert!(row.ends_with("project 1 \u{b7} memory 2"), "{row}");
        let empty = text(&[run_line(&run(Vec::new()), false, &theme::current())]);
        assert!(empty.ends_with(NO_CHANGES), "{empty}");
    }

    #[test]
    fn an_instruction_change_shows_only_the_bullets_it_moved() {
        let changed = edit(
            0,
            LearningTarget::Project,
            Some(block(&[KEPT, LOST])),
            &block(&[KEPT, GAINED]),
        );
        let shown = text(&diff_lines(&changed, &theme::current()));
        assert_eq!(
            shown,
            format!("{DETAIL_INDENT}{REMOVED}{LOST}\n{DETAIL_INDENT}{ADDED}{GAINED}")
        );
    }

    #[test_case(None, NEW_NOTE; "created")]
    #[test_case(Some(KEPT), REWROTE_NOTE; "rewritten")]
    fn a_note_change_says_what_became_of_the_note(before: Option<&str>, expected: &str) {
        let changed = edit(0, LearningTarget::Memory, before.map(str::to_owned), GAINED);
        let shown = text(&diff_lines(&changed, &theme::current()));
        assert_eq!(shown, format!("{DETAIL_INDENT}{expected}"));
    }

    #[test]
    fn revert_asks_for_the_selected_change_once() {
        let mut first = edit(0, LearningTarget::Memory, None, KEPT);
        first.reverted_ms = Some(REVERTED_AT);
        let second = edit(1, LearningTarget::Memory, None, GAINED);
        let mut inspector = inspector(vec![run(vec![first, second.clone()])]);

        assert_eq!(
            press(&mut inspector, KeyCode::Char('u')),
            LearnAction::Flash(ALREADY_REVERTED)
        );
        press(&mut inspector, KeyCode::Right);
        assert_eq!(
            press(&mut inspector, KeyCode::Char('u')),
            LearnAction::Revert(second)
        );
    }

    #[test]
    fn revert_without_changes_says_so() {
        let mut inspector = inspector(vec![run(Vec::new())]);
        assert_eq!(
            press(&mut inspector, KeyCode::Char('u')),
            LearnAction::Flash(NOTHING_SELECTED)
        );
    }

    #[test_case(false => LearnAction::SetPaused(true); "running_pauses")]
    #[test_case(true => LearnAction::SetPaused(false); "paused_resumes")]
    fn pause_toggles_the_stored_state(paused: bool) -> LearnAction {
        press(&mut inspector_paused(paused), KeyCode::Char('p'))
    }

    #[test_case(false => LearnAction::RunNow; "running_runs")]
    #[test_case(true => LearnAction::Flash(PAUSED_NO_RUN); "paused_says_so")]
    fn run_now_respects_a_pause(paused: bool) -> LearnAction {
        press(&mut inspector_paused(paused), KeyCode::Char('n'))
    }

    fn inspector_paused(paused: bool) -> LearnInspector {
        let mut inspector = inspector(Vec::new());
        if let Some(Ok(snapshot)) = &mut inspector.snapshot {
            snapshot.state = Some(LearningProject {
                paused,
                ..LearningProject::default()
            });
        }
        inspector
    }

    #[test]
    fn a_reread_keeps_the_selected_run() {
        let older = run(Vec::new());
        let mut newer = run(Vec::new());
        newer.run.id = RUN_ID + 1;
        let mut inspector = inspector(vec![newer.clone(), older.clone()]);
        press(&mut inspector, KeyCode::Down);
        let mut newest = run(Vec::new());
        newest.run.id = RUN_ID + 2;
        inspector.fill(
            Ok(LearnSnapshot {
                project: PROJECT.to_owned(),
                state: None,
                runs: vec![newest, newer, older],
            }),
            true,
        );
        assert_eq!(inspector.selected_run().map(|run| run.run.id), Some(RUN_ID));
    }

    #[test]
    fn the_newest_selected_follows_a_run_that_starts() {
        let older = run(Vec::new());
        let mut inspector = inspector(vec![older.clone()]);
        let mut started = run(Vec::new());
        started.run.id = RUN_ID + 1;
        inspector.fill(
            Ok(LearnSnapshot {
                project: PROJECT.to_owned(),
                state: None,
                runs: vec![started, older],
            }),
            true,
        );
        assert_eq!(
            press(&mut inspector, KeyCode::Char('t')),
            LearnAction::Transcript(RUN_ID + 1)
        );
    }

    #[test]
    fn a_reread_of_the_same_runs_keeps_the_selected_change() {
        let edits = vec![
            edit(0, LearningTarget::Memory, None, KEPT),
            edit(1, LearningTarget::Memory, None, GAINED),
        ];
        let mut inspector = inspector(vec![run(edits.clone())]);
        press(&mut inspector, KeyCode::Right);
        inspector.fill(
            Ok(LearnSnapshot {
                project: PROJECT.to_owned(),
                state: None,
                runs: vec![run(edits.clone())],
            }),
            true,
        );
        assert_eq!(inspector.selected_edit(), edits.get(1));
    }

    #[test]
    fn no_runs_says_so() {
        let inspector = inspector(Vec::new());
        let body = inspector.body(&theme::current(), 0);
        assert!(text(&body.lines).contains(NO_RUNS));
        assert_eq!(body.focus(0, 0), None);
    }

    fn draw(inspector: &mut LearnInspector) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                inspector.view(frame, frame.area());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn mouse(kind: MouseEventKind, at: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A press and its release, answering whichever of the two acted.
    fn click(inspector: &mut LearnInspector, at: Position) -> LearnAction {
        let pressed = inspector.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at));
        let released = inspector.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), at));
        match pressed {
            LearnAction::Consumed => released,
            acted => acted,
        }
    }

    fn row_of(inspector: &LearnInspector, target: RowTarget) -> Position {
        let (rows, _) = inspector
            .hits
            .iter()
            .find(|(_, at)| *at == target)
            .expect(NOT_DRAWN);
        Position::new(
            inspector.content.x,
            inspector.content.y + rows.start - inspector.scroll.offset(),
        )
    }

    fn two_runs() -> LearnInspector {
        let older = run(Vec::new());
        let mut newer = run(Vec::new());
        newer.run.id = RUN_ID + 1;
        inspector(vec![newer, older])
    }

    #[test]
    fn a_click_selects_a_run_and_a_second_opens_its_transcript() {
        let mut inspector = two_runs();
        draw(&mut inspector);
        let older = row_of(&inspector, RowTarget::Run(1));

        assert_eq!(click(&mut inspector, older), LearnAction::Consumed);
        assert_eq!(inspector.selected_run().map(|run| run.run.id), Some(RUN_ID));
        draw(&mut inspector);
        let older = row_of(&inspector, RowTarget::Run(1));
        assert_eq!(
            click(&mut inspector, older),
            LearnAction::Transcript(RUN_ID)
        );
    }

    #[test]
    fn a_click_on_a_change_or_what_it_did_selects_it() {
        let edits = vec![
            edit(0, LearningTarget::Memory, None, KEPT),
            edit(1, LearningTarget::Memory, None, GAINED),
        ];
        let mut inspector = inspector(vec![run(edits.clone())]);
        draw(&mut inspector);
        let change = row_of(&inspector, RowTarget::Edit(1));
        let diff = Position::new(change.x, change.y + 1);

        click(&mut inspector, diff);
        assert_eq!(inspector.selected_edit(), edits.get(1));
    }

    #[test]
    fn the_row_under_the_pointer_is_marked() {
        let mut inspector = two_runs();
        draw(&mut inspector);
        let (newer, older) = (
            row_of(&inspector, RowTarget::Run(0)),
            row_of(&inspector, RowTarget::Run(1)),
        );
        inspector.handle_mouse(mouse(MouseEventKind::Moved, older));
        let buffer = draw(&mut inspector);

        let reversed = |at: Position| buffer[at].modifier.contains(Modifier::REVERSED);
        assert!(reversed(older));
        assert!(!reversed(newer));
    }

    #[test_case(TRANSCRIPT_HINT => LearnAction::Transcript(RUN_ID + 1); "transcript")]
    #[test_case(CLOSE_HINT => LearnAction::Close; "close")]
    #[test_case(GROUP_HINT => LearnAction::Consumed; "group_hint_is_inert")]
    fn a_footer_hint_presses_its_key(hint: usize) -> LearnAction {
        let mut inspector = two_runs();
        draw(&mut inspector);
        let footer = Rect {
            y: inspector.content.bottom(),
            height: 1,
            ..inspector.content
        };
        let hit = hint_hits(HINTS, footer)[hint];
        click(&mut inspector, hit.as_position())
    }

    #[test]
    fn every_hint_fits_the_narrowest_modal() {
        let width = Modal::inner_width(NARROW_TERMINAL, WIDTH_PERCENT).saturating_sub(H_PAD * 2);
        let footer = Rect::new(0, 0, width, 1);
        assert_eq!(hint_hits(HINTS, footer).len(), HINTS.len());
    }
}
