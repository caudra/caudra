//! The `/shells` modal: every native shell command the session's agents ran,
//! in the foreground or the background, running ones first.
//!
//! It keeps no shell state of its own. The app hands it the tracker's snapshot
//! and the background runtime's cards as [`ShellInputs`], and the rows are
//! rebuilt only when those change. Output is shown as the command wrote it,
//! with terminal controls escaped and no Markdown.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use caudra_agent::background::{ShellLive, ShellOutputView, ShellSnapshot, ShellView};
use caudra_agent::{ShellOutput, SnapshotLine, TaskCard, ToolOutput, format_settled_duration};
use caudra_grab::grab_scope;
use caudra_highlight::Highlighter;
use caudra_storage::auth::now_millis;
use caudra_storage::background::{JobKind, JobOwner};
use caudra_storage::now_epoch;
use caudra_storage::shell_history::ShellExecutionOwner;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::code_view::{WrappedRows, highlight_spans};
use crate::components::keybindings::{Bind, key};
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::task_card::{self, fact, literal_body, literal_code, status_style};
use crate::components::{
    HISTORY_HINTS, Hint, HintBar, ModalScroll, Overlay, escape_terminal_controls, format_elapsed,
};
use crate::repaint::{Cadence, Dirty, Watch};
use crate::theme;

const TITLE: &str = " Shell ";
const SHELL_LANGUAGE: &str = "bash";
/// The most of a row's label that is coloured. A row draws one line cut to the
/// modal's width, so only its head is parsed, and the rest keeps the row's
/// style.
const MAX_HIGHLIGHTED_LABEL_BYTES: usize = 256;
const MAX_VISIBLE: u16 = 15;
const WIDTH_PERCENT: u16 = 85;
const MAX_HEIGHT_PERCENT: u16 = 80;
const H_PAD: u16 = 1;
const INDENT: &str = "  ";
const EMPTY_TEXT: &str = "No shell commands yet";
const RUNNING_SECTION: &str = "Running";
const FINISHED_SECTION: &str = "Finished";
const BACKGROUND_BADGE: &str = "bg";
const SEPARATOR: &str = " · ";
const CANCELLING: &str = "cancelling";
const MAIN_OWNER: &str = "main";
const AGENT_OWNER: &str = "agent";
const WORKFLOW_OWNER: &str = "workflow";
const FOREGROUND: &str = "foreground";
const BACKGROUND: &str = "background";
const LOCAL: &str = "local";
const REMOTE: &str = "remote";
const STATE_LABEL: &str = "State";
const OWNER_LABEL: &str = "Owner";
const WORKDIR_LABEL: &str = "Workdir";
const TIMEOUT_LABEL: &str = "Timeout";
const ELAPSED_LABEL: &str = "Elapsed";
const EXIT_LABEL: &str = "Exit";
const REASON_LABEL: &str = "Reason";
const HISTORY_LABEL: &str = "History";
const ID_LABEL: &str = "Id";
const EXIT_CODE: &str = "code";
const SIGNAL: &str = "signal";
const TIMED_OUT: &str = "timed out";
const OUTPUT_LIMIT: &str = "output limit exceeded";
const COMMAND_HEADING: &str = "Command";
const OUTPUT_HEADING: &str = "Output";
const COMMAND_CUT: &str = "History keeps only the start of this command.";
const OUTPUT_TAIL: &str = "Only the end of this output was kept.";
const NO_OUTPUT_YET: &str = "No output yet.";
const EMPTY_OUTPUT: &str = "The command printed nothing.";
const LOADING_OUTPUT: &str = "Loading output…";
const NO_OUTPUT_KEPT: &str = "No output was kept.";
const SHELL_RESULT_FIELD: &str = "shell";
const MILLIS_PER_SECOND: u64 = 1_000;
/// The furthest a scroll offset reaches. Longer output keeps its end, which is
/// where a command reports how it went.
const MAX_BODY_ROWS: usize = u16::MAX as usize;
const DETAILS: &str = "details";
const CLOSE: &str = "close";
const BACK: &str = "back";
const STOP_COMMAND: &str = "stop command";
const SCROLL_HINT: Hint = Hint::inert("↑↓", "scroll");
const STOP: Bind = Bind {
    code: KeyCode::Char('k'),
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+K",
};

/// What the rows are built from, kept so a repeat is told apart from a change
/// without rebuilding anything.
#[derive(Default)]
pub(crate) struct ShellInputs {
    pub(crate) snapshot: Option<Arc<ShellSnapshot>>,
    /// Every background card: shells become rows, agents name their owners.
    pub(crate) cards: Vec<TaskCard>,
}

impl ShellInputs {
    fn same(&self, other: &Self) -> bool {
        self.snapshot.as_ref().map(Arc::as_ptr) == other.snapshot.as_ref().map(Arc::as_ptr)
            && self.cards == other.cards
    }
}

#[must_use]
pub enum ShellModalAction {
    Consumed,
    History {
        older: bool,
    },
    Stop(ShellStop),
    /// The details page needs what an earlier runtime kept of this execution.
    LoadOutput(String),
    Copy(String),
}

/// Exactly the execution the row showed, so a stop can never land on another.
pub enum ShellStop {
    Foreground(String),
    Background(Box<TaskCard>),
}

enum ShellSource {
    Foreground(Arc<ShellView>),
    Background {
        task: Box<TaskCard>,
        live: Option<ShellLive>,
        output: Option<Box<ToolOutput>>,
    },
}

