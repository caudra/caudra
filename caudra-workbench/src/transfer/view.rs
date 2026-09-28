//! Painting for the transfer view: the toolbar, the local and sandbox panes
//! side by side over one tree, the panel that stands in the tree's place, the
//! sidebar's summary and the status bar.

use caudra_grab::grab_scope;
use ratatui::buffer::Buffer as Surface;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::tree::{Note, NoteAction, Row, name};
use super::{
    Button, DIFF_WRAPS, Hits, Notice, Panel, ROOT_SEPARATOR, TransferAction, TransferDirection,
    TransferEffect, TransferEntry, TransferFileOutcome, TransferNodeKind, TransferOutcome,
    TransferPhase, TransferPreview, TransferPreviewSide, TransferProgress, TransferReview,
    TransferScanLimit, TransferSide, TransferState, TransferStatus,
};
use crate::chrome::ELLIPSIS;
use crate::view::{
    CARET, COLLAPSED_MARK, ENTER_LABEL, EXPANDED_MARK, GUIDE, HINT_GAP, LEAF_INDENT, TAB_GAP,
    emphasize, emphasized, hints, indent_guides, line_at, paint_tab, placeholder, scroll_column,
    truncate,
};
use crate::{Workbench, WorkbenchStyles, chrome, keys};

const TITLE_MARK: &str = "\u{21c5} ";
const DISCONNECTED: &str = "Disconnected";
const SEED_NAME: &str = "Initial seed";
const UPLOAD_NAME: &str = "Upload";
const DOWNLOAD_NAME: &str = "Download";
const IGNORED_BADGE: &str = "Including ignored";
const DOTFILES_BADGE: &str = "Skipping dotfiles";
const CHANGES_BADGE: &str = "Changes only";
const LOCAL_NAME: &str = "Local";
const SANDBOX_NAME: &str = "Sandbox";
const CHECK_MARK: &str = "\u{2713}";
/// Where a row that is not chosen keeps its check column: as wide as the mark
/// and the gap after it. Ahead of the guides, so the marks line up however
/// deep the rows around them sit.
const CHECK_BLANK: &str = "  ";
const ABSENT_MARK: &str = "\u{00b7}";
const DIFFERENT_MARK: &str = "\u{2260}";
const ONLY_HERE_MARK: &str = "+";
const CONFLICT_MARK: &str = "!";
const UNKNOWN_MARK: &str = "?";
const CHANGED_MARK: &str = "\u{25cf}";
const FAILED_MARK: &str = "\u{2717}";
const CANCELLED_MARK: &str = "\u{25cb}";
const UPLOAD_MARK: &str = "\u{2191}";
const DOWNLOAD_MARK: &str = "\u{2193}";
/// Narrower than this and a pane could not hold a name beside its marks, so
/// only the focused side shows.
const MIN_TWO_PANE_WIDTH: u16 = 48;
/// The rule between the panes and a column of air either side of it.
const RULE_COLUMNS: u16 = 3;
const RULE_OFFSET: u16 = 1;
/// The diff is read rather than edited, so it paints no caret.
const DIFF_CARET: bool = false;
const SIZE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
const SIZE_STEP: f64 = 1_024.0;
/// As wide as `OVERWRITE`, the longest effect.
const EFFECT_COLUMNS: usize = 9;
/// As wide as `1023.9 KiB`.
const SIZE_COLUMNS: usize = 10;
/// As wide as `SANDBOX` and the gap after it.
const SIDE_COLUMNS: usize = 8;
/// As long as a short Git hash, which tells two versions apart at a glance.
const SHORT_DIGEST: usize = 12;
/// Ends the algorithm a digest names ahead of its hex, as in `sha256:`.
const DIGEST_ALGORITHM_END: char = ':';
const PARTS_GAP: &str = " \u{00b7} ";
const LIST_GAP: &str = ", ";
const COMPARING: &str = "Comparing\u{2026}";
const NOT_COMPARED: &str = "Not compared yet";
const EMPTY_COMPARISON: &str = "Both roots are empty";
const NO_DIFFERENCES: &str = "No differences";
const LOCAL_PROMPT: &str = "Local root: ";
const SANDBOX_PROMPT: &str = "Sandbox root: ";
const TRUNCATED_BADGE: &str = "truncated";
const BINARY_BADGE: &str = "binary";
const NOT_TEXT_BADGE: &str = "not text";
const MISSING: &str = "missing";
const DIFF_LEGEND: &str = "- local  + sandbox";
const REVIEW_TITLE: &str = "REVIEW";
const REPORT_TITLE: &str = "LAST TRANSFER";
const CHANGES_TITLE: &str = "CHANGES";
const SELECTION_TITLE: &str = "SELECTION";
const SKIPPED_TITLE: &str = "SKIPPED";
const NEW_EFFECT: &str = "NEW";
const OVERWRITE_EFFECT: &str = "OVERWRITE";
const MKDIR_EFFECT: &str = "MKDIR";
const DIGEST_LABEL: &str = "Digest ";
const NOT_EXECUTABLE: &str = "This review cannot be approved";
const NO_TRANSFER: &str = "No transfer yet";
const STOPPED_LABEL: &str = "Stopped: ";
const RECOVERY_REQUIRED: &str = "Publication unknown";
const RECONCILE_OFFER: &str = "reconciles, never replays";
const COMPARE_FAILED: &str = "Compare failed: ";
const SCAN_UNSUPPORTED: &str = "cannot be scanned safely";
const SCAN_INCOMPLETE: &str = "scan incomplete";
const SCAN_ACTION: &str = "compare a smaller folder to review";
const DROPPED_ROW: &str = "malformed row left out";
const DROPPED_ROWS: &str = "malformed rows left out";
const NOTHING_CHOSEN: &str = "U and D take the cursor row";
const PATH_LABEL: &str = "path";
const PATHS_LABEL: &str = "paths";
const CHANGED_LABEL: &str = "changed";
const UNKNOWN_LABEL: &str = "unknown";
const SKIPPED_LABEL: &str = "skipped";
const CHANGE_LABEL: &str = "change";
const CHANGES_LABEL: &str = "changes";
const DIFFERENT_LABEL: &str = "different";
const LOCAL_ONLY_LABEL: &str = "local only";
const SANDBOX_ONLY_LABEL: &str = "sandbox only";
const CONFLICT_LABEL: &str = "type conflict";
const CONFLICTS_LABEL: &str = "type conflicts";
const BLOCKED_LABEL: &str = "never transferred";
const INCLUDE_IGNORED_OFFER: &str = "includes ignored files";
const INCLUDE_DOTFILES_OFFER: &str = "includes dotfiles";
const COMPARE_FOLDER_OFFER: &str = "compares this folder";
/// Stands in the header of a pane with no root yet.
const ROOT_OFFER: &str = "picks a folder";
const OUTCOMES: [TransferFileOutcome; 4] = [
    TransferFileOutcome::Confirmed,
    TransferFileOutcome::Failed,
    TransferFileOutcome::Cancelled,
    TransferFileOutcome::Unknown,
];

