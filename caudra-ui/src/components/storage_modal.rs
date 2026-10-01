use arc_swap::ArcSwapOption;
use caudra_grab::grab_scope;
use caudra_storage::sessions::SessionStorageStats;
use caudra_storage::sessions::change_stores::StoreSummary;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::keybindings::key;
use crate::components::modal::{
    CHROME_LINES, CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR,
};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    ModalScroll, Overlay, apportion, escape_terminal_controls, format_iec_bytes, format_integer,
    format_usize,
};
use crate::repaint::{Dirty, Watch};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Storage ";
pub(crate) const EXPANDED_TITLE: &str = " Storage - all stores ";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 82;
const H_PAD: u16 = 2;
const H_PAD_STEP_WIDTH: u16 = 16;
const GRID_CELL_COUNT: usize = 100;
const CATEGORY_COUNT: usize = 4;
const PERCENT_TENTHS_SCALE: u64 = 1_000;
const COLLAPSED_STORES: usize = 6;
const LEGEND_GAP: &str = "   ";
const SIZE_WIDTH: usize = 10;
const OBJECTS_WIDTH: usize = 8;
const COUNT_WIDTH: usize = 8;
/// Enough of a store's key to tell the stores apart.
const KEY_WIDTH: usize = 12;
const ORPHANED_STORE: &str = "(no holder has a session)";
const LOADING: &str = "Measuring the state directory…";
const LOADING_HINT: &str = "Change record stores are read on disk, so this takes a moment.";
const NO_STORES: &str = "No file changes have been recorded.";
/// The footer's targets in the order [`footer`] lays them out: the view
/// switch, then the close control.
const SWITCH_TARGET: usize = 0;
const CLOSE_TARGET: usize = 1;

/// What the background measurement produced. Held in a slot rather than
/// computed in `view` because reading the change record stores walks the
/// state directory, which no frame can afford.
pub enum StorageFetchState {
    Loading,
    Ready(Box<StorageReport>),
    Error(String),
}

pub struct StorageReport {
    pub stats: SessionStorageStats,
    pub stores: Vec<StoreSummary>,
    /// Why this session's file changes are not recorded, when that is a
    /// verdict rather than an absence. Without it an empty list reads as
    /// "nothing recorded yet".
    pub unavailable: Option<String>,
}

pub struct StorageModal {
    open: bool,
    expanded: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
    footer: FooterHits,
    report: Watch<StorageFetchState>,
}

impl StorageModal {
    pub fn new() -> Self {
        Self {
            open: false,
            expanded: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            popup: Rect::default(),
            footer: FooterHits::default(),
            report: Watch::default(),
        }
    }