pub struct ShellItem {
    id: String,
    label: String,
    owner: String,
    detail: String,
    search: String,
    section: Option<&'static str>,
    source: ShellSource,
    /// The label's colours and the theme generation they were painted for,
    /// filled the first time the row is drawn so a row never shown is never
    /// parsed.
    coloured: RefCell<Option<(u64, Vec<Span<'static>>)>>,
}

impl ShellItem {
    fn new(id: String, owner: String, source: ShellSource) -> Self {
        let mut item = Self {
            id,
            label: String::new(),
            owner,
            detail: String::new(),
            search: String::new(),
            section: None,
            source,
            coloured: RefCell::default(),
        };
        let label = match item.command().lines().next() {
            Some(line) if !line.trim().is_empty() => escape_terminal_controls(line),
            _ => item.id.clone(),
        };
        let search = format!(
            "{} {} {} {}",
            item.command(),
            item.owner,
            item.state(),
            item.id
        );
        let detail = format!("{}{SEPARATOR}{}", item.owner, item.state());
        item.label = label;
        item.search = search;
        item.detail = detail;
        item
    }

    fn foreground(view: &Arc<ShellView>) -> Self {
        Self::new(
            view.record.execution_id.clone(),
            foreground_owner(&view.record.owner),
            ShellSource::Foreground(Arc::clone(view)),
        )
    }

    fn background(
        task: &TaskCard,
        cards: &[TaskCard],
        jobs: Option<&BTreeMap<String, ShellLive>>,
    ) -> Self {
        let output = task
            .result
            .as_ref()
            .and_then(|result| result.get(SHELL_RESULT_FIELD))
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        Self::new(
            task.task_id.clone(),
            background_owner(task, cards),
            ShellSource::Background {
                task: Box::new(task.clone()),
                live: jobs.and_then(|jobs| jobs.get(&task.invocation_id)).cloned(),
                output,
            },
        )
    }

    fn command(&self) -> &str {
        match &self.source {
            ShellSource::Foreground(view) => &view.record.command,
            ShellSource::Background { task, .. } => task
                .shell
                .as_ref()
                .map_or(task.label.as_str(), |shell| shell.command.as_str()),
        }
    }

    fn state(&self) -> &str {
        match &self.source {
            ShellSource::Foreground(view) => view.record.state.as_str(),
            ShellSource::Background { task, .. } => &task.state,
        }
    }

    fn active(&self) -> bool {
        match &self.source {
            ShellSource::Foreground(view) => view.record.state.is_active(),
            ShellSource::Background { task, .. } => task.active(),
        }
    }

    /// Live and not already on its way out.
    fn stoppable(&self) -> bool {
        self.active() && self.state() != CANCELLING
    }

    fn is_background(&self) -> bool {
        matches!(self.source, ShellSource::Background { .. })
    }

    fn live(&self) -> Option<&ShellLive> {
        match &self.source {
            ShellSource::Foreground(view) => match &view.output {
                ShellOutputView::Live(live) => Some(live),
                _ => None,
            },
            ShellSource::Background { live, .. } => live.as_ref(),
        }
    }

    fn stored(&self) -> bool {
        matches!(&self.source, ShellSource::Foreground(view) if matches!(view.output, ShellOutputView::Stored))
    }

    fn stop(&self) -> ShellStop {
        match &self.source {
            ShellSource::Foreground(view) => {
                ShellStop::Foreground(view.record.execution_id.clone())
            }
            ShellSource::Background { task, .. } => ShellStop::Background(task.clone()),
        }
    }
}

impl PickerItem for ShellItem {
    fn label(&self) -> &str {
        &self.label
    }

    fn search_text(&self) -> &str {
        &self.search
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn badge(&self) -> Option<&str> {
        self.is_background().then_some(BACKGROUND_BADGE)
    }

    /// None for a row named by its id, which stands in for a blank first line
    /// and is not shell.
    fn label_colours(&self) -> Option<Vec<Span<'static>>> {
        if self.label == self.id {
            return None;
        }
        let generation = theme::generation();
        let mut coloured = self.coloured.borrow_mut();
        if coloured
            .as_ref()
            .is_none_or(|(painted, _)| *painted != generation)
        {
            let head = &self.label[..self.label.floor_char_boundary(MAX_HIGHLIGHTED_LABEL_BYTES)];
            let spans = highlight_spans(&mut Highlighter::for_token(SHELL_LANGUAGE), head);
            *coloured = Some((generation, spans));
        }
        coloured.as_ref().map(|(_, spans)| spans.clone())
    }

    fn section(&self) -> Option<&str> {
        self.section
    }

    fn is_spinning(&self) -> bool {
        self.active()
    }
}

/// One execution's details, in place of the list until Escape.
struct DetailsPage {
    id: String,
    command: CommandRows,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    hints: HintBar,
    popup: Rect,
}

/// A command's coloured rows, kept for the width and theme they were painted
/// for. The command never changes, but its page is drawn on every frame,
/// including one for each change to the output below it.
#[derive(Default)]
struct CommandRows {
    key: Option<(u16, u64)>,
    lines: Vec<Line<'static>>,
}

impl CommandRows {
    fn rows(&mut self, command: &str, width: u16) -> &[Line<'static>] {
        let key = (width, theme::generation());
        if self.key != Some(key) {
            self.lines = literal_code(command, SHELL_LANGUAGE, width);
            self.key = Some(key);
        }
        &self.lines
    }
}

pub struct ShellModal {
    picker: ListPicker<ShellItem>,
    inputs: ShellInputs,
    page: Option<DetailsPage>,
    /// The shown execution's live output, as the last poll found it.
    live: Watch<Vec<SnapshotLine>>,
}

impl ShellModal {
    pub fn new() -> Self {
        let mut picker = ListPicker::new()
            .with_width_percent(WIDTH_PERCENT)
            .with_max_visible(MAX_VISIBLE)
            .with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            inputs: ShellInputs::default(),
            page: None,
            live: Watch::default(),
        }
    }