impl Workbench {
    /// The editor's area while transfer is up: the toolbar, a banner when a
    /// side needs explaining, and the tree or the panel standing in for it.
    pub(crate) fn render_transfer(&mut self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_transfer", area);
        self.transfer.hits = Hits::default();
        let banner = self.transfer.banner(&self.styles, area.width);
        let prompt = self.transfer.prompt.is_some();
        let [toolbar, notice, heading, body, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(u16::from(prompt)),
        ])
        .areas(area);
        self.render_transfer_toolbar(buf, toolbar);
        if let Some(banner) = banner {
            chrome::render_line(buf, notice, banner);
        }
        match self.transfer.panel.is_some() {
            true => self.render_transfer_panel(buf, heading, body),
            false => self.render_transfer_tree(buf, heading, body),
        }
        if prompt {
            self.render_transfer_prompt(buf, input);
        }
    }

    /// The sandbox and the modes on the left, the buttons on the right. A
    /// button that cannot act now is dim and records no hit, so it is inert.
    fn render_transfer_toolbar(&mut self, buf: &mut Surface, area: Rect) {
        let state = &self.transfer;
        let styles = &self.styles;
        let buttons: usize = Button::ALL
            .iter()
            .map(|button| HINT_GAP.width() + button.label().width())
            .sum();
        let room = usize::from(area.width) > buttons;
        let mut right = Vec::new();
        let mut hits = Vec::new();
        if room {
            let mut x = area.right() - buttons as u16;
            for button in Button::ALL {
                let label = button.label();
                let rect = Rect::new(
                    x + HINT_GAP.width() as u16,
                    area.y,
                    label.width() as u16,
                    area.height,
                );
                x = rect.right();
                let style = match state.enabled(button) {
                    true => {
                        hits.push((rect, button));
                        emphasized(styles.text, self.hovering(rect).is_some(), styles)
                    }
                    false => styles.dim,
                };
                right.push(Span::styled(HINT_GAP, styles.background));
                right.push(Span::styled(label, style));
            }
        }
        let label = state
            .availability
            .as_ref()
            .map_or(DISCONNECTED, |available| available.label.as_str());
        let mut left = vec![Span::styled(format!("{TITLE_MARK}{label}"), styles.title)];
        for badge in state.badges() {
            left.push(Span::styled(HINT_GAP, styles.background));
            left.push(Span::styled(badge, styles.accent));
        }
        let budget = usize::from(area.width) - if room { buttons } else { 0 };
        let line = chrome::status_line(
            truncate(left, budget).spans,
            right,
            area.width,
            styles.background,
        );
        chrome::render_line(buf, area, line);
        self.transfer.hits.buttons = hits;
    }

    /// Both panes over the same rows, each with its root above it, and the
    /// rule between them when there is room for two.
    fn render_transfer_tree(&mut self, buf: &mut Surface, heading: Rect, body: Rect) {
        let total = self.transfer.rows.len();
        let (rows, bar) = scroll_column(self.scrollbars, body, total);
        self.transfer.follow_cursor(usize::from(rows.height));
        let focus = self.transfer.focus;
        let (panes, rule) = columns(rows, focus);
        let (headers, _) = columns(
            Rect {
                y: heading.y,
                height: heading.height,
                ..rows
            },
            focus,
        );
        if let Some(rule) = rule {
            let rule = Rect::new(
                rule.x + RULE_OFFSET,
                heading.y,
                1,
                heading.height + rows.height,
            );
            chrome::vertical_rule(buf, rule, self.styles.border);
        }
        let state = &self.transfer;
        let styles = &self.styles;
        for side in TransferSide::BOTH {
            let (header, pane) = (headers[side.index()], panes[side.index()]);
            if header.is_empty() {
                continue;
            }
            let line = state.header_line(side, styles, header.width);
            let line = emphasize(line, self.hovering(header).is_some(), styles);
            chrome::render_line(buf, header, line);
            if state.rows.is_empty() {
                placeholder(buf, pane, state.placeholder(), styles.dim);
                continue;
            }
            let pointed = self.hovered_row(pane);
            let highlight = match focus == side {
                true => styles.selected,
                false => styles.selection,
            };
            let shown = state
                .rows
                .iter()
                .enumerate()
                .skip(state.scroll)
                .take(usize::from(pane.height));
            for (offset, (index, row)) in shown.enumerate() {
                let cursor = index == state.cursor;
                let line =
                    state.row_line(row, side, cursor.then_some(highlight), styles, pane.width);
                let line = emphasize(line, pointed == Some(offset) && !cursor, styles);
                chrome::render_line(buf, line_at(pane, offset), line);
            }
        }
        let scroll = state.scroll;
        self.transfer.hits.headers = headers;
        self.transfer.hits.panes = panes;
        self.transfer.hits.rows = rows;
        self.text_bar(buf, bar, total, scroll);
    }

    /// The panel over the tree: a diff painted as the editor paints one, or
    /// the lines of whichever report is up.
    fn render_transfer_panel(&mut self, buf: &mut Surface, heading: Rect, body: Rect) {
        let line = self.transfer.panel_heading(&self.styles, heading.width);
        chrome::render_line(buf, heading, line);
        if let Some(Panel::Diff { tab, .. }) = &mut self.transfer.panel {
            let painted = paint_tab(
                buf,
                body,
                tab,
                &self.styles,
                DIFF_CARET,
                DIFF_WRAPS,
                self.scrollbars,
            );
            self.transfer.hits.panel = painted.text;
            self.text_bar(buf, painted.bar, painted.lines, painted.first);
            return;
        }
        // How many lines there are never depends on the width, so they are
        // laid out again only when the scrollbar takes a column from them.
        let mut lines = self.transfer.panel_lines(&self.styles, body.width);
        let total = lines.len();
        let (text, bar) = scroll_column(self.scrollbars, body, total);
        if text.width < body.width {
            lines = self.transfer.panel_lines(&self.styles, text.width);
        }
        let height = usize::from(text.height);
        let top = self.transfer.offset.min(total.saturating_sub(height));
        for (offset, line) in lines.into_iter().skip(top).take(height).enumerate() {
            chrome::render_line(buf, line_at(text, offset), fitted(line, text.width));
        }
        self.transfer.offset = top;
        self.transfer.hits.panel = text;
        self.text_bar(buf, bar, total, top);
    }

    /// The root being typed, shaped like the editor's find prompt.
    fn render_transfer_prompt(&self, buf: &mut Surface, area: Rect) {
        let Some(prompt) = &self.transfer.prompt else {
            return;
        };
        let styles = &self.styles;
        let label = match prompt.side {
            TransferSide::Local => LOCAL_PROMPT,
            TransferSide::Remote => SANDBOX_PROMPT,
        };
        let caret = self.transfer.text_input_active();
        let budget = usize::from(area.width).saturating_sub(label.width() + usize::from(caret));
        let mut left = vec![
            Span::styled(label, styles.accent),
            Span::styled(chrome::fit_end(&prompt.text, budget), styles.text),
        ];
        if caret {
            left.push(Span::styled(CARET, styles.cursor));
        }
        let line = chrome::status_line(left, Vec::new(), area.width, styles.dim);
        chrome::render_line(buf, area, line);
    }

    /// The sidebar while transfer is up: what differs, what is chosen, and how
    /// the last transfer ended.
    pub(crate) fn render_transfer_sidebar(&self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_transfer_sidebar", area);
        let lines = self.transfer.sidebar_lines(&self.styles, area.width);
        for (offset, line) in lines.into_iter().take(usize::from(area.height)).enumerate() {
            chrome::render_line(buf, line_at(area, offset), fitted(line, area.width));
        }
    }

    pub(crate) fn render_transfer_status(&self, buf: &mut Surface, area: Rect) {
        grab_scope!("workbench_transfer_status", area);
        let state = &self.transfer;
        let styles = &self.styles;
        let (text, style) = state.status(styles);
        let (width, right) = if style == styles.error {
            (area.width, Vec::new())
        } else {
            (area.width / 2, hints(&state.hints(), styles))
        };
        let left = vec![Span::styled(chrome::fit(&text, usize::from(width)), style)];
        let line = chrome::status_line(left, right, area.width, styles.dim);
        chrome::render_line(buf, area, line);
    }
}

