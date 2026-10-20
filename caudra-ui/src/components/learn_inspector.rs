//! `/learn`: the project's learning runs, newest first, with the selected
//! run opened under its row to show what it changed and what it dropped.
//! Reads and writes go through the app, which answers on a later tick.

use std::mem;

use caudra_agent::learning::apply::Dropped;
use caudra_agent::learning::pump::{CONFLICT, FAILED, LEARNED, PARTIAL};
use caudra_agent::learning::targets::bullets;
use caudra_config::LearningTarget;
use caudra_grab::grab_scope;
use caudra_providers::token_label;
use caudra_storage::learning::{LearningProject, LearningRun, StoredEdit};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
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
    Hint, ModalScroll, Overlay, escape_terminal_controls, format_elapsed, hint_line_hovered,
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
const HINTS: &[Hint] = &[
    Hint::inert("\u{2191}\u{2193}", "run"),
    Hint::inert("\u{2190}\u{2192}", "change"),
    Hint::char("n", "run now"),
    Hint::char("u", "revert"),
    Hint::char("p", "pause/resume"),
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
    Flash(&'static str),
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

    /// Lands a read, keeping the selected run and change where they still exist.
    pub fn fill(&mut self, snapshot: Result<LearnSnapshot, String>, live: bool) {
        let selected = self.selected_run().map(|run| run.run.id);
        self.live = live;
        self.snapshot = Some(snapshot);
        let runs = self.runs();
        match selected.and_then(|id| runs.iter().position(|run| run.run.id == id)) {
            Some(index) => self.run = index,
            None => {
                self.run = 0;
                self.edit = 0;
            }
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
            KeyCode::Char('n') => return LearnAction::RunNow,
            KeyCode::Char('u') => return self.revert(),
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

    fn select_run(&mut self, delta: isize) {
        let last = self.runs().len().saturating_sub(1);
        let run = self.run.saturating_add_signed(delta).min(last);
        if run != self.run {
            self.run = run;
            self.edit = 0;
        }
        self.follow = true;
    }

    fn select_edit(&mut self, delta: isize) {
        let last = self
            .selected_run()
            .map_or(0, |run| run.edits.len().saturating_sub(1));
        self.edit = self.edit.saturating_add_signed(delta).min(last);
        self.follow = true;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return;
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::ScrollDown => self.scroll(1),
            _ => {}
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("learn_inspector", area);

        let theme = theme::current();
        let (lines, focus) = self.lines(&theme, Timestamp::now().as_millisecond());
        let width = Modal::inner_width(area.width, WIDTH_PERCENT)
            .saturating_sub(H_PAD * 2)
            .max(1);
        let wrapped = |lines: &[Line<'static>]| {
            let rows = Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .line_count(width);
            u16::try_from(rows).unwrap_or(u16::MAX)
        };
        let total = wrapped(&lines).min(u16::MAX - CHROME_LINES - FOOTER_ROWS);
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
            && let Some(focus) = focus
        {
            let top = wrapped(&lines[..focus]);
            let height = wrapped(&lines[focus..=focus]);
            self.scroll.reveal_and_hold(top, height);
        }
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
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
        frame.render_widget(Paragraph::new(hint_line_hovered(HINTS, None)), footer);

        self.popup = popup;
        popup
    }

    /// The modal's lines, and which of them the selection is on.
    fn lines(&self, theme: &Theme, now_ms: i64) -> (Vec<Line<'static>>, Option<usize>) {
        let snapshot = match &self.snapshot {
            None => return (vec![Line::styled(READING, theme.status_dim)], None),
            Some(Err(error)) => {
                let text = format!("{READ_FAILED}{}", escape_terminal_controls(error));
                return (vec![Line::styled(text, theme.tool_error)], None);
            }
            Some(Ok(snapshot)) => snapshot,
        };
        let mut lines = header_lines(snapshot, self.live, now_ms, theme);
        lines.push(Line::default());
        if snapshot.runs.is_empty() {
            lines.push(Line::styled(NO_RUNS, theme.status_dim));
            return (lines, None);
        }
        let mut focus = None;
        for (index, run) in snapshot.runs.iter().enumerate() {
            let selected = index == self.run;
            if selected {
                focus = Some(lines.len());
            }
            lines.push(run_line(run, selected, theme));
            if selected {
                let edit_focus = detail_lines(run, self.edit, theme, &mut lines);
                focus = edit_focus.or(focus);
            }
        }
        (lines, focus)
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

/// Pushes the run's summary, changes and dropped items, and answers the line
/// of the selected change.
fn detail_lines(
    run: &LearnRun,
    selected: usize,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
) -> Option<usize> {
    if !run.run.summary.is_empty() {
        lines.push(Line::styled(
            format!(
                "{EDIT_INDENT}{}",
                escape_terminal_controls(&run.run.summary)
            ),
            theme.tool_dim,
        ));
    }
    let mut focus = None;
    for (index, edit) in run.edits.iter().enumerate() {
        if index == selected {
            focus = Some(lines.len());
        }
        lines.push(edit_line(edit, index == selected, theme));
        lines.extend(diff_lines(edit, theme));
    }
    if !run.dropped.is_empty() {
        lines.push(Line::styled(
            format!("{EDIT_INDENT}{DROPPED_SECTION}"),
            theme.keybind_key,
        ));
        for dropped in &run.dropped {
            lines.push(Line::from(vec![
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
            ]));
        }
    }
    focus
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
    use test_case::test_case;

    use super::{
        ADDED, ALREADY_REVERTED, DETAIL_INDENT, KeyCode, KeyEvent, LEARNED, LearnAction,
        LearnInspector, LearnRun, LearnSnapshot, LearningProject, LearningRun, LearningTarget,
        Line, NEW_NOTE, NO_CHANGES, NO_RUNS, NOTHING_SELECTED, REMOVED, REWROTE_NOTE, StoredEdit,
        diff_lines, run_line, theme,
    };

    const RUN_ID: i64 = 7;
    const PROJECT: &str = "/repo";
    const AGENTS: &str = "/repo/AGENTS.md";
    const NOTE: &str = "build.md";
    const KEPT: &str = "Use make check";
    const GAINED: &str = "Run nextest per crate";
    const LOST: &str = "Prefer cargo test";
    const REVERTED_AT: i64 = 5;

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
        let mut inspector = inspector(Vec::new());
        if let Some(Ok(snapshot)) = &mut inspector.snapshot {
            snapshot.state = Some(LearningProject {
                paused,
                ..LearningProject::default()
            });
        }
        press(&mut inspector, KeyCode::Char('p'))
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
    fn no_runs_says_so() {
        let inspector = inspector(Vec::new());
        let (lines, focus) = inspector.lines(&theme::current(), 0);
        assert!(text(&lines).contains(NO_RUNS));
        assert_eq!(focus, None);
    }
}