    pub fn open(&mut self, inputs: ShellInputs) {
        self.inputs = inputs;
        self.picker.open(build_items(&self.inputs), TITLE);
        self.close_page();
    }

    /// Rebuilds the rows when what they are built from changed. The selection
    /// and an open details page follow their execution by id, so a command
    /// moving from Running to Finished does not drag the cursor with it.
    pub fn refresh(&mut self, inputs: ShellInputs) -> bool {
        if !self.picker.is_open() || self.inputs.same(&inputs) {
            return false;
        }
        let selected = self.selected_id();
        self.inputs = inputs;
        self.picker.replace_items(build_items(&self.inputs));
        if let Some(id) = selected {
            self.picker.select_item_by(|item| item.id == id);
        }
        if self
            .page
            .as_ref()
            .is_some_and(|page| find(&self.picker, &page.id).is_none())
        {
            self.close_page();
        }
        true
    }

    /// Opens the details of one execution, as a status command asks for it.
    /// `None` when no row shows it.
    pub fn show(&mut self, id: &str) -> Option<ShellModalAction> {
        self.picker.clear_search();
        self.picker
            .select_item_by(|item| item.id == id)
            .then(|| self.open_page(id.to_owned()))
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.inputs = ShellInputs::default();
        self.close_page();
    }

    pub fn contains(&self, pos: Position) -> bool {
        match &self.page {
            Some(page) => page.popup.contains(pos),
            None => self.picker.contains(pos),
        }
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        match &self.page {
            Some(page) => Some(page.id.clone()),
            None => self.picker.selected_item().map(|item| item.id.clone()),
        }
    }

    /// Re-reads the shown execution's live output. Only the details page shows
    /// output, so the list costs nothing to keep open.
    pub fn poll_live(&mut self) -> Dirty {
        let latest = self
            .page
            .as_ref()
            .and_then(|page| find(&self.picker, &page.id))
            .and_then(ShellItem::live)
            .and_then(ShellLive::lines);
        self.live.poll(latest)
    }

    pub fn scroll(&mut self, delta: i32) {
        match &mut self.page {
            Some(page) => page.scroll.scroll(delta),
            None => self.picker.scroll(delta),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ShellModalAction {
        let older = key::OLDER_HISTORY.matches(key);
        if self.page.is_none() && (older || key::RECENT_HISTORY.matches(key)) {
            return ShellModalAction::History { older };
        }
        if STOP.matches(key) {
            return self
                .selected_id()
                .and_then(|id| find(&self.picker, &id))
                .filter(|item| item.stoppable())
                .map_or(ShellModalAction::Consumed, |item| {
                    ShellModalAction::Stop(item.stop())
                });
        }
        if let Some(page) = &mut self.page {
            if key::ESC.matches(key) {
                self.close_page();
            } else {
                page.scroll.handle_key(key);
            }
            return ShellModalAction::Consumed;
        }
        if key::ENTER.matches(key) {
            return match self.picker.selected_item() {
                Some(item) => self.open_page(item.id.clone()),
                None => ShellModalAction::Consumed,
            };
        }
        let search = self.picker.search_text();
        let action = self.picker.handle_key(key);
        self.map_action(action, &search)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ShellModalAction {
        if let Some(page) = &mut self.page {
            match page.scrollbar.handle(&event) {
                ScrollbarMouse::Ignored => {}
                ScrollbarMouse::Consumed => return ShellModalAction::Consumed,
                ScrollbarMouse::ScrollTo(top) => {
                    page.scroll.scroll_to(top as u16);
                    return ShellModalAction::Consumed;
                }
            }
            return match page.hints.handle_mouse(event) {
                Some(key) => self.handle_key(key),
                None => ShellModalAction::Consumed,
            };
        }
        let search = self.picker.search_text();
        let action = self.picker.handle_mouse(event);
        self.map_action(action, &search)
    }

    /// The details page has nothing to paste into, but it still owns the
    /// screen, so a paste never reaches the composer behind it.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        match self.page {
            Some(_) => self.picker.is_open(),
            None => self.picker.handle_paste(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("shell_modal", area);
        if self.page.is_some() {
            return self.view_page(frame, area);
        }
        let stoppable = self
            .picker
            .selected_item()
            .is_some_and(ShellItem::stoppable);
        self.picker
            .set_footer_builder(if stoppable { running_footer } else { footer });
        self.picker.view(frame, area)
    }

    fn view_page(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Self {
            picker, page, live, ..
        } = self;
        let Some(page) = page.as_mut() else {
            return Rect::default();
        };
        let Some(item) = find(picker, &page.id) else {
            return Rect::default();
        };
        let width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);
        let header = header_lines(item, width);
        let command = page.command.rows(item.command(), width);
        let mut body = body_lines(item, command, live.get().map(Vec::as_slice), width);
        if body.len() > MAX_BODY_ROWS {
            body.drain(..body.len() - MAX_BODY_ROWS);
        }
        let header_rows = header.len() + 1;
        let footer_rows = 1;
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(
            frame,
            area,
            u16::try_from(header_rows + body.len() + footer_rows).unwrap_or(u16::MAX),
        );
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        let [header_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(
                u16::try_from(header_rows)
                    .unwrap_or(u16::MAX)
                    .min(padded.height / 2),
            ),
            Constraint::Min(0),
            Constraint::Length(footer_rows as u16),
        ])
        .areas(padded);
        frame.render_widget(Paragraph::new(header), header_area);
        let total = body.len() as u16;
        page.scroll.update_dimensions(total, body_area.height);
        let offset = page.scroll.offset();
        let shown: Vec<_> = body
            .into_iter()
            .skip(usize::from(offset))
            .take(usize::from(body_area.height))
            .collect();
        frame.render_widget(Paragraph::new(shown), body_area);
        page.scrollbar.draw(
            frame,
            Rect {
                x: inner.x,
                width: inner.width,
                ..body_area
            },
            total,
            offset,
        );
        page.hints
            .draw(frame, footer_area, page_hints(item.stoppable()));
        page.popup = popup;
        popup
    }

    fn open_page(&mut self, id: String) -> ShellModalAction {
        let Some(item) = find(&self.picker, &id) else {
            return ShellModalAction::Consumed;
        };
        let action = if item.stored() {
            ShellModalAction::LoadOutput(id.clone())
        } else {
            ShellModalAction::Consumed
        };
        let scroll = if item.active() {
            ModalScroll::new()
        } else {
            ModalScroll::new_top()
        };
        self.page = Some(DetailsPage {
            id,
            command: CommandRows::default(),
            scroll,
            scrollbar: Scrollbar::default(),
            hints: HintBar::default(),
            popup: Rect::default(),
        });
        self.live = Watch::default();
        let _ = self.poll_live();
        action
    }

    fn close_page(&mut self) {
        self.page = None;
        self.live = Watch::default();
    }

    /// A committed row closes a list picker, and this one stays open behind
    /// the page it opens, so the list is rebuilt as it stood.
    fn map_action(&mut self, action: PickerAction<ShellItem>, search: &str) -> ShellModalAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => ShellModalAction::Consumed,
            PickerAction::Select(item) => {
                self.picker.open(build_items(&self.inputs), TITLE);
                self.picker.set_search_text(search);
                self.picker.select_item_by(|row| row.id == item.id);
                self.open_page(item.id)
            }
            PickerAction::Close => {
                self.close();
                ShellModalAction::Consumed
            }
            PickerAction::Key(key) => self.handle_key(key),
            PickerAction::Copy(text) => ShellModalAction::Copy(text),
        }
    }
}