    pub fn open(&mut self, expanded: bool) {
        self.open = true;
        self.expanded = expanded;
        self.scroll.reset();
        self.footer.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.footer.reset();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Picks up a finished measurement. Nothing wakes the loop when the task
    /// stores its result, so an unpolled modal sits on `Loading`.
    pub fn poll(&mut self, slot: &ArcSwapOption<StorageFetchState>) -> Dirty {
        if !self.open {
            return Dirty::NO;
        }
        self.report.poll(slot.load_full())
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if matches!(key_event.code, KeyCode::Esc | KeyCode::Char('q'))
            || key::QUIT.matches(key_event)
        {
            self.close();
        } else {
            self.scroll.handle_key(key_event);
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// Toggling through the footer keeps the measurement: only the store list
    /// grows, and re-walking the state directory to show rows already in hand
    /// would stall the very frame the click asked for.
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return;
            }
        }
        match self.footer.handle_mouse(event) {
            Some(SWITCH_TARGET) => self.open(!self.expanded),
            Some(CLOSE_TARGET) => self.close(),
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self) -> Rect {
        self.footer.hit(SWITCH_TARGET)
    }

    #[cfg(test)]
    pub(crate) fn close_hit(&self) -> Rect {
        self.footer.hit(CLOSE_TARGET)
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("storage_modal", area);

        let content_width = content_width(area);
        let theme = theme::current();
        let report = self.report.held();
        let report = report.as_deref();
        let mut lines = build_lines(report, self.expanded, content_width, &theme);
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let visual_height = paragraph.line_count(content_width.max(1));
        let total = u16::try_from(visual_height)
            .unwrap_or(u16::MAX)
            .min(u16::MAX.saturating_sub(CHROME_LINES));
        let modal = Modal {
            title: if self.expanded { EXPANDED_TITLE } else { TITLE },
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let horizontal_padding = horizontal_padding(inner.width);
        let padded = Rect {
            x: inner.x.saturating_add(horizontal_padding),
            width: inner
                .width
                .saturating_sub(horizontal_padding.saturating_mul(2)),
            ..inner
        };
        self.scroll.update_dimensions(total, padded.height);
        let offset = self.scroll.offset();
        let footer = footer(self.expanded, &theme);
        self.footer.set(footer.hits(padded, offset, total));
        if let Some(index) = self.footer.hovered()
            && let Some(last) = lines.last_mut()
        {
            *last = footer.line(Some(index));
        }

        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().fg(theme.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        self.scrollbar.draw(frame, inner, total, offset);

        self.popup = popup;
        popup
    }
}

impl Default for StorageModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for StorageModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

/// What the state directory spends its bytes on. The split exists because the
/// two costs have entirely different remedies: the database shrinks by trimming
/// sessions, the change stores by releasing the sessions that hold records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Consumer {
    Database,
    ToolOutputs,
    ChangeRecords,
    Archives,
}

impl Consumer {
    fn glyph(self) -> &'static str {
        match self {
            Self::Database => "D",
            Self::ToolOutputs => "O",
            Self::ChangeRecords => "C",
            Self::Archives => "A",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Database => "Database",
            Self::ToolOutputs => "Tool outputs",
            Self::ChangeRecords => "File change records",
            Self::Archives => "Archives",
        }
    }

    fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Database => theme.heading,
            Self::ToolOutputs => theme.tool,
            Self::ChangeRecords => theme.accent,
            Self::Archives => theme.todo_pending,
        }
    }
}

fn consumers(stats: &SessionStorageStats) -> [(Consumer, u64); CATEGORY_COUNT] {
    [
        (Consumer::Database, database_bytes(stats)),
        (Consumer::ToolOutputs, stats.tool_output_file_bytes),
        (Consumer::ChangeRecords, stats.snapshot_bytes),
        (Consumer::Archives, stats.archive_bytes),
    ]
}

/// The database is its file plus the sidecars it cannot be read without, so a
/// large WAL is charged to the database rather than quietly missing.
fn database_bytes(stats: &SessionStorageStats) -> u64 {
    stats
        .database_bytes
        .saturating_add(stats.wal_bytes)
        .saturating_add(stats.shm_bytes)
}

fn total_bytes(stats: &SessionStorageStats) -> u64 {
    consumers(stats)
        .iter()
        .fold(0, |total, (_, bytes)| total.saturating_add(*bytes))
}

fn build_lines(
    report: Option<&StorageFetchState>,
    expanded: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = match report {
        Some(StorageFetchState::Ready(report)) => report_lines(report, expanded, width, theme),
        Some(StorageFetchState::Error(message)) => vec![Line::from(Span::styled(
            escape_terminal_controls(message),
            theme.error,
        ))],
        Some(StorageFetchState::Loading) | None => vec![
            Line::from(Span::styled(LOADING, theme.status_dim)),
            Line::from(Span::styled(LOADING_HINT, theme.tool_dim)),
        ],
    };
    lines.push(Line::default());
    lines.push(footer(expanded, theme).line(None));
    lines
}

fn report_lines(
    report: &StorageReport,
    expanded: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = summary_lines(&report.stats, width, theme);
    lines.extend(database_lines(&report.stats, theme));
    lines.extend(store_lines(&report.stores, expanded, theme));
    lines.extend(
        report
            .unavailable
            .as_deref()
            .map(|reason| Line::from(Span::styled(reason.to_owned(), theme.status_notice))),
    );
    lines
}

fn summary_lines(stats: &SessionStorageStats, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let total = total_bytes(stats);
    let mut lines = vec![
        labeled_line("On disk", format_iec_bytes(total), theme),
        Line::default(),
    ];
    lines.extend(grid_lines(stats, width, theme));
    lines.push(Line::default());
    lines.extend(legend_lines(stats, width, theme));
    lines.push(Line::default());
    lines
}

