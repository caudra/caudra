use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use crate::{Focus, SidebarView, Workbench, WorkbenchAction, WorkbenchStyles, chrome, keys};

pub const MAX_TRANSFER_ENTRIES: usize = 20_000;
pub const MAX_TRANSFER_OPERATIONS: usize = 4_096;
pub const MAX_TRANSFER_PREVIEW_BYTES: usize = 262_144;
const MAX_ROOT_LENGTH: usize = 4_096;
const MAX_PATH_DEPTH: usize = 128;
const MIN_TWO_PANE_WIDTH: u16 = 48;
const PAGE_ROWS: isize = 10;
const FULL_CHROME_HEIGHT: u16 = 16;
const HORIZONTAL_STEP: usize = 8;
const LIMIT_NOTICE: &str = "Presentation limit exceeded; choose a smaller selection or root. Nothing was truncated or approved.";
const ROOT_NOTICE: &str = "Confirm an existing absolute local root and workspace-relative sandbox root with Compare. The host validates both.";
const DRAIN_NOTICE: &str =
    "Cancelling and draining; editor input remains locked until cleanup completes.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferAvailability {
    pub attachment: String,
    pub label: String,
    pub local_root: Option<String>,
    pub remote_root: String,
    pub directory_effects: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferRoots {
    pub local: String,
    pub remote: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TransferDirection {
    #[default]
    Push,
    Pull,
    Seed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferNodeKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    Equal,
    LocalOnly,
    RemoteOnly,
    Different,
    TypeConflict,
    Excluded,
    Unsupported,
    Incomplete,
}

impl TransferStatus {
    fn blocked(&self) -> bool {
        matches!(
            self,
            Self::TypeConflict | Self::Excluded | Self::Unsupported | Self::Incomplete
        )
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Equal => "same",
            Self::LocalOnly => "local only",
            Self::RemoteOnly => "sandbox only",
            Self::Different => "different",
            Self::TypeConflict => "type conflict",
            Self::Excluded => "excluded",
            Self::Unsupported => "unsupported",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEntry {
    pub path: String,
    pub local: Option<TransferNodeKind>,
    pub remote: Option<TransferNodeKind>,
    pub status: TransferStatus,
    pub bytes: u64,
}

impl TransferEntry {
    fn directory(&self) -> bool {
        self.local == Some(TransferNodeKind::Directory)
            || self.remote == Some(TransferNodeKind::Directory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSnapshot {
    pub roots: TransferRoots,
    pub entries: Vec<TransferEntry>,
    pub complete: bool,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPreview {
    pub path: String,
    pub local: Option<String>,
    pub remote: Option<String>,
    pub summary: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEffect {
    New,
    Overwrite,
    Mkdir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReviewEntry {
    pub path: String,
    pub effect: TransferEffect,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReview {
    pub digest: String,
    pub roots: TransferRoots,
    pub direction: TransferDirection,
    pub entries: Vec<TransferReviewEntry>,
    pub skipped: Vec<String>,
    pub executable: bool,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    pub message: String,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub entries: Vec<String>,
    pub recovery_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAction {
    Compare {
        generation: u64,
        roots: TransferRoots,
    },
    Inspect {
        generation: u64,
        path: String,
    },
    Review {
        generation: u64,
        direction: TransferDirection,
        paths: Vec<String>,
    },
    Execute {
        generation: u64,
        digest: String,
    },
    Cancel {
        generation: u64,
    },
    Reconcile {
        generation: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Summary {
    changed: usize,
    blocked: usize,
    incomplete: usize,
}

#[derive(Default)]
struct ComparisonTree {
    entries: BTreeMap<String, TransferEntry>,
    children: BTreeMap<String, Vec<String>>,
    summaries: BTreeMap<String, Summary>,
}

impl ComparisonTree {
    fn new(entries: Vec<TransferEntry>) -> Option<Self> {
        let mut tree = Self::default();
        for entry in entries {
            if !valid_relative(&entry.path, false) || tree.entries.contains_key(&entry.path) {
                return None;
            }
            tree.children
                .entry(parent(&entry.path).to_owned())
                .or_default()
                .push(entry.path.clone());
            tree.entries.insert(entry.path.clone(), entry);
        }
        for entry in tree.entries.values() {
            let blocked = usize::from(tree.blocked_ancestor(&entry.path));
            let changed = usize::from(entry.status != TransferStatus::Equal && blocked == 0);
            let incomplete = usize::from(tree.incomplete_ancestor(&entry.path));
            let mut ancestor = entry.path.as_str();
            loop {
                let summary = tree.summaries.entry(ancestor.to_owned()).or_default();
                summary.changed += changed;
                summary.blocked += blocked;
                summary.incomplete += incomplete;
                if ancestor.is_empty() {
                    break;
                }
                ancestor = parent(ancestor);
            }
        }
        for children in tree.children.values_mut() {
            children.sort_by(|a, b| {
                tree.entries[b]
                    .directory()
                    .cmp(&tree.entries[a].directory())
                    .then(a.cmp(b))
            });
        }
        Some(tree)
    }

    fn blocked_ancestor(&self, path: &str) -> bool {
        let mut ancestor = path;
        while !ancestor.is_empty() {
            if self
                .entries
                .get(ancestor)
                .is_some_and(|entry| entry.status.blocked())
            {
                return true;
            }
            ancestor = parent(ancestor);
        }
        false
    }

    fn incomplete_ancestor(&self, path: &str) -> bool {
        let mut ancestor = path;
        while !ancestor.is_empty() {
            if self
                .entries
                .get(ancestor)
                .is_some_and(|entry| entry.status == TransferStatus::Incomplete)
            {
                return true;
            }
            ancestor = parent(ancestor);
        }
        false
    }
}

enum TransferExit {
    View(SidebarView),
    Close,
}

#[derive(Default)]
pub(crate) struct TransferState {
    availability: Option<TransferAvailability>,
    generation: u64,
    pub(crate) busy: bool,
    draining: bool,
    pending: bool,
    request: Option<TransferAction>,
    next_availability: Option<Option<TransferAvailability>>,
    roots: TransferRoots,
    confirmed_roots: Option<TransferRoots>,
    direction: TransferDirection,
    linked: bool,
    directory: String,
    history: [Vec<String>; 2],
    side: usize,
    cursors: [usize; 2],
    offsets: [usize; 2],
    tree: ComparisonTree,
    visible_rows: Vec<String>,
    changes_only: bool,
    complete: bool,
    selected: BTreeSet<String>,
    review: Option<TransferReview>,
    detail: Vec<String>,
    detail_open: bool,
    detail_offset: usize,
    detail_column: usize,
    progress: Option<TransferProgress>,
    outcome: Option<TransferOutcome>,
    recovery: Option<TransferOutcome>,
    notice: String,
    field: Option<usize>,
    pane_rects: [Rect; 2],
    root_rects: [Rect; 2],
    buttons: Vec<(Rect, KeyCode)>,
    exit: Option<TransferExit>,
}

impl TransferState {
    pub(crate) fn close(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.clear_comparison();
        self.field = None;
        self.directory.clear();
        self.history = [Vec::new(), Vec::new()];
        self.progress = None;
        self.pane_rects = [Rect::default(); 2];
        self.root_rects = [Rect::default(); 2];
        self.buttons.clear();
        self.exit = None;
    }

    pub(crate) fn lease_active(&self) -> bool {
        self.busy || self.draining
    }
    pub(crate) fn connection_active(&self) -> bool {
        self.busy || self.draining || self.pending
    }
    pub(crate) fn text_input_active(&self) -> bool {
        self.field.is_some() && !self.pending && !self.draining
    }

    pub(crate) fn scroll(&mut self, delta: isize) {
        self.move_cursor(delta);
    }

    pub(crate) fn available(&self) -> bool {
        self.availability.is_some()
    }

    fn invalidate(&mut self) {
        self.review = None;
        self.detail_open = false;
        self.detail.clear();
        self.detail_offset = 0;
        self.detail_column = 0;
    }

    fn clear_comparison(&mut self) {
        self.invalidate();
        self.tree = ComparisonTree::default();
        self.visible_rows.clear();
        self.complete = false;
        self.selected.clear();
        self.cursors = [0; 2];
        self.offsets = [0; 2];
    }

    fn rows(&self) -> &[String] {
        &self.visible_rows
    }

    fn refresh_rows(&mut self) {
        self.visible_rows = self
            .tree
            .children
            .get(&self.directory)
            .into_iter()
            .flatten()
            .filter(|path| {
                let summary = &self.tree.summaries[*path];
                !self.changes_only || summary.changed > 0 || summary.blocked > 0
            })
            .cloned()
            .collect();
        self.cursors = [0; 2];
        self.offsets = [0; 2];
    }

    fn selection(&self) -> (Vec<String>, usize, u64) {
        let mut paths = Vec::new();
        let mut skipped = 0;
        let mut bytes = 0u64;
        for (path, entry) in &self.tree.entries {
            if !self.selected_path(path) {
                continue;
            }
            let (source, destination) = if self.direction == TransferDirection::Pull {
                (&entry.remote, &entry.local)
            } else {
                (&entry.local, &entry.remote)
            };
            if self.tree.blocked_ancestor(path) || source.is_none() {
                skipped += 1;
                continue;
            }
            if entry.status == TransferStatus::Equal {
                continue;
            }
            if self.direction == TransferDirection::Seed && destination.is_some() {
                skipped += 1;
                continue;
            }
            if source == &Some(TransferNodeKind::Directory) {
                if destination.is_some() {
                    continue;
                }
                if !self
                    .availability
                    .as_ref()
                    .is_some_and(|available| available.directory_effects)
                {
                    skipped += 1;
                    continue;
                }
            } else {
                bytes = bytes.saturating_add(entry.bytes);
            }
            paths.push(path.clone());
        }
        (paths, skipped, bytes)
    }

    fn selected_path(&self, path: &str) -> bool {
        let mut ancestor = path;
        while !ancestor.is_empty() {
            if self.selected.contains(ancestor) {
                return true;
            }
            ancestor = parent(ancestor);
        }
        false
    }

    fn action(&mut self, action: TransferAction) -> WorkbenchAction {
        self.pending = true;
        self.request = Some(action.clone());
        WorkbenchAction::Transfer(action)
    }

    fn cancel(&mut self) -> WorkbenchAction {
        self.invalidate();
        self.request = None;
        self.draining = true;
        self.notice = DRAIN_NOTICE.to_owned();
        WorkbenchAction::Transfer(TransferAction::Cancel {
            generation: self.generation,
        })
    }

    fn compare(&mut self) -> WorkbenchAction {
        if self.roots.remote == "." {
            self.roots.remote.clear();
        }
        if !Path::new(&self.roots.local).is_absolute() || !valid_relative(&self.roots.remote, true)
        {
            self.notice = ROOT_NOTICE.to_owned();
            return WorkbenchAction::Consumed;
        }
        if self.confirmed_roots.as_ref() != Some(&self.roots) {
            self.outcome = None;
            self.recovery = None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.clear_comparison();
        self.notice = "Validating roots and comparing content…".to_owned();
        self.action(TransferAction::Compare {
            generation: self.generation,
            roots: self.roots.clone(),
        })
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.detail_open {
            self.detail_offset = self
                .detail_offset
                .saturating_add_signed(delta)
                .min(self.detail.len().saturating_sub(1));
        } else {
            self.cursors[self.side] = self.cursors[self.side]
                .saturating_add_signed(delta)
                .min(self.rows().len().saturating_sub(1));
        }
    }

    fn navigate(&mut self, directory: String) -> WorkbenchAction {
        if self.linked {
            self.directory = directory;
            self.refresh_rows();
            return WorkbenchAction::Consumed;
        }
        if !directory.is_empty()
            && self.tree.entries.get(&directory).is_some_and(|entry| {
                if self.side == 0 {
                    entry.local != Some(TransferNodeKind::Directory)
                } else {
                    entry.remote != Some(TransferNodeKind::Directory)
                }
            })
        {
            self.notice = "A missing-side directory can only be entered in linked mode.".to_owned();
            return WorkbenchAction::Consumed;
        }
        let root = if self.side == 0 {
            &mut self.roots.local
        } else {
            &mut self.roots.remote
        };
        if directory.is_empty() {
            let Some(previous) = self.history[self.side].pop() else {
                return WorkbenchAction::Consumed;
            };
            *root = previous;
        } else {
            self.history[self.side].push(root.clone());
            *root = join(root, &directory);
        }
        self.directory.clear();
        self.invalidate();
        self.compare()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if key.code == KeyCode::Esc && self.field.take().is_some() {
            return WorkbenchAction::Consumed;
        }
        if key.code == KeyCode::Esc && self.detail_open {
            self.detail_open = false;
            return WorkbenchAction::Consumed;
        }
        if self.draining || self.pending {
            return if key.code == KeyCode::Char('c') && key.modifiers.is_empty() {
                self.cancel()
            } else {
                WorkbenchAction::Consumed
            };
        }
        if let Some(field) = self.field {
            match key.code {
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.invalidate();
                    if field == 0 {
                        self.roots.local.clear();
                    } else {
                        self.roots.remote.clear();
                    }
                }
                KeyCode::Enter => self.field = None,
                KeyCode::Tab | KeyCode::BackTab => self.field = Some(1 - field),
                KeyCode::Backspace => {
                    self.invalidate();
                    if field == 0 {
                        self.roots.local.pop();
                    } else {
                        self.roots.remote.pop();
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.paste(&ch.to_string())
                }
                _ => {}
            }
            return WorkbenchAction::Consumed;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return WorkbenchAction::Consumed;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => self.side = 1 - self.side,
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-PAGE_ROWS),
            KeyCode::PageDown => self.move_cursor(PAGE_ROWS),
            KeyCode::Left => {
                self.detail_column = self.detail_column.saturating_sub(HORIZONTAL_STEP)
            }
            KeyCode::Right => {
                self.detail_column = self.detail_column.saturating_add(HORIZONTAL_STEP)
            }
            KeyCode::Char('f') => {
                self.changes_only = !self.changes_only;
                self.refresh_rows();
            }
            KeyCode::Char('l') => self.field = Some(0),
            KeyCode::Char('s') => self.field = Some(1),
            KeyCode::Char('c') => return self.cancel(),
            KeyCode::Char('n') => {
                if self.linked
                    && !self.directory.is_empty()
                    && self.tree.entries.get(&self.directory).is_some_and(|entry| {
                        entry.local != Some(TransferNodeKind::Directory)
                            || entry.remote != Some(TransferNodeKind::Directory)
                    })
                {
                    self.notice = "Missing-side navigation must remain linked until the directory exists on both sides.".to_owned();
                    return WorkbenchAction::Consumed;
                }
                self.linked = !self.linked;
                self.invalidate();
                if !self.linked && !self.directory.is_empty() {
                    for side in 0..2 {
                        let root = if side == 0 {
                            &mut self.roots.local
                        } else {
                            &mut self.roots.remote
                        };
                        self.history[side].push(root.clone());
                        *root = join(root, &self.directory);
                    }
                    self.directory.clear();
                    return self.compare();
                }
            }
            KeyCode::Char('=') | KeyCode::F(5) => return self.compare(),
            KeyCode::Char('u') => {
                self.direction = TransferDirection::Push;
                self.invalidate();
            }
            KeyCode::Char('d') => {
                self.direction = TransferDirection::Pull;
                self.invalidate();
            }
            KeyCode::Char(' ') => {
                if let Some(path) = self.rows().get(self.cursors[self.side]).cloned() {
                    self.invalidate();
                    if !self.selected.remove(&path) {
                        self.selected.insert(path);
                    }
                }
            }
            KeyCode::Backspace => return self.navigate(parent(&self.directory).to_owned()),
            KeyCode::Enter | KeyCode::Char('i') => {
                if let Some(path) = self.rows().get(self.cursors[self.side]).cloned() {
                    if self.tree.entries[&path].directory() {
                        return self.navigate(path);
                    }
                    self.invalidate();
                    return self.action(TransferAction::Inspect {
                        generation: self.generation,
                        path,
                    });
                }
            }
            KeyCode::Char('r') => {
                self.invalidate();
                let (paths, skipped, _) = self.selection();
                if !self.complete || self.confirmed_roots.as_ref() != Some(&self.roots) {
                    self.notice = "Comparison incomplete or roots changed; compare a smaller validated root before review.".to_owned();
                } else if paths.len() > MAX_TRANSFER_OPERATIONS {
                    self.notice = LIMIT_NOTICE.to_owned();
                } else if paths.is_empty() {
                    self.notice =
                        format!("No eligible changes selected; {skipped} skipped/blocked.");
                } else {
                    return self.action(TransferAction::Review {
                        generation: self.generation,
                        direction: self.direction.clone(),
                        paths,
                    });
                }
            }
            KeyCode::Char('a') => {
                if let Some(review) = self
                    .review
                    .take()
                    .filter(|review| review.executable && !review.digest.is_empty())
                {
                    self.detail_open = false;
                    return self.action(TransferAction::Execute {
                        generation: self.generation,
                        digest: review.digest,
                    });
                }
            }
            KeyCode::Char('o') => {
                self.detail.clear();
                if let Some(outcome) = &self.outcome {
                    self.detail.extend(outcome.entries.iter().cloned());
                }
                if let Some(recovery) = &self.recovery {
                    self.detail.extend(recovery.entries.iter().cloned());
                }
                self.detail_open = true;
                self.detail_offset = 0;
                self.detail_column = 0;
            }
            KeyCode::Char('q')
                if self
                    .recovery
                    .as_ref()
                    .is_some_and(|recovery| recovery.recovery_required) =>
            {
                return self.action(TransferAction::Reconcile {
                    generation: self.generation,
                });
            }
            _ => {}
        }
        WorkbenchAction::Consumed
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.pending || self.draining {
            return;
        }
        if let Some(field) = self.field {
            let root = if field == 0 {
                &mut self.roots.local
            } else {
                &mut self.roots.remote
            };
            if root.len().saturating_add(text.len()) > MAX_ROOT_LENGTH
                || text.chars().any(char::is_control)
            {
                self.notice =
                    "Root input rejected: control characters or root length limit exceeded."
                        .to_owned();
            } else {
                root.push_str(text);
                self.invalidate();
            }
        }
    }

    pub(crate) fn mouse(&mut self, event: MouseEvent, clicks: u8) -> WorkbenchAction {
        let position = (event.column, event.row).into();
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, key)) = self
                .buttons
                .iter()
                .find(|(rect, _)| rect.contains(position))
            {
                return self.key(KeyEvent::new(*key, KeyModifiers::NONE));
            }
            if self.pending || self.draining {
                return WorkbenchAction::Consumed;
            }
            if let Some(field) = self
                .root_rects
                .iter()
                .position(|rect| rect.contains(position))
            {
                self.field = Some(field);
                return WorkbenchAction::Consumed;
            }
            if !self.detail_open
                && let Some(side) = self
                    .pane_rects
                    .iter()
                    .position(|rect| rect.contains(position))
            {
                self.side = side;
                let row = usize::from(event.row - self.pane_rects[side].y) + self.offsets[side];
                if row < self.rows().len() {
                    self.cursors[side] = row;
                    let code = if clicks >= 2 {
                        KeyCode::Enter
                    } else {
                        KeyCode::Char(' ')
                    };
                    return self.key(KeyEvent::new(code, KeyModifiers::NONE));
                }
            }
        } else if matches!(
            event.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            if let Some(side) = self
                .pane_rects
                .iter()
                .position(|rect| rect.contains(position))
            {
                self.side = side;
            }
            self.move_cursor(if event.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            });
        }
        WorkbenchAction::Consumed
    }

    pub(crate) fn render_sidebar(&self, buf: &mut Buffer, area: Rect, styles: &WorkbenchStyles) {
        let (paths, skipped, bytes) = self.selection();
        let summary = self.tree.summaries.get("").cloned().unwrap_or_default();
        let lines = [
            format!("{:?}: {} operations", self.direction, paths.len()),
            format!("{bytes} bytes; {skipped} skipped/blocked"),
            format!("{} changed; {} blocked", summary.changed, summary.blocked),
            format!(
                "{} incomplete; scan {}",
                summary.incomplete,
                if self.complete {
                    "complete"
                } else {
                    "incomplete"
                }
            ),
            format!(
                "Navigation: {}",
                if self.linked {
                    "linked"
                } else {
                    "independent pairing"
                }
            ),
            format!("Limits: {MAX_TRANSFER_ENTRIES} rows"),
            format!("{MAX_TRANSFER_OPERATIONS} reviewed operations"),
            "Tab: local / sandbox".to_owned(),
            "Space: select file/folder".to_owned(),
            "Enter: enter / inspect".to_owned(),
            "Backspace: parent".to_owned(),
            "L / S: edit roots".to_owned(),
            "N: linked navigation".to_owned(),
            format!(
                "F: filter {}",
                if self.changes_only { "changes" } else { "all" }
            ),
            "U / D: upload / download".to_owned(),
            "=: compare  R: review".to_owned(),
            "A: approve exact digest".to_owned(),
            "C: cancel  Q: reconcile".to_owned(),
            "O: outcomes  Esc: back".to_owned(),
            "Ctrl+X 1/2/3/4: views".to_owned(),
        ];
        paint_lines(buf, area, &lines, 0, styles);
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect, styles: &WorkbenchStyles) {
        self.buttons.clear();
        self.pane_rects = [Rect::default(); 2];
        self.root_rects = [Rect::default(); 2];
        let full_chrome = area.height >= FULL_CHROME_HEIGHT;
        let [
            identity,
            local,
            remote,
            controls,
            navigation,
            content,
            progress,
            notice,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(if full_chrome { 2 } else { 0 }),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(u16::from(full_chrome)),
            Constraint::Length(if full_chrome { 2 } else { 1 }),
        ])
        .areas(area);
        let label = self
            .availability
            .as_ref()
            .map_or("Disconnected", |available| available.label.as_str());
        paint(
            buf,
            identity,
            format!(
                "Transfer · {label} · {:?} · {}",
                self.direction,
                if self.draining {
                    "draining"
                } else if self.pending {
                    "working"
                } else {
                    "read-only until approval"
                }
            ),
            styles,
        );
        self.root_rects = [local, remote];
        paint(
            buf,
            local,
            format!(
                "{}Local: {}",
                if self.field == Some(0) { "> " } else { "  " },
                self.roots.local
            ),
            styles,
        );
        paint(
            buf,
            remote,
            format!(
                "{}Sandbox: {}",
                if self.field == Some(1) { "> " } else { "  " },
                self.roots.remote
            ),
            styles,
        );
        let mut x = controls.x;
        let mut y = controls.y;
        for (label, code) in [
            ("[U Upload]", 'u'),
            ("[D Download]", 'd'),
            ("[= Compare]", '='),
            ("[R Review]", 'r'),
            ("[A Approve]", 'a'),
            ("[C Cancel]", 'c'),
            ("[N Link]", 'n'),
            ("[Q Recover]", 'q'),
        ] {
            let width = label.len() as u16;
            if x.saturating_add(width) > controls.right() {
                x = controls.x;
                y = y.saturating_add(1);
            }
            if width > controls.width || y >= controls.bottom() {
                continue;
            }
            let rect = Rect::new(x, y, width, 1);
            paint(buf, rect, label.to_owned(), styles);
            self.buttons.push((rect, KeyCode::Char(code)));
            x = x.saturating_add(width + 1);
        }
        paint(
            buf,
            navigation,
            format!(
                "{} /{} · Tab switches {}",
                if self.linked {
                    "Linked"
                } else {
                    "Independent roots"
                },
                self.directory,
                if self.side == 0 {
                    "Local → Sandbox"
                } else {
                    "Sandbox → Local"
                }
            ),
            styles,
        );
        if self.detail_open {
            for (row, text) in self
                .detail
                .iter()
                .skip(self.detail_offset)
                .take(usize::from(content.height))
                .enumerate()
            {
                paint(
                    buf,
                    Rect::new(content.x, content.y + row as u16, content.width, 1),
                    text.chars().skip(self.detail_column).collect(),
                    styles,
                );
            }
        } else {
            let panes: [Rect; 2] = if content.width >= MIN_TWO_PANE_WIDTH {
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(content)
            } else {
                let mut panes = [Rect::default(); 2];
                panes[self.side] = content;
                panes
            };
            for (side, pane) in panes.into_iter().enumerate() {
                if pane.is_empty() {
                    continue;
                }
                let [heading, rows] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(pane);
                self.pane_rects[side] = rows;
                paint(
                    buf,
                    heading,
                    format!(
                        "{}{}",
                        if self.side == side { "> " } else { "  " },
                        if side == 0 { "Local" } else { "Sandbox" }
                    ),
                    styles,
                );
                let height = usize::from(rows.height);
                if self.cursors[side] < self.offsets[side] {
                    self.offsets[side] = self.cursors[side];
                }
                if self.cursors[side] >= self.offsets[side] + height {
                    self.offsets[side] =
                        self.cursors[side].saturating_sub(height.saturating_sub(1));
                }
                for (offset, path) in self
                    .rows()
                    .iter()
                    .skip(self.offsets[side])
                    .take(height)
                    .enumerate()
                {
                    let entry = &self.tree.entries[path];
                    let kind = if side == 0 {
                        &entry.local
                    } else {
                        &entry.remote
                    };
                    let selected = self.selected_path(path);
                    let mark = if selected { "[x]" } else { "[ ]" };
                    let name = path.rsplit('/').next().unwrap_or(path);
                    let status = if entry.directory() {
                        let summary = &self.tree.summaries[path];
                        format!(
                            "{} Δ{} !{} ?{}",
                            entry.status.label(),
                            summary.changed,
                            summary.blocked,
                            summary.incomplete
                        )
                    } else {
                        entry.status.label().to_owned()
                    };
                    let text = format!(
                        "{mark} {name}{} [{status}]{}",
                        if entry.directory() { "/" } else { "" },
                        if kind.is_none() { " (missing)" } else { "" }
                    );
                    let style =
                        if self.side == side && offset + self.offsets[side] == self.cursors[side] {
                            styles.selected
                        } else {
                            styles.text
                        };
                    chrome::render_line(
                        buf,
                        Rect::new(rows.x, rows.y + offset as u16, rows.width, 1),
                        Line::from(Span::styled(text, style)),
                    );
                }
            }
        }
        let progress_text = self.progress.as_ref().map_or_else(
            || {
                format!(
                    "{} outcome rows; {} recovery rows (O) · {}",
                    self.outcome
                        .as_ref()
                        .map_or(0, |outcome| outcome.entries.len()),
                    self.recovery
                        .as_ref()
                        .map_or(0, |recovery| recovery.entries.len()),
                    if self
                        .recovery
                        .as_ref()
                        .is_some_and(|recovery| recovery.recovery_required)
                    {
                        "unknown publication: Q queries status, never replays"
                    } else {
                        "no pending recovery"
                    }
                )
            },
            |progress| {
                format!(
                    "{}/{} {}",
                    progress.completed, progress.total, progress.message
                )
            },
        );
        paint(buf, progress, progress_text, styles);
        paint_lines(buf, notice, &[self.notice.clone(), "U/D direction · = compare · R review · A approve · C cancel · Tab panes · Esc back".to_owned()], 0, styles);
    }
}

impl Workbench {
    pub fn set_transfer_availability(&mut self, availability: Option<TransferAvailability>) {
        let changed = self
            .transfer
            .availability
            .as_ref()
            .map(|value| &value.attachment)
            != availability.as_ref().map(|value| &value.attachment);
        if changed && self.transfer.connection_active() {
            self.transfer.next_availability = Some(availability);
            self.transfer.draining = true;
            self.transfer
                .exit
                .get_or_insert(TransferExit::View(SidebarView::Explorer));
            self.transfer.invalidate();
            self.transfer.notice = DRAIN_NOTICE.to_owned();
            return;
        }
        if changed {
            self.transfer.generation = self.transfer.generation.wrapping_add(1);
            self.transfer.clear_comparison();
            self.transfer.outcome = None;
            self.transfer.recovery = None;
            self.transfer.confirmed_roots = None;
            self.transfer.pending = false;
            self.transfer.directory.clear();
            self.transfer.history = [Vec::new(), Vec::new()];
            self.transfer.linked = true;
            self.transfer.roots =
                availability
                    .as_ref()
                    .map_or_else(TransferRoots::default, |available| TransferRoots {
                        local: available.local_root.clone().unwrap_or_default(),
                        remote: available.remote_root.clone(),
                    });
        }
        self.transfer.availability = availability;
        if !self.transfer.available() && self.sidebar == SidebarView::Transfer {
            if self.transfer.busy || self.transfer.draining {
                self.transfer.draining = true;
                self.transfer
                    .exit
                    .get_or_insert(TransferExit::View(SidebarView::Explorer));
                self.transfer.notice = DRAIN_NOTICE.to_owned();
            } else {
                self.sidebar = SidebarView::Explorer;
            }
        }
    }

    pub fn transfer_generation(&self) -> u64 {
        self.transfer.generation
    }

    pub fn cancel_transfer(&mut self) -> WorkbenchAction {
        self.transfer.cancel()
    }

    pub fn close_transfer(&mut self) -> WorkbenchAction {
        if self.transfer.connection_active() {
            self.transfer.exit = Some(TransferExit::Close);
            self.transfer.cancel()
        } else {
            self.close();
            WorkbenchAction::Consumed
        }
    }

    pub fn transfer_input_active(&self) -> bool {
        self.sidebar == SidebarView::Transfer || self.transfer.busy || self.transfer.draining
    }

    pub fn open_transfer(&mut self) -> bool {
        if !self.transfer.available() || self.transfer.draining {
            return false;
        }
        self.open = true;
        self.sidebar = SidebarView::Transfer;
        self.focus = Focus::Editor;
        self.palette.close();
        self.confirm = None;
        self.menu = None;
        self.input = None;
        self.goto = None;
        self.drag = crate::Drag::None;
        if self.transfer.confirmed_roots.is_none() {
            self.transfer.notice = ROOT_NOTICE.to_owned();
        }
        true
    }

    pub fn show_transfer(&mut self, roots: TransferRoots, direction: TransferDirection) -> bool {
        if self.transfer.busy || self.transfer.draining || !self.open_transfer() {
            return false;
        }
        self.transfer.generation = self.transfer.generation.wrapping_add(1);
        self.transfer.clear_comparison();
        self.transfer.outcome = None;
        self.transfer.recovery = None;
        self.transfer.confirmed_roots = None;
        self.transfer.roots = roots;
        self.transfer.direction = direction;
        self.transfer.directory.clear();
        true
    }

    pub fn set_transfer_connection(&mut self, generation: u64, busy: bool, draining: bool) -> bool {
        if generation != self.transfer.generation {
            return false;
        }
        let was_draining = self.transfer.draining;
        self.transfer.busy = busy;
        self.transfer.draining = draining;
        if !busy && !draining {
            self.transfer.pending = false;
            self.transfer.request = None;
            let exit = self.transfer.exit.take();
            if was_draining {
                self.transfer.generation = self.transfer.generation.wrapping_add(1);
                self.transfer.clear_comparison();
                self.transfer.notice =
                    "Cleanup complete. Compare to validate the current roots again.".to_owned();
            }
            if let Some(availability) = self.transfer.next_availability.take() {
                self.set_transfer_availability(availability);
            }
            match exit {
                Some(TransferExit::View(view)) => {
                    self.sidebar = view;
                    self.focus = Focus::Sidebar;
                }
                Some(TransferExit::Close) => {
                    self.transfer.close();
                    self.sidebar = SidebarView::Explorer;
                    self.close();
                }
                None => {}
            }
        }
        true
    }

    pub fn receive_transfer_snapshot(
        &mut self,
        generation: u64,
        mut snapshot: TransferSnapshot,
    ) -> bool {
        if generation != self.transfer.generation || self.transfer.draining {
            return false;
        }
        self.transfer.pending = false;
        self.transfer.request = None;
        if snapshot.roots.remote == "." {
            snapshot.roots.remote.clear();
        }
        if self
            .transfer
            .confirmed_roots
            .as_ref()
            .is_some_and(|roots| roots != &snapshot.roots)
        {
            self.transfer.outcome = None;
            self.transfer.recovery = None;
        }
        self.transfer.invalidate();
        if snapshot.entries.len() > MAX_TRANSFER_ENTRIES {
            self.transfer.clear_comparison();
            self.transfer.notice = LIMIT_NOTICE.to_owned();
            return false;
        }
        let Some(tree) = ComparisonTree::new(snapshot.entries) else {
            self.transfer.clear_comparison();
            self.transfer.notice =
                "Invalid or duplicate comparison paths; comparison rejected.".to_owned();
            return false;
        };
        self.transfer.tree = tree;
        self.transfer.refresh_rows();
        self.transfer.roots = snapshot.roots.clone();
        self.transfer.confirmed_roots = Some(snapshot.roots);
        self.transfer.complete = snapshot.complete;
        self.transfer.selected.clear();
        self.transfer.notice = snapshot.notice.unwrap_or_else(|| {
            if snapshot.complete {
                "Compared by content, not modification time. Select files or folders to review."
                    .to_owned()
            } else {
                "Scan incomplete; reduce roots or limits before review.".to_owned()
            }
        });
        true
    }

    pub fn receive_transfer_preview(&mut self, generation: u64, preview: TransferPreview) -> bool {
        if generation != self.transfer.generation
            || self.transfer.draining
            || !matches!(&self.transfer.request, Some(TransferAction::Inspect { path, .. }) if path == &preview.path)
        {
            return false;
        }
        self.transfer.pending = false;
        self.transfer.request = None;
        if preview
            .local
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(preview.remote.as_ref().map_or(0, String::len))
            > MAX_TRANSFER_PREVIEW_BYTES
        {
            self.transfer.notice = LIMIT_NOTICE.to_owned();
            return false;
        }
        self.transfer.detail = vec![
            format!("Read-only: {} · Local → Sandbox", preview.path),
            preview.summary,
        ];
        if preview.truncated {
            self.transfer
                .detail
                .push("TRUNCATED preview: matching prefixes do not establish equality.".to_owned());
        }
        if preview.local.is_some() || preview.remote.is_some() {
            self.transfer.detail.extend(
                caudra_diff::unified_text(
                    preview.local.as_deref().unwrap_or_default(),
                    preview.remote.as_deref().unwrap_or_default(),
                    "Local → Sandbox",
                    &preview.path,
                )
                .lines()
                .map(str::to_owned),
            );
        }
        self.transfer.detail_open = true;
        self.transfer.detail_offset = 0;
        true
    }

    pub fn receive_transfer_review(&mut self, generation: u64, mut review: TransferReview) -> bool {
        if review.roots.remote == "." {
            review.roots.remote.clear();
        }
        if generation != self.transfer.generation
            || self.transfer.draining
            || review.roots != self.transfer.roots
            || review.direction != self.transfer.direction
            || !matches!(&self.transfer.request, Some(TransferAction::Review { .. }))
        {
            return false;
        }
        self.transfer.pending = false;
        self.transfer.request = None;
        self.transfer.invalidate();
        if review.entries.len() > MAX_TRANSFER_OPERATIONS
            || review.skipped.len() > MAX_TRANSFER_ENTRIES
        {
            self.transfer.notice = LIMIT_NOTICE.to_owned();
            return false;
        }
        let files = review
            .entries
            .iter()
            .filter(|entry| entry.effect != TransferEffect::Mkdir)
            .count();
        let bytes = review
            .entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.bytes));
        let directories = review.entries.len() - files;
        review.executable &=
            self.transfer.complete && !review.entries.is_empty() && !review.digest.is_empty();
        self.transfer.detail = vec![
            format!(
                "Review {:?} · {}",
                review.direction,
                if review.executable {
                    "A approves this exact digest"
                } else {
                    "NOT EXECUTABLE"
                }
            ),
            format!("Local: {}", review.roots.local),
            format!("Sandbox: {}", review.roots.remote),
            format!(
                "{files} files; {directories} directories; {bytes} bytes; {} skipped",
                review.skipped.len()
            ),
            format!("Digest: {}", review.digest),
        ];
        if let Some(notice) = &review.notice {
            self.transfer.detail.push(notice.clone());
        }
        self.transfer
            .detail
            .extend(review.entries.iter().map(|entry| {
                format!(
                    "{} {} ({} bytes)",
                    match entry.effect {
                        TransferEffect::New => "NEW",
                        TransferEffect::Overwrite => "OVERWRITE",
                        TransferEffect::Mkdir => "MKDIR",
                    },
                    entry.path,
                    entry.bytes
                )
            }));
        self.transfer
            .detail
            .extend(review.skipped.iter().map(|path| format!("SKIPPED {path}")));
        self.transfer.detail_open = true;
        self.transfer.review = Some(review);
        true
    }

    pub fn receive_transfer_progress(
        &mut self,
        generation: u64,
        progress: TransferProgress,
    ) -> bool {
        if generation != self.transfer.generation {
            return false;
        }
        self.transfer.progress = Some(progress);
        true
    }

    pub fn receive_transfer_outcome(&mut self, generation: u64, outcome: TransferOutcome) -> bool {
        if generation != self.transfer.generation {
            return false;
        }
        self.transfer.pending = false;
        self.transfer.request = None;
        self.transfer.progress = None;
        self.transfer.invalidate();
        self.transfer.complete = false;
        self.transfer.detail = outcome.entries.clone();
        self.transfer.detail_open = true;
        self.transfer.recovery = Some(TransferOutcome {
            entries: Vec::new(),
            recovery_required: outcome.recovery_required,
        });
        self.transfer.outcome = Some(outcome);
        true
    }

    pub fn receive_transfer_recovery(
        &mut self,
        generation: u64,
        entries: Vec<String>,
        required: bool,
    ) -> bool {
        if generation != self.transfer.generation || self.transfer.draining {
            return false;
        }
        self.transfer.recovery = Some(TransferOutcome {
            entries,
            recovery_required: required,
        });
        true
    }

    pub(crate) fn transfer_switch(&mut self, view: SidebarView) -> WorkbenchAction {
        if view == SidebarView::Transfer {
            self.open_transfer();
        } else if self.transfer_input_active() {
            if !matches!(self.transfer.exit, Some(TransferExit::Close)) {
                self.transfer.exit = Some(TransferExit::View(view));
            }
            return self.transfer.cancel();
        } else {
            self.sidebar = view;
            self.focus = Focus::Sidebar;
        }
        self.sidebar_collapsed = false;
        WorkbenchAction::Consumed
    }

    pub(crate) fn transfer_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if keys::LEADER.matches(key) {
            return WorkbenchAction::Passthrough;
        }
        if key.code == KeyCode::Esc && self.transfer.field.is_none() && !self.transfer.detail_open {
            return self.transfer_switch(SidebarView::Explorer);
        }
        self.transfer.key(key)
    }
}

fn valid_relative(path: &str, empty: bool) -> bool {
    (empty && path.is_empty())
        || (!path.is_empty()
            && path.len() <= MAX_ROOT_LENGTH
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path.split('/').count() <= MAX_PATH_DEPTH
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn join(root: &str, path: &str) -> String {
    if root.is_empty() || root == "." {
        path.to_owned()
    } else {
        format!("{}/{path}", root.trim_end_matches('/'))
    }
}

fn paint(buf: &mut Buffer, area: Rect, text: String, styles: &WorkbenchStyles) {
    if area.is_empty() {
        return;
    }
    chrome::render_line(
        buf,
        Rect::new(area.x, area.y, area.width, 1),
        Line::from(Span::styled(text, styles.text)),
    );
}

fn paint_lines(
    buf: &mut Buffer,
    area: Rect,
    lines: &[String],
    offset: usize,
    styles: &WorkbenchStyles,
) {
    for (row, line) in lines
        .iter()
        .skip(offset)
        .take(usize::from(area.height))
        .enumerate()
    {
        paint(
            buf,
            Rect::new(area.x, area.y + row as u16, area.width, 1),
            line.clone(),
            styles,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ComparisonTree, LIMIT_NOTICE, MAX_TRANSFER_ENTRIES, MAX_TRANSFER_OPERATIONS,
        TransferAction, TransferAvailability, TransferDirection, TransferEffect, TransferEntry,
        TransferNodeKind, TransferOutcome, TransferPreview, TransferProgress, TransferReview,
        TransferReviewEntry, TransferRoots, TransferSnapshot, TransferStatus, join,
    };
    use crate::{
        DocumentKey, Layout, SidebarView, TabLabel, Workbench, WorkbenchAction, WorkbenchStyles,
        keys,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    const ATTACHMENT: &str = "sandbox:one:revision";
    const LOCAL_ROOT: &str = "/host/project";
    const REMOTE_ROOT: &str = "workspace";
    const CONTENTS: &str = "unchanged\n";
    const UPDATED: &str = "published\n";
    const DIGEST: &str = "opaque-reviewed-digest";
    const RECOVERY: &str = "UNKNOWN empty/: publication status must be queried, never replayed";
    const PREVIEW_SUMMARY: &str = "content or executable metadata changed";
    const FILE_NAME: &str = "file.txt";
    const STALE: &str = "stale replies must not mutate the current scope";
    const ISOLATED: &str = "transfer input must never edit or save a preserved tab";
    const RESULT: &str =
        "Result receipt-123; stopped: permission denied; file.txt CONFIRMED; empty/ UNKNOWN";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn roots() -> TransferRoots {
        TransferRoots {
            local: LOCAL_ROOT.to_owned(),
            remote: REMOTE_ROOT.to_owned(),
        }
    }

    fn availability() -> TransferAvailability {
        TransferAvailability {
            attachment: ATTACHMENT.to_owned(),
            label: "Sandbox One".to_owned(),
            local_root: Some(LOCAL_ROOT.to_owned()),
            remote_root: REMOTE_ROOT.to_owned(),
            directory_effects: true,
        }
    }

    fn workbench() -> Workbench {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.set_transfer_availability(Some(availability()));
        assert!(workbench.open_transfer());
        workbench
    }

    fn entry(path: &str, directory: bool, status: TransferStatus) -> TransferEntry {
        let kind = if directory {
            TransferNodeKind::Directory
        } else {
            TransferNodeKind::File
        };
        TransferEntry {
            path: path.to_owned(),
            local: (status != TransferStatus::RemoteOnly).then_some(kind.clone()),
            remote: (status != TransferStatus::LocalOnly).then_some(kind),
            status,
            bytes: if directory { 0 } else { CONTENTS.len() as u64 },
        }
    }

    fn compare(workbench: &mut Workbench, entries: Vec<TransferEntry>, complete: bool) -> u64 {
        let WorkbenchAction::Transfer(TransferAction::Compare { generation, roots }) =
            workbench.handle_key(key(KeyCode::Char('=')))
        else {
            panic!("compare must request host validation");
        };
        assert!(workbench.receive_transfer_snapshot(
            generation,
            TransferSnapshot {
                roots,
                entries,
                complete,
                notice: None
            }
        ));
        generation
    }

    fn review(workbench: &mut Workbench) -> TransferReview {
        let WorkbenchAction::Transfer(TransferAction::Review {
            direction, paths, ..
        }) = workbench.handle_key(key(KeyCode::Char('r')))
        else {
            panic!("eligible selection must request review");
        };
        TransferReview {
            digest: DIGEST.to_owned(),
            roots: workbench.transfer.roots.clone(),
            direction,
            entries: paths
                .into_iter()
                .map(|path| TransferReviewEntry {
                    effect: if workbench.transfer.tree.entries[&path].directory() {
                        TransferEffect::Mkdir
                    } else {
                        TransferEffect::New
                    },
                    path,
                    bytes: 0,
                })
                .collect(),
            skipped: Vec::new(),
            executable: true,
            notice: None,
        }
    }

    fn draw(workbench: &mut Workbench, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .unwrap();
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(false; "unavailable")]
    #[test_case(true; "authenticated_attachment")]
    fn availability_controls_shortcut_and_restore(available: bool) {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        if available {
            workbench.set_transfer_availability(Some(availability()));
        }
        workbench.handle_leader(key(keys::VIEW_TRANSFER.code));
        assert_eq!(workbench.sidebar_view() == SidebarView::Transfer, available);
        workbench.restore(Layout {
            sidebar: SidebarView::Transfer,
            ..Layout::default()
        });
        assert_eq!(workbench.sidebar_view(), SidebarView::Explorer);
    }

    #[test_case(""; "empty_workspace_root")]
    #[test_case("."; "displayed_workspace_root")]
    fn initial_compare_pending_does_not_block_admission(remote_root: &str) {
        let mut workbench = workbench();
        workbench.transfer.roots.remote = remote_root.to_owned();
        let WorkbenchAction::Transfer(TransferAction::Compare { roots, generation }) =
            workbench.handle_key(key(KeyCode::Char('=')))
        else {
            panic!("workspace root must be accepted");
        };
        assert!(roots.remote.is_empty());
        assert!(workbench.is_busy());
        assert!(!workbench.blocks_transfer_start());
        assert!(workbench.set_transfer_connection(generation, true, false));
        assert!(workbench.blocks_transfer_start());
    }

    #[test_case(false; "dirty_tab")]
    #[test_case(true; "pending_write")]
    fn actual_editor_work_blocks_transfer_admission(pending_write: bool) {
        let mut workbench = workbench();
        workbench.open_document(
            DocumentKey(FILE_NAME.to_owned()),
            TabLabel {
                title: FILE_NAME.to_owned(),
                status: FILE_NAME.to_owned(),
            },
            CONTENTS,
        );
        workbench.open_transfer();
        if pending_write {
            workbench
                .pending_save
                .insert(1, workbench.editor.active().unwrap().path.clone());
        } else {
            let tab = workbench.editor.active_mut().unwrap();
            let edit = tab.buffer.insert(UPDATED);
            tab.record(edit);
        }
        workbench.handle_key(key(KeyCode::Char('=')));
        assert!(workbench.blocks_transfer_start());
    }

    #[test_case(TransferDirection::Push; "upload")]
    #[test_case(TransferDirection::Pull; "download")]
    fn folder_selection_deduplicates_component_wise_and_keeps_empty_directories(
        direction: TransferDirection,
    ) {
        let mut workbench = workbench();
        let status = if direction == TransferDirection::Pull {
            TransferStatus::RemoteOnly
        } else {
            TransferStatus::LocalOnly
        };
        compare(
            &mut workbench,
            vec![
                entry("a", true, status.clone()),
                entry("a/empty", true, status.clone()),
                entry("a/file", false, status.clone()),
                entry("ab", false, status),
                entry("a/excluded", true, TransferStatus::Excluded),
                entry("a/excluded/child", false, TransferStatus::Different),
            ],
            true,
        );
        workbench.transfer.direction = direction;
        workbench
            .transfer
            .selected
            .extend(["a".to_owned(), "a/file".to_owned()]);
        let (paths, skipped, bytes) = workbench.transfer.selection();
        assert_eq!(paths, ["a", "a/empty", "a/file"]);
        assert_eq!(skipped, 2);
        assert_eq!(bytes, CONTENTS.len() as u64);
        let summary = &workbench.transfer.tree.summaries["a"];
        assert_eq!(summary.blocked, 2);
    }

    #[test_case(TransferStatus::Different, 1, 0, 0; "equal_directory_changed_child")]
    #[test_case(TransferStatus::Incomplete, 0, 1, 1; "incomplete_child")]
    #[test_case(TransferStatus::TypeConflict, 0, 1, 0; "type_conflict_child")]
    fn matching_directory_does_not_imply_identical_subtree(
        status: TransferStatus,
        changed: usize,
        blocked: usize,
        incomplete: usize,
    ) {
        let tree = ComparisonTree::new(vec![
            entry("a", true, TransferStatus::Equal),
            entry("a/file", false, status),
        ])
        .unwrap();
        let summary = &tree.summaries["a"];
        assert_eq!(
            (summary.changed, summary.blocked, summary.incomplete),
            (changed, blocked, incomplete)
        );
    }

    #[test_case(false; "old_peer_directory_capability")]
    #[test_case(true; "seed_create_only")]
    fn blocked_selection_is_never_claimed_as_copied(seed: bool) {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            vec![
                entry("empty", true, TransferStatus::LocalOnly),
                entry(FILE_NAME, false, TransferStatus::Different),
            ],
            true,
        );
        workbench
            .transfer
            .selected
            .extend(["empty".to_owned(), FILE_NAME.to_owned()]);
        if seed {
            workbench.transfer.direction = TransferDirection::Seed;
        } else {
            workbench
                .transfer
                .availability
                .as_mut()
                .unwrap()
                .directory_effects = false;
        }
        let (paths, skipped, _) = workbench.transfer.selection();
        assert_eq!(paths, [if seed { "empty" } else { FILE_NAME }]);
        assert_eq!(skipped, 1);
    }

    #[test_case(false; "linked_missing_counterpart")]
    #[test_case(true; "independent_pairing")]
    fn navigation_never_rewrites_reviewed_paths(independent: bool) {
        let mut workbench = workbench();
        workbench.transfer.roots.remote = ".".to_owned();
        let generation = compare(
            &mut workbench,
            vec![
                entry("dir", true, TransferStatus::LocalOnly),
                entry("dir/file", false, TransferStatus::LocalOnly),
            ],
            true,
        );
        assert!(workbench.set_transfer_connection(generation, true, false));
        if independent {
            workbench.handle_key(key(KeyCode::Char('n')));
        }
        let action = workbench.handle_key(key(KeyCode::Enter));
        if independent {
            let WorkbenchAction::Transfer(TransferAction::Compare {
                roots,
                generation: next,
            }) = action
            else {
                panic!("independent navigation must compare new roots");
            };
            assert_eq!(roots.local, format!("{LOCAL_ROOT}/dir"));
            assert!(roots.remote.is_empty());
            assert_ne!(next, generation);
        } else {
            assert_eq!(action, WorkbenchAction::Consumed);
            assert_eq!(workbench.transfer.directory, "dir");
            assert_eq!(workbench.transfer.rows(), ["dir/file"]);
            assert_eq!(workbench.transfer_generation(), generation);
        }
        assert_eq!(join(".", "dir"), "dir");
    }

    #[test]
    fn unlink_inside_folder_recompares_both_roots_with_live_worker() {
        let mut workbench = workbench();
        workbench.transfer.roots.remote = ".".to_owned();
        let generation = compare(
            &mut workbench,
            vec![
                entry("dir", true, TransferStatus::Equal),
                entry("dir/file", false, TransferStatus::Different),
            ],
            true,
        );
        assert!(workbench.set_transfer_connection(generation, true, false));
        assert_eq!(
            workbench.handle_key(key(KeyCode::Enter)),
            WorkbenchAction::Consumed
        );
        let WorkbenchAction::Transfer(TransferAction::Compare {
            generation: next,
            roots,
        }) = workbench.handle_key(key(KeyCode::Char('n')))
        else {
            panic!("unlink must schedule replacement comparison while lease is active");
        };
        assert_eq!(roots.local, format!("{LOCAL_ROOT}/dir"));
        assert_eq!(roots.remote, "dir");
        assert_ne!(next, generation);
        assert!(workbench.transfer.busy);
        assert!(workbench.transfer.pending);
        assert!(!workbench.transfer.linked);
    }

    #[test]
    fn missing_counterpart_cannot_become_independent_pairing() {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            vec![entry("dir", true, TransferStatus::LocalOnly)],
            true,
        );
        workbench.handle_key(key(KeyCode::Enter));
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('n'))),
            WorkbenchAction::Consumed
        );
        assert!(workbench.transfer.linked);
        assert_eq!(workbench.transfer.roots, roots());
    }

    #[test_case(false; "incomplete_scan")]
    #[test_case(true; "operation_limit")]
    fn review_refuses_incomplete_or_oversized_selection(oversized: bool) {
        let mut workbench = workbench();
        let count = if oversized {
            MAX_TRANSFER_OPERATIONS + 1
        } else {
            1
        };
        compare(
            &mut workbench,
            (0..count)
                .map(|index| entry(&format!("file{index}"), false, TransferStatus::LocalOnly))
                .collect(),
            oversized,
        );
        workbench
            .transfer
            .selected
            .extend(workbench.transfer.tree.entries.keys().cloned());
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('r'))),
            WorkbenchAction::Consumed
        );
        assert!(workbench.transfer.review.is_none());
        if oversized {
            assert_eq!(workbench.transfer.notice, LIMIT_NOTICE);
        }
    }

    #[test]
    fn oversized_snapshot_is_rejected_not_truncated() {
        let mut workbench = workbench();
        let generation = workbench.transfer_generation();
        assert!(!workbench.receive_transfer_snapshot(
            generation,
            TransferSnapshot {
                roots: roots(),
                entries: vec![
                    entry(FILE_NAME, false, TransferStatus::LocalOnly);
                    MAX_TRANSFER_ENTRIES + 1
                ],
                complete: true,
                notice: None
            }
        ));
        assert!(workbench.transfer.tree.entries.is_empty());
        assert_eq!(workbench.transfer.notice, LIMIT_NOTICE);
    }

    #[test]
    fn review_approval_is_exact_and_one_shot() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry("empty", true, TransferStatus::LocalOnly)],
            true,
        );
        workbench.handle_key(key(KeyCode::Char(' ')));
        let review = review(&mut workbench);
        assert!(workbench.receive_transfer_review(generation, review));
        assert!(
            workbench
                .transfer
                .detail
                .iter()
                .any(|line| line.starts_with("MKDIR empty"))
        );
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('a'))),
            WorkbenchAction::Transfer(TransferAction::Execute {
                generation,
                digest: DIGEST.to_owned()
            })
        );
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('a'))),
            WorkbenchAction::Consumed
        );
    }

    #[test]
    fn stale_review_and_direction_changes_cannot_approve() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::LocalOnly)],
            true,
        );
        workbench.handle_key(key(KeyCode::Char(' ')));
        let reviewed = review(&mut workbench);
        assert!(
            !workbench.receive_transfer_review(generation.wrapping_sub(1), reviewed.clone()),
            "{STALE}"
        );
        assert!(workbench.receive_transfer_review(generation, reviewed.clone()));
        workbench.handle_key(key(KeyCode::Char('d')));
        assert!(
            !workbench.receive_transfer_review(generation, reviewed),
            "{STALE}"
        );
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('a'))),
            WorkbenchAction::Consumed
        );
    }

    #[test_case(false; "binary_metadata")]
    #[test_case(true; "truncated_text")]
    fn inspection_is_read_only_and_does_not_approve(text: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::Different)],
            true,
        );
        assert!(matches!(
            workbench.handle_key(key(KeyCode::Enter)),
            WorkbenchAction::Transfer(TransferAction::Inspect { .. })
        ));
        assert!(workbench.receive_transfer_preview(
            generation,
            TransferPreview {
                path: FILE_NAME.to_owned(),
                local: text.then(|| CONTENTS.to_owned()),
                remote: text.then(|| UPDATED.to_owned()),
                summary: PREVIEW_SUMMARY.to_owned(),
                truncated: text
            }
        ));
        assert!(
            workbench
                .transfer
                .detail
                .iter()
                .any(|line| line == PREVIEW_SUMMARY)
        );
        assert_eq!(
            workbench
                .transfer
                .detail
                .iter()
                .any(|line| line.contains("TRUNCATED")),
            text
        );
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('a'))),
            WorkbenchAction::Consumed
        );
        assert!(workbench.editor.tabs().is_empty());
    }

    #[test]
    fn persisted_recovery_enables_reconcile_without_invalidating_comparison() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::Different)],
            true,
        );
        assert!(workbench.receive_transfer_recovery(generation, vec![RECOVERY.to_owned()], true));
        assert!(workbench.transfer.complete);
        assert_eq!(workbench.transfer.tree.entries.len(), 1);
        workbench.handle_key(key(KeyCode::Char('o')));
        assert_eq!(workbench.transfer.detail, [RECOVERY]);
        assert_eq!(
            workbench.handle_key(key(KeyCode::Char('q'))),
            WorkbenchAction::Transfer(TransferAction::Reconcile { generation })
        );
        assert!(
            !workbench.receive_transfer_recovery(generation.wrapping_sub(1), Vec::new(), false),
            "{STALE}"
        );
    }

    #[test_case(false; "empty_recovery_after_settled_execution")]
    #[test_case(true; "unknown_execution_with_pending_recovery")]
    fn automatic_compare_and_recovery_preserve_last_execution_report(required: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::Different)],
            true,
        );
        assert!(workbench.receive_transfer_outcome(
            generation,
            TransferOutcome {
                entries: vec![RESULT.to_owned()],
                recovery_required: required
            }
        ));
        assert!(workbench.receive_transfer_snapshot(
            generation,
            TransferSnapshot {
                roots: roots(),
                entries: vec![entry(FILE_NAME, false, TransferStatus::Equal)],
                complete: true,
                notice: None
            }
        ));
        let recovery = if required {
            vec![RECOVERY.to_owned()]
        } else {
            Vec::new()
        };
        assert!(workbench.receive_transfer_recovery(generation, recovery, required));
        workbench.handle_key(key(KeyCode::Char('o')));
        assert!(workbench.transfer.detail.iter().any(|line| line == RESULT));
        assert_eq!(
            workbench
                .transfer
                .detail
                .iter()
                .any(|line| line == RECOVERY),
            required
        );
        assert!(workbench.transfer.complete);
        let action = workbench.handle_key(key(KeyCode::Char('q')));
        assert_eq!(
            matches!(
                action,
                WorkbenchAction::Transfer(TransferAction::Reconcile { .. })
            ),
            required
        );
    }

    #[test_case(false; "different_roots")]
    #[test_case(true; "different_attachment")]
    fn execution_reports_never_cross_authority_or_root_pairs(attachment: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::Different)],
            true,
        );
        workbench.receive_transfer_outcome(
            generation,
            TransferOutcome {
                entries: vec![RESULT.to_owned()],
                recovery_required: true,
            },
        );
        workbench.receive_transfer_recovery(generation, vec![RECOVERY.to_owned()], true);
        if attachment {
            let mut available = availability();
            available.attachment.push_str("-next");
            workbench.set_transfer_availability(Some(available));
        } else {
            workbench.transfer.roots.remote.push_str("/next");
            assert!(matches!(
                workbench.handle_key(key(KeyCode::Char('='))),
                WorkbenchAction::Transfer(TransferAction::Compare { .. })
            ));
        }
        assert!(workbench.transfer.outcome.is_none());
        assert!(workbench.transfer.recovery.is_none());
    }

    #[test_case(false; "sidebar_switch")]
    #[test_case(true; "disconnect")]
    fn leaving_drains_before_restoring_editor_input(disconnect: bool) {
        let mut workbench = workbench();
        let generation = workbench.transfer_generation();
        assert!(workbench.set_transfer_connection(generation, true, false));
        if disconnect {
            workbench.set_transfer_availability(None);
            assert_eq!(workbench.transfer_generation(), generation);
            assert!(matches!(
                workbench.cancel_transfer(),
                WorkbenchAction::Transfer(TransferAction::Cancel { .. })
            ));
        } else {
            assert_eq!(
                workbench.handle_leader(key(keys::VIEW_EXPLORER.code)),
                WorkbenchAction::Transfer(TransferAction::Cancel { generation })
            );
        }
        assert!(workbench.transfer_input_active());
        assert_eq!(workbench.sidebar_view(), SidebarView::Transfer);
        assert_eq!(
            workbench.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            WorkbenchAction::Consumed
        );
        assert!(
            !workbench.set_transfer_connection(generation.wrapping_sub(1), false, false),
            "{STALE}"
        );
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert!(!workbench.transfer_input_active());
        assert_eq!(workbench.sidebar_view(), SidebarView::Explorer);
        assert!(
            !workbench.receive_transfer_progress(
                generation,
                TransferProgress {
                    message: RECOVERY.to_owned(),
                    completed: 0,
                    total: 1
                }
            ),
            "{STALE}"
        );
        assert!(
            !workbench.receive_transfer_outcome(
                generation,
                TransferOutcome {
                    entries: vec![RECOVERY.to_owned()],
                    recovery_required: true
                }
            ),
            "{STALE}"
        );
    }

    #[test_case(false; "idle_close_request")]
    #[test_case(true; "idle_direct_close")]
    fn close_transfer_without_worker_closes_immediately_and_preserves_file_tabs(direct: bool) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, CONTENTS).unwrap();
        let mut workbench = workbench();
        workbench.open_at(dir.path(), &path, None);
        workbench.open_transfer();
        workbench.transfer.field = Some(0);
        let generation = workbench.transfer_generation();
        if direct {
            workbench.close();
        } else {
            assert_eq!(workbench.close_transfer(), WorkbenchAction::Consumed);
        }
        assert!(!workbench.is_open());
        assert!(!workbench.transfer_input_active());
        assert!(workbench.transfer.field.is_none());
        assert!(workbench.transfer_generation() > generation);
        assert_eq!(workbench.editor.tabs().len(), 1);
        assert_eq!(workbench.editor.active().unwrap().contents(), CONTENTS);
    }

    #[test_case(false, false; "active_worker_lease")]
    #[test_case(true, false; "pending_command_without_worker")]
    #[test_case(false, true; "disconnect_preserves_close_intent")]
    fn close_transfer_waits_for_matching_cleanup_and_preserves_file_tabs(
        pending: bool,
        disconnect: bool,
    ) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, CONTENTS).unwrap();
        let mut workbench = workbench();
        workbench.open_at(dir.path(), &path, None);
        workbench.open_transfer();
        if pending {
            workbench.handle_key(key(KeyCode::Char('=')));
        }
        let generation = workbench.transfer_generation();
        if !pending {
            workbench.set_transfer_connection(generation, true, false);
        }
        workbench.transfer.field = Some(0);
        workbench.close();
        assert!(workbench.is_open());
        assert_eq!(
            workbench.close_transfer(),
            WorkbenchAction::Transfer(TransferAction::Cancel { generation })
        );
        assert!(workbench.is_open());
        assert!(workbench.transfer_input_active());
        if disconnect {
            workbench.set_transfer_availability(None);
        }
        workbench.handle_leader(key(keys::VIEW_SEARCH.code));
        assert!(!workbench.set_transfer_connection(generation.wrapping_sub(1), false, false));
        assert!(workbench.is_open());
        assert!(workbench.set_transfer_connection(generation, false, true));
        assert!(workbench.is_open());
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert!(!workbench.is_open());
        assert!(!workbench.transfer_input_active());
        assert!(workbench.transfer.field.is_none());
        assert!(workbench.transfer.exit.is_none());
        assert_eq!(workbench.editor.tabs().len(), 1);
        assert_eq!(workbench.editor.active().unwrap().contents(), CONTENTS);
    }

    #[test_case(20, 8; "narrow_short")]
    #[test_case(40, 12; "one_pane")]
    #[test_case(80, 24; "compact_switcher")]
    #[test_case(120, 30; "both_panes")]
    #[test_case(1, 1; "minimal")]
    fn small_terminals_have_bounded_hits_and_both_sides(width: u16, height: u16) {
        let mut workbench = workbench();
        workbench.sidebar_width = crate::MIN_SIDEBAR_WIDTH;
        compare(
            &mut workbench,
            vec![entry(FILE_NAME, false, TransferStatus::LocalOnly)],
            true,
        );
        draw(&mut workbench, width, height);
        for (rect, _) in &workbench.switcher {
            assert!(rect.right() <= width && rect.bottom() <= height);
        }
        if width >= 80 {
            assert_eq!(workbench.switcher.len(), 4);
            let (rect, _) = workbench
                .switcher
                .iter()
                .find(|(_, view)| *view == SidebarView::Transfer)
                .unwrap();
            assert_eq!(
                workbench.handle_mouse(click(rect.x, rect.y)),
                WorkbenchAction::Consumed
            );
        }
        for side in 0..2 {
            workbench.transfer.side = side;
            draw(&mut workbench, width, height);
            let pane = workbench.transfer.pane_rects[side];
            if height >= 8 {
                assert!(!pane.is_empty());
            }
            assert!(pane.right() <= width && pane.bottom() <= height);
        }
        assert!(workbench.panes.text.is_empty());
        assert!(workbench.panes.tabs.is_empty());
    }

    #[test]
    fn editor_tabs_cursor_and_contents_survive_transfer_input_and_refresh() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, CONTENTS).unwrap();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open_at(dir.path(), &path, None);
        let before = workbench.editor.active().unwrap().buffer.cursor();
        workbench.set_transfer_availability(Some(availability()));
        workbench.open_transfer();
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Delete),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                workbench.handle_key(event),
                WorkbenchAction::Consumed,
                "{ISOLATED}"
            );
        }
        assert!(workbench.paste(UPDATED));
        assert_eq!(
            workbench.editor.active().unwrap().contents(),
            CONTENTS,
            "{ISOLATED}"
        );
        assert_eq!(workbench.editor.active().unwrap().buffer.cursor(), before);
        assert_eq!(fs::read_to_string(&path).unwrap(), CONTENTS, "{ISOLATED}");
        workbench.handle_key(key(KeyCode::Char('l')));
        workbench.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        workbench.handle_key(key(KeyCode::Char('c')));
        assert_eq!(workbench.transfer.roots.local, "c");
        workbench.paste(LOCAL_ROOT);
        assert_eq!(workbench.transfer.roots.local, format!("c{LOCAL_ROOT}"));
        fs::write(&path, UPDATED).unwrap();
        workbench.refresh_after_transfer();
        assert_eq!(workbench.editor.tabs().len(), 1);
        assert_eq!(workbench.editor.active().unwrap().contents(), UPDATED);
        let tab = workbench.editor.active_mut().unwrap();
        let edit = tab.buffer.insert(CONTENTS);
        tab.record(edit);
        let dirty = workbench.editor.active().unwrap().contents();
        fs::write(&path, CONTENTS).unwrap();
        workbench.refresh_after_transfer();
        assert_eq!(
            workbench.editor.active().unwrap().contents(),
            dirty,
            "{ISOLATED}"
        );
        assert!(workbench.editor.active().unwrap().is_dirty());
    }
}