impl Overlay for ShellModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        match &self.page {
            Some(page) => Cadence::when(
                find(&self.picker, &page.id).is_some_and(ShellItem::active),
                Cadence::CLOCK,
            ),
            None => self.picker.cadence(),
        }
    }
}

/// The running rows the modal would list, for the chip that opens it.
pub(crate) fn active_count(snapshot: Option<&ShellSnapshot>, cards: &[TaskCard]) -> usize {
    snapshot.map_or(0, ShellSnapshot::active_count)
        + cards
            .iter()
            .filter(|task| task.kind == JobKind::Shell && task.active())
            .count()
}

fn find<'a>(picker: &'a ListPicker<ShellItem>, id: &str) -> Option<&'a ShellItem> {
    (0..)
        .map_while(|index| picker.item(index))
        .find(|item| item.id == id)
}

/// Running commands oldest first, so a long one keeps its place, then finished
/// ones newest first.
fn build_items(inputs: &ShellInputs) -> Vec<ShellItem> {
    let (mut running, mut finished) = (Vec::new(), Vec::new());
    for view in inputs
        .snapshot
        .iter()
        .flat_map(|snapshot| &snapshot.executions)
    {
        let record = &view.record;
        if record.state.is_active() {
            running.push((record.created_at_ms, ShellItem::foreground(view)));
        } else {
            finished.push((
                record.finished_at_ms.unwrap_or(record.created_at_ms),
                ShellItem::foreground(view),
            ));
        }
    }
    let jobs = inputs.snapshot.as_ref().map(|snapshot| &snapshot.jobs);
    for task in inputs
        .cards
        .iter()
        .filter(|task| task.kind == JobKind::Shell)
    {
        let item = ShellItem::background(task, &inputs.cards, jobs);
        if task.active() {
            running.push((task.created_at.saturating_mul(MILLIS_PER_SECOND), item));
        } else {
            finished.push((task.updated_at.saturating_mul(MILLIS_PER_SECOND), item));
        }
    }
    running.sort_by_key(|(started, _)| *started);
    finished.sort_by_key(|(ended, _)| Reverse(*ended));
    for (heading, bucket) in [
        (RUNNING_SECTION, &mut running),
        (FINISHED_SECTION, &mut finished),
    ] {
        if let Some((_, first)) = bucket.first_mut() {
            first.section = Some(heading);
        }
    }
    running
        .into_iter()
        .chain(finished)
        .map(|(_, item)| item)
        .collect()
}

fn foreground_owner(owner: &ShellExecutionOwner) -> String {
    let agent = match (&owner.task_id, &owner.job) {
        (Some(task_id), _) => task_id.clone(),
        (None, JobOwner::Main) => MAIN_OWNER.to_owned(),
        (None, JobOwner::Child { .. }) => AGENT_OWNER.to_owned(),
    };
    match owner.workflow {
        Some(_) => format!("{agent}{SEPARATOR}{WORKFLOW_OWNER}"),
        None => agent,
    }
}

fn background_owner(task: &TaskCard, cards: &[TaskCard]) -> String {
    match &task.owner {
        JobOwner::Main => MAIN_OWNER.to_owned(),
        JobOwner::Child { invocation_id } => cards
            .iter()
            .find(|card| card.kind == JobKind::Agent && card.invocation_id == *invocation_id)
            .map_or_else(|| AGENT_OWNER.to_owned(), |card| card.task_id.clone()),
    }
}