impl TransferState {
    /// Brings the cursor into a view `height` rows tall, and the view back
    /// over the rows when they shrank under it.
    fn follow_cursor(&mut self, height: usize) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if height > 0 && self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(height));
    }

    /// Whether `button` can act now. None can while a root is being typed,
    /// since the key it stands for would land in the prompt.
    pub(super) fn enabled(&self, button: Button) -> bool {
        if self.prompt.is_some() {
            return false;
        }
        let idle = self.available() && !self.pending && !self.draining;
        match button {
            Button::Compare => idle,
            Button::Upload | Button::Download => {
                idle && self.complete && self.confirmed_roots.as_ref() == Some(&self.roots)
            }
            Button::Approve => {
                idle && matches!(&self.panel, Some(Panel::Review(review)) if review.executable)
            }
            Button::Stop => self.connection_active(),
        }
    }

    fn badges(&self) -> Vec<&'static str> {
        [
            (self.seed, SEED_NAME),
            (self.include_ignored, IGNORED_BADGE),
            (self.skip_dotfiles, DOTFILES_BADGE),
            (self.changes_only, CHANGES_BADGE),
        ]
        .into_iter()
        .filter_map(|(on, badge)| on.then_some(badge))
        .collect()
    }

    /// One row, only when something needs saying: why the comparison failed,
    /// why a side is partial and what to do about it, and how many rows the
    /// tree could not place.
    fn banner(&self, styles: &WorkbenchStyles, width: u16) -> Option<Line<'static>> {
        let (text, style) = match &self.failure {
            Some(reason) => (format!("{COMPARE_FAILED}{reason}"), styles.error),
            None => {
                let mut parts: Vec<String> = TransferSide::BOTH
                    .into_iter()
                    .filter_map(|side| {
                        let scan = self.scan(side);
                        if scan.unsupported {
                            return Some(format!("{} {SCAN_UNSUPPORTED}", side_name(side)));
                        }
                        (!scan.limits.is_empty()).then(|| {
                            let limits: Vec<&str> =
                                scan.limits.iter().map(|limit| limit_text(*limit)).collect();
                            format!(
                                "{} {SCAN_INCOMPLETE}: {}",
                                side_name(side),
                                limits.join(LIST_GAP)
                            )
                        })
                    })
                    .collect();
                if !parts.is_empty() {
                    parts.push(SCAN_ACTION.to_owned());
                }
                let dropped = self.tree.dropped();
                if dropped > 0 {
                    parts.push(format!(
                        "{dropped} {}",
                        noun(dropped, DROPPED_ROW, DROPPED_ROWS)
                    ));
                }
                if parts.is_empty() {
                    return None;
                }
                (parts.join(PARTS_GAP), styles.accent)
            }
        };
        Some(Line::from(Span::styled(
            chrome::fit(&text, usize::from(width)),
            style,
        )))
    }

    /// What the panes say while they have no rows to show.
    fn placeholder(&self) -> &'static str {
        if matches!(self.request, Some(TransferAction::Compare { .. })) {
            COMPARING
        } else if !self.tree.is_empty() {
            NO_DIFFERENCES
        } else if self.complete {
            EMPTY_COMPARISON
        } else {
            NOT_COMPARED
        }
    }

    fn header_line(
        &self,
        side: TransferSide,
        styles: &WorkbenchStyles,
        width: u16,
    ) -> Line<'static> {
        let title = side_name(side).to_uppercase();
        let root = match side {
            TransferSide::Local => self.roots.local.clone(),
            TransferSide::Remote => format!("{ROOT_SEPARATOR}{}", self.roots.remote),
        };
        let style = match self.focus == side {
            true => styles.title,
            false => styles.dim,
        };
        let budget = usize::from(width).saturating_sub(title.width() + TAB_GAP.width());
        let mut spans = vec![
            Span::styled(title, style),
            Span::styled(TAB_GAP, styles.background),
        ];
        if root.is_empty() {
            spans.push(Span::styled(side.root_key().label, styles.accent));
            spans.push(Span::styled(format!("{TAB_GAP}{ROOT_OFFER}"), styles.dim));
            return truncate(spans, usize::from(width));
        }
        spans.push(Span::styled(chrome::fit_end(&root, budget), styles.dim));
        Line::from(spans)
    }

    /// One side of one row, shaped like the explorer's: the check column, the
    /// guides, the fold marker and the name, with the marks on the right.
    fn row_line(
        &self,
        row: &Row,
        side: TransferSide,
        highlight: Option<Style>,
        styles: &WorkbenchStyles,
        width: u16,
    ) -> Line<'static> {
        let mut left = vec![
            self.check(row, styles),
            Span::styled(indent_guides(row.depth()), styles.border),
        ];
        match row {
            Row::Entry(path) => {
                let Some(entry) = self.tree.entry(path) else {
                    return Line::default();
                };
                let (label, style, marks) = self.entry_label(entry, side, styles);
                let budget =
                    usize::from(width).saturating_sub(spans_width(&left) + spans_width(&marks));
                left.push(Span::styled(
                    chrome::fit(&label, budget),
                    highlight.unwrap_or(style),
                ));
                chrome::status_line(left, marks, width, styles.background)
            }
            // The offer yields to the note when both do not fit, since the
            // note is what explains the empty folder.
            Row::Note(folder) => {
                let note = self.tree.note(folder, side, self.scan(side));
                let offer = note
                    .and_then(Note::action)
                    .map_or_else(Vec::new, |action| hints(&[offer_hint(action)], styles));
                if let Some(note) = note {
                    let budget = usize::from(width).saturating_sub(spans_width(&left));
                    let text = format!("{LEAF_INDENT}{}", note.text());
                    left.push(Span::styled(
                        chrome::fit(&text, budget),
                        highlight.unwrap_or(styles.dim),
                    ));
                }
                chrome::status_line(left, offer, width, styles.background)
            }
        }
    }

    /// An accent mark on a chosen row, a dim one on a row a chosen folder
    /// carries along, and air everywhere else.
    fn check(&self, row: &Row, styles: &WorkbenchStyles) -> Span<'static> {
        let style = match row {
            Row::Entry(path) if self.selected.contains(path) => styles.accent,
            Row::Entry(path) if self.implied(path) => styles.dim,
            _ => return Span::styled(CHECK_BLANK, styles.background),
        };
        Span::styled(format!("{CHECK_MARK}{TAB_GAP}"), style)
    }

    /// The marker and name an entry shows on `side`, the style they take, and
    /// the marks on the right. A side without the path keeps only a dot, so
    /// the rows beside it stay aligned.
    fn entry_label(
        &self,
        entry: &TransferEntry,
        side: TransferSide,
        styles: &WorkbenchStyles,
    ) -> (String, Style, Vec<Span<'static>>) {
        let Some(kind) = entry.kind(side) else {
            return (
                format!("{LEAF_INDENT}{ABSENT_MARK}"),
                styles.dim,
                Vec::new(),
            );
        };
        let badge = self
            .tree
            .note(&entry.path, side, self.scan(side))
            .and_then(Note::badge);
        let marker = match (kind.expandable(), self.expanded.contains(&entry.path)) {
            (true, true) => EXPANDED_MARK,
            (true, false) => COLLAPSED_MARK,
            (false, _) => LEAF_INDENT,
        };
        let style = match (badge, kind) {
            (Some(_), _) => styles.dim,
            (None, TransferNodeKind::Directory) => styles.directory,
            (None, _) => styles.text,
        };
        let mut marks = Vec::new();
        if let Some(badge) = badge {
            marks.push(Span::styled(format!("{TAB_GAP}{badge}"), styles.dim));
        }
        let summary = self.tree.summary(&entry.path);
        if summary.changed > 0 {
            marks.push(Span::styled(
                format!("{TAB_GAP}{CHANGED_MARK}{TAB_GAP}{}", summary.changed),
                styles.git_modified,
            ));
        }
        if let Some((mark, mark_style)) = status_mark(entry, side, summary.unknown > 0, styles) {
            marks.push(Span::styled(format!("{TAB_GAP}{mark}"), mark_style));
        }
        (format!("{marker}{}", name(&entry.path)), style, marks)
    }

    fn panel_heading(&self, styles: &WorkbenchStyles, width: u16) -> Line<'static> {
        let (left, right) = match &self.panel {
            Some(Panel::Diff {
                path, truncated, ..
            }) => {
                let right = vec![Span::styled(DIFF_LEGEND, styles.dim)];
                let badge = match truncated {
                    true => vec![
                        Span::styled(HINT_GAP, styles.background),
                        Span::styled(TRUNCATED_BADGE, styles.accent),
                    ],
                    false => Vec::new(),
                };
                let room =
                    usize::from(width).saturating_sub(spans_width(&badge) + spans_width(&right));
                let mut left = vec![Span::styled(chrome::fit_end(path, room), styles.title)];
                left.extend(badge);
                (left, right)
            }
            Some(Panel::Binary(preview)) => (
                vec![Span::styled(
                    chrome::fit_end(&preview.path, usize::from(width)),
                    styles.title,
                )],
                Vec::new(),
            ),
            Some(Panel::Review(review)) => (
                vec![
                    Span::styled(REVIEW_TITLE, styles.title),
                    Span::styled(format!("{HINT_GAP}{}", review_summary(review)), styles.dim),
                ],
                Vec::new(),
            ),
            Some(Panel::Report) => {
                let summary = self.outcome.as_ref().map_or_else(String::new, |outcome| {
                    outcome_summary(outcome, self.reported.as_ref())
                });
                (
                    vec![
                        Span::styled(REPORT_TITLE, styles.title),
                        Span::styled(format!("{HINT_GAP}{summary}"), styles.dim),
                    ],
                    Vec::new(),
                )
            }
            None => (Vec::new(), Vec::new()),
        };
        let budget = usize::from(width).saturating_sub(spans_width(&right));
        chrome::status_line(
            truncate(left, budget).spans,
            right,
            width,
            styles.background,
        )
    }

    fn panel_lines(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        match &self.panel {
            Some(Panel::Binary(preview)) => TransferSide::BOTH
                .into_iter()
                .map(|side| preview_line(side, preview_side(preview, side), styles))
                .collect(),
            Some(Panel::Review(review)) => review_lines(review, styles, width),
            Some(Panel::Report) => self.report_lines(styles, width),
            Some(Panel::Diff { .. }) | None => Vec::new(),
        }
    }

    /// Every path the last transfer touched and how it ended, then whatever
    /// recovery says.
    fn report_lines(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(outcome) = &self.outcome {
            for entry in &outcome.entries {
                let (mark, style) = outcome_mark(entry.outcome, styles);
                let prefix = vec![Span::styled(format!("{mark}{TAB_GAP}"), style)];
                lines.push(path_line(prefix, &entry.path, styles.text, width));
            }
            if let Some(reason) = &outcome.stopped {
                lines.push(Line::from(Span::styled(
                    format!("{STOPPED_LABEL}{reason}"),
                    styles.error,
                )));
            }
        }
        if let Some(recovery) = &self.recovery {
            lines.extend(
                recovery
                    .lines
                    .iter()
                    .map(|line| Line::from(Span::styled(line.clone(), styles.dim))),
            );
            if recovery.required {
                lines.extend(recovery_lines(styles));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(NO_TRANSFER, styles.dim)));
        }
        lines
    }

    fn sidebar_lines(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.changes_section(styles, width);
        lines.push(Line::default());
        lines.extend(self.selection_section(styles, width));
        lines.push(Line::default());
        lines.extend(self.report_section(styles, width));
        lines
    }

    fn changes_section(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        let summary = self.tree.summary("");
        let mut lines = vec![section(CHANGES_TITLE, summary.changed, styles, width)];
        if self.tree.is_empty() {
            lines.push(Line::from(Span::styled(self.placeholder(), styles.dim)));
            return lines;
        }
        let count = |wanted: &[TransferStatus]| {
            self.tree
                .entries()
                .filter(|entry| wanted.contains(&entry.status))
                .count()
        };
        let conflicts = count(&[TransferStatus::TypeConflict]);
        let tallies = [
            (
                DIFFERENT_MARK,
                styles.git_modified,
                count(&[TransferStatus::Different]),
                DIFFERENT_LABEL,
            ),
            (
                ONLY_HERE_MARK,
                styles.git_added,
                count(&[TransferStatus::LocalOnly]),
                LOCAL_ONLY_LABEL,
            ),
            (
                ONLY_HERE_MARK,
                styles.git_added,
                count(&[TransferStatus::RemoteOnly]),
                SANDBOX_ONLY_LABEL,
            ),
            (
                CONFLICT_MARK,
                styles.git_conflicted,
                conflicts,
                noun(conflicts, CONFLICT_LABEL, CONFLICTS_LABEL),
            ),
            (UNKNOWN_MARK, styles.dim, summary.unknown, UNKNOWN_LABEL),
            (
                ABSENT_MARK,
                styles.dim,
                count(&[TransferStatus::Excluded, TransferStatus::Unsupported]),
                BLOCKED_LABEL,
            ),
        ];
        lines.extend(
            tallies
                .into_iter()
                .filter(|(_, _, count, _)| *count > 0)
                .map(|(mark, style, count, label)| tally(mark, style, count, label, styles)),
        );
        if lines.len() == 1 {
            lines.push(Line::from(Span::styled(NO_DIFFERENCES, styles.dim)));
        }
        lines
    }

    /// How much each direction would carry, then every chosen path.
    fn selection_section(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![section(SELECTION_TITLE, self.selected.len(), styles, width)];
        for (mark, direction) in [
            (UPLOAD_MARK, self.upload()),
            (DOWNLOAD_MARK, TransferDirection::Pull),
        ] {
            let selection = self.selection(&direction);
            let mut parts = vec![
                format!("{} {}", direction_name(&direction), selection.paths.len()),
                size(selection.bytes),
            ];
            if selection.skipped > 0 {
                parts.push(format!("{} {SKIPPED_LABEL}", selection.skipped));
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{mark}{TAB_GAP}"), styles.accent),
                Span::styled(parts.join(PARTS_GAP), styles.text),
            ]));
        }
        if self.selected.is_empty() {
            lines.push(Line::from(Span::styled(NOTHING_CHOSEN, styles.dim)));
        }
        lines.extend(self.selected.iter().map(|path| {
            let prefix = vec![Span::styled(
                format!("{CHECK_MARK}{TAB_GAP}"),
                styles.accent,
            )];
            path_line(prefix, path, styles.text, width)
        }));
        lines
    }

    fn report_section(&self, styles: &WorkbenchStyles, width: u16) -> Vec<Line<'static>> {
        let reported = self
            .outcome
            .as_ref()
            .map_or(0, |outcome| outcome.entries.len());
        let mut lines = vec![section(REPORT_TITLE, reported, styles, width)];
        if let Some(outcome) = &self.outcome {
            for kind in OUTCOMES {
                let count = outcome
                    .entries
                    .iter()
                    .filter(|entry| entry.outcome == kind)
                    .count();
                if count > 0 {
                    let (mark, style) = outcome_mark(kind, styles);
                    lines.push(tally(mark, style, count, outcome_name(kind), styles));
                }
            }
            if let Some(reason) = &outcome.stopped {
                lines.push(Line::from(Span::styled(
                    format!("{STOPPED_LABEL}{reason}"),
                    styles.error,
                )));
            }
        }
        if self
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.required)
        {
            lines.extend(recovery_lines(styles));
        }
        if lines.len() == 1 {
            lines.push(Line::from(Span::styled(NO_TRANSFER, styles.dim)));
        }
        lines
    }

    /// The status bar's left side: what is running, else the last notice,
    /// else what the comparison found. Stopping outranks progress, since
    /// input stays locked until cleanup ends.
    fn status(&self, styles: &WorkbenchStyles) -> (String, Style) {
        if self.pending
            && !self.draining
            && let Some(progress) = &self.progress
        {
            return (progress_text(progress, self.approved.as_ref()), styles.text);
        }
        match &self.notice {
            Some(Notice::Error(text)) => (text.clone(), styles.error),
            Some(Notice::Info(text)) => (text.clone(), styles.dim),
            None => (self.summary(), styles.dim),
        }
    }

    fn summary(&self) -> String {
        if matches!(self.request, Some(TransferAction::Compare { .. })) {
            return COMPARING.to_owned();
        }
        if self.tree.is_empty() {
            return String::new();
        }
        let summary = self.tree.summary("");
        let paths = self.tree.len();
        let mut parts = vec![
            format!("{paths} {}", noun(paths, PATH_LABEL, PATHS_LABEL)),
            format!("{} {CHANGED_LABEL}", summary.changed),
        ];
        if summary.unknown > 0 {
            parts.push(format!("{} {UNKNOWN_LABEL}", summary.unknown));
        }
        parts.join(PARTS_GAP)
    }

    /// What the next key can do, for whatever stands in front.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.prompt.is_some() {
            return vec![
                (ENTER_LABEL, "compare"),
                (keys::CLEAR_ROOT.label, "clear"),
                (keys::CLOSE.label, "cancel"),
            ];
        }
        if self.pending || self.draining {
            return vec![(keys::STOP.label, "stop")];
        }
        match &self.panel {
            Some(Panel::Review(review)) if review.executable => {
                vec![
                    (keys::APPROVE.label, "approve"),
                    (keys::CLOSE.label, "close"),
                ]
            }
            Some(_) => vec![(keys::CLOSE.label, "close")],
            None => vec![
                (keys::SELECT.label, "select"),
                (keys::UPLOAD.label, "upload"),
                (keys::DOWNLOAD.label, "download"),
                (keys::COMPARE.label, "compare"),
                (keys::CLOSE.label, "back"),
            ],
        }
    }
}