fn database_lines(stats: &SessionStorageStats, theme: &Theme) -> Vec<Line<'static>> {
    let reclaimable = stats.freelist_count.saturating_mul(stats.page_size);
    vec![
        section_line("Database", theme),
        labeled_line(
            "Files",
            format!(
                "{} · wal {} · shm {}",
                format_iec_bytes(stats.database_bytes),
                format_iec_bytes(stats.wal_bytes),
                format_iec_bytes(stats.shm_bytes)
            ),
            theme,
        ),
        labeled_line(
            "Pages",
            format!(
                "{} × {} · {} free ({} reclaimable)",
                format_integer(stats.page_count),
                format_iec_bytes(stats.page_size),
                format_integer(stats.freelist_count),
                format_iec_bytes(reclaimable)
            ),
            theme,
        ),
        labeled_line(
            "Sessions",
            format!(
                "{} · {} pinned · {} trimmed",
                format_integer(stats.session_count),
                format_integer(stats.pinned_count),
                format_integer(stats.trimmed_count)
            ),
            theme,
        ),
        labeled_line(
            "Items",
            format!(
                "{} history · {} tool outputs · {} subagent",
                format_integer(stats.history_item_count),
                format_integer(stats.tool_output_count),
                format_integer(stats.subagent_item_count)
            ),
            theme,
        ),
        labeled_line(
            "Content",
            format!(
                "{} logical · {} cleanup jobs pending",
                format_iec_bytes(stats.logical_bytes),
                format_integer(stats.pending_cleanup_jobs)
            ),
            theme,
        ),
        Line::default(),
    ]
}

fn store_lines(stores: &[StoreSummary], expanded: bool, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![section_line(Consumer::ChangeRecords.label(), theme)];
    if stores.is_empty() {
        lines.push(Line::from(Span::styled(NO_STORES, theme.tool_dim)));
        return lines;
    }
    lines.push(store_header(theme));
    let shown = if expanded {
        stores.len()
    } else {
        stores.len().min(COLLAPSED_STORES)
    };
    lines.extend(stores[..shown].iter().map(|entry| store_row(entry, theme)));
    if let Some(hidden) = stores.len().checked_sub(shown).filter(|count| *count > 0) {
        let hidden_bytes = stores[shown..].iter().fold(0_u64, |total, store| {
            total.saturating_add(store.usage.bytes)
        });
        lines.push(Line::from(Span::styled(
            format!(
                "{} more {} holding {}",
                format_usize(hidden),
                if hidden == 1 { "store" } else { "stores" },
                format_iec_bytes(hidden_bytes)
            ),
            theme.tool_dim,
        )));
    }
    lines
}

fn store_header(theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{:>SIZE_WIDTH$} {:>OBJECTS_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} \
             {:>COUNT_WIDTH$} {:>COUNT_WIDTH$}  Workspace",
            "Size", "Objects", "Records", "Holders", "Open", "Pending"
        ),
        theme.tool_dim,
    ))
}

fn store_row(store: &StoreSummary, theme: &Theme) -> Line<'static> {
    let usage = &store.usage;
    let name = store.workspace.as_ref().map_or_else(
        || store.key.chars().take(KEY_WIDTH).collect(),
        |root| root.display().to_string(),
    );
    let mut spans = vec![
        Span::raw(format!(
            "{:>SIZE_WIDTH$} {:>OBJECTS_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} \
             {:>COUNT_WIDTH$} {:>COUNT_WIDTH$}  ",
            format_iec_bytes(usage.bytes),
            format_integer(usage.objects),
            format_integer(u64::from(usage.records)),
            format_usize(store.holders.len()),
            format_integer(u64::from(usage.open_records)),
            format_integer(u64::from(usage.pending_reverts)),
        )),
        Span::styled(escape_terminal_controls(&name), theme.tool_path),
    ];
    if store.orphaned {
        spans.push(Span::styled(format!(" {ORPHANED_STORE}"), theme.error));
    }
    Line::from(spans)
}