fn header_lines(item: &ShellItem, width: u16) -> Vec<Line<'static>> {
    let t = theme::current();
    let state = item.state();
    let placement = if item.is_background() {
        BACKGROUND
    } else {
        FOREGROUND
    };
    let mut facts = vec![
        fact(
            STATE_LABEL,
            &format!("{state}{SEPARATOR}{placement}"),
            status_style(state),
        ),
        fact(OWNER_LABEL, &item.owner, t.tool),
    ];
    match &item.source {
        ShellSource::Foreground(view) => {
            let record = &view.record;
            let workdir = match record.remote {
                Some(remote) => format!(
                    "{}{SEPARATOR}{}",
                    record.workdir,
                    if remote { REMOTE } else { LOCAL }
                ),
                None => record.workdir.clone(),
            };
            facts.push(fact(WORKDIR_LABEL, &workdir, t.tool));
            if let Some(timeout) = record.timeout_ms {
                facts.push(fact(
                    TIMEOUT_LABEL,
                    &format_settled_duration(Duration::from_millis(timeout)),
                    t.tool,
                ));
            }
            let elapsed = match record.finished_at_ms {
                Some(finished) => format_settled_duration(Duration::from_millis(
                    finished.saturating_sub(record.created_at_ms),
                )),
                None => format_elapsed(
                    now_millis().saturating_sub(record.created_at_ms) / MILLIS_PER_SECOND,
                ),
            };
            facts.push(fact(ELAPSED_LABEL, &elapsed, t.tool));
            if let ShellOutputView::Kept(output) = &view.output
                && let ToolOutput::Shell(shell) = output.as_ref()
            {
                facts.extend(exit_fact(shell, state));
            }
            if let Some(reason) = &record.reason {
                facts.push(fact(REASON_LABEL, reason, t.tool));
            }
            if let Some(error) = &view.history_error {
                facts.push(fact(HISTORY_LABEL, error, t.tool_error));
            }
        }
        ShellSource::Background { task, output, .. } => {
            if let Some(shell) = &task.shell {
                facts.push(fact(WORKDIR_LABEL, &shell.workdir, t.tool));
                facts.push(fact(
                    TIMEOUT_LABEL,
                    &format_settled_duration(Duration::from_millis(shell.timeout_ms)),
                    t.tool,
                ));
            }
            let end = if task.active() {
                now_epoch()
            } else {
                task.updated_at
            };
            facts.push(fact(
                ELAPSED_LABEL,
                &format_elapsed(end.saturating_sub(task.created_at)),
                t.tool,
            ));
            if let Some(ToolOutput::Shell(shell)) = output.as_deref() {
                facts.extend(exit_fact(shell, state));
            }
        }
    }
    facts.push(fact(ID_LABEL, &item.id, t.tool_dim));
    WrappedRows::new(facts, 0, width).lines()
}

fn exit_fact(shell: &ShellOutput, state: &str) -> Option<Line<'static>> {
    let mut parts = Vec::new();
    if let Some(code) = shell.exit_code {
        parts.push(format!("{EXIT_CODE} {code}"));
    }
    if let Some(signal) = shell.signal {
        parts.push(format!("{SIGNAL} {signal}"));
    }
    if shell.timed_out {
        parts.push(TIMED_OUT.to_owned());
    }
    if shell.output_limit_exceeded {
        parts.push(OUTPUT_LIMIT.to_owned());
    }
    (!parts.is_empty()).then(|| fact(EXIT_LABEL, &parts.join(SEPARATOR), status_style(state)))
}

fn body_lines(
    item: &ShellItem,
    command: &[Line<'static>],
    live: Option<&[SnapshotLine]>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(COMMAND_HEADING)];
    lines.extend_from_slice(command);
    if matches!(&item.source, ShellSource::Foreground(view) if view.record.command_truncated) {
        lines.push(notice(COMMAND_CUT));
    }
    lines.push(Line::default());
    lines.push(heading(OUTPUT_HEADING));
    lines.extend(output_lines(item, live, width));
    lines
}

fn output_lines(item: &ShellItem, live: Option<&[SnapshotLine]>, width: u16) -> Vec<Line<'static>> {
    if item.active() {
        return match live.filter(|lines| !lines.is_empty()) {
            Some(lines) => literal_body(&live_text(lines), width).0,
            None => vec![notice(NO_OUTPUT_YET)],
        };
    }
    match &item.source {
        ShellSource::Foreground(view) => match &view.output {
            ShellOutputView::Stored | ShellOutputView::Loading => vec![notice(LOADING_OUTPUT)],
            ShellOutputView::Kept(output) => kept_lines(output, width),
            ShellOutputView::Live(_) | ShellOutputView::Missing => vec![notice(NO_OUTPUT_KEPT)],
        },
        ShellSource::Background {
            output: Some(output),
            ..
        } => kept_lines(output, width),
        ShellSource::Background { task, .. } => {
            let (lines, _) = task_card::details(task, width);
            if lines.is_empty() {
                vec![notice(NO_OUTPUT_KEPT)]
            } else {
                lines
            }
        }
    }
}

