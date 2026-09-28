//! Reviewed transfers between the local project and an attached sandbox.
//!
//! The view compares two roots by content and shows the result as one tree
//! aligned across a local and a sandbox pane. Everything that reads or writes
//! leaves as a [`TransferAction`] tagged with the generation it was asked
//! under, so an answer to a question the view has since moved on from is
//! dropped rather than shown.

mod tree;
mod view;

use std::collections::BTreeSet;
use std::mem;
use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::editor::Tab;
use crate::scm::diff;
use crate::{Drag, Focus, SCROLL_LINES, SidebarView, Workbench, WorkbenchAction, keys, taken};
use tree::{
    ComparisonTree, MAX_PATH_LENGTH, Note, NoteAction, Row, ancestors, depth, join, name, parent,
    relative_to, valid_relative,
};

pub const MAX_TRANSFER_ENTRIES: usize = 20_000;
pub const MAX_TRANSFER_OPERATIONS: usize = 4_096;
/// How many paths one review may name. The engine refuses a larger selection
/// outright, so the view says so before asking.
pub const MAX_TRANSFER_SELECTION: usize = 128;
pub const MAX_TRANSFER_PREVIEW_BYTES: usize = 262_144;
/// How far one step pans the diff sideways, in display columns.
const PAN_COLUMNS: isize = 8;
/// The diff pans sideways instead, which is what `←` and `→` do in it.
const DIFF_WRAPS: bool = false;
/// How a sandbox root may spell the workspace itself.
const WORKSPACE_ROOT: &str = ".";
const ROOT_SEPARATOR: char = '/';
const LIMIT_NOTICE: &str = "Presentation limit exceeded; choose a smaller selection or root. Nothing was truncated or approved.";
const LOCAL_ROOT_RELATIVE: &str = "Local root must be an absolute path";
const SANDBOX_ROOT_OUTSIDE: &str = "Sandbox root must be a folder inside the workspace";
const ROOT_FIX: &str = "to pick a folder";
const DRAIN_NOTICE: &str = "Stopping; input stays locked until cleanup completes";
const CLEANUP_NOTICE: &str = "Cleanup complete. Compare to validate the current roots again";
const REVIEW_INCOMPLETE: &str = "Review needs a complete comparison of the current roots";
const NOTHING_ELIGIBLE: &str = "Nothing to transfer";
const INPUT_REJECTED: &str = "Root rejected: control characters or root length limit exceeded";
const NO_HISTORY: &str = "No earlier root pair";
const FOLDER_NOT_PAIRED: &str = "Only a folder present on both sides can be compared on its own";
const DIFF_TRUNCATED: &str = "Diffs need a complete scan; compare a smaller folder";
const SELECTION_LIMIT: &str = "Too many paths for one transfer; choose at most";
const RECONCILE_OFFLINE: &str = "Reconcile needs a live connection; compare the roots again first";

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

impl TransferRoots {
    /// The view keeps the sandbox workspace itself as an empty root, which
    /// the pane header shows as `/`.
    fn normalize(&mut self) {
        if self.remote == WORKSPACE_ROOT {
            self.remote.clear();
        }
    }

    fn side(&self, side: TransferSide) -> &str {
        match side {
            TransferSide::Local => &self.local,
            TransferSide::Remote => &self.remote,
        }
    }

    fn side_mut(&mut self, side: TransferSide) -> &mut String {
        match side {
            TransferSide::Local => &mut self.local,
            TransferSide::Remote => &mut self.remote,
        }
    }

    /// The first side whose root cannot be compared, and why.
    fn problem(&self) -> Option<(TransferSide, &'static str)> {
        TransferSide::BOTH
            .into_iter()
            .find_map(|side| root_problem(side, self.side(side)).map(|problem| (side, problem)))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TransferDirection {
    #[default]
    Push,
    Pull,
    Seed,
}

/// One end of a transfer, and the pane that shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransferSide {
    #[default]
    Local,
    Remote,
}

impl TransferSide {
    const BOTH: [Self; 2] = [Self::Local, Self::Remote];

    fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Remote => 1,
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Local,
        }
    }

    /// The key that opens this side's root prompt.
    fn root_key(self) -> keys::Bind {
        match self {
            Self::Local => keys::LOCAL_ROOT,
            Self::Remote => keys::SANDBOX_ROOT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferNodeKind {
    File,
    Directory,
    Symlink,
    Repository,
    Special,
}

impl TransferNodeKind {
    /// A folder, or something standing where one could be. Either unfolds, to
    /// list what it holds or to say why it lists nothing.
    fn expandable(self) -> bool {
        self != Self::File
    }
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

    /// A difference the scan settled, as opposed to one it could not.
    fn changed(&self) -> bool {
        matches!(
            self,
            Self::LocalOnly | Self::RemoteOnly | Self::Different | Self::TypeConflict
        )
    }
}

/// Why a path is left out of every transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferExclusion {
    Protected,
    Pattern,
    Gitignore,
    /// Named with a leading dot while dotfiles are skipped.
    Dotfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEntry {
    pub path: String,
    pub local: Option<TransferNodeKind>,
    pub remote: Option<TransferNodeKind>,
    pub status: TransferStatus,
    /// The file's size on each side, zero where that side holds no file.
    pub local_bytes: u64,
    pub remote_bytes: u64,
    pub excluded: Option<TransferExclusion>,
    /// A folder at least one side never finished listing.
    pub unlisted: bool,
}

impl TransferEntry {
    fn kind(&self, side: TransferSide) -> Option<TransferNodeKind> {
        match side {
            TransferSide::Local => self.local,
            TransferSide::Remote => self.remote,
        }
    }

    fn bytes(&self, side: TransferSide) -> u64 {
        match side {
            TransferSide::Local => self.local_bytes,
            TransferSide::Remote => self.remote_bytes,
        }
    }

    fn is_dir(&self) -> bool {
        self.local == Some(TransferNodeKind::Directory)
            || self.remote == Some(TransferNodeKind::Directory)
    }

    fn expandable(&self) -> bool {
        [self.local, self.remote]
            .into_iter()
            .flatten()
            .any(TransferNodeKind::expandable)
    }
}

/// Why one side's scan stopped short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransferScanLimit {
    Entries,
    Pages,
    Depth,
    Bytes,
    WorkcellIncomplete,
    ListingFailed,
    Changed,
    Unreadable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferScan {
    pub unsupported: bool,
    pub limits: BTreeSet<TransferScanLimit>,
}

impl TransferScan {
    pub fn complete(&self) -> bool {
        !self.unsupported && self.limits.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSnapshot {
    pub roots: TransferRoots,
    pub entries: Vec<TransferEntry>,
    pub local: TransferScan,
    pub remote: TransferScan,
}

impl TransferSnapshot {
    /// A comparison is complete only when both of its sides are.
    pub fn complete(&self) -> bool {
        self.local.complete() && self.remote.complete()
    }
}

/// What one side of an inspected file holds, as far as the host read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPreviewSide {
    pub kind: TransferNodeKind,
    pub bytes: u64,
    pub digest: String,
    pub text: Option<String>,
    pub binary: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPreview {
    pub path: String,
    pub local: Option<TransferPreviewSide>,
    pub remote: Option<TransferPreviewSide>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    Scanning,
    Staging,
    Sealing,
    Preparing,
    Reviewing,
    Publishing,
    Reconciling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    pub phase: TransferPhase,
    pub side: Option<TransferSide>,
    pub path: Option<String>,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFileOutcome {
    Confirmed,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcomeEntry {
    pub path: String,
    pub outcome: TransferFileOutcome,
}

/// What the journal still holds for the current roots, and whether it blocks
/// until reconciled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferRecovery {
    pub lines: Vec<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferOutcome {
    pub entries: Vec<TransferOutcomeEntry>,
    pub stopped: Option<String>,
    /// The journal as the worker last read it, or `None` for a request that
    /// never reached a worker, which keeps the recovery and the report
    /// already on screen.
    pub recovery: Option<TransferRecovery>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAction {
    Compare {
        generation: u64,
        roots: TransferRoots,
        include_ignored: bool,
        skip_dotfiles: bool,
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

enum TransferExit {
    View(SidebarView),
    /// The Explorer, with quick open up over it.
    QuickOpen,
    Close,
}

/// What stands in the tree's place until it is closed.
enum Panel {
    Diff {
        path: String,
        truncated: bool,
        tab: Box<Tab>,
    },
    Binary(TransferPreview),
    Review(TransferReview),
    Report,
}

/// A root pair the view has moved on from, and how it was left.
struct Visit {
    roots: TransferRoots,
    expanded: BTreeSet<String>,
    folder: Option<String>,
}

/// A root being typed. Nothing changes until it is confirmed.
struct Prompt {
    side: TransferSide,
    text: String,
}

enum Notice {
    Info(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Compare,
    Upload,
    Download,
    Approve,
    Stop,
}

impl Button {
    const ALL: [Self; 5] = [
        Self::Compare,
        Self::Upload,
        Self::Download,
        Self::Approve,
        Self::Stop,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Compare => "Compare",
            Self::Upload => "Upload",
            Self::Download => "Download",
            Self::Approve => "Approve",
            Self::Stop => "Stop",
        }
    }

    fn bind(self) -> keys::Bind {
        match self {
            Self::Compare => keys::COMPARE,
            Self::Upload => keys::UPLOAD,
            Self::Download => keys::DOWNLOAD,
            Self::Approve => keys::APPROVE,
            Self::Stop => keys::STOP,
        }
    }
}

/// Where the last frame put what a click can land on.
#[derive(Default)]
struct Hits {
    buttons: Vec<(Rect, Button)>,
    headers: [Rect; 2],
    panes: [Rect; 2],
    rows: Rect,
    panel: Rect,
}

#[derive(Default)]
struct Selection<'a> {
    paths: Vec<&'a str>,
    skipped: usize,
    bytes: u64,
}

#[derive(Default)]
pub(crate) struct TransferState {
    availability: Option<TransferAvailability>,
    generation: u64,
    busy: bool,
    draining: bool,
    pending: bool,
    request: Option<TransferAction>,
    next_availability: Option<Option<TransferAvailability>>,
    roots: TransferRoots,
    confirmed_roots: Option<TransferRoots>,
    history: Vec<Visit>,
    /// Set by the initial seed, which uploads create-only until it runs.
    seed: bool,
    include_ignored: bool,
    skip_dotfiles: bool,
    changes_only: bool,
    tree: ComparisonTree,
    scans: [TransferScan; 2],
    complete: bool,
    expanded: BTreeSet<String>,
    rows: Vec<Row>,
    cursor: usize,
    scroll: usize,
    /// Where the cursor goes once the comparison in flight arrives.
    anchor: Option<String>,
    focus: TransferSide,
    selected: BTreeSet<String>,
    panel: Option<Panel>,
    offset: usize,
    progress: Option<TransferProgress>,
    outcome: Option<TransferOutcome>,
    /// Which way the transfer in `outcome` went, when it was one approved here.
    reported: Option<TransferDirection>,
    recovery: Option<TransferRecovery>,
    /// Which way the transfer being executed goes.
    approved: Option<TransferDirection>,
    notice: Option<Notice>,
    failure: Option<String>,
    prompt: Option<Prompt>,
    hits: Hits,
    exit: Option<TransferExit>,
}

impl TransferState {
    pub(crate) fn close(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.clear_comparison();
        self.panel = None;
        self.prompt = None;
        self.history.clear();
        self.expanded.clear();
        self.anchor = None;
        self.seed = false;
        self.include_ignored = false;
        self.skip_dotfiles = false;
        self.progress = None;
        self.hits = Hits::default();
        self.exit = None;
    }

    pub(crate) fn lease_active(&self) -> bool {
        self.busy || self.draining
    }

    pub(crate) fn connection_active(&self) -> bool {
        self.busy || self.draining || self.pending
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.prompt.is_some() && !self.pending && !self.draining
    }

    pub(crate) fn available(&self) -> bool {
        self.availability.is_some()
    }

    fn scan(&self, side: TransferSide) -> &TransferScan {
        &self.scans[side.index()]
    }

    fn upload(&self) -> TransferDirection {
        match self.seed {
            true => TransferDirection::Seed,
            false => TransferDirection::Push,
        }
    }

    /// Closes the panel drawn from the comparison. A report speaks for a
    /// transfer rather than a comparison, so it stays up until closed.
    fn invalidate(&mut self) {
        if !matches!(self.panel, Some(Panel::Report)) {
            self.panel = None;
        }
    }

    /// Drops the last report and recovery, which only ever speak for the
    /// roots and the attachment they were made under.
    fn forget_transfer(&mut self) {
        self.outcome = None;
        self.reported = None;
        self.recovery = None;
        if matches!(self.panel, Some(Panel::Report)) {
            self.panel = None;
        }
    }

    fn open_panel(&mut self, panel: Panel) {
        self.panel = Some(panel);
        self.offset = 0;
    }

    fn clear_comparison(&mut self) {
        self.invalidate();
        self.tree = ComparisonTree::default();
        self.rows.clear();
        self.scans = Default::default();
        self.complete = false;
        self.failure = None;
        self.selected.clear();
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Forgets everything learned under the current roots and starts over at
    /// `roots`.
    fn reset(&mut self, roots: TransferRoots) {
        self.generation = self.generation.wrapping_add(1);
        self.clear_comparison();
        self.forget_transfer();
        self.pending = false;
        self.request = None;
        self.confirmed_roots = None;
        self.history.clear();
        self.expanded.clear();
        self.anchor = None;
        self.seed = false;
        self.include_ignored = false;
        self.skip_dotfiles = false;
        self.roots = roots;
        self.roots.normalize();
    }

    fn cursor_row(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    fn cursor_entry(&self) -> Option<&str> {
        match self.cursor_row()? {
            Row::Entry(path) => Some(path),
            Row::Note(_) => None,
        }
    }

    fn viewport(&self) -> usize {
        usize::from(self.hits.rows.height)
    }

    /// Rebuilds the rows after the tree, the folds or the filter changed, and
    /// puts the cursor back on `keep` or on the nearest folder above it that
    /// is still listed.
    fn refresh_rows(&mut self, keep: Option<Row>) {
        self.rows = self.tree.rows(&self.expanded, self.changes_only);
        self.cursor = keep
            .and_then(|keep| {
                self.rows.iter().position(|row| *row == keep).or_else(|| {
                    ancestors(keep.path()).find_map(|ancestor| {
                        self.rows
                            .iter()
                            .position(|row| matches!(row, Row::Entry(path) if path == ancestor))
                    })
                })
            })
            .unwrap_or(0);
    }

    /// Whether a folder above `path` is chosen, which carries `path` along.
    fn implied(&self, path: &str) -> bool {
        ancestors(parent(path)).any(|ancestor| self.selected.contains(ancestor))
    }

    /// What a transfer in `direction` would carry: the chosen rows, or the
    /// row under the cursor when none are, with everything under a chosen
    /// folder. Blocked paths, and ones this peer cannot create, are skipped;
    /// equal ones need nothing.
    fn selection(&self, direction: &TransferDirection) -> Selection<'_> {
        let chosen: Vec<&str> = match self.selected.is_empty() {
            true => self.cursor_entry().into_iter().collect(),
            false => self
                .selected
                .iter()
                .map(String::as_str)
                .filter(|path| !self.implied(path))
                .collect(),
        };
        let directory_effects = self
            .availability
            .as_ref()
            .is_some_and(|available| available.directory_effects);
        let source_side = match direction {
            TransferDirection::Pull => TransferSide::Remote,
            TransferDirection::Push | TransferDirection::Seed => TransferSide::Local,
        };
        let mut selection = Selection::default();
        for entry in chosen.into_iter().flat_map(|path| self.tree.subtree(path)) {
            let (source, destination) = (entry.kind(source_side), entry.kind(source_side.other()));
            if self.tree.blocked_ancestor(&entry.path)
                || !matches!(
                    source,
                    Some(TransferNodeKind::File | TransferNodeKind::Directory)
                )
            {
                selection.skipped += 1;
                continue;
            }
            if entry.status == TransferStatus::Equal {
                continue;
            }
            if *direction == TransferDirection::Seed && destination.is_some() {
                selection.skipped += 1;
                continue;
            }
            if source == Some(TransferNodeKind::Directory) {
                if destination.is_some() {
                    continue;
                }
                if !directory_effects {
                    selection.skipped += 1;
                    continue;
                }
            } else {
                selection.bytes = selection.bytes.saturating_add(entry.bytes(source_side));
            }
            selection.paths.push(&entry.path);
        }
        selection.paths.sort_unstable();
        selection
    }

    fn action(&mut self, action: TransferAction) -> WorkbenchAction {
        self.pending = true;
        self.progress = None;
        self.request = Some(action.clone());
        WorkbenchAction::Transfer(action)
    }

    fn cancel(&mut self) -> WorkbenchAction {
        self.invalidate();
        self.request = None;
        self.draining = true;
        self.notice = Some(Notice::Info(DRAIN_NOTICE.to_owned()));
        WorkbenchAction::Transfer(TransferAction::Cancel {
            generation: self.generation,
        })
    }

    fn compare(&mut self) -> WorkbenchAction {
        self.roots.normalize();
        if let Some((side, problem)) = self.roots.problem() {
            let fix = side.root_key().label;
            self.notice = Some(Notice::Error(format!("{problem}; press {fix} {ROOT_FIX}")));
            return WorkbenchAction::Consumed;
        }
        if self.confirmed_roots.as_ref() == Some(&self.roots) {
            if self.anchor.is_none() {
                self.anchor = self.cursor_row().map(|row| row.path().to_owned());
            }
        } else {
            self.forget_transfer();
        }
        self.generation = self.generation.wrapping_add(1);
        self.panel = None;
        self.clear_comparison();
        self.notice = None;
        self.action(TransferAction::Compare {
            generation: self.generation,
            roots: self.roots.clone(),
            include_ignored: self.include_ignored,
            skip_dotfiles: self.skip_dotfiles,
        })
    }

    fn toggle_ignored(&mut self) -> WorkbenchAction {
        self.include_ignored = !self.include_ignored;
        self.compare()
    }

    fn toggle_dotfiles(&mut self) -> WorkbenchAction {
        self.skip_dotfiles = !self.skip_dotfiles;
        self.compare()
    }

    /// Re-roots both sides at `folder`, keeping what was unfolded inside it.
    /// The pair it leaves is what `Backspace` goes back to.
    fn compare_folder(&mut self, folder: &str) -> WorkbenchAction {
        let paired = self.tree.entry(folder).is_some_and(|entry| {
            entry.local == Some(TransferNodeKind::Directory)
                && entry.remote == Some(TransferNodeKind::Directory)
        });
        if !paired {
            self.notice = Some(Notice::Error(FOLDER_NOT_PAIRED.to_owned()));
            return WorkbenchAction::Consumed;
        }
        let expanded = self
            .expanded
            .iter()
            .filter_map(|open| relative_to(open, folder))
            .map(str::to_owned)
            .collect();
        let roots = TransferRoots {
            local: join(&self.roots.local, folder),
            remote: join(&self.roots.remote, folder),
        };
        self.history.push(Visit {
            roots: mem::replace(&mut self.roots, roots),
            expanded: mem::replace(&mut self.expanded, expanded),
            folder: Some(folder.to_owned()),
        });
        self.anchor = None;
        self.compare()
    }

    fn restore_roots(&mut self) -> WorkbenchAction {
        let Some(visit) = self.history.pop() else {
            self.notice = Some(Notice::Info(NO_HISTORY.to_owned()));
            return WorkbenchAction::Consumed;
        };
        self.roots = visit.roots;
        self.expanded = visit.expanded;
        self.anchor = visit.folder;
        self.compare()
    }

    fn edit_root(&mut self, side: TransferSide) {
        let text = match side {
            TransferSide::Local => self.roots.local.clone(),
            TransferSide::Remote => format!("{ROOT_SEPARATOR}{}", self.roots.remote),
        };
        self.prompt = Some(Prompt { side, text });
    }

    /// Takes the typed root and compares under it. A typed root that cannot
    /// be right keeps the prompt up with the text still in it. Only that root
    /// is judged here: a bad root on the other side is refused by the
    /// comparison instead, which names it.
    fn apply_prompt(&mut self) -> WorkbenchAction {
        let Some(prompt) = self.prompt.take() else {
            return WorkbenchAction::Consumed;
        };
        let mut roots = self.roots.clone();
        *roots.side_mut(prompt.side) = match prompt.side {
            TransferSide::Local => prompt.text.clone(),
            TransferSide::Remote => prompt.text.trim_matches(ROOT_SEPARATOR).to_owned(),
        };
        roots.normalize();
        if let Some(problem) = root_problem(prompt.side, roots.side(prompt.side)) {
            self.notice = Some(Notice::Error(problem.to_owned()));
            self.prompt = Some(prompt);
            return WorkbenchAction::Consumed;
        }
        if roots != self.roots {
            self.history.push(Visit {
                roots: mem::replace(&mut self.roots, roots),
                expanded: mem::take(&mut self.expanded),
                folder: None,
            });
            self.anchor = None;
        }
        self.compare()
    }

    fn review(&mut self, direction: TransferDirection) -> WorkbenchAction {
        self.invalidate();
        let selection = self.selection(&direction);
        let refusal = if !self.complete || self.confirmed_roots.as_ref() != Some(&self.roots) {
            REVIEW_INCOMPLETE.to_owned()
        } else if selection.paths.len() > MAX_TRANSFER_SELECTION {
            format!("{SELECTION_LIMIT} {MAX_TRANSFER_SELECTION}")
        } else if selection.paths.is_empty() {
            format!(
                "{NOTHING_ELIGIBLE}; {} skipped or blocked",
                selection.skipped
            )
        } else {
            let paths = selection.paths.into_iter().map(str::to_owned).collect();
            return self.action(TransferAction::Review {
                generation: self.generation,
                direction,
                paths,
            });
        };
        self.notice = Some(Notice::Error(refusal));
        WorkbenchAction::Consumed
    }

    /// Approves the review on screen and nothing else: a review that was
    /// closed, replaced or refused cannot be approved blind.
    fn approve(&mut self) -> WorkbenchAction {
        match self.panel.take() {
            Some(Panel::Review(review)) if review.executable && !review.digest.is_empty() => {
                self.approved = Some(review.direction);
                self.action(TransferAction::Execute {
                    generation: self.generation,
                    digest: review.digest,
                })
            }
            panel => {
                self.panel = panel;
                WorkbenchAction::Consumed
            }
        }
    }

    fn toggle(&mut self, path: &str) {
        if self.expanded.remove(path) {
            self.expanded
                .retain(|open| relative_to(open, path).is_none());
        } else {
            self.expanded.insert(path.to_owned());
        }
        self.refresh_rows(Some(Row::Entry(path.to_owned())));
    }

    fn collapse_all(&mut self) {
        let keep = self.cursor_row().cloned();
        self.expanded.clear();
        self.refresh_rows(keep);
    }

    /// Folds the folder under the cursor, or steps out to the folder above.
    fn collapse_or_parent(&mut self) {
        let Some(row) = self.cursor_row().cloned() else {
            return;
        };
        let target = match row {
            Row::Entry(path) if self.expanded.contains(&path) => {
                self.toggle(&path);
                return;
            }
            Row::Entry(path) => parent(&path).to_owned(),
            Row::Note(folder) => folder,
        };
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Entry(path) if *path == target))
        {
            self.cursor = index;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
    }

    fn toggle_selected(&mut self) {
        if let Some(path) = self.cursor_entry().map(str::to_owned)
            && !self.selected.remove(&path)
        {
            self.selected.insert(path);
        }
    }

    /// `Enter` on the cursor: a folder folds or unfolds, a file opens its
    /// diff, and a note does what it offers.
    fn activate(&mut self) -> WorkbenchAction {
        let Some(row) = self.cursor_row().cloned() else {
            return WorkbenchAction::Consumed;
        };
        let path = match row {
            Row::Note(folder) => return self.note_action(&folder),
            Row::Entry(path) => path,
        };
        let Some(entry) = self.tree.entry(&path) else {
            return WorkbenchAction::Consumed;
        };
        if entry.expandable() {
            self.toggle(&path);
            return WorkbenchAction::Consumed;
        }
        if matches!(
            entry.status,
            TransferStatus::Excluded | TransferStatus::Unsupported
        ) {
            self.notice = TransferSide::BOTH
                .into_iter()
                .find_map(|side| self.tree.note(&path, side, self.scan(side)))
                .map(|note| Notice::Info(note.text().to_owned()));
            return WorkbenchAction::Consumed;
        }
        self.invalidate();
        if TransferSide::BOTH.into_iter().any(|side| {
            self.scan(side)
                .limits
                .contains(&TransferScanLimit::WorkcellIncomplete)
        }) {
            self.notice = Some(Notice::Error(DIFF_TRUNCATED.to_owned()));
            return WorkbenchAction::Consumed;
        }
        self.action(TransferAction::Inspect {
            generation: self.generation,
            path,
        })
    }

    /// What the note under `folder` offers, asked of each side in turn.
    fn offer(&self, folder: &str) -> Option<NoteAction> {
        TransferSide::BOTH
            .into_iter()
            .filter_map(|side| self.tree.note(folder, side, self.scan(side)))
            .find_map(Note::action)
    }

    fn note_action(&mut self, folder: &str) -> WorkbenchAction {
        match self.offer(folder) {
            Some(NoteAction::IncludeIgnored) => self.toggle_ignored(),
            Some(NoteAction::IncludeDotfiles) => self.toggle_dotfiles(),
            Some(NoteAction::CompareFolder) => self.compare_folder(folder),
            None => WorkbenchAction::Consumed,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if self.draining || self.pending {
            return match keys::STOP.matches(key) {
                true => self.cancel(),
                false => WorkbenchAction::Consumed,
            };
        }
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        self.notice = None;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return WorkbenchAction::Consumed;
        }
        if keys::COMPARE.matches(key) || keys::REFRESH.matches(key) {
            return self.compare();
        }
        if keys::UPLOAD.matches(key) {
            return self.review(self.upload());
        }
        if keys::DOWNLOAD.matches(key) {
            return self.review(TransferDirection::Pull);
        }
        if keys::APPROVE.matches(key) {
            return self.approve();
        }
        if keys::INCLUDE_IGNORED.matches(key) {
            return self.toggle_ignored();
        }
        if keys::SKIP_DOTFILES.matches(key) {
            return self.toggle_dotfiles();
        }
        if keys::PREVIOUS_ROOTS.matches(key) {
            return self.restore_roots();
        }
        if keys::STOP.matches(key) && self.connection_active() {
            return self.cancel();
        }
        if keys::RECONCILE.matches(key)
            && self
                .recovery
                .as_ref()
                .is_some_and(|recovery| recovery.required)
        {
            if !self.busy {
                self.notice = Some(Notice::Error(RECONCILE_OFFLINE.to_owned()));
                return WorkbenchAction::Consumed;
            }
            return self.action(TransferAction::Reconcile {
                generation: self.generation,
            });
        }
        if keys::CHANGES_ONLY.matches(key) {
            self.changes_only = !self.changes_only;
            self.refresh_rows(self.cursor_row().cloned());
        } else if let Some(side) = TransferSide::BOTH
            .into_iter()
            .find(|side| side.root_key().matches(key))
        {
            self.edit_root(side);
        } else if keys::REPORT.matches(key) {
            self.open_panel(Panel::Report);
        } else if keys::FOCUS_NEXT.matches(key) || key.code == KeyCode::BackTab {
            self.focus = self.focus.other();
        } else if self.panel.is_some() {
            self.panel_key(key);
        } else {
            return self.tree_key(key);
        }
        WorkbenchAction::Consumed
    }

    fn tree_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        let page = self.viewport().max(1) as isize;
        match key.code {
            _ if keys::SELECT.matches(key) => self.toggle_selected(),
            _ if keys::COLLAPSE_ALL.matches(key) => self.collapse_all(),
            _ if keys::NEXT_ROW.matches(key) => self.move_cursor(1),
            _ if keys::PREVIOUS_ROW.matches(key) => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.rows.len().saturating_sub(1),
            KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Right | KeyCode::Enter => return self.activate(),
            _ => {}
        }
        WorkbenchAction::Consumed
    }

    fn panel_key(&mut self, key: KeyEvent) {
        let page = self.hits.panel.height.max(1) as isize;
        match key.code {
            _ if keys::NEXT_ROW.matches(key) => self.scroll_panel(1),
            _ if keys::PREVIOUS_ROW.matches(key) => self.scroll_panel(-1),
            KeyCode::Down => self.scroll_panel(1),
            KeyCode::Up => self.scroll_panel(-1),
            KeyCode::PageDown => self.scroll_panel(page),
            KeyCode::PageUp => self.scroll_panel(-page),
            KeyCode::Home => self.scroll_panel(isize::MIN),
            KeyCode::End => self.scroll_panel(isize::MAX),
            KeyCode::Left => self.pan(-PAN_COLUMNS),
            KeyCode::Right => self.pan(PAN_COLUMNS),
            _ => {}
        }
    }

    fn prompt_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        if key.code == KeyCode::Enter {
            return self.apply_prompt();
        }
        if keys::CLEAR_ROOT.matches(key) {
            if let Some(prompt) = &mut self.prompt {
                prompt.text.clear();
            }
        } else if key.code == KeyCode::Backspace {
            if let Some(prompt) = &mut self.prompt {
                prompt.text.pop();
            }
        } else if let KeyCode::Char(ch) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.paste(ch.encode_utf8(&mut [0; 4]));
        }
        WorkbenchAction::Consumed
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.pending || self.draining {
            return;
        }
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        if prompt.text.len().saturating_add(text.len()) > MAX_PATH_LENGTH
            || text.chars().any(char::is_control)
        {
            self.notice = Some(Notice::Error(INPUT_REJECTED.to_owned()));
        } else {
            prompt.text.push_str(text);
        }
    }

    /// Closes whatever stands over the tree, one layer per call. Reports
    /// whether there was anything to close.
    fn dismiss(&mut self) -> bool {
        self.prompt.take().is_some() || self.panel.take().is_some()
    }

    /// A wheel turn, which moves the view and keeps the cursor inside it.
    pub(crate) fn scroll(&mut self, delta: isize) {
        if self.panel.is_some() {
            self.scroll_panel(delta);
            return;
        }
        let viewport = self.viewport().max(1);
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(viewport));
        self.cursor = self
            .cursor
            .clamp(self.scroll, self.scroll + viewport - 1)
            .min(self.rows.len().saturating_sub(1));
    }

    fn scroll_to(&mut self, offset: usize) {
        match &mut self.panel {
            Some(Panel::Diff { tab, .. }) => tab.set_scroll(offset),
            Some(_) => self.offset = offset,
            None => self.scroll(offset as isize - self.scroll as isize),
        }
    }

    fn scroll_panel(&mut self, delta: isize) {
        let area = self.hits.panel;
        match &mut self.panel {
            Some(Panel::Diff { tab, .. }) => {
                let lines = tab.buffer.line_count() as isize;
                tab.scroll_by(
                    delta.clamp(-lines, lines),
                    usize::from(area.height),
                    usize::from(area.width),
                    DIFF_WRAPS,
                );
            }
            Some(_) => self.offset = self.offset.saturating_add_signed(delta),
            None => {}
        }
    }

    fn pan(&mut self, delta: isize) {
        let area = self.hits.panel;
        if let Some(Panel::Diff { tab, .. }) = &mut self.panel {
            tab.h_scroll_by(delta, usize::from(area.height), usize::from(area.width));
        }
    }

    /// Has a diff on screen colour itself again after a palette change, as
    /// the editor's tabs do.
    pub(crate) fn set_theme_generation(&mut self, generation: u64) {
        if let Some(Panel::Diff { tab, .. }) = &mut self.panel {
            tab.set_theme_generation(generation);
        }
    }

    fn press(&mut self, at: (u16, u16), clicks: u8) -> WorkbenchAction {
        let position = at.into();
        if let Some(button) = self
            .hits
            .buttons
            .iter()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, button)| *button)
        {
            // Asked again, since the frame that placed the button may predate
            // a prompt that would otherwise take its key as typing.
            if !self.enabled(button) {
                return WorkbenchAction::Consumed;
            }
            let bind = button.bind();
            return self.key(KeyEvent::new(bind.code, bind.modifiers));
        }
        if self.pending || self.draining {
            return WorkbenchAction::Consumed;
        }
        if let Some(side) = TransferSide::BOTH
            .into_iter()
            .find(|side| self.hits.headers[side.index()].contains(position))
        {
            self.edit_root(side);
            return WorkbenchAction::Consumed;
        }
        let Some(side) = TransferSide::BOTH
            .into_iter()
            .find(|side| self.hits.panes[side.index()].contains(position))
        else {
            return WorkbenchAction::Consumed;
        };
        let pane = self.hits.panes[side.index()];
        let index = self.scroll + usize::from(at.1 - pane.y);
        let Some(row) = self.rows.get(index).cloned() else {
            return WorkbenchAction::Consumed;
        };
        self.focus = side;
        self.cursor = index;
        let column = usize::from(at.0 - pane.x);
        match row {
            Row::Entry(_) if view::on_check(column) => self.toggle_selected(),
            Row::Entry(path)
                if view::on_marker(column, depth(&path))
                    && self
                        .tree
                        .entry(&path)
                        .is_some_and(TransferEntry::expandable) =>
            {
                self.toggle(&path)
            }
            _ if clicks >= 2 => return self.activate(),
            _ => {}
        }
        WorkbenchAction::Consumed
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
            self.transfer.notice = Some(Notice::Info(DRAIN_NOTICE.to_owned()));
            return;
        }
        if changed {
            self.transfer.reset(availability.as_ref().map_or_else(
                TransferRoots::default,
                |available| TransferRoots {
                    local: available.local_root.clone().unwrap_or_default(),
                    remote: available.remote_root.clone(),
                },
            ));
        }
        self.transfer.availability = availability;
        if !self.transfer.available() && self.sidebar == SidebarView::Transfer {
            if self.transfer.busy || self.transfer.draining {
                self.transfer.draining = true;
                self.transfer
                    .exit
                    .get_or_insert(TransferExit::View(SidebarView::Explorer));
                self.transfer.notice = Some(Notice::Info(DRAIN_NOTICE.to_owned()));
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

    /// Brings the view up. Coming from another view starts without ignored
    /// files and with dotfiles, since those choices last only as long as the
    /// view stays up.
    pub fn open_transfer(&mut self) -> bool {
        if !self.transfer.available() || self.transfer.draining {
            return false;
        }
        if self.sidebar != SidebarView::Transfer {
            self.transfer.include_ignored = false;
            self.transfer.skip_dotfiles = false;
        }
        self.open = true;
        self.sidebar = SidebarView::Transfer;
        self.focus = Focus::Editor;
        self.palette.close();
        self.confirm = None;
        self.menu = None;
        self.input = None;
        self.goto = None;
        self.drag = Drag::None;
        true
    }

    pub fn show_transfer(&mut self, roots: TransferRoots, direction: TransferDirection) -> bool {
        if self.transfer.busy || self.transfer.draining || !self.open_transfer() {
            return false;
        }
        self.transfer.reset(roots);
        self.transfer.seed = direction == TransferDirection::Seed;
        true
    }

    /// The worker is gone while the comparison replacing it waits to start.
    /// Its lease ends, so that comparison can be admitted, and the view keeps
    /// waiting on it.
    pub fn release_transfer_worker(&mut self, generation: u64) -> bool {
        if generation != self.transfer.generation {
            return false;
        }
        self.transfer.busy = false;
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
            self.transfer.approved = None;
            let exit = self.transfer.exit.take();
            if was_draining {
                self.transfer.generation = self.transfer.generation.wrapping_add(1);
                self.transfer.clear_comparison();
                self.transfer.progress = None;
                self.transfer.notice = Some(Notice::Info(CLEANUP_NOTICE.to_owned()));
            }
            if let Some(availability) = self.transfer.next_availability.take() {
                self.set_transfer_availability(availability);
            }
            match exit {
                Some(TransferExit::View(view)) => {
                    self.sidebar = view;
                    self.focus = Focus::Sidebar;
                }
                Some(TransferExit::QuickOpen) => {
                    self.sidebar = SidebarView::Explorer;
                    self.focus = Focus::Sidebar;
                    self.quick_open();
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

    /// Takes a comparison. Rows the tree cannot place are dropped one by one
    /// and counted on screen; only a comparison past the presentation limit is
    /// refused whole.
    pub fn receive_transfer_snapshot(
        &mut self,
        generation: u64,
        mut snapshot: TransferSnapshot,
    ) -> bool {
        let state = &mut self.transfer;
        if generation != state.generation || state.draining {
            return false;
        }
        // The worker compares again on its own after a transfer, so a request
        // asked meanwhile is still owed its own answer.
        if matches!(state.request, Some(TransferAction::Compare { .. })) {
            state.pending = false;
            state.request = None;
        }
        snapshot.roots.normalize();
        if state
            .confirmed_roots
            .as_ref()
            .is_some_and(|roots| roots != &snapshot.roots)
        {
            state.forget_transfer();
        }
        state.invalidate();
        if snapshot.entries.len() > MAX_TRANSFER_ENTRIES {
            state.clear_comparison();
            state.notice = Some(Notice::Error(LIMIT_NOTICE.to_owned()));
            return false;
        }
        state.complete = snapshot.complete();
        state.tree = ComparisonTree::new(snapshot.entries);
        state.scans = [snapshot.local, snapshot.remote];
        state.roots = snapshot.roots.clone();
        state.confirmed_roots = Some(snapshot.roots);
        state.selected.clear();
        let anchor = state.anchor.take().map(Row::Entry);
        state.refresh_rows(anchor);
        true
    }

    pub fn receive_transfer_preview(&mut self, generation: u64, preview: TransferPreview) -> bool {
        let state = &mut self.transfer;
        if generation != state.generation
            || state.draining
            || !matches!(&state.request, Some(TransferAction::Inspect { path, .. }) if *path == preview.path)
        {
            return false;
        }
        state.pending = false;
        state.request = None;
        let (local, remote) = (
            preview_text(preview.local.as_ref()),
            preview_text(preview.remote.as_ref()),
        );
        if local
            .map_or(0, str::len)
            .saturating_add(remote.map_or(0, str::len))
            > MAX_TRANSFER_PREVIEW_BYTES
        {
            state.notice = Some(Notice::Error(LIMIT_NOTICE.to_owned()));
            return false;
        }
        let sides = [&preview.local, &preview.remote];
        let readable = sides.iter().any(|side| side.is_some())
            && sides.iter().all(|side| {
                side.as_ref()
                    .is_none_or(|side| side.text.is_some() && !side.binary)
            });
        let panel = match readable {
            true => {
                let rows = diff::unified(
                    &preview.path,
                    local.unwrap_or_default(),
                    remote.unwrap_or_default(),
                )
                .rows;
                let tab = Tab::synthetic(
                    Path::new(&preview.path),
                    name(&preview.path).to_owned(),
                    rows,
                    self.theme_generation,
                );
                Panel::Diff {
                    truncated: sides
                        .iter()
                        .flat_map(|side| side.as_ref())
                        .any(|side| side.truncated),
                    path: preview.path,
                    tab: Box::new(tab),
                }
            }
            false => Panel::Binary(preview),
        };
        state.open_panel(panel);
        true
    }

    pub fn receive_transfer_review(&mut self, generation: u64, mut review: TransferReview) -> bool {
        let state = &mut self.transfer;
        review.roots.normalize();
        if generation != state.generation
            || state.draining
            || review.roots != state.roots
            || !matches!(&state.request, Some(TransferAction::Review { direction, .. }) if *direction == review.direction)
        {
            return false;
        }
        state.pending = false;
        state.request = None;
        state.invalidate();
        if review.entries.len() > MAX_TRANSFER_OPERATIONS
            || review.skipped.len() > MAX_TRANSFER_ENTRIES
        {
            state.notice = Some(Notice::Error(LIMIT_NOTICE.to_owned()));
            return false;
        }
        review.executable &=
            state.complete && !review.entries.is_empty() && !review.digest.is_empty();
        state.open_panel(Panel::Review(review));
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

    /// Takes how a request ended. A transfer or reconcile a worker ran gets a
    /// report that stays on screen; the seed ends only once a path moved. A
    /// request that never reached a worker, or any other request that
    /// stopped, says why where it was asked and leaves the last report and
    /// recovery as they were.
    pub fn receive_transfer_outcome(
        &mut self,
        generation: u64,
        mut outcome: TransferOutcome,
    ) -> bool {
        let state = &mut self.transfer;
        if generation != state.generation {
            return false;
        }
        let request = state.request.take();
        let approved = state.approved.take();
        state.pending = false;
        state.progress = None;
        state.invalidate();
        state.complete = false;
        let ran = matches!(
            request,
            Some(TransferAction::Execute { .. } | TransferAction::Reconcile { .. })
        ) && outcome.recovery.is_some();
        if let Some(recovery) = outcome.recovery.take() {
            state.recovery = Some(recovery);
        }
        if ran || !outcome.entries.is_empty() {
            if outcome
                .entries
                .iter()
                .any(|entry| entry.outcome == TransferFileOutcome::Confirmed)
            {
                state.seed = false;
            }
            state.reported = approved;
            state.outcome = Some(outcome);
            state.open_panel(Panel::Report);
        } else if let Some(reason) = outcome.stopped {
            match request {
                Some(TransferAction::Compare { .. }) => state.failure = Some(reason),
                _ => state.notice = Some(Notice::Error(reason)),
            }
        }
        true
    }

    pub fn receive_transfer_recovery(
        &mut self,
        generation: u64,
        recovery: TransferRecovery,
    ) -> bool {
        if generation != self.transfer.generation || self.transfer.draining {
            return false;
        }
        self.transfer.recovery = Some(recovery);
        true
    }

    /// Stops whatever runs and goes to `exit` once cleanup ends. A close
    /// already asked for still wins.
    fn leave_transfer(&mut self, exit: TransferExit) -> WorkbenchAction {
        if !matches!(self.transfer.exit, Some(TransferExit::Close)) {
            self.transfer.exit = Some(exit);
        }
        self.transfer.cancel()
    }

    pub(crate) fn transfer_switch(&mut self, view: SidebarView) -> WorkbenchAction {
        if view == SidebarView::Transfer {
            self.open_transfer();
        } else if self.transfer_input_active() {
            return self.leave_transfer(TransferExit::View(view));
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
        if keys::TOGGLE_SIDEBAR.matches(key) {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            return WorkbenchAction::Consumed;
        }
        if keys::QUICK_OPEN.matches(key) {
            return self.leave_transfer(TransferExit::QuickOpen);
        }
        if keys::CLOSE.matches(key) {
            return match self.transfer.dismiss() {
                true => WorkbenchAction::Consumed,
                false => self.transfer_switch(SidebarView::Explorer),
            };
        }
        self.transfer.key(key)
    }

    pub(crate) fn transfer_mouse(&mut self, event: MouseEvent) -> WorkbenchAction {
        let at = (event.column, event.row);
        if let Some(offset) = taken(self.bars.text.handle(&event)) {
            if let Some(offset) = offset {
                self.transfer.scroll_to(offset as usize);
            }
            return WorkbenchAction::Consumed;
        }
        match event.kind {
            MouseEventKind::Moved => self.hover = Some(at),
            MouseEventKind::Down(MouseButton::Left) => {
                self.hover = Some(at);
                let clicks = self.clicks.press(at, Instant::now());
                return self.transfer.press(at, clicks);
            }
            MouseEventKind::ScrollUp => self.transfer.scroll(-SCROLL_LINES),
            MouseEventKind::ScrollDown => self.transfer.scroll(SCROLL_LINES),
            MouseEventKind::ScrollLeft => self.transfer.pan(-PAN_COLUMNS),
            MouseEventKind::ScrollRight => self.transfer.pan(PAN_COLUMNS),
            _ => {}
        }
        WorkbenchAction::Consumed
    }
}

/// Why `root` cannot be `side`'s root, if it cannot.
fn root_problem(side: TransferSide, root: &str) -> Option<&'static str> {
    match side {
        TransferSide::Local if !Path::new(root).is_absolute() => Some(LOCAL_ROOT_RELATIVE),
        TransferSide::Remote if !valid_relative(root, true) => Some(SANDBOX_ROOT_OUTSIDE),
        _ => None,
    }
}

fn preview_text(side: Option<&TransferPreviewSide>) -> Option<&str> {
    side.and_then(|side| side.text.as_deref())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::tree::{Note, Row};
    use super::view::on_marker;
    use super::{
        Button, DIFF_TRUNCATED, FOLDER_NOT_PAIRED, LIMIT_NOTICE, LOCAL_ROOT_RELATIVE,
        MAX_TRANSFER_ENTRIES, MAX_TRANSFER_OPERATIONS, MAX_TRANSFER_PREVIEW_BYTES,
        MAX_TRANSFER_SELECTION, NO_HISTORY, Notice, Panel, RECONCILE_OFFLINE, REVIEW_INCOMPLETE,
        ROOT_FIX, SANDBOX_ROOT_OUTSIDE, SELECTION_LIMIT, TransferAction, TransferAvailability,
        TransferDirection, TransferEffect, TransferEntry, TransferExclusion, TransferFileOutcome,
        TransferNodeKind, TransferOutcome, TransferOutcomeEntry, TransferPhase, TransferPreview,
        TransferPreviewSide, TransferProgress, TransferRecovery, TransferReview,
        TransferReviewEntry, TransferRoots, TransferScan, TransferScanLimit, TransferSide,
        TransferSnapshot, TransferStatus,
    };
    use crate::{
        DocumentKey, Layout, MIN_SIDEBAR_WIDTH, SidebarView, TabLabel, Workbench, WorkbenchAction,
        WorkbenchStyles, keys,
    };

    pub(super) const NARROW: (u16, u16) = (80, 24);
    pub(super) const WIDE: (u16, u16) = (160, 48);
    const ATTACHMENT: &str = "sandbox:one:revision";
    const NEXT_ATTACHMENT: &str = "sandbox:two:revision";
    pub(super) const LABEL: &str = "Sandbox One";
    pub(super) const LOCAL_ROOT: &str = "/host/project";
    pub(super) const REMOTE_ROOT: &str = "workspace";
    /// What the sandbox root prompt opens with.
    pub(super) const SANDBOX_DRAFT: &str = "/workspace";
    const LOCAL_FOLDER_ROOT: &str = "/host/project/src";
    const REMOTE_FOLDER_ROOT: &str = "workspace/src";
    const OTHER_ROOT: &str = "/host/other";
    const RELATIVE_ROOT: &str = "relative";
    const ESCAPING_ROOT: &str = "../outside";
    const NESTED_SANDBOX_ROOT: &str = "/next";
    const SEPARATOR: &str = "/";
    /// The workspace root as the host prints it, and the row it once sent
    /// for it.
    pub(super) const DOT: &str = ".";
    pub(super) const CONTENTS: &str = "unchanged\n";
    pub(super) const UPDATED: &str = "published\n";
    /// What a sandbox file holds, sized apart from the local one so a
    /// transfer that counts the wrong side shows.
    const SANDBOX_CONTENTS: &str = "rewritten in the sandbox\n";
    pub(super) const DIGEST: &str = "opaque-reviewed-digest";
    pub(super) const PREVIEW_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    pub(super) const RECOVERY: &str = "empty/ publication unknown";
    pub(super) const STOPPED: &str = "permission denied";
    pub(super) const FILE_NAME: &str = "file.txt";
    const PULLED_FILE: &str = "pulled.txt";
    const SECRET_FILE: &str = ".env";
    pub(super) const FOLDER: &str = "src";
    pub(super) const NESTED: &str = "src/nested";
    pub(super) const NESTED_FILE: &str = "src/nested/deep.rs";
    pub(super) const CHANGED_FILE: &str = "src/main.rs";
    pub(super) const EMPTY_FOLDER: &str = "empty";
    pub(super) const STAGE: &str = "stage";
    pub(super) const DOT_FOLDER: &str = ".cache";
    /// `src/nested` and `src/nested/deep.rs` once `src` is the root.
    const INNER_FOLDER: &str = "nested";
    const INNER_FILE: &str = "nested/deep.rs";
    const MANY_ROWS: usize = 100;
    const WHEEL_ROWS: isize = 5;
    const TYPED: char = 'c';
    const STALE: &str = "stale replies must not mutate the current scope";
    /// Why a request that never reached a worker stopped.
    const UNREACHED: &str = "sandbox connection closed";
    const ISOLATED: &str = "transfer input must never edit or save a preserved tab";
    const NO_COMPARE: &str = "the key must ask the host for a comparison";
    const NO_REVIEW: &str = "an eligible selection must request a review";
    const ROW_MISSING: &str = "the row the test points at is not listed";
    const NO_MARKER: &str = "a pane wide enough for a fold marker";
    const CURSOR_LOST: &str =
        "the cursor must come back to the same path or the nearest folder still listed";
    const DRAFT_APPLIED: &str = "a root being typed must change nothing until it is confirmed";
    const NO_TERMINAL: &str = "a test terminal";
    const NO_FRAME: &str = "a frame";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn press(workbench: &mut Workbench, bind: keys::Bind) -> WorkbenchAction {
        workbench.handle_key(KeyEvent::new(bind.code, bind.modifiers))
    }

    pub(super) fn enter(workbench: &mut Workbench) -> WorkbenchAction {
        workbench.handle_key(key(KeyCode::Enter))
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    pub(super) fn roots() -> TransferRoots {
        TransferRoots {
            local: LOCAL_ROOT.to_owned(),
            remote: REMOTE_ROOT.to_owned(),
        }
    }

    fn availability() -> TransferAvailability {
        TransferAvailability {
            attachment: ATTACHMENT.to_owned(),
            label: LABEL.to_owned(),
            local_root: Some(LOCAL_ROOT.to_owned()),
            remote_root: REMOTE_ROOT.to_owned(),
            directory_effects: true,
        }
    }

    pub(super) fn workbench() -> Workbench {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.set_transfer_availability(Some(availability()));
        assert!(workbench.open_transfer());
        workbench
    }

    fn entry(path: &str, kind: TransferNodeKind, status: TransferStatus) -> TransferEntry {
        let local = (status != TransferStatus::RemoteOnly).then_some(kind);
        let remote = (status != TransferStatus::LocalOnly).then_some(kind);
        let size = |side: Option<TransferNodeKind>, contents: &str| match side {
            Some(TransferNodeKind::File) => contents.len() as u64,
            _ => 0,
        };
        TransferEntry {
            path: path.to_owned(),
            local_bytes: size(local, CONTENTS),
            remote_bytes: size(remote, SANDBOX_CONTENTS),
            local,
            remote,
            status,
            excluded: None,
            unlisted: false,
        }
    }

    pub(super) fn file(path: &str, status: TransferStatus) -> TransferEntry {
        entry(path, TransferNodeKind::File, status)
    }

    pub(super) fn folder(path: &str, status: TransferStatus) -> TransferEntry {
        entry(path, TransferNodeKind::Directory, status)
    }

    /// A folder at least one side never finished listing.
    pub(super) fn unlisted(path: &str, status: TransferStatus) -> TransferEntry {
        TransferEntry {
            unlisted: true,
            ..folder(path, status)
        }
    }

    /// A folder left out of every transfer, for `reason`.
    pub(super) fn excluded(path: &str, reason: TransferExclusion) -> TransferEntry {
        TransferEntry {
            excluded: Some(reason),
            ..folder(path, TransferStatus::Excluded)
        }
    }

    /// A side whose scan stopped at the entry limit.
    pub(super) fn partial() -> TransferScan {
        TransferScan {
            unsupported: false,
            limits: BTreeSet::from([TransferScanLimit::Entries]),
        }
    }

    /// `src` holding a changed file and a folder with another, an empty
    /// folder beside it, and a file both sides agree on.
    pub(super) fn project() -> Vec<TransferEntry> {
        vec![
            folder(FOLDER, TransferStatus::Equal),
            folder(NESTED, TransferStatus::Equal),
            file(NESTED_FILE, TransferStatus::Different),
            file(CHANGED_FILE, TransferStatus::Different),
            folder(EMPTY_FOLDER, TransferStatus::Equal),
            file(FILE_NAME, TransferStatus::Equal),
        ]
    }

    /// Answers the comparison `action` asked for with `entries`, the sandbox
    /// side scanned as `remote` says.
    pub(super) fn answer(
        workbench: &mut Workbench,
        action: WorkbenchAction,
        entries: Vec<TransferEntry>,
        remote: TransferScan,
    ) -> u64 {
        let WorkbenchAction::Transfer(TransferAction::Compare {
            generation, roots, ..
        }) = action
        else {
            panic!("{NO_COMPARE}");
        };
        assert!(workbench.receive_transfer_snapshot(
            generation,
            TransferSnapshot {
                roots,
                entries,
                local: TransferScan::default(),
                remote,
            }
        ));
        generation
    }

    pub(super) fn compare(workbench: &mut Workbench, entries: Vec<TransferEntry>) -> u64 {
        let action = press(workbench, keys::COMPARE);
        answer(workbench, action, entries, TransferScan::default())
    }

    /// The comparison the worker runs on its own once a transfer ends.
    fn recompared(entries: Vec<TransferEntry>) -> TransferSnapshot {
        TransferSnapshot {
            roots: roots(),
            entries,
            local: TransferScan::default(),
            remote: TransferScan::default(),
        }
    }

    /// Asks for the review `bind` stands for and answers it as the host
    /// would, with every path it asked for as one approvable operation.
    pub(super) fn request_review(workbench: &mut Workbench, bind: keys::Bind) -> TransferReview {
        let WorkbenchAction::Transfer(TransferAction::Review {
            direction, paths, ..
        }) = press(workbench, bind)
        else {
            panic!("{NO_REVIEW}");
        };
        TransferReview {
            digest: DIGEST.to_owned(),
            roots: workbench.transfer.roots.clone(),
            direction,
            entries: paths
                .into_iter()
                .map(|path| TransferReviewEntry {
                    path,
                    effect: TransferEffect::New,
                    bytes: CONTENTS.len() as u64,
                })
                .collect(),
            skipped: Vec::new(),
            executable: true,
            notice: None,
        }
    }

    fn preview_side(contents: &str, text: bool) -> TransferPreviewSide {
        TransferPreviewSide {
            kind: TransferNodeKind::File,
            bytes: contents.len() as u64,
            digest: PREVIEW_DIGEST.to_owned(),
            text: text.then(|| contents.to_owned()),
            binary: !text,
            truncated: text,
        }
    }

    /// Both sides of `FILE_NAME`, as truncated text or as binary.
    pub(super) fn preview(text: bool) -> TransferPreview {
        TransferPreview {
            path: FILE_NAME.to_owned(),
            local: Some(preview_side(CONTENTS, text)),
            remote: Some(preview_side(UPDATED, text)),
        }
    }

    /// A transfer that stopped part way: one path confirmed, one the peer
    /// could not vouch for.
    pub(super) fn outcome(required: bool) -> TransferOutcome {
        TransferOutcome {
            entries: vec![
                TransferOutcomeEntry {
                    path: FILE_NAME.to_owned(),
                    outcome: TransferFileOutcome::Confirmed,
                },
                TransferOutcomeEntry {
                    path: EMPTY_FOLDER.to_owned(),
                    outcome: TransferFileOutcome::Unknown,
                },
            ],
            stopped: Some(STOPPED.to_owned()),
            recovery: Some(TransferRecovery {
                lines: Vec::new(),
                required,
            }),
        }
    }

    /// A journal holding one publication nobody could vouch for.
    pub(super) fn pending_recovery() -> TransferRecovery {
        TransferRecovery {
            lines: vec![RECOVERY.to_owned()],
            required: true,
        }
    }

    fn notice(workbench: &Workbench) -> Option<&str> {
        workbench
            .transfer
            .notice
            .as_ref()
            .map(|notice| match notice {
                Notice::Info(text) | Notice::Error(text) => text.as_str(),
            })
    }

    fn draft(workbench: &Workbench) -> Option<&str> {
        workbench
            .transfer
            .prompt
            .as_ref()
            .map(|prompt| prompt.text.as_str())
    }

    fn listed(paths: &[&str]) -> Vec<Row> {
        paths
            .iter()
            .map(|path| Row::Entry((*path).to_owned()))
            .collect()
    }

    fn row_index(workbench: &Workbench, row: &Row) -> usize {
        workbench
            .transfer
            .rows
            .iter()
            .position(|listed| listed == row)
            .expect(ROW_MISSING)
    }

    pub(super) fn point_at(workbench: &mut Workbench, row: Row) {
        workbench.transfer.cursor = row_index(workbench, &row);
    }

    /// Unfolds each of `folders` in turn, the way `→` does.
    pub(super) fn unfold(workbench: &mut Workbench, folders: &[&str]) {
        for folder in folders {
            point_at(workbench, Row::Entry((*folder).to_owned()));
            workbench.handle_key(key(KeyCode::Right));
        }
    }

    fn cursor_at(workbench: &Workbench, path: &str) -> bool {
        workbench.transfer.cursor_row() == Some(&Row::Entry(path.to_owned()))
    }

    /// Paints one frame, which is also what records where a click lands, and
    /// reads it back one screen row per line.
    pub(super) fn paint(workbench: &mut Workbench, (width, height): (u16, u16)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect(NO_TERMINAL);
        terminal
            .draw(|frame| workbench.view(frame, frame.area()))
            .expect(NO_FRAME);
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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
    #[test_case(DOT; "displayed_workspace_root")]
    fn initial_compare_pending_does_not_block_admission(remote_root: &str) {
        let mut workbench = workbench();
        workbench.transfer.roots.remote = remote_root.to_owned();
        let WorkbenchAction::Transfer(TransferAction::Compare {
            roots, generation, ..
        }) = press(&mut workbench, keys::COMPARE)
        else {
            panic!("{NO_COMPARE}");
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
        press(&mut workbench, keys::COMPARE);
        assert!(workbench.blocks_transfer_start());
    }

    #[test_case(TransferDirection::Push; "upload")]
    #[test_case(TransferDirection::Pull; "download")]
    fn folder_selection_deduplicates_component_wise_and_keeps_empty_directories(
        direction: TransferDirection,
    ) {
        let mut workbench = workbench();
        let status = match direction {
            TransferDirection::Pull => TransferStatus::RemoteOnly,
            TransferDirection::Push | TransferDirection::Seed => TransferStatus::LocalOnly,
        };
        compare(
            &mut workbench,
            vec![
                folder("a", status.clone()),
                folder("a/empty", status.clone()),
                file("a/file", status.clone()),
                file("ab", status),
                folder("a/excluded", TransferStatus::Excluded),
                file("a/excluded/child", TransferStatus::Different),
            ],
        );
        workbench
            .transfer
            .selected
            .extend(["a".to_owned(), "a/file".to_owned()]);
        let selection = workbench.transfer.selection(&direction);
        assert_eq!(selection.paths, ["a", "a/empty", "a/file"]);
        assert_eq!(selection.skipped, 2);
        let source = match direction {
            TransferDirection::Pull => SANDBOX_CONTENTS,
            TransferDirection::Push | TransferDirection::Seed => CONTENTS,
        };
        assert_eq!(selection.bytes, source.len() as u64);
    }

    #[test_case(false; "old_peer_directory_capability")]
    #[test_case(true; "seed_create_only")]
    fn blocked_selection_is_never_claimed_as_copied(seed: bool) {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            vec![
                folder(EMPTY_FOLDER, TransferStatus::LocalOnly),
                file(FILE_NAME, TransferStatus::Different),
            ],
        );
        workbench
            .transfer
            .selected
            .extend([EMPTY_FOLDER.to_owned(), FILE_NAME.to_owned()]);
        workbench.transfer.seed = seed;
        if !seed {
            workbench
                .transfer
                .availability
                .as_mut()
                .unwrap()
                .directory_effects = false;
        }
        let selection = workbench.transfer.selection(&workbench.transfer.upload());
        assert_eq!(
            selection.paths,
            [if seed { EMPTY_FOLDER } else { FILE_NAME }]
        );
        assert_eq!(selection.skipped, 1);
    }

    #[test]
    fn a_root_row_costs_itself_and_not_the_comparison() {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            vec![
                folder(DOT, TransferStatus::Incomplete),
                file(FILE_NAME, TransferStatus::LocalOnly),
            ],
        );
        assert_eq!(workbench.transfer.tree.dropped(), 1);
        assert_eq!(workbench.transfer.rows, listed(&[FILE_NAME]));
    }

    #[test_case(None, NESTED_FILE; "same_path")]
    #[test_case(Some(NESTED_FILE), NESTED; "nearest_folder")]
    #[test_case(Some(NESTED), FOLDER; "folder_above_gone")]
    fn folds_and_the_cursor_survive_a_recompare(removed: Option<&str>, expected: &str) {
        let mut workbench = workbench();
        compare(&mut workbench, project());
        unfold(&mut workbench, &[FOLDER, NESTED]);
        point_at(&mut workbench, Row::Entry(NESTED_FILE.to_owned()));
        let action = press(&mut workbench, keys::COMPARE);
        let entries = project()
            .into_iter()
            .filter(|entry| Some(entry.path.as_str()) != removed)
            .collect();
        answer(&mut workbench, action, entries, TransferScan::default());
        assert!(cursor_at(&workbench, expected), "{CURSOR_LOST}");
        assert!(
            workbench
                .transfer
                .rows
                .contains(&Row::Entry(CHANGED_FILE.to_owned()))
        );
    }

    #[test]
    fn collapse_all_folds_everything_and_keeps_the_cursor_on_its_top_folder() {
        let mut workbench = workbench();
        compare(&mut workbench, project());
        unfold(&mut workbench, &[FOLDER, NESTED]);
        point_at(&mut workbench, Row::Entry(NESTED_FILE.to_owned()));
        press(&mut workbench, keys::COLLAPSE_ALL);
        assert_eq!(
            workbench.transfer.rows,
            listed(&[EMPTY_FOLDER, FOLDER, FILE_NAME])
        );
        assert!(cursor_at(&workbench, FOLDER), "{CURSOR_LOST}");
    }

    #[test]
    fn left_steps_out_to_the_folder_then_folds_it() {
        let mut workbench = workbench();
        compare(&mut workbench, project());
        unfold(&mut workbench, &[FOLDER]);
        point_at(&mut workbench, Row::Entry(CHANGED_FILE.to_owned()));
        workbench.handle_key(key(KeyCode::Left));
        assert!(cursor_at(&workbench, FOLDER));
        assert!(
            workbench
                .transfer
                .rows
                .contains(&Row::Entry(CHANGED_FILE.to_owned()))
        );
        workbench.handle_key(key(KeyCode::Left));
        assert_eq!(
            workbench.transfer.rows,
            listed(&[EMPTY_FOLDER, FOLDER, FILE_NAME])
        );
    }

    #[test]
    fn changes_only_keeps_what_differs_and_the_folders_on_the_way() {
        let mut workbench = workbench();
        compare(&mut workbench, project());
        unfold(&mut workbench, &[FOLDER, NESTED]);
        point_at(&mut workbench, Row::Entry(CHANGED_FILE.to_owned()));
        press(&mut workbench, keys::CHANGES_ONLY);
        assert_eq!(
            workbench.transfer.rows,
            listed(&[FOLDER, NESTED, NESTED_FILE, CHANGED_FILE])
        );
        assert!(cursor_at(&workbench, CHANGED_FILE), "{CURSOR_LOST}");
    }

    #[test_case(keys::COMPARE, STAGE, TransferExclusion::Gitignore, true; "ignored")]
    #[test_case(keys::SKIP_DOTFILES, DOT_FOLDER, TransferExclusion::Dotfile, false; "dotfile")]
    fn a_folder_left_out_by_a_toggle_offers_to_bring_it_back(
        start: keys::Bind,
        name: &str,
        reason: TransferExclusion,
        include_ignored: bool,
    ) {
        let mut workbench = workbench();
        let action = press(&mut workbench, start);
        let generation = answer(
            &mut workbench,
            action,
            vec![excluded(name, reason)],
            TransferScan::default(),
        );
        unfold(&mut workbench, &[name]);
        let note = Row::Note(name.to_owned());
        assert_eq!(
            workbench.transfer.rows,
            [Row::Entry(name.to_owned()), note.clone()]
        );
        point_at(&mut workbench, note);
        assert_eq!(
            enter(&mut workbench),
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(1),
                roots: roots(),
                include_ignored,
                skip_dotfiles: false,
            })
        );
    }

    #[test_case(TransferExclusion::Protected; "protected")]
    #[test_case(TransferExclusion::Pattern; "pattern")]
    fn a_note_that_only_explains_does_nothing(reason: TransferExclusion) {
        let mut workbench = workbench();
        compare(&mut workbench, vec![excluded(STAGE, reason)]);
        unfold(&mut workbench, &[STAGE]);
        point_at(&mut workbench, Row::Note(STAGE.to_owned()));
        assert_eq!(enter(&mut workbench), WorkbenchAction::Consumed);
        assert!(!workbench.transfer.pending);
    }

    #[test]
    fn a_file_left_out_says_why_instead_of_opening() {
        let mut workbench = workbench();
        let secret = TransferEntry {
            excluded: Some(TransferExclusion::Protected),
            ..file(SECRET_FILE, TransferStatus::Excluded)
        };
        compare(&mut workbench, vec![secret]);
        assert_eq!(enter(&mut workbench), WorkbenchAction::Consumed);
        assert_eq!(notice(&workbench), Some(Note::Protected.text()));
    }

    #[test]
    fn a_folder_not_fully_scanned_compares_on_its_own_and_backspace_restores_the_pair() {
        let mut workbench = workbench();
        let entries = vec![
            unlisted(FOLDER, TransferStatus::Equal),
            folder(NESTED, TransferStatus::Equal),
            file(NESTED_FILE, TransferStatus::Different),
        ];
        let generation = compare(&mut workbench, entries.clone());
        assert!(workbench.set_transfer_connection(generation, true, false));
        unfold(&mut workbench, &[FOLDER, NESTED]);
        point_at(&mut workbench, Row::Note(FOLDER.to_owned()));
        let action = enter(&mut workbench);
        assert_eq!(
            action,
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(1),
                roots: TransferRoots {
                    local: LOCAL_FOLDER_ROOT.to_owned(),
                    remote: REMOTE_FOLDER_ROOT.to_owned(),
                },
                include_ignored: false,
                skip_dotfiles: false,
            })
        );
        assert!(workbench.transfer.busy && workbench.transfer.pending);
        answer(
            &mut workbench,
            action,
            vec![
                folder(INNER_FOLDER, TransferStatus::Equal),
                file(INNER_FILE, TransferStatus::Different),
            ],
            TransferScan::default(),
        );
        assert!(
            workbench
                .transfer
                .rows
                .contains(&Row::Entry(INNER_FILE.to_owned()))
        );
        let action = press(&mut workbench, keys::PREVIOUS_ROOTS);
        assert!(matches!(
            &action,
            WorkbenchAction::Transfer(TransferAction::Compare { roots: restored, .. })
                if *restored == roots()
        ));
        answer(&mut workbench, action, entries, TransferScan::default());
        assert!(cursor_at(&workbench, FOLDER), "{CURSOR_LOST}");
        assert!(
            workbench
                .transfer
                .rows
                .contains(&Row::Entry(NESTED_FILE.to_owned()))
        );
        assert_eq!(
            press(&mut workbench, keys::PREVIOUS_ROOTS),
            WorkbenchAction::Consumed
        );
        assert_eq!(notice(&workbench), Some(NO_HISTORY));
    }

    #[test]
    fn a_folder_missing_on_one_side_cannot_be_compared_on_its_own() {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            vec![unlisted(FOLDER, TransferStatus::LocalOnly)],
        );
        unfold(&mut workbench, &[FOLDER]);
        point_at(&mut workbench, Row::Note(FOLDER.to_owned()));
        assert_eq!(enter(&mut workbench), WorkbenchAction::Consumed);
        assert_eq!(notice(&workbench), Some(FOLDER_NOT_PAIRED));
        assert_eq!(workbench.transfer.roots, roots());
        assert!(workbench.transfer.history.is_empty());
    }

    #[test]
    fn a_partial_side_offers_to_compare_a_folder_it_may_not_have_listed() {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        let generation = answer(
            &mut workbench,
            action,
            vec![folder(FOLDER, TransferStatus::Equal)],
            partial(),
        );
        unfold(&mut workbench, &[FOLDER]);
        point_at(&mut workbench, Row::Note(FOLDER.to_owned()));
        assert!(matches!(
            enter(&mut workbench),
            WorkbenchAction::Transfer(TransferAction::Compare { generation: next, .. })
                if next == generation.wrapping_add(1)
        ));
    }

    #[test_case(keys::INCLUDE_IGNORED, (true, false); "ignored")]
    #[test_case(keys::SKIP_DOTFILES, (false, true); "dotfiles")]
    fn a_filter_toggle_compares_again_under_a_fresh_generation(
        toggle: keys::Bind,
        (include_ignored, skip_dotfiles): (bool, bool),
    ) {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, project());
        let action = press(&mut workbench, toggle);
        assert_eq!(
            action,
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(1),
                roots: roots(),
                include_ignored,
                skip_dotfiles,
            })
        );
        answer(&mut workbench, action, project(), TransferScan::default());
        assert_eq!(
            press(&mut workbench, toggle),
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(2),
                roots: roots(),
                include_ignored: false,
                skip_dotfiles: false,
            })
        );
    }

    #[test_case(keys::INCLUDE_IGNORED, false, (true, false); "ignored_same_view")]
    #[test_case(keys::INCLUDE_IGNORED, true, (false, false); "ignored_another_view")]
    #[test_case(keys::SKIP_DOTFILES, false, (false, true); "dotfiles_same_view")]
    #[test_case(keys::SKIP_DOTFILES, true, (false, false); "dotfiles_another_view")]
    fn a_filter_toggle_lasts_while_the_view_stays_up(
        toggle: keys::Bind,
        leave: bool,
        expected: (bool, bool),
    ) {
        let mut workbench = workbench();
        let action = press(&mut workbench, toggle);
        let generation = answer(&mut workbench, action, project(), TransferScan::default());
        if leave {
            assert_eq!(
                workbench.handle_leader(key(keys::VIEW_EXPLORER.code)),
                WorkbenchAction::Transfer(TransferAction::Cancel { generation })
            );
            assert!(workbench.set_transfer_connection(generation, false, false));
            assert_eq!(workbench.sidebar_view(), SidebarView::Explorer);
        }
        assert!(workbench.open_transfer());
        let WorkbenchAction::Transfer(TransferAction::Compare {
            include_ignored,
            skip_dotfiles,
            ..
        }) = press(&mut workbench, keys::COMPARE)
        else {
            panic!("{NO_COMPARE}");
        };
        assert_eq!((include_ignored, skip_dotfiles), expected);
    }

    #[test]
    fn a_root_is_a_draft_until_enter_and_backspace_restores_the_pair() {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, project());
        press(&mut workbench, keys::LOCAL_ROOT);
        assert_eq!(draft(&workbench), Some(LOCAL_ROOT));
        press(&mut workbench, keys::CLEAR_ROOT);
        assert!(workbench.paste(OTHER_ROOT));
        assert_eq!(workbench.transfer.roots, roots(), "{DRAFT_APPLIED}");
        assert_eq!(
            workbench.transfer_generation(),
            generation,
            "{DRAFT_APPLIED}"
        );
        let action = enter(&mut workbench);
        assert_eq!(
            action,
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(1),
                roots: TransferRoots {
                    local: OTHER_ROOT.to_owned(),
                    remote: REMOTE_ROOT.to_owned(),
                },
                include_ignored: false,
                skip_dotfiles: false,
            })
        );
        answer(&mut workbench, action, Vec::new(), TransferScan::default());
        assert_eq!(
            press(&mut workbench, keys::PREVIOUS_ROOTS),
            WorkbenchAction::Transfer(TransferAction::Compare {
                generation: generation.wrapping_add(2),
                roots: roots(),
                include_ignored: false,
                skip_dotfiles: false,
            })
        );
    }

    #[test]
    fn a_rejected_root_keeps_the_prompt_and_its_text() {
        let mut workbench = workbench();
        press(&mut workbench, keys::LOCAL_ROOT);
        press(&mut workbench, keys::CLEAR_ROOT);
        workbench.paste(RELATIVE_ROOT);
        assert_eq!(enter(&mut workbench), WorkbenchAction::Consumed);
        assert_eq!(notice(&workbench), Some(LOCAL_ROOT_RELATIVE));
        assert_eq!(draft(&workbench), Some(RELATIVE_ROOT));
        assert_eq!(workbench.transfer.roots, roots(), "{DRAFT_APPLIED}");
    }

    #[test_case("", REMOTE_ROOT, keys::LOCAL_ROOT, LOCAL_ROOT_RELATIVE; "unset_local")]
    #[test_case(RELATIVE_ROOT, REMOTE_ROOT, keys::LOCAL_ROOT, LOCAL_ROOT_RELATIVE; "relative_local")]
    #[test_case(LOCAL_ROOT, ESCAPING_ROOT, keys::SANDBOX_ROOT, SANDBOX_ROOT_OUTSIDE; "escaping_sandbox")]
    fn compare_names_the_root_it_refuses_and_the_key_that_edits_it(
        local: &str,
        remote: &str,
        edit: keys::Bind,
        problem: &str,
    ) {
        let mut workbench = workbench();
        assert!(workbench.show_transfer(
            TransferRoots {
                local: local.to_owned(),
                remote: remote.to_owned(),
            },
            TransferDirection::Push
        ));
        assert_eq!(
            press(&mut workbench, keys::COMPARE),
            WorkbenchAction::Consumed
        );
        let expected = format!("{problem}; press {} {ROOT_FIX}", edit.label);
        assert_eq!(notice(&workbench), Some(expected.as_str()));
    }

    #[test]
    fn a_sandbox_root_is_applied_while_the_local_root_is_refused() {
        let mut workbench = workbench();
        assert!(workbench.show_transfer(
            TransferRoots {
                local: RELATIVE_ROOT.to_owned(),
                ..roots()
            },
            TransferDirection::Push
        ));
        press(&mut workbench, keys::SANDBOX_ROOT);
        press(&mut workbench, keys::CLEAR_ROOT);
        workbench.paste(NESTED_SANDBOX_ROOT);
        assert_eq!(enter(&mut workbench), WorkbenchAction::Consumed);
        assert_eq!(draft(&workbench), None);
        assert_eq!(
            workbench.transfer.roots.remote,
            NESTED_SANDBOX_ROOT.trim_start_matches(SEPARATOR)
        );
        let expected = format!(
            "{LOCAL_ROOT_RELATIVE}; press {} {ROOT_FIX}",
            keys::LOCAL_ROOT.label
        );
        assert_eq!(notice(&workbench), Some(expected.as_str()));
    }

    #[test_case(SEPARATOR; "separator")]
    #[test_case(DOT; "dot")]
    fn the_sandbox_root_names_the_workspace_either_way(typed: &str) {
        let mut workbench = workbench();
        press(&mut workbench, keys::SANDBOX_ROOT);
        assert_eq!(draft(&workbench), Some(SANDBOX_DRAFT));
        press(&mut workbench, keys::CLEAR_ROOT);
        workbench.paste(typed);
        let WorkbenchAction::Transfer(TransferAction::Compare { roots, .. }) =
            enter(&mut workbench)
        else {
            panic!("{NO_COMPARE}");
        };
        assert!(roots.remote.is_empty());
    }

    #[test_case(true; "attachment")]
    #[test_case(false; "seed")]
    fn an_offered_workspace_root_opens_as_the_separator(attachment: bool) {
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.set_transfer_availability(Some(TransferAvailability {
            remote_root: if attachment { DOT } else { REMOTE_ROOT }.to_owned(),
            ..availability()
        }));
        if attachment {
            assert!(workbench.open_transfer());
        } else {
            assert!(workbench.show_transfer(
                TransferRoots {
                    remote: DOT.to_owned(),
                    ..roots()
                },
                TransferDirection::Seed
            ));
        }
        press(&mut workbench, keys::SANDBOX_ROOT);
        assert_eq!(draft(&workbench), Some(SEPARATOR));
    }

    #[test]
    fn escape_closes_the_prompt_then_the_panel_then_the_view() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        enter(&mut workbench);
        assert!(workbench.receive_transfer_preview(generation, preview(true)));
        press(&mut workbench, keys::LOCAL_ROOT);
        press(&mut workbench, keys::CLEAR_ROOT);
        workbench.paste(OTHER_ROOT);
        assert_eq!(
            press(&mut workbench, keys::CLOSE),
            WorkbenchAction::Consumed
        );
        assert!(workbench.transfer.prompt.is_none());
        assert!(workbench.transfer.panel.is_some());
        assert_eq!(workbench.transfer.roots, roots(), "{DRAFT_APPLIED}");
        assert_eq!(
            press(&mut workbench, keys::CLOSE),
            WorkbenchAction::Consumed
        );
        assert!(workbench.transfer.panel.is_none());
        assert_eq!(workbench.sidebar_view(), SidebarView::Transfer);
        assert_eq!(
            press(&mut workbench, keys::CLOSE),
            WorkbenchAction::Transfer(TransferAction::Cancel { generation })
        );
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert_eq!(workbench.sidebar_view(), SidebarView::Explorer);
    }

    #[test]
    fn stop_acts_only_while_a_request_runs() {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, project());
        assert_eq!(press(&mut workbench, keys::STOP), WorkbenchAction::Consumed);
        assert!(!workbench.transfer.draining);
        assert!(!workbench.transfer.tree.is_empty());
        press(&mut workbench, keys::COMPARE);
        assert_eq!(
            press(&mut workbench, keys::STOP),
            WorkbenchAction::Transfer(TransferAction::Cancel {
                generation: generation.wrapping_add(1)
            })
        );
        assert!(workbench.transfer.draining);
    }

    #[test]
    fn the_next_key_clears_the_last_notice() {
        let mut workbench = workbench();
        press(&mut workbench, keys::PREVIOUS_ROOTS);
        assert_eq!(notice(&workbench), Some(NO_HISTORY));
        press(&mut workbench, keys::NEXT_ROW);
        assert_eq!(notice(&workbench), None);
    }

    #[test_case(false; "incomplete_scan")]
    #[test_case(true; "selection_limit")]
    fn review_refuses_incomplete_or_oversized_selection(oversized: bool) {
        let mut workbench = workbench();
        let (count, scan) = match oversized {
            true => (MAX_TRANSFER_SELECTION + 1, TransferScan::default()),
            false => (1, partial()),
        };
        let action = press(&mut workbench, keys::COMPARE);
        answer(
            &mut workbench,
            action,
            (0..count)
                .map(|index| file(&format!("file{index}"), TransferStatus::LocalOnly))
                .collect(),
            scan,
        );
        let paths: Vec<String> = workbench
            .transfer
            .tree
            .entries()
            .map(|entry| entry.path.clone())
            .collect();
        workbench.transfer.selected.extend(paths);
        assert_eq!(
            press(&mut workbench, keys::UPLOAD),
            WorkbenchAction::Consumed
        );
        assert!(workbench.transfer.panel.is_none());
        let refusal = match oversized {
            true => format!("{SELECTION_LIMIT} {MAX_TRANSFER_SELECTION}"),
            false => REVIEW_INCOMPLETE.to_owned(),
        };
        assert_eq!(notice(&workbench), Some(refusal.as_str()));
    }

    #[test]
    fn an_oversized_review_is_refused_whole() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        let mut reviewed = request_review(&mut workbench, keys::UPLOAD);
        reviewed.entries = vec![reviewed.entries[0].clone(); MAX_TRANSFER_OPERATIONS + 1];
        assert!(!workbench.receive_transfer_review(generation, reviewed));
        assert_eq!(notice(&workbench), Some(LIMIT_NOTICE));
        assert!(workbench.transfer.panel.is_none());
    }

    #[test]
    fn oversized_snapshot_is_rejected_not_truncated() {
        let mut workbench = workbench();
        let generation = workbench.transfer_generation();
        assert!(!workbench.receive_transfer_snapshot(
            generation,
            TransferSnapshot {
                roots: roots(),
                entries: vec![file(FILE_NAME, TransferStatus::LocalOnly); MAX_TRANSFER_ENTRIES + 1],
                local: TransferScan::default(),
                remote: TransferScan::default(),
            }
        ));
        assert!(workbench.transfer.tree.is_empty());
        assert_eq!(notice(&workbench), Some(LIMIT_NOTICE));
    }

    #[test]
    fn review_approval_is_exact_and_one_shot() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![folder(EMPTY_FOLDER, TransferStatus::LocalOnly)],
        );
        press(&mut workbench, keys::SELECT);
        let reviewed = request_review(&mut workbench, keys::UPLOAD);
        assert!(workbench.receive_transfer_review(generation, reviewed));
        assert_eq!(
            press(&mut workbench, keys::APPROVE),
            WorkbenchAction::Transfer(TransferAction::Execute {
                generation,
                digest: DIGEST.to_owned()
            })
        );
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert_eq!(
            press(&mut workbench, keys::APPROVE),
            WorkbenchAction::Consumed
        );
    }

    #[test_case(true; "closed")]
    #[test_case(false; "not_executable")]
    fn only_the_review_on_screen_can_be_approved(executable: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        let reviewed = TransferReview {
            executable,
            ..request_review(&mut workbench, keys::UPLOAD)
        };
        assert!(workbench.receive_transfer_review(generation, reviewed));
        if executable {
            assert_eq!(
                press(&mut workbench, keys::CLOSE),
                WorkbenchAction::Consumed
            );
        }
        assert_eq!(
            press(&mut workbench, keys::APPROVE),
            WorkbenchAction::Consumed
        );
        assert_eq!(workbench.transfer.panel.is_some(), !executable);
    }

    #[test]
    fn stale_review_and_direction_changes_cannot_approve() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        press(&mut workbench, keys::SELECT);
        let reviewed = request_review(&mut workbench, keys::UPLOAD);
        assert!(
            !workbench.receive_transfer_review(generation.wrapping_sub(1), reviewed.clone()),
            "{STALE}"
        );
        let crossed = TransferReview {
            direction: TransferDirection::Pull,
            ..reviewed.clone()
        };
        assert!(
            !workbench.receive_transfer_review(generation, crossed),
            "{STALE}"
        );
        assert!(workbench.receive_transfer_review(generation, reviewed.clone()));
        press(&mut workbench, keys::DOWNLOAD);
        assert!(
            !workbench.receive_transfer_review(generation, reviewed),
            "{STALE}"
        );
        assert_eq!(
            press(&mut workbench, keys::APPROVE),
            WorkbenchAction::Consumed
        );
    }

    #[test_case(TransferDirection::Seed; "initial_seed")]
    #[test_case(TransferDirection::Push; "ordinary_upload")]
    fn upload_reviews_what_the_view_was_shown_for(shown: TransferDirection) {
        let mut workbench = workbench();
        assert!(workbench.show_transfer(roots(), shown.clone()));
        let generation = compare(
            &mut workbench,
            vec![
                file(FILE_NAME, TransferStatus::LocalOnly),
                file(PULLED_FILE, TransferStatus::RemoteOnly),
            ],
        );
        press(&mut workbench, keys::SELECT);
        press(&mut workbench, keys::NEXT_ROW);
        press(&mut workbench, keys::SELECT);
        assert_eq!(
            press(&mut workbench, keys::UPLOAD),
            WorkbenchAction::Transfer(TransferAction::Review {
                generation,
                direction: shown,
                paths: vec![FILE_NAME.to_owned()],
            })
        );
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert_eq!(
            press(&mut workbench, keys::DOWNLOAD),
            WorkbenchAction::Transfer(TransferAction::Review {
                generation,
                direction: TransferDirection::Pull,
                paths: vec![PULLED_FILE.to_owned()],
            })
        );
    }

    #[test_case(true, TransferDirection::Push; "once_a_path_moved")]
    #[test_case(false, TransferDirection::Seed; "not_while_nothing_moved")]
    fn the_seed_ends_once_it_has_run(moved: bool, next: TransferDirection) {
        let mut workbench = workbench();
        assert!(workbench.show_transfer(roots(), TransferDirection::Seed));
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        let reviewed = request_review(&mut workbench, keys::UPLOAD);
        assert_eq!(reviewed.direction, TransferDirection::Seed);
        assert!(workbench.receive_transfer_review(generation, reviewed));
        press(&mut workbench, keys::APPROVE);
        let mut ended = outcome(false);
        if !moved {
            for entry in &mut ended.entries {
                entry.outcome = TransferFileOutcome::Failed;
            }
        }
        assert!(workbench.receive_transfer_outcome(generation, ended));
        assert!(matches!(workbench.transfer.panel, Some(Panel::Report)));
        compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        let WorkbenchAction::Transfer(TransferAction::Review { direction, .. }) =
            press(&mut workbench, keys::UPLOAD)
        else {
            panic!("{NO_REVIEW}");
        };
        assert_eq!(direction, next);
    }

    #[test_case(false; "binary_metadata")]
    #[test_case(true; "truncated_text")]
    fn inspection_is_read_only_and_does_not_approve(text: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert_eq!(
            enter(&mut workbench),
            WorkbenchAction::Transfer(TransferAction::Inspect {
                generation,
                path: FILE_NAME.to_owned()
            })
        );
        let elsewhere = TransferPreview {
            path: CHANGED_FILE.to_owned(),
            ..preview(text)
        };
        assert!(
            !workbench.receive_transfer_preview(generation, elsewhere),
            "{STALE}"
        );
        assert!(workbench.receive_transfer_preview(generation, preview(text)));
        assert_eq!(
            matches!(
                workbench.transfer.panel,
                Some(Panel::Diff {
                    truncated: true,
                    ..
                })
            ),
            text
        );
        assert_eq!(
            matches!(workbench.transfer.panel, Some(Panel::Binary(_))),
            !text
        );
        assert_eq!(
            press(&mut workbench, keys::APPROVE),
            WorkbenchAction::Consumed
        );
        assert!(workbench.editor.tabs().is_empty());
    }

    #[test]
    fn an_oversized_preview_is_refused_whole() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        enter(&mut workbench);
        let mut oversized = preview(true);
        if let Some(side) = &mut oversized.local {
            side.text = Some(TYPED.to_string().repeat(MAX_TRANSFER_PREVIEW_BYTES + 1));
        }
        assert!(!workbench.receive_transfer_preview(generation, oversized));
        assert_eq!(notice(&workbench), Some(LIMIT_NOTICE));
        assert!(workbench.transfer.panel.is_none());
    }

    #[test_case(TransferScanLimit::WorkcellIncomplete, false; "truncated_snapshot")]
    #[test_case(TransferScanLimit::Entries, true; "caudra_entry_cap")]
    fn a_truncated_snapshot_refuses_diffs_without_asking(limit: TransferScanLimit, asks: bool) {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        answer(
            &mut workbench,
            action,
            vec![file(FILE_NAME, TransferStatus::Different)],
            TransferScan {
                unsupported: false,
                limits: BTreeSet::from([limit]),
            },
        );
        let asked = matches!(
            enter(&mut workbench),
            WorkbenchAction::Transfer(TransferAction::Inspect { .. })
        );
        assert_eq!(asked, asks);
        assert_eq!(notice(&workbench), (!asks).then_some(DIFF_TRUNCATED));
    }

    #[test_case(true; "live_connection")]
    #[test_case(false; "offline")]
    fn persisted_recovery_enables_reconcile_without_invalidating_comparison(live: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert!(workbench.set_transfer_connection(generation, live, false));
        assert!(workbench.receive_transfer_recovery(generation, pending_recovery()));
        assert!(workbench.transfer.complete);
        assert_eq!(workbench.transfer.tree.len(), 1);
        press(&mut workbench, keys::REPORT);
        assert!(matches!(workbench.transfer.panel, Some(Panel::Report)));
        let expected = match live {
            true => WorkbenchAction::Transfer(TransferAction::Reconcile { generation }),
            false => WorkbenchAction::Consumed,
        };
        assert_eq!(press(&mut workbench, keys::RECONCILE), expected);
        assert_eq!(notice(&workbench), (!live).then_some(RECONCILE_OFFLINE));
        assert!(
            !workbench
                .receive_transfer_recovery(generation.wrapping_sub(1), TransferRecovery::default()),
            "{STALE}"
        );
    }

    #[test_case(false; "empty_recovery_after_settled_execution")]
    #[test_case(true; "unknown_execution_with_pending_recovery")]
    fn automatic_compare_and_recovery_preserve_last_execution_report(required: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert!(workbench.set_transfer_connection(generation, true, false));
        assert!(workbench.receive_transfer_outcome(generation, outcome(required)));
        assert!(matches!(workbench.transfer.panel, Some(Panel::Report)));
        workbench.refresh_after_transfer();
        assert!(workbench.receive_transfer_snapshot(
            generation,
            recompared(vec![file(FILE_NAME, TransferStatus::Equal)])
        ));
        let recovery = TransferRecovery {
            lines: match required {
                true => vec![RECOVERY.to_owned()],
                false => Vec::new(),
            },
            required,
        };
        assert!(workbench.receive_transfer_recovery(generation, recovery.clone()));
        assert!(matches!(workbench.transfer.panel, Some(Panel::Report)));
        assert_eq!(
            workbench.transfer.outcome,
            Some(TransferOutcome {
                recovery: None,
                ..outcome(required)
            })
        );
        assert_eq!(workbench.transfer.recovery, Some(recovery));
        assert!(workbench.transfer.complete);
        let action = press(&mut workbench, keys::RECONCILE);
        assert_eq!(
            matches!(
                action,
                WorkbenchAction::Transfer(TransferAction::Reconcile { .. })
            ),
            required
        );
    }

    #[test]
    fn a_request_no_worker_ran_leaves_the_report_and_recovery_alone() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert!(workbench.receive_transfer_outcome(generation, outcome(true)));
        assert!(workbench.receive_transfer_snapshot(
            generation,
            recompared(vec![file(FILE_NAME, TransferStatus::Different)])
        ));
        assert!(matches!(
            press(&mut workbench, keys::UPLOAD),
            WorkbenchAction::Transfer(TransferAction::Review { .. })
        ));
        assert!(workbench.receive_transfer_outcome(
            generation,
            TransferOutcome {
                stopped: Some(UNREACHED.to_owned()),
                ..TransferOutcome::default()
            }
        ));
        assert_eq!(notice(&workbench), Some(UNREACHED));
        assert!(matches!(workbench.transfer.panel, Some(Panel::Report)));
        assert_eq!(
            workbench.transfer.outcome,
            Some(TransferOutcome {
                recovery: None,
                ..outcome(true)
            })
        );
        assert_eq!(workbench.transfer.recovery, outcome(true).recovery);
    }

    #[test]
    fn a_comparison_the_worker_ran_on_its_own_still_owes_the_diff_asked_for() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert!(matches!(
            enter(&mut workbench),
            WorkbenchAction::Transfer(TransferAction::Inspect { .. })
        ));
        assert!(workbench.receive_transfer_snapshot(
            generation,
            recompared(vec![file(FILE_NAME, TransferStatus::Different)])
        ));
        assert!(workbench.transfer.pending);
        assert!(workbench.receive_transfer_preview(generation, preview(true)));
        assert!(matches!(workbench.transfer.panel, Some(Panel::Diff { .. })));
    }

    #[test_case(false; "different_roots")]
    #[test_case(true; "different_attachment")]
    fn execution_reports_never_cross_authority_or_root_pairs(attachment: bool) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        assert!(workbench.receive_transfer_outcome(generation, outcome(true)));
        assert!(workbench.receive_transfer_recovery(generation, pending_recovery()));
        if attachment {
            workbench.set_transfer_availability(Some(TransferAvailability {
                attachment: NEXT_ATTACHMENT.to_owned(),
                ..availability()
            }));
        } else {
            press(&mut workbench, keys::SANDBOX_ROOT);
            workbench.paste(NESTED_SANDBOX_ROOT);
            assert!(matches!(
                enter(&mut workbench),
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
        assert_eq!(press(&mut workbench, keys::SAVE), WorkbenchAction::Consumed);
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
                    phase: TransferPhase::Publishing,
                    side: None,
                    path: Some(FILE_NAME.to_owned()),
                    completed: 0,
                    total: 1,
                }
            ),
            "{STALE}"
        );
        assert!(
            !workbench.receive_transfer_outcome(generation, outcome(true)),
            "{STALE}"
        );
    }

    #[test]
    fn quick_open_leaves_for_the_explorer_once_cleanup_ends() {
        let dir = TempDir::new().unwrap();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(dir.path());
        workbench.set_transfer_availability(Some(availability()));
        assert!(workbench.open_transfer());
        let generation = workbench.transfer_generation();
        assert_eq!(
            press(&mut workbench, keys::QUICK_OPEN),
            WorkbenchAction::Transfer(TransferAction::Cancel { generation })
        );
        assert!(!workbench.palette.is_open());
        assert!(workbench.set_transfer_connection(generation, false, false));
        assert_eq!(workbench.sidebar_view(), SidebarView::Explorer);
        assert!(workbench.palette.is_open());
    }

    #[test]
    fn the_sidebar_folds_and_resizes_from_transfer() {
        let mut workbench = workbench();
        let before = workbench.sidebar_width;
        let grow = KeyEvent::new(keys::GROW_SIDEBAR.code, keys::GROW_SIDEBAR.modifiers);
        assert_eq!(workbench.handle_leader(grow), WorkbenchAction::Consumed);
        assert!(workbench.sidebar_width > before);
        assert_eq!(
            press(&mut workbench, keys::TOGGLE_SIDEBAR),
            WorkbenchAction::Consumed
        );
        assert!(workbench.sidebar_collapsed);
        assert_eq!(workbench.sidebar_view(), SidebarView::Transfer);
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
        press(&mut workbench, keys::LOCAL_ROOT);
        assert!(workbench.transfer.prompt.is_some());
        let generation = workbench.transfer_generation();
        if direct {
            workbench.close();
        } else {
            assert_eq!(workbench.close_transfer(), WorkbenchAction::Consumed);
        }
        assert!(!workbench.is_open());
        assert!(!workbench.transfer_input_active());
        assert!(workbench.transfer.prompt.is_none());
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
            press(&mut workbench, keys::COMPARE);
        }
        let generation = workbench.transfer_generation();
        if !pending {
            assert!(workbench.set_transfer_connection(generation, true, false));
            press(&mut workbench, keys::LOCAL_ROOT);
        }
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
        assert!(workbench.transfer.prompt.is_none());
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
        workbench.sidebar_width = MIN_SIDEBAR_WIDTH;
        compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
        );
        paint(&mut workbench, (width, height));
        for (rect, _) in &workbench.switcher {
            assert!(rect.right() <= width && rect.bottom() <= height);
        }
        if width >= NARROW.0 {
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
        for side in TransferSide::BOTH {
            workbench.transfer.focus = side;
            paint(&mut workbench, (width, height));
            let pane = workbench.transfer.hits.panes[side.index()];
            if height >= 8 {
                assert!(!pane.is_empty());
            }
            assert!(pane.right() <= width && pane.bottom() <= height);
        }
        assert!(workbench.panes.text.is_empty());
        assert!(workbench.panes.tabs.is_empty());
    }

    #[test]
    fn clicks_choose_fold_and_open_rows() {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, project());
        paint(&mut workbench, WIDE);
        let pane = workbench.transfer.hits.panes[TransferSide::Local.index()];
        let row = |workbench: &Workbench, path: &str| {
            pane.y + row_index(workbench, &Row::Entry(path.to_owned())) as u16
        };
        let marker = (0..pane.width)
            .find(|column| on_marker(usize::from(*column), 0))
            .expect(NO_MARKER);
        workbench.handle_mouse(click(pane.x, row(&workbench, FOLDER)));
        assert!(workbench.transfer.selected.contains(FOLDER));
        workbench.handle_mouse(click(pane.x + marker, row(&workbench, FOLDER)));
        assert!(workbench.transfer.expanded.contains(FOLDER));
        let (body, changed) = (pane.right() - 1, row(&workbench, CHANGED_FILE));
        workbench.handle_mouse(click(body, changed));
        assert!(cursor_at(&workbench, CHANGED_FILE));
        assert_eq!(
            workbench.handle_mouse(click(body, changed)),
            WorkbenchAction::Transfer(TransferAction::Inspect {
                generation,
                path: CHANGED_FILE.to_owned()
            })
        );
    }

    #[test]
    fn buttons_and_headers_answer_clicks_only_when_they_can_act() {
        let mut workbench = workbench();
        let enabled = |workbench: &Workbench| -> Vec<Button> {
            workbench
                .transfer
                .hits
                .buttons
                .iter()
                .map(|(_, button)| *button)
                .collect()
        };
        paint(&mut workbench, WIDE);
        assert_eq!(enabled(&workbench), [Button::Compare]);
        let (button, _) = workbench.transfer.hits.buttons[0];
        let action = workbench.handle_mouse(click(button.x, button.y));
        paint(&mut workbench, WIDE);
        assert_eq!(enabled(&workbench), [Button::Stop]);
        answer(
            &mut workbench,
            action,
            vec![file(FILE_NAME, TransferStatus::LocalOnly)],
            TransferScan::default(),
        );
        paint(&mut workbench, WIDE);
        assert_eq!(
            enabled(&workbench),
            [Button::Compare, Button::Upload, Button::Download]
        );
        let header = workbench.transfer.hits.headers[TransferSide::Remote.index()];
        workbench.handle_mouse(click(header.x, header.y));
        assert_eq!(draft(&workbench), Some(SANDBOX_DRAFT));
        let (button, _) = workbench.transfer.hits.buttons[0];
        assert_eq!(
            workbench.handle_mouse(click(button.x, button.y)),
            WorkbenchAction::Consumed
        );
        assert_eq!(draft(&workbench), Some(SANDBOX_DRAFT));
        paint(&mut workbench, WIDE);
        assert!(enabled(&workbench).is_empty());
    }

    #[test]
    fn the_wheel_scrolls_and_keeps_the_cursor_in_view() {
        let mut workbench = workbench();
        compare(
            &mut workbench,
            (0..MANY_ROWS)
                .map(|index| file(&format!("{index:03}"), TransferStatus::Equal))
                .collect(),
        );
        paint(&mut workbench, NARROW);
        let pane = workbench.transfer.hits.panes[TransferSide::Local.index()];
        workbench.scroll(pane.x, pane.y, WHEEL_ROWS);
        assert_eq!(workbench.transfer.scroll, WHEEL_ROWS.unsigned_abs());
        assert_eq!(workbench.transfer.cursor, WHEEL_ROWS.unsigned_abs());
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
            key(KeyCode::Char(TYPED)),
            key(KeyCode::Delete),
            KeyEvent::new(keys::SAVE.code, keys::SAVE.modifiers),
            KeyEvent::new(keys::PASTE.code, keys::PASTE.modifiers),
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
        press(&mut workbench, keys::LOCAL_ROOT);
        press(&mut workbench, keys::CLEAR_ROOT);
        workbench.handle_key(key(KeyCode::Char(TYPED)));
        workbench.paste(LOCAL_ROOT);
        assert_eq!(
            draft(&workbench),
            Some(format!("{TYPED}{LOCAL_ROOT}").as_str())
        );
        assert_eq!(
            workbench.transfer.roots.local, LOCAL_ROOT,
            "{DRAFT_APPLIED}"
        );
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