/// Whether a click `column` cells into a row landed on its check column.
pub(super) fn on_check(column: usize) -> bool {
    column < CHECK_BLANK.len()
}

/// Whether a click `column` cells into a row `depth` folders deep landed on
/// its fold marker, which follows the check column and the guides.
pub(super) fn on_marker(column: usize, depth: usize) -> bool {
    let start = CHECK_BLANK.len() + depth * GUIDE.width();
    (start..start + EXPANDED_MARK.width()).contains(&column)
}

/// Where each side's pane goes across `area`: side by side with the rule
/// between them when there is room, otherwise only the focused one.
fn columns(area: Rect, focus: TransferSide) -> ([Rect; 2], Option<Rect>) {
    if area.width < MIN_TWO_PANE_WIDTH {
        let mut panes = [Rect::default(); 2];
        panes[focus.index()] = area;
        return (panes, None);
    }
    let [local, rule, remote] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(RULE_COLUMNS),
        Constraint::Fill(1),
    ])
    .areas(area);
    ([local, remote], Some(rule))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn section(
    title: &'static str,
    count: usize,
    styles: &WorkbenchStyles,
    width: u16,
) -> Line<'static> {
    chrome::status_line(
        vec![Span::styled(title, styles.title)],
        vec![Span::styled(count.to_string(), styles.dim)],
        width,
        styles.background,
    )
}