fn kept_lines(output: &ToolOutput, width: u16) -> Vec<Line<'static>> {
    let (text, tail) = match output {
        ToolOutput::Shell(shell) => (
            shell.raw_text(),
            shell.stdout_preview_truncated
                || shell.stderr_preview_truncated
                || shell.stdout_capture_truncated
                || shell.stderr_capture_truncated,
        ),
        other => (other.as_text(), false),
    };
    if text.is_empty() {
        return vec![notice(EMPTY_OUTPUT)];
    }
    let mut lines = literal_body(&text, width).0;
    if tail {
        lines.push(notice(OUTPUT_TAIL));
    }
    lines
}

fn live_text(lines: &[SnapshotLine]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn heading(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, theme::current().keybind_key))
}

fn notice(text: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw(INDENT),
        Span::styled(text, theme::current().tool_dim),
    ])
}

fn footer() -> Vec<Hint> {
    footer_with(None)
}

fn running_footer() -> Vec<Hint> {
    footer_with(Some(Hint::bind(STOP, STOP_COMMAND)))
}

/// History paging comes last, so a narrow bar drops it before the stop control.
fn footer_with(stop: Option<Hint>) -> Vec<Hint> {
    [Hint::bind(key::ENTER, DETAILS), Hint::bind(key::ESC, CLOSE)]
        .into_iter()
        .chain(stop)
        .chain(HISTORY_HINTS)
        .collect()
}