fn labeled_line(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), theme.tool_dim),
        Span::raw(value),
    ])
}

fn section_line(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_owned(), theme.keybind_section))
}

fn footer_command(expanded: bool) -> &'static str {
    if expanded { "/storage" } else { "/storage all" }
}

fn footer(expanded: bool, theme: &Theme) -> FooterLine {
    let mut footer = FooterLine::default();
    footer.command(footer_command(expanded), theme.keybind_key);
    footer.describe(
        if expanded {
            " largest stores"
        } else {
            " every store"
        },
        theme.tool_dim,
    );
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(ESC_LABEL, theme.keybind_key);
    footer.describe(CLOSE_HINT, theme.tool_dim);
    footer
}

fn grid_cells(stats: &SessionStorageStats) -> Vec<Consumer> {
    let categories = consumers(stats);
    let weights = categories.map(|(_, bytes)| bytes);
    let counts = apportion(&weights, GRID_CELL_COUNT);
    categories
        .iter()
        .zip(counts)
        .flat_map(|((kind, _), count)| std::iter::repeat_n(*kind, count))
        .collect()
}

fn grid_lines(stats: &SessionStorageStats, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let row_width = usize::from(width).min(GRID_CELL_COUNT);
    grid_cells(stats)
        .chunks(row_width)
        .map(|row| {
            let mut spans = Vec::new();
            let mut start = 0;
            while start < row.len() {
                let kind = row[start];
                let mut end = start + 1;
                while end < row.len() && row[end] == kind {
                    end += 1;
                }
                spans.push(Span::styled(
                    kind.glyph().repeat(end - start),
                    kind.style(theme),
                ));
                start = end;
            }
            Line::from(spans)
        })
        .collect()
}