fn tally(
    mark: &'static str,
    style: Style,
    count: usize,
    label: &'static str,
    styles: &WorkbenchStyles,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{mark}{TAB_GAP}"), style),
        Span::styled(format!("{count} {label}"), styles.text),
    ])
}

/// That a publication may or may not have landed, then the key that settles it.
fn recovery_lines(styles: &WorkbenchStyles) -> [Line<'static>; 2] {
    [
        Line::from(Span::styled(RECOVERY_REQUIRED, styles.error)),
        Line::from(hints(&[(keys::RECONCILE.label, RECONCILE_OFFER)], styles)),
    ]
}

/// `prefix`, then `path` cut from the left to the room the prefix leaves, so
/// the file name survives a narrow pane.
fn path_line(
    mut prefix: Vec<Span<'static>>,
    path: &str,
    style: Style,
    width: u16,
) -> Line<'static> {
    let room = usize::from(width).saturating_sub(spans_width(&prefix));
    prefix.push(Span::styled(chrome::fit_end(path, room), style));
    Line::from(prefix)
}

/// `line` cut to `width` columns with an ellipsis, keeping its fill.
fn fitted(line: Line<'static>, width: u16) -> Line<'static> {
    let style = line.style;
    truncate(line.spans, usize::from(width)).style(style)
}

/// Whichever of `one` and `many` agrees with `count`.
fn noun(count: usize, one: &'static str, many: &'static str) -> &'static str {
    match count {
        1 => one,
        _ => many,
    }
}

fn offer_hint(action: NoteAction) -> (&'static str, &'static str) {
    match action {
        NoteAction::IncludeIgnored => (keys::INCLUDE_IGNORED.label, INCLUDE_IGNORED_OFFER),
        NoteAction::IncludeDotfiles => (keys::SKIP_DOTFILES.label, INCLUDE_DOTFILES_OFFER),
        NoteAction::CompareFolder => (ENTER_LABEL, COMPARE_FOLDER_OFFER),
    }
}

/// The mark an entry's own status earns on `side`. Only the side that holds a
/// one-sided path is marked as having it; a folder with anything unsettled
/// under it is unknown even where it matches.
fn status_mark(
    entry: &TransferEntry,
    side: TransferSide,
    unsettled_below: bool,
    styles: &WorkbenchStyles,
) -> Option<(&'static str, Style)> {
    match (&entry.status, side) {
        (TransferStatus::Different, _) => Some((DIFFERENT_MARK, styles.git_modified)),
        (TransferStatus::LocalOnly, TransferSide::Local)
        | (TransferStatus::RemoteOnly, TransferSide::Remote) => {
            Some((ONLY_HERE_MARK, styles.git_added))
        }
        (TransferStatus::TypeConflict, _) => Some((CONFLICT_MARK, styles.git_conflicted)),
        (TransferStatus::Incomplete, _) => Some((UNKNOWN_MARK, styles.dim)),
        _ if entry.unlisted || unsettled_below => Some((UNKNOWN_MARK, styles.dim)),
        _ => None,
    }
}

fn side_name(side: TransferSide) -> &'static str {
    match side {
        TransferSide::Local => LOCAL_NAME,
        TransferSide::Remote => SANDBOX_NAME,
    }
}

fn direction_name(direction: &TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Push => UPLOAD_NAME,
        TransferDirection::Pull => DOWNLOAD_NAME,
        TransferDirection::Seed => SEED_NAME,
    }
}

fn phase_verb(phase: TransferPhase, direction: Option<&TransferDirection>) -> &'static str {
    match (phase, direction) {
        (TransferPhase::Scanning, _) => "Scanning",
        (TransferPhase::Staging, _) => "Staging",
        (TransferPhase::Sealing, _) => "Sealing",
        (TransferPhase::Preparing, _) => "Preparing",
        (TransferPhase::Reviewing, _) => "Reviewing",
        (TransferPhase::Publishing, Some(TransferDirection::Push)) => "Uploading",
        (TransferPhase::Publishing, Some(TransferDirection::Pull)) => "Downloading",
        (TransferPhase::Publishing, Some(TransferDirection::Seed)) => "Seeding",
        (TransferPhase::Publishing, None) => "Publishing",
        (TransferPhase::Reconciling, _) => "Reconciling",
    }
}

/// Progress as a sentence: `Scanning sandbox src/…` while a side is walked,
/// `Uploading 2/5 · src/new.rs` while paths move.
fn progress_text(progress: &TransferProgress, direction: Option<&TransferDirection>) -> String {
    let mut text = phase_verb(progress.phase, direction).to_owned();
    if let Some(side) = progress.side {
        text.push(' ');
        text.push_str(&side_name(side).to_lowercase());
    }
    if progress.total > 0 {
        text.push_str(&format!(" {}/{}", progress.completed, progress.total));
    }
    match (&progress.path, progress.phase) {
        (Some(path), TransferPhase::Scanning) => {
            text.push_str(&format!(" {path}{ROOT_SEPARATOR}{ELLIPSIS}"));
        }
        (Some(path), _) => text.push_str(&format!("{PARTS_GAP}{path}")),
        (None, _) => text.push(ELLIPSIS),
    }
    text
}

fn limit_text(limit: TransferScanLimit) -> &'static str {
    match limit {
        TransferScanLimit::Entries => "entry limit",
        TransferScanLimit::Pages => "page limit",
        TransferScanLimit::Depth => "depth limit",
        TransferScanLimit::Bytes => "size limit",
        TransferScanLimit::WorkcellIncomplete => "listing incomplete",
        TransferScanLimit::ListingFailed => "listing failed",
        TransferScanLimit::Changed => "changed while scanned",
        TransferScanLimit::Unreadable => "unreadable entries",
    }
}

fn kind_name(kind: TransferNodeKind) -> &'static str {
    match kind {
        TransferNodeKind::File => "file",
        TransferNodeKind::Directory => "folder",
        TransferNodeKind::Symlink => "symbolic link",
        TransferNodeKind::Repository => "nested repository",
        TransferNodeKind::Special => "special file",
    }
}

fn outcome_mark(outcome: TransferFileOutcome, styles: &WorkbenchStyles) -> (&'static str, Style) {
    match outcome {
        TransferFileOutcome::Confirmed => (CHECK_MARK, styles.git_added),
        TransferFileOutcome::Failed => (FAILED_MARK, styles.error),
        TransferFileOutcome::Cancelled => (CANCELLED_MARK, styles.dim),
        TransferFileOutcome::Unknown => (UNKNOWN_MARK, styles.dim),
    }
}

fn outcome_name(outcome: TransferFileOutcome) -> &'static str {
    match outcome {
        TransferFileOutcome::Confirmed => "confirmed",
        TransferFileOutcome::Failed => "failed",
        TransferFileOutcome::Cancelled => "cancelled",
        TransferFileOutcome::Unknown => "unknown",
    }
}