fn page_hints(stoppable: bool) -> Vec<Hint> {
    let mut hints = vec![Hint::bind(key::ESC, BACK), SCROLL_HINT];
    if stoppable {
        hints.push(Hint::bind(STOP, STOP_COMMAND));
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use caudra_storage::shell_history::{ShellExecutionRecord, ShellExecutionState};
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::HashSet;
    use test_case::test_case;

    const FIRST_ID: &str = "exec-first";
    const SECOND_ID: &str = "exec-second";
    const FINISHED_ID: &str = "exec-finished";
    const MISSING_ID: &str = "exec-missing";
    const JOB_ID: &str = "shell-cargo-test";
    const INVOCATION: &str = "job-invocation";
    const COMMAND: &str = "cargo test";
    const WORKDIR: &str = ".";
    const MODE: &str = "build";
    const PASTED: &str = "pasted";
    const STARTED_MS: u64 = 5_000;
    const JOB_STARTED_SECS: u64 = 1;
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 30;
    const RAW_OUTPUT: &str = "**bold** \u{1b}[31mred";
    const MARKDOWN_KEPT: &str = "**bold**";
    const CONTROL_ESCAPED: &str = "\\u{1b}[31mred";
    /// Wider than the narrow modal's command column, narrower than the wide one's.
    const COLOURED: &str = "echo \"done\" | grep -c done > out.txt";
    const COLOURED_TAIL: &str = "out.txt";
    const NARROW_WIDTH: u16 = 40;
    const FIRST_THEME: &str = "dracula";
    const SECOND_THEME: &str = "tokyonight";
    const NOT_DRAWN: &str = "the text is drawn whole on one row";
    const NOT_COLOURED: &str = "the command is drawn in more than one colour";
    const THEME_IGNORED: &str = "a theme switch recolours the command";
    const NOT_REWRAPPED: &str = "a narrower modal rewraps the command rather than clipping it";
    const ID_COLOURED: &str = "a row named by its id is not shell and keeps the row's style";

    fn record(id: &str, state: ShellExecutionState, created_at_ms: u64) -> ShellExecutionRecord {
        ShellExecutionRecord {
            execution_id: id.into(),
            owner: ShellExecutionOwner::default(),
            call_id: id.into(),
            command: COMMAND.into(),
            command_truncated: false,
            workdir: WORKDIR.into(),
            remote: Some(false),
            timeout_ms: Some(MILLIS_PER_SECOND),
            created_at_ms,
            finished_at_ms: (!state.is_active()).then_some(created_at_ms + 1),
            state,
            started: true,
            reason: None,
        }
    }

    fn view(record: ShellExecutionRecord, output: ShellOutputView) -> Arc<ShellView> {
        Arc::new(ShellView {
            record,
            output,
            history_error: None,
        })
    }

    fn running(id: &str, created_at_ms: u64) -> Arc<ShellView> {
        view(
            record(id, ShellExecutionState::Running, created_at_ms),
            ShellOutputView::Live(ShellLive::default()),
        )
    }

    fn finished(command: &str) -> Arc<ShellView> {
        let mut record = record(FINISHED_ID, ShellExecutionState::Succeeded, STARTED_MS);
        record.command = command.into();
        view(record, ShellOutputView::Missing)
    }

    fn job(state: &str) -> TaskCard {
        serde_json::from_value(serde_json::json!({
            "task_id": JOB_ID, "invocation_id": INVOCATION, "call_id": JOB_ID,
            "root_call_id": JOB_ID, "label": COMMAND, "state": state, "kind": "shell",
            "background": true, "mode": MODE, "generation": 1,
            "created_at": JOB_STARTED_SECS, "updated_at": JOB_STARTED_SECS,
            "shell": {
                "call_id": JOB_ID, "root_call_id": JOB_ID, "command": COMMAND,
                "workdir": WORKDIR, "timeout_ms": MILLIS_PER_SECOND, "mode": MODE
            }
        }))
        .unwrap()
    }

    fn shell_output(stdout: &str) -> ToolOutput {
        ToolOutput::Shell(ShellOutput {
            model_text: String::new(),
            relative_workdir: WORKDIR.into(),
            timeout_ms: MILLIS_PER_SECOND,
            duration_ms: 1,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 0,
            stdout_utf8_bytes: stdout.len() as u64,
            stderr_utf8_bytes: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: None,
        })
    }

    fn inputs(executions: Vec<Arc<ShellView>>, cards: Vec<TaskCard>) -> ShellInputs {
        ShellInputs {
            snapshot: Some(Arc::new(ShellSnapshot {
                executions,
                jobs: BTreeMap::new(),
            })),
            cards,
        }
    }

    fn opened(executions: Vec<Arc<ShellView>>, cards: Vec<TaskCard>) -> ShellModal {
        let mut modal = ShellModal::new();
        modal.open(inputs(executions, cards));
        modal
    }

    fn rows(modal: &ShellModal) -> Vec<&ShellItem> {
        (0..).map_while(|index| modal.picker.item(index)).collect()
    }

    fn draw(modal: &mut ShellModal, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn paint(modal: &mut ShellModal) -> String {
        draw(modal, WIDTH)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// Where `text` first appears whole on one row.
    fn locate(buffer: &Buffer, text: &str) -> Option<Position> {
        let glyphs: Vec<String> = text.chars().map(String::from).collect();
        let area = buffer.area;
        (area.top()..area.bottom()).find_map(|y| {
            let row: Vec<&str> = (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.windows(glyphs.len())
                .position(|cells| cells == glyphs.as_slice())
                .map(|x| Position::new(area.left() + x as u16, y))
        })
    }

    /// The colours of the cells that spell `text` where it first appears.
    fn colours_of(buffer: &Buffer, text: &str) -> Vec<Color> {
        let at = locate(buffer, text).expect(NOT_DRAWN);
        (at.x..at.x + text.chars().count() as u16)
            .map(|x| buffer[(x, at.y)].fg)
            .collect()
    }

    /// The colours `COLOURED` is drawn in once the theme is `name`.
    fn colours_under(modal: &mut ShellModal, name: &str) -> Vec<Color> {
        theme::set(theme::load_by_name(name).unwrap());
        colours_of(&draw(modal, WIDTH), COLOURED)
    }

    fn distinct(colours: &[Color]) -> usize {
        colours.iter().collect::<HashSet<_>>().len()
    }

    #[test]
    fn running_commands_list_oldest_first_above_newest_finished() {
        let modal = opened(
            vec![
                view(
                    record(FINISHED_ID, ShellExecutionState::Succeeded, STARTED_MS),
                    ShellOutputView::Stored,
                ),
                running(SECOND_ID, STARTED_MS + 2),
                running(FIRST_ID, STARTED_MS + 1),
            ],
            vec![job("succeeded")],
        );
        let listed = rows(&modal);
        let ids: Vec<_> = listed.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, [FIRST_ID, SECOND_ID, FINISHED_ID, JOB_ID]);
        let sections: Vec<_> = listed.iter().map(|row| row.section()).collect();
        assert_eq!(
            sections,
            [Some(RUNNING_SECTION), None, Some(FINISHED_SECTION), None]
        );
        let badges: Vec<_> = listed.iter().map(|row| row.badge()).collect();
        assert_eq!(badges, [None, None, None, Some(BACKGROUND_BADGE)]);
        assert_eq!(modal.selected_id().as_deref(), Some(FIRST_ID));
        assert_eq!(
            active_count(modal.inputs.snapshot.as_deref(), &modal.inputs.cards),
            listed.iter().filter(|row| row.active()).count()
        );
    }

    #[test_case(ShellExecutionState::Running, true; "running")]
    #[test_case(ShellExecutionState::Preparing, true; "preparing")]
    #[test_case(ShellExecutionState::Cancelling, false; "cancelling")]
    #[test_case(ShellExecutionState::Succeeded, false; "succeeded")]
    #[test_case(ShellExecutionState::Interrupted, false; "interrupted")]
    fn foreground_stop_names_only_a_live_execution(state: ShellExecutionState, stoppable: bool) {
        let mut modal = opened(
            vec![view(
                record(FIRST_ID, state, STARTED_MS),
                ShellOutputView::Missing,
            )],
            Vec::new(),
        );
        assert_eq!(
            matches!(
                modal.handle_key(STOP.to_key_event()),
                ShellModalAction::Stop(ShellStop::Foreground(id)) if id == FIRST_ID
            ),
            stoppable
        );
    }

    #[test_case("running", true; "running")]
    #[test_case("cancelling", false; "cancelling")]
    #[test_case("failed", false; "failed")]
    fn background_stop_carries_the_listed_invocation(state: &str, stoppable: bool) {
        let mut modal = opened(Vec::new(), vec![job(state)]);
        assert_eq!(
            matches!(
                modal.handle_key(STOP.to_key_event()),
                ShellModalAction::Stop(ShellStop::Background(task)) if task.invocation_id == INVOCATION
            ),
            stoppable
        );
    }

    #[test_case(key::OLDER_HISTORY, true; "older")]
    #[test_case(key::RECENT_HISTORY, false; "recent")]
    fn history_keys_page_the_list_but_not_the_details(bind: Bind, older: bool) {
        let mut modal = opened(vec![running(FIRST_ID, STARTED_MS)], Vec::new());
        assert!(matches!(
            modal.handle_key(bind.to_key_event()),
            ShellModalAction::History { older: paged } if paged == older
        ));
        let _ = modal.handle_key(key_event(KeyCode::Enter));
        assert!(matches!(
            modal.handle_key(bind.to_key_event()),
            ShellModalAction::Consumed
        ));
    }

    #[test]
    fn a_settling_command_keeps_the_cursor_and_its_details() {
        let mut modal = opened(
            vec![
                running(FIRST_ID, STARTED_MS),
                running(SECOND_ID, STARTED_MS + 1),
            ],
            Vec::new(),
        );
        assert!(matches!(
            modal.handle_key(key_event(KeyCode::Enter)),
            ShellModalAction::Consumed
        ));
        assert!(modal.page.is_some());
        let settled = Arc::new(ShellSnapshot {
            executions: vec![
                running(SECOND_ID, STARTED_MS + 1),
                view(
                    record(FIRST_ID, ShellExecutionState::Succeeded, STARTED_MS),
                    ShellOutputView::Kept(Arc::new(shell_output(COMMAND))),
                ),
            ],
            jobs: BTreeMap::new(),
        });
        let same = || ShellInputs {
            snapshot: Some(Arc::clone(&settled)),
            cards: Vec::new(),
        };
        assert!(modal.refresh(same()));
        assert!(
            !modal.refresh(same()),
            "an unchanged snapshot rebuilds nothing"
        );
        let ids: Vec<_> = rows(&modal).iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, [SECOND_ID, FIRST_ID]);
        assert_eq!(modal.selected_id().as_deref(), Some(FIRST_ID));
        assert!(modal.page.is_some());

        let _ = modal.handle_key(key_event(KeyCode::Esc));
        assert!(modal.page.is_none());
        assert!(modal.is_open());
        assert_eq!(modal.selected_id().as_deref(), Some(FIRST_ID));
    }

    #[test_case(ShellOutputView::Stored, true; "stored")]
    #[test_case(ShellOutputView::Missing, false; "missing")]
    fn details_ask_for_output_an_earlier_runtime_kept(output: ShellOutputView, asked: bool) {
        let mut modal = opened(
            vec![view(
                record(FINISHED_ID, ShellExecutionState::Succeeded, STARTED_MS),
                output,
            )],
            Vec::new(),
        );
        assert!(modal.show(MISSING_ID).is_none());
        assert!(modal.page.is_none());
        assert_eq!(
            matches!(
                modal.show(FINISHED_ID),
                Some(ShellModalAction::LoadOutput(id)) if id == FINISHED_ID
            ),
            asked
        );
        assert_eq!(modal.selected_id().as_deref(), Some(FINISHED_ID));
        assert!(modal.page.is_some());
    }

    #[test]
    fn output_is_literal_with_terminal_controls_escaped() {
        let mut modal = opened(
            vec![view(
                record(FINISHED_ID, ShellExecutionState::Succeeded, STARTED_MS),
                ShellOutputView::Kept(Arc::new(shell_output(RAW_OUTPUT))),
            )],
            Vec::new(),
        );
        let _ = modal.handle_key(key_event(KeyCode::Enter));
        let painted = paint(&mut modal);
        assert!(painted.contains(MARKDOWN_KEPT), "{painted}");
        assert!(painted.contains(CONTROL_ESCAPED), "{painted}");
        assert!(!painted.contains('\u{1b}'));
    }

    #[test]
    fn details_hold_a_paste_and_escape_goes_back_before_closing() {
        let mut modal = opened(
            vec![view(
                record(FINISHED_ID, ShellExecutionState::Failed, STARTED_MS),
                ShellOutputView::Missing,
            )],
            Vec::new(),
        );
        let _ = modal.handle_key(key_event(KeyCode::Enter));
        assert!(modal.handle_paste(PASTED));
        assert!(modal.picker.search_text().is_empty());
        let _ = modal.handle_key(key_event(KeyCode::Esc));
        assert!(modal.is_open());
        assert!(modal.page.is_none());
        let _ = modal.handle_key(key_event(KeyCode::Esc));
        assert!(!modal.is_open());
        assert_eq!(modal.selected_id(), None);
    }

    /// The coloured row is the second, below the selection. Each paint is kept
    /// for the theme it was made under, so a cache that ignored the theme would
    /// draw the first palette again.
    #[test]
    fn a_theme_switch_recolours_the_command_in_its_row_and_on_its_page() {
        let mut modal = opened(
            vec![running(FIRST_ID, STARTED_MS), finished(COLOURED)],
            Vec::new(),
        );
        let row = colours_under(&mut modal, FIRST_THEME);
        assert!(distinct(&row) > 1, "{NOT_COLOURED}");
        assert_ne!(
            colours_under(&mut modal, SECOND_THEME),
            row,
            "{THEME_IGNORED}"
        );

        assert!(modal.show(FINISHED_ID).is_some());
        let page = colours_under(&mut modal, FIRST_THEME);
        assert!(distinct(&page) > 1, "{NOT_COLOURED}");
        assert_ne!(
            colours_under(&mut modal, SECOND_THEME),
            page,
            "{THEME_IGNORED}"
        );
    }

    /// The page is painted wide first. Rows kept from then would run past the
    /// narrower border and lose the command's tail.
    #[test]
    fn a_narrower_modal_rewraps_the_command() {
        let mut modal = opened(vec![finished(COLOURED)], Vec::new());
        let _ = modal.handle_key(key_event(KeyCode::Enter));
        assert!(
            locate(&draw(&mut modal, WIDTH), COLOURED).is_some(),
            "{NOT_DRAWN}"
        );
        assert!(
            locate(&draw(&mut modal, NARROW_WIDTH), COLOURED_TAIL).is_some(),
            "{NOT_REWRAPPED}"
        );
    }

    #[test_case(COLOURED, true; "command")]
    #[test_case("\nls -la", false; "blank_first_line")]
    fn only_a_command_line_colours_its_row(command: &str, coloured: bool) {
        let modal = opened(vec![finished(command)], Vec::new());
        assert_eq!(
            rows(&modal)[0].label_colours().is_some(),
            coloured,
            "{ID_COLOURED}"
        );
    }
}