fn legend_lines(stats: &SessionStorageStats, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let total = total_bytes(stats);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0_usize;
    for (kind, bytes) in consumers(stats) {
        let entry = legend_entry(kind, bytes, total, theme);
        let entry_width = entry.iter().map(|span| span.content.len()).sum::<usize>();
        let separator = if current.is_empty() {
            0
        } else {
            LEGEND_GAP.len()
        };
        if !current.is_empty() && used + separator + entry_width > usize::from(width) {
            lines.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        } else if separator > 0 {
            current.push(Span::styled(LEGEND_GAP, theme.tool_dim));
            used += separator;
        }
        used += entry_width;
        current.extend(entry);
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

fn legend_entry(kind: Consumer, bytes: u64, total: u64, theme: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(kind.glyph(), kind.style(theme)),
        Span::styled(format!(" {} ", kind.label()), theme.tool_dim),
        Span::raw(format_iec_bytes(bytes)),
        Span::styled(
            format!(" ({})", format_percentage(bytes, total)),
            theme.tool_dim,
        ),
    ]
}

fn content_width(area: Rect) -> u16 {
    let inner_width = Modal::inner_width(area.width, WIDTH_PERCENT);
    let horizontal_padding = horizontal_padding(inner_width);
    inner_width.saturating_sub(horizontal_padding.saturating_mul(2))
}

fn horizontal_padding(width: u16) -> u16 {
    (width / H_PAD_STEP_WIDTH).min(H_PAD)
}

fn format_percentage(bytes: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".to_owned();
    }
    let tenths = bytes.saturating_mul(PERCENT_TENTHS_SCALE) / total;
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_storage::sessions::change_stores::StoreUsage;
    use caudra_workspace::{HolderSummary, RecordHolder};
    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    use super::*;

    const DATABASE_BYTES: u64 = 40 * 1024 * 1024;
    const WAL_BYTES: u64 = 4 * 1024 * 1024;
    const SHM_BYTES: u64 = 32 * 1024;
    const PAGE_SIZE: u64 = 4_096;
    const CHANGE_RECORD_BYTES: u64 = 7 * 1024 * 1024 * 1024;
    const TOOL_OUTPUT_BYTES: u64 = 128 * 1024 * 1024;
    const ARCHIVE_BYTES: u64 = 512 * 1024;
    const WORKFLOW_RUN_COUNT: u64 = 24;
    const WORKFLOW_CALL_COUNT: u64 = 960;
    const WORKFLOW_BYTES: u64 = 3 * 1024 * 1024;
    const BIG_STORE: &str = "atlas-key";
    const SMALL_STORE: &str = "small-key";
    const ORPHAN_STORE: &str = "orphan-key";
    const SESSION_ID: &str = "store-session";
    const OBJECTS: u64 = 444;
    const RECORDS: u32 = 111;
    const OPEN_RECORDS: u32 = 222;
    const PENDING_REVERTS: u32 = 333;
    const MISSING_TOTAL: &str = "grid must spend every cell";
    const MISSING_ORPHAN: &str = "an orphaned store must be named, not hidden";
    const MISSING_HIDDEN: &str = "a collapsed list must account for what it hid";
    const MISSING_CHANGE_RECORDS: &str = "the change record total must be visible";
    const MISSING_COUNT: &str = "a store's row must show every count it keeps";
    const MISSING_NAME: &str = "a store is named by its workspace directory, else by its key";
    const WORKSPACE_ROOT: &str = "/home/user/atlas";
    const HOVER_MISSED: &str = "the footer command must reverse under the pointer";
    const BAR_IGNORED: &str = "a press on the bar's column must scroll the body";

    fn stats() -> SessionStorageStats {
        SessionStorageStats {
            database_bytes: DATABASE_BYTES,
            wal_bytes: WAL_BYTES,
            shm_bytes: SHM_BYTES,
            page_size: PAGE_SIZE,
            page_count: 10_240,
            freelist_count: 512,
            auto_vacuum: 0,
            schema_version: 7,
            session_count: 128,
            pinned_count: 3,
            trimmed_count: 12,
            history_item_count: 40_213,
            tool_output_count: 5_120,
            subagent_item_count: 812,
            logical_bytes: 212 * 1024 * 1024,
            workflow_run_count: WORKFLOW_RUN_COUNT,
            workflow_call_count: WORKFLOW_CALL_COUNT,
            workflow_bytes: WORKFLOW_BYTES,
            background_invocation_count: 0,
            background_bytes: 0,
            tool_output_file_bytes: TOOL_OUTPUT_BYTES,
            snapshot_bytes: CHANGE_RECORD_BYTES,
            archive_bytes: ARCHIVE_BYTES,
            pending_cleanup_jobs: 0,
        }
    }

    fn store(key: &str, bytes: u64, orphaned: bool) -> StoreSummary {
        StoreSummary {
            key: key.to_owned(),
            workspace: None,
            usage: StoreUsage {
                bytes,
                objects: OBJECTS,
                records: RECORDS,
                open_records: OPEN_RECORDS,
                pending_reverts: PENDING_REVERTS,
            },
            holders: vec![HolderSummary {
                holder: RecordHolder::new(SESSION_ID).unwrap(),
                records: RECORDS,
                open_records: OPEN_RECORDS,
                pending_reverts: PENDING_REVERTS,
            }],
            orphaned,
        }
    }

    fn stores(count: usize) -> Vec<StoreSummary> {
        (0..count)
            .map(|index| {
                store(
                    &format!("store-{index}"),
                    u64::try_from(count - index).unwrap_or(1) * 1024,
                    false,
                )
            })
            .collect()
    }

    fn report(stores: Vec<StoreSummary>) -> StorageFetchState {
        report_with(stores, None)
    }

    fn report_with(stores: Vec<StoreSummary>, unavailable: Option<String>) -> StorageFetchState {
        StorageFetchState::Ready(Box::new(StorageReport {
            stats: stats(),
            stores,
            unavailable,
        }))
    }

    fn text(lines: &[Line<'static>]) -> String {
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

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test_case(1, 100 ; "one_column")]
    #[test_case(7, 15 ; "narrow_wrapping")]
    #[test_case(100, 1 ; "full_grid_row")]
    fn the_grid_wraps_without_losing_cells(width: u16, expected_rows: usize) {
        let lines = grid_lines(&stats(), width, &theme::current());
        assert_eq!(lines.len(), expected_rows);
        assert_eq!(
            lines.iter().map(Line::width).sum::<usize>(),
            GRID_CELL_COUNT,
            "{MISSING_TOTAL}"
        );
    }

    /// The bar is proportional, so a consumer under half a percent draws
    /// nothing. The legend is what keeps it from disappearing: every consumer
    /// is named with its exact bytes whether or not it earned a cell.
    #[test]
    fn a_consumer_too_small_to_draw_is_still_named() {
        let stats = stats();
        assert!(!grid_cells(&stats).contains(&Consumer::Archives));
        let legend = text(&legend_lines(&stats, 200, &theme::current()));
        for (kind, _) in consumers(&stats) {
            assert!(legend.contains(kind.label()), "{MISSING_TOTAL}");
        }
        assert!(legend.contains("512.0 KiB"), "{MISSING_TOTAL}");
    }

    #[test]
    fn the_database_is_charged_for_its_sidecars() {
        assert_eq!(
            database_bytes(&stats()),
            DATABASE_BYTES + WAL_BYTES + SHM_BYTES
        );
    }

    #[test]
    fn a_collapsed_list_accounts_for_the_stores_it_hid() {
        let theme = theme::current();
        let all = stores(COLLAPSED_STORES + 3);
        let collapsed = text(&store_lines(&all, false, &theme));
        let expanded = text(&store_lines(&all, true, &theme));

        assert!(collapsed.contains("3 more stores"), "{MISSING_HIDDEN}");
        assert!(!expanded.contains("more stores"), "{MISSING_HIDDEN}");
        assert!(expanded.contains("store-8"), "{MISSING_HIDDEN}");
        assert!(!collapsed.contains("store-8"), "{MISSING_HIDDEN}");
    }

    #[test]
    fn an_orphaned_store_is_named_not_hidden() {
        let entries = vec![
            store(BIG_STORE, 4096, false),
            store(ORPHAN_STORE, 2048, true),
            store(SMALL_STORE, 1024, false),
        ];
        let rendered = text(&store_lines(&entries, false, &theme::current()));
        assert!(rendered.contains(ORPHANED_STORE), "{MISSING_ORPHAN}");
        assert!(rendered.contains(SMALL_STORE), "{MISSING_ORPHAN}");
    }

    /// What fills a store and what still depends on it: records, the
    /// sessions holding them, and the work in flight against them.
    #[test]
    fn a_store_row_shows_every_count_the_store_keeps() {
        let row = text(&[store_row(&store(BIG_STORE, 4096, false), &theme::current())]);
        for count in [
            OBJECTS,
            u64::from(RECORDS),
            u64::from(OPEN_RECORDS),
            u64::from(PENDING_REVERTS),
        ] {
            assert!(row.contains(&count.to_string()), "{MISSING_COUNT}: {row}");
        }
        assert!(row.contains(BIG_STORE), "{MISSING_COUNT}: {row}");
    }

    #[test_case(Some(WORKSPACE_ROOT), WORKSPACE_ROOT ; "a_known_workspace_by_its_directory")]
    #[test_case(None, BIG_STORE ; "an_unknown_one_by_its_key")]
    fn a_store_row_names_its_workspace_or_else_its_key(workspace: Option<&str>, named: &str) {
        let row = text(&[store_row(
            &StoreSummary {
                workspace: workspace.map(PathBuf::from),
                ..store(BIG_STORE, 4096, false)
            },
            &theme::current(),
        )]);
        assert!(row.trim_end().ends_with(named), "{MISSING_NAME}: {row}");
    }

    #[test]
    fn an_empty_store_list_says_so_instead_of_printing_a_header() {
        let rendered = text(&store_lines(&[], false, &theme::current()));
        assert!(rendered.contains(NO_STORES));
        assert!(!rendered.contains("Objects"));
    }

    /// An empty list reads as "nothing recorded yet", which is the wrong
    /// story when the session's file changes cannot be recorded.
    #[test]
    fn an_unrecorded_session_says_why_it_has_no_store() {
        const REASON: &str = "file change recording is turned off";
        const MISSING_REASON: &str = "an unrecorded session must explain its empty store list";
        const WIDTH: u16 = 100;
        let state = report_with(Vec::new(), Some(REASON.to_owned()));
        let rendered = text(&build_lines(Some(&state), false, WIDTH, &theme::current()));
        assert!(rendered.contains(REASON), "{MISSING_REASON}");
    }

    #[test]
    fn the_summary_names_both_halves_of_the_cost() {
        const WIDTH: u16 = 100;
        let state = report(stores(2));
        let rendered = text(&build_lines(Some(&state), false, WIDTH, &theme::current()));
        assert!(rendered.contains("7.0 GiB"), "{MISSING_CHANGE_RECORDS}");
        assert!(rendered.contains("44.0 MiB"), "{MISSING_CHANGE_RECORDS}");
        assert!(rendered.contains(Consumer::Database.label()));
        assert!(rendered.contains(Consumer::ChangeRecords.label()));
    }

    #[test]
    fn a_pending_measurement_says_it_is_working() {
        let rendered = text(&build_lines(None, false, 80, &theme::current()));
        assert!(rendered.contains(LOADING));
        assert!(rendered.contains(footer_command(false)));
    }

    #[test]
    fn a_failed_measurement_shows_the_reason() {
        const REASON: &str = "session database is missing";
        let state = StorageFetchState::Error(REASON.to_owned());
        let rendered = text(&build_lines(Some(&state), false, 80, &theme::current()));
        assert!(rendered.contains(REASON));
    }

    #[test]
    fn the_footer_command_hovers_and_switches_views() {
        const WIDTH: u16 = 120;
        const HEIGHT: u16 = 50;

        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut modal = StorageModal::new();
        let slot = ArcSwapOption::from(Some(Arc::new(report(stores(COLLAPSED_STORES + 3)))));
        modal.open(false);
        let _ = modal.poll(&slot);
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();

        let hit = modal.footer_hit();
        assert!(!hit.is_empty());
        modal.handle_mouse(mouse(MouseEventKind::Moved, hit));
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        let reversed = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| Position::new(x, y)))
            .filter(|position| {
                terminal.backend().buffer()[(position.x, position.y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect::<Vec<_>>();
        assert!(
            reversed.len() == usize::from(hit.width)
                && reversed.iter().all(|position| hit.contains(*position)),
            "{HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit));
        assert!(modal.is_expanded());

        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        let close_hit = modal.close_hit();
        assert_eq!(
            usize::from(close_hit.width),
            ESC_LABEL.len() + CLOSE_HINT.len(),
            "the close control spans the key and its gloss"
        );
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), close_hit));
        modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), close_hit));
        assert!(!modal.is_open());
    }

    #[test]
    fn a_press_on_the_bar_scrolls_the_body() {
        const WIDTH: u16 = 120;
        const HEIGHT: u16 = 20;

        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut modal = StorageModal::new();
        let slot = ArcSwapOption::from(Some(Arc::new(report(stores(COLLAPSED_STORES + 3)))));
        modal.open(true);
        let _ = modal.poll(&slot);
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        assert_eq!(modal.scroll.offset(), 0);

        // The last row of a track is the end of the document by definition, so
        // the press lands there wherever rounding painted the thumb.
        let bar = Rect::new(modal.popup.right() - 2, modal.popup.bottom() - 2, 1, 1);
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), bar));

        assert!(modal.scroll.offset() > 0, "{BAR_IGNORED}");
    }

    /// The click that expands must not throw away the measurement: re-walking
    /// the state directory to show rows already in hand would stall the frame.
    #[test]
    fn expanding_through_the_footer_keeps_the_measurement() {
        let mut modal = StorageModal::new();
        let slot = ArcSwapOption::from(Some(Arc::new(report(stores(1)))));
        modal.open(false);
        let _ = modal.poll(&slot);
        modal.open(true);
        assert!(matches!(
            modal.report.get(),
            Some(StorageFetchState::Ready(_))
        ));
    }

    #[test]
    fn a_hidden_modal_does_not_poll() {
        let mut modal = StorageModal::new();
        let slot = ArcSwapOption::from(Some(Arc::new(report(stores(1)))));
        assert_eq!(modal.poll(&slot), Dirty::NO);
        modal.open(false);
        assert_eq!(modal.poll(&slot), Dirty::YES);
    }
}