fn outcome_summary(outcome: &TransferOutcome, direction: Option<&TransferDirection>) -> String {
    let mut parts: Vec<String> = direction
        .map(|direction| direction_name(direction).to_owned())
        .into_iter()
        .collect();
    for kind in OUTCOMES {
        let count = outcome
            .entries
            .iter()
            .filter(|entry| entry.outcome == kind)
            .count();
        if count > 0 {
            parts.push(format!("{count} {}", outcome_name(kind)));
        }
    }
    parts.join(PARTS_GAP)
}

fn preview_side(preview: &TransferPreview, side: TransferSide) -> Option<&TransferPreviewSide> {
    match side {
        TransferSide::Local => preview.local.as_ref(),
        TransferSide::Remote => preview.remote.as_ref(),
    }
}

/// What one side of a file that has no diff to show holds.
fn preview_line(
    side: TransferSide,
    preview: Option<&TransferPreviewSide>,
    styles: &WorkbenchStyles,
) -> Line<'static> {
    let title = Span::styled(
        format!("{:<SIDE_COLUMNS$}", side_name(side).to_uppercase()),
        styles.title,
    );
    let Some(preview) = preview else {
        return Line::from(vec![title, Span::styled(MISSING, styles.dim)]);
    };
    let mut parts = vec![kind_name(preview.kind).to_owned(), size(preview.bytes)];
    if !preview.digest.is_empty() {
        parts.push(short_digest(&preview.digest).to_owned());
    }
    if preview.binary {
        parts.push(BINARY_BADGE.to_owned());
    } else if preview.text.is_none() {
        parts.push(NOT_TEXT_BADGE.to_owned());
    }
    if preview.truncated {
        parts.push(TRUNCATED_BADGE.to_owned());
    }
    Line::from(vec![
        title,
        Span::styled(parts.join(PARTS_GAP), styles.text),
    ])
}

fn review_summary(review: &TransferReview) -> String {
    let bytes = review
        .entries
        .iter()
        .fold(0, |total: u64, entry| total.saturating_add(entry.bytes));
    let changes = review.entries.len();
    let mut parts = vec![
        direction_name(&review.direction).to_owned(),
        format!("{changes} {}", noun(changes, CHANGE_LABEL, CHANGES_LABEL)),
        size(bytes),
    ];
    if !review.skipped.is_empty() {
        parts.push(format!("{} {SKIPPED_LABEL}", review.skipped.len()));
    }
    parts.join(PARTS_GAP)
}

/// Why a review cannot be approved when it cannot, every operation it would
/// perform, what it skips, and the digest approval binds to.
fn review_lines(
    review: &TransferReview,
    styles: &WorkbenchStyles,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match (&review.notice, review.executable) {
        (notice, false) => lines.push(Line::from(Span::styled(
            notice.clone().unwrap_or_else(|| NOT_EXECUTABLE.to_owned()),
            styles.error,
        ))),
        (Some(notice), true) => lines.push(Line::from(Span::styled(notice.clone(), styles.dim))),
        (None, true) => {}
    }
    for entry in &review.entries {
        let (mark, effect, style) = match entry.effect {
            TransferEffect::New => (ONLY_HERE_MARK, NEW_EFFECT, styles.git_added),
            TransferEffect::Overwrite => (DIFFERENT_MARK, OVERWRITE_EFFECT, styles.git_modified),
            TransferEffect::Mkdir => (ONLY_HERE_MARK, MKDIR_EFFECT, styles.directory),
        };
        let bytes = match entry.effect {
            TransferEffect::Mkdir => String::new(),
            TransferEffect::New | TransferEffect::Overwrite => size(entry.bytes),
        };
        let prefix = vec![
            Span::styled(format!("{mark}{TAB_GAP}{effect:<EFFECT_COLUMNS$}"), style),
            Span::styled(format!("{bytes:>SIZE_COLUMNS$}{HINT_GAP}"), styles.dim),
        ];
        lines.push(path_line(prefix, &entry.path, styles.text, width));
    }
    if !review.skipped.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(SKIPPED_TITLE, styles.title),
            Span::styled(format!("{TAB_GAP}{}", review.skipped.len()), styles.dim),
        ]));
        lines.extend(
            review
                .skipped
                .iter()
                .map(|path| path_line(Vec::new(), path, styles.dim, width)),
        );
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{DIGEST_LABEL}{}", review.digest),
        styles.dim,
    )));
    lines
}

/// The head of a digest's hex, without the algorithm naming it.
fn short_digest(digest: &str) -> &str {
    let hex = digest
        .split_once(DIGEST_ALGORITHM_END)
        .map_or(digest, |(_, hex)| hex);
    hex.char_indices()
        .nth(SHORT_DIGEST)
        .map_or(hex, |(end, _)| &hex[..end])
}

/// A byte count the way a file manager prints one.
fn size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= SIZE_STEP && unit + 1 < SIZE_UNITS.len() {
        value /= SIZE_STEP;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} {}", SIZE_UNITS[0]),
        _ => format!("{value:.1} {}", SIZE_UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        ABSENT_MARK, BINARY_BADGE, CANCELLED_MARK, CHANGED_MARK, CHANGES_BADGE, CHANGES_TITLE,
        CHECK_MARK, COMPARE_FAILED, DIFF_LEGEND, DIFFERENT_MARK, DIGEST_LABEL, DOTFILES_BADGE,
        DROPPED_ROW, FAILED_MARK, IGNORED_BADGE, INCLUDE_DOTFILES_OFFER, INCLUDE_IGNORED_OFFER,
        LOCAL_NAME, MKDIR_EFFECT, NEW_EFFECT, NOT_EXECUTABLE, NOTHING_CHOSEN, ONLY_HERE_MARK,
        OVERWRITE_EFFECT, RECONCILE_OFFER, RECOVERY_REQUIRED, REPORT_TITLE, REVIEW_TITLE,
        ROOT_OFFER, SANDBOX_NAME, SCAN_ACTION, SCAN_INCOMPLETE, SEED_NAME, SELECTION_TITLE,
        SKIPPED_TITLE, STOPPED_LABEL, TITLE_MARK, TRUNCATED_BADGE, UNKNOWN_MARK, limit_text,
        progress_text, size,
    };
    use crate::chrome::{ELLIPSIS, VERTICAL};
    use crate::transfer::tests::{
        CHANGED_FILE, CONTENTS, DIGEST, DOT, DOT_FOLDER, EMPTY_FOLDER, FILE_NAME, FOLDER, LABEL,
        LOCAL_ROOT, NARROW, PREVIEW_DIGEST, RECOVERY, SANDBOX_DRAFT, STAGE, STOPPED, UPDATED, WIDE,
        answer, compare, enter, excluded, file, folder, outcome, paint, partial, pending_recovery,
        point_at, press, preview, project, roots, unfold, workbench,
    };
    use crate::transfer::tree::{Note, Row};
    use crate::transfer::{
        Button, DIFF_TRUNCATED, TransferAction, TransferDirection, TransferEffect, TransferEntry,
        TransferExclusion, TransferFileOutcome, TransferOutcome, TransferOutcomeEntry,
        TransferPhase, TransferProgress, TransferReview, TransferReviewEntry, TransferRoots,
        TransferScan, TransferScanLimit, TransferSide, TransferStatus,
    };
    use crate::{Workbench, WorkbenchAction, keys};
    use std::collections::BTreeSet;

    /// What Debug formatting or an unmapped kind would leave on screen.
    const DEBUG_TEXT: [&str; 3] = ["Some(", "{ ", "(missing)"];
    const PROTECTED_FOLDER: &str = ".caudra";
    const NEW_FILE: &str = "src/new.rs";
    const README: &str = "README.md";
    const CHANGED_NAME: &str = "main.rs";
    const NEW_NAME: &str = "new.rs";
    const FAILED_FILE: &str = "failed.txt";
    const CANCELLED_FILE: &str = "cancelled.txt";
    const NEW_BYTES: u64 = 2_048;
    const NEW_SIZE: &str = "2.0 KiB";
    const CONTENTS_SIZE: &str = "10 B";
    const SHORT_PREVIEW_DIGEST: &str = "0123456789ab";
    const PROJECT_SUMMARY: &str = "6 paths \u{00b7} 2 changed";
    const ONE_PATH_SUMMARY: &str = "1 path \u{00b7} 1 changed";
    const DEEP_FILE: &str = "src/a/folder/nested/deeper/than/the/panel/is/wide/deep.rs";
    const DEEP_NAME: &str = "deep.rs";
    const SCANNING_SANDBOX: &str = "Scanning sandbox src/\u{2026}";
    const UPLOADING: &str = "Uploading 2/5 \u{00b7} src/new.rs";
    const REVIEWING: &str = "Reviewing\u{2026}";
    const UPLOADED: usize = 2;
    const TO_UPLOAD: usize = 5;
    const NOT_PAINTED: &str = "the frame is missing something it must show";
    const UNREADABLE: &str = "Debug text or an unmapped kind leaked onto the screen";
    const NO_BADGE: &str = "a note that is never transferred carries a badge";
    const NO_COMPARE: &str = "the key must ask the host for a comparison";
    const NAME_LOST: &str = "a path too long for its pane must keep its file name";

    fn assert_painted(screen: &str, expected: &[&str]) {
        for text in expected {
            assert!(screen.contains(text), "{NOT_PAINTED}: {text}\n{screen}");
        }
        for debug in DEBUG_TEXT {
            assert!(!screen.contains(debug), "{UNREADABLE}: {debug}\n{screen}");
        }
    }

    /// The plan's picture: a protected folder, `src` holding a changed file
    /// and one only the local side has, an ignored folder, an empty one, and
    /// a file both sides agree on.
    fn tree() -> Vec<TransferEntry> {
        vec![
            excluded(PROTECTED_FOLDER, TransferExclusion::Protected),
            folder(FOLDER, TransferStatus::Equal),
            file(CHANGED_FILE, TransferStatus::Different),
            file(NEW_FILE, TransferStatus::LocalOnly),
            excluded(STAGE, TransferExclusion::Gitignore),
            folder(EMPTY_FOLDER, TransferStatus::Equal),
            file(README, TransferStatus::Equal),
        ]
    }

    /// The tree with every folder that can list something unfolded, and the
    /// changed file chosen.
    fn unfolded() -> Workbench {
        let mut workbench = workbench();
        compare(&mut workbench, tree());
        unfold(&mut workbench, &[FOLDER, STAGE, EMPTY_FOLDER]);
        point_at(&mut workbench, Row::Entry(CHANGED_FILE.to_owned()));
        press(&mut workbench, keys::SELECT);
        workbench
    }

    fn review() -> TransferReview {
        TransferReview {
            digest: DIGEST.to_owned(),
            roots: roots(),
            direction: TransferDirection::Push,
            entries: vec![
                TransferReviewEntry {
                    path: NEW_FILE.to_owned(),
                    effect: TransferEffect::New,
                    bytes: NEW_BYTES,
                },
                TransferReviewEntry {
                    path: CHANGED_FILE.to_owned(),
                    effect: TransferEffect::Overwrite,
                    bytes: CONTENTS.len() as u64,
                },
                TransferReviewEntry {
                    path: EMPTY_FOLDER.to_owned(),
                    effect: TransferEffect::Mkdir,
                    bytes: 0,
                },
            ],
            skipped: vec![STAGE.to_owned()],
            executable: true,
            notice: None,
        }
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn the_tree_paints_both_sides_aligned_with_marks_badges_and_notes(terminal: (u16, u16)) {
        let mut workbench = unfolded();
        let screen = paint(&mut workbench, terminal);
        let [protected, ignored] =
            [Note::Protected, Note::Ignored].map(|note| note.badge().expect(NO_BADGE));
        assert_painted(
            &screen,
            &[
                &LOCAL_NAME.to_uppercase(),
                &SANDBOX_NAME.to_uppercase(),
                LOCAL_ROOT,
                SANDBOX_DRAFT,
                PROTECTED_FOLDER,
                CHANGED_NAME,
                NEW_NAME,
                STAGE,
                README,
                protected,
                ignored,
                Note::Empty.text(),
                DIFFERENT_MARK,
                ONLY_HERE_MARK,
                CHANGED_MARK,
                CHECK_MARK,
                ABSENT_MARK,
                CHANGES_TITLE,
                SELECTION_TITLE,
                REPORT_TITLE,
            ],
        );
    }

    #[test_case(NARROW, true; "narrow_set")]
    #[test_case(NARROW, false; "narrow_unset")]
    #[test_case(WIDE, false; "wide_unset")]
    fn only_an_unset_local_root_offers_its_key_in_the_header(terminal: (u16, u16), set: bool) {
        let mut workbench = workbench();
        let local = if set { LOCAL_ROOT } else { "" };
        assert!(workbench.show_transfer(
            TransferRoots {
                local: local.to_owned(),
                ..roots()
            },
            TransferDirection::Push
        ));
        let offer = format!("{} {ROOT_OFFER}", keys::LOCAL_ROOT.label);
        let screen = paint(&mut workbench, terminal);
        assert_eq!(screen.contains(&offer), !set, "{offer}\n{screen}");
    }

    #[test]
    fn a_wide_tree_shows_each_note_whole_with_what_it_offers() {
        let mut workbench = unfolded();
        let screen = paint(&mut workbench, WIDE);
        assert_painted(
            &screen,
            &[
                &format!("{TITLE_MARK}{LABEL}"),
                Note::Ignored.text(),
                INCLUDE_IGNORED_OFFER,
            ],
        );
    }

    #[test]
    fn a_skipped_dotfile_folder_says_why_and_offers_them_back() {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::SKIP_DOTFILES);
        answer(
            &mut workbench,
            action,
            vec![excluded(DOT_FOLDER, TransferExclusion::Dotfile)],
            TransferScan::default(),
        );
        unfold(&mut workbench, &[DOT_FOLDER]);
        let screen = paint(&mut workbench, WIDE);
        assert_painted(
            &screen,
            &[
                Note::Dotfile.badge().unwrap(),
                Note::Dotfile.text(),
                INCLUDE_DOTFILES_OFFER,
            ],
        );
    }

    #[test]
    fn the_toolbar_names_the_sandbox_every_mode_in_force_and_every_button() {
        let mut workbench = workbench();
        assert!(workbench.show_transfer(roots(), TransferDirection::Seed));
        for toggle in [keys::INCLUDE_IGNORED, keys::SKIP_DOTFILES] {
            let action = press(&mut workbench, toggle);
            answer(&mut workbench, action, project(), TransferScan::default());
        }
        press(&mut workbench, keys::CHANGES_ONLY);
        let screen = paint(&mut workbench, WIDE);
        let title = format!("{TITLE_MARK}{LABEL}");
        let mut expected = vec![
            title.as_str(),
            SEED_NAME,
            IGNORED_BADGE,
            DOTFILES_BADGE,
            CHANGES_BADGE,
        ];
        expected.extend(Button::ALL.map(Button::label));
        assert_painted(&screen, &expected);
    }

    #[test_case(NARROW, true; "narrow")]
    #[test_case(WIDE, true; "wide")]
    #[test_case(NARROW, false; "narrow_refused")]
    fn a_review_paints_every_operation_its_size_and_the_digest(
        terminal: (u16, u16),
        executable: bool,
    ) {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, tree());
        point_at(&mut workbench, Row::Entry(FOLDER.to_owned()));
        press(&mut workbench, keys::UPLOAD);
        assert!(workbench.receive_transfer_review(
            generation,
            TransferReview {
                executable,
                ..review()
            }
        ));
        let screen = paint(&mut workbench, terminal);
        let digest = format!("{DIGEST_LABEL}{DIGEST}");
        assert_painted(
            &screen,
            &[
                REVIEW_TITLE,
                NEW_EFFECT,
                OVERWRITE_EFFECT,
                MKDIR_EFFECT,
                NEW_SIZE,
                CONTENTS_SIZE,
                NEW_FILE,
                CHANGED_FILE,
                SKIPPED_TITLE,
                STAGE,
                &digest,
            ],
        );
        assert_eq!(screen.contains(NOT_EXECUTABLE), !executable);
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn a_diff_paints_both_sides_the_way_the_editor_does(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        enter(&mut workbench);
        assert!(workbench.receive_transfer_preview(generation, preview(true)));
        let screen = paint(&mut workbench, terminal);
        assert_painted(
            &screen,
            &[
                FILE_NAME,
                TRUNCATED_BADGE,
                DIFF_LEGEND,
                CONTENTS.trim_end(),
                UPDATED.trim_end(),
            ],
        );
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn a_binary_file_paints_each_side_kind_size_and_short_digest(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        enter(&mut workbench);
        assert!(workbench.receive_transfer_preview(generation, preview(false)));
        let screen = paint(&mut workbench, terminal);
        assert_painted(
            &screen,
            &[
                FILE_NAME,
                &LOCAL_NAME.to_uppercase(),
                &SANDBOX_NAME.to_uppercase(),
                BINARY_BADGE,
                CONTENTS_SIZE,
                SHORT_PREVIEW_DIGEST,
            ],
        );
        assert!(!screen.contains(PREVIEW_DIGEST));
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn the_last_transfer_paints_every_path_and_the_recovery_offer(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        let mut ended = outcome(true);
        ended.entries.extend([
            TransferOutcomeEntry {
                path: FAILED_FILE.to_owned(),
                outcome: TransferFileOutcome::Failed,
            },
            TransferOutcomeEntry {
                path: CANCELLED_FILE.to_owned(),
                outcome: TransferFileOutcome::Cancelled,
            },
        ]);
        ended.recovery = Some(pending_recovery());
        assert!(workbench.receive_transfer_outcome(generation, ended));
        let screen = paint(&mut workbench, terminal);
        let stopped = format!("{STOPPED_LABEL}{STOPPED}");
        assert_painted(
            &screen,
            &[
                REPORT_TITLE,
                CHECK_MARK,
                FAILED_MARK,
                CANCELLED_MARK,
                UNKNOWN_MARK,
                FILE_NAME,
                FAILED_FILE,
                CANCELLED_FILE,
                EMPTY_FOLDER,
                &stopped,
                RECOVERY,
                RECOVERY_REQUIRED,
            ],
        );
    }

    #[test]
    fn the_sidebar_says_its_sentences_whole_at_its_default_width() {
        let mut workbench = workbench();
        let generation = compare(
            &mut workbench,
            vec![file(FILE_NAME, TransferStatus::Different)],
        );
        let mut ended = outcome(true);
        ended.recovery = Some(pending_recovery());
        assert!(workbench.receive_transfer_outcome(generation, ended));
        press(&mut workbench, keys::CLOSE);
        let screen = paint(&mut workbench, NARROW);
        assert_painted(
            &screen,
            &[NOTHING_CHOSEN, RECOVERY_REQUIRED, RECONCILE_OFFER],
        );
    }

    #[test]
    fn a_path_too_long_for_the_review_keeps_its_file_name() {
        let mut workbench = workbench();
        let generation = compare(&mut workbench, tree());
        point_at(&mut workbench, Row::Entry(FOLDER.to_owned()));
        press(&mut workbench, keys::UPLOAD);
        let mut long = review();
        long.entries[0].path = DEEP_FILE.to_owned();
        assert!(workbench.receive_transfer_review(generation, long));
        let screen = paint(&mut workbench, NARROW);
        let row = screen
            .lines()
            .find(|row| row.contains(NEW_EFFECT))
            .and_then(|row| row.rsplit(VERTICAL).next())
            .expect(NOT_PAINTED)
            .trim_end();
        assert!(
            row.ends_with(DEEP_NAME) && row.contains(ELLIPSIS),
            "{NAME_LOST}: {row}"
        );
    }

    #[test_case(vec![file(FILE_NAME, TransferStatus::Different)], ONE_PATH_SUMMARY; "one_path")]
    #[test_case(project(), PROJECT_SUMMARY; "many_paths")]
    fn the_status_bar_counts_paths_in_the_number_they_agree_with(
        entries: Vec<TransferEntry>,
        expected: &str,
    ) {
        let mut workbench = workbench();
        compare(&mut workbench, entries);
        assert_painted(&paint(&mut workbench, NARROW), &[expected]);
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn the_banner_names_the_side_whose_scan_stopped_short(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        answer(
            &mut workbench,
            action,
            vec![
                folder(DOT, TransferStatus::Incomplete),
                file(FILE_NAME, TransferStatus::Equal),
            ],
            partial(),
        );
        let screen = paint(&mut workbench, terminal);
        assert_painted(&screen, &[&format!("{SANDBOX_NAME} {SCAN_INCOMPLETE}")]);
    }

    #[test]
    fn a_wide_banner_says_why_what_to_do_and_what_was_left_out() {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        answer(
            &mut workbench,
            action,
            vec![
                folder(DOT, TransferStatus::Incomplete),
                file(FILE_NAME, TransferStatus::Equal),
            ],
            partial(),
        );
        let screen = paint(&mut workbench, WIDE);
        assert_painted(
            &screen,
            &[
                limit_text(TransferScanLimit::Entries),
                SCAN_ACTION,
                &format!("1 {DROPPED_ROW}"),
            ],
        );
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn a_failed_comparison_says_why_in_the_banner(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let WorkbenchAction::Transfer(TransferAction::Compare { generation, .. }) =
            press(&mut workbench, keys::COMPARE)
        else {
            panic!("{NO_COMPARE}");
        };
        assert!(workbench.receive_transfer_outcome(
            generation,
            TransferOutcome {
                stopped: Some(STOPPED.to_owned()),
                ..TransferOutcome::default()
            }
        ));
        let screen = paint(&mut workbench, terminal);
        assert_painted(&screen, &[&format!("{COMPARE_FAILED}{STOPPED}")]);
    }

    #[test]
    fn an_error_takes_the_whole_status_bar() {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        answer(
            &mut workbench,
            action,
            vec![file(FILE_NAME, TransferStatus::Different)],
            TransferScan {
                unsupported: false,
                limits: BTreeSet::from([TransferScanLimit::WorkcellIncomplete]),
            },
        );
        enter(&mut workbench);
        assert_painted(&paint(&mut workbench, NARROW), &[DIFF_TRUNCATED]);
    }

    #[test_case(NARROW; "narrow")]
    #[test_case(WIDE; "wide")]
    fn progress_shows_in_the_status_bar_only_while_its_request_runs(terminal: (u16, u16)) {
        let mut workbench = workbench();
        let action = press(&mut workbench, keys::COMPARE);
        let WorkbenchAction::Transfer(TransferAction::Compare { generation, .. }) = &action else {
            panic!("{NO_COMPARE}");
        };
        assert!(workbench.receive_transfer_progress(
            *generation,
            TransferProgress {
                phase: TransferPhase::Scanning,
                side: Some(TransferSide::Remote),
                path: Some(FOLDER.to_owned()),
                completed: 0,
                total: 0,
            }
        ));
        assert_painted(&paint(&mut workbench, terminal), &[SCANNING_SANDBOX]);
        answer(&mut workbench, action, project(), TransferScan::default());
        let screen = paint(&mut workbench, terminal);
        assert!(!screen.contains(SCANNING_SANDBOX));
        assert_painted(&screen, &[PROJECT_SUMMARY]);
    }

    #[test_case(TransferPhase::Scanning, Some(TransferSide::Remote), Some(FOLDER), 0, 0, None, SCANNING_SANDBOX; "scanning_a_side")]
    #[test_case(TransferPhase::Publishing, None, Some(NEW_FILE), UPLOADED, TO_UPLOAD, Some(TransferDirection::Push), UPLOADING; "uploading")]
    #[test_case(TransferPhase::Reviewing, None, None, 0, 0, None, REVIEWING; "no_detail")]
    fn progress_reads_as_a_sentence(
        phase: TransferPhase,
        side: Option<TransferSide>,
        path: Option<&str>,
        completed: usize,
        total: usize,
        direction: Option<TransferDirection>,
        expected: &str,
    ) {
        let progress = TransferProgress {
            phase,
            side,
            path: path.map(str::to_owned),
            completed,
            total,
        };
        assert_eq!(progress_text(&progress, direction.as_ref()), expected);
    }

    #[test_case(0, "0 B"; "nothing")]
    #[test_case(1_023, "1023 B"; "under_a_kibibyte")]
    #[test_case(1_536, "1.5 KiB"; "kibibytes")]
    #[test_case(u64::MAX, "16777216.0 TiB"; "largest_unit_caps")]
    fn sizes_read_the_way_a_file_manager_prints_them(bytes: u64, expected: &str) {
        assert_eq!(size(bytes), expected);
    }
}
