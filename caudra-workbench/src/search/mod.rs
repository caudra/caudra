//! The Search pane: two fields, three toggles, and a two-level result list.
//!
//! Unlike source control, this one is genuinely slow, so the walk runs on a
//! worker and the pane grows as answers arrive. Everything here is the part
//! that can be reasoned about without a thread: what was typed, what came back,
//! and where the cursor is in it.

pub mod engine;

use std::path::Path;

use crate::editor::text_field::{FieldKind, TextField};
use crate::fs::backend::{RequestId, ResourceEntry, SearchResult, WorkbenchPath};
use engine::{Event, Hit, Query, Run};

/// Which of the pane's two fields the caret is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field {
    #[default]
    Query,
    Include,
}

/// A row of the result list. Both kinds are selectable: a file row opens the
/// file, a hit row opens it at the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    File(usize),
    Hit(usize),
}

pub struct Search {
    text: TextField,
    include: TextField,
    field: Field,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    /// Whether the last run walked hidden files. Only a run takes the host's
    /// current answer, so turning them on marks nothing stale by itself.
    hidden: bool,
    files: Vec<WorkbenchPath>,
    hits: Vec<Hit>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    follow_selection: bool,
    run: Option<Run>,
    truncated: bool,
    error: Option<String>,
    /// The query the listed results came from. Enter runs the search while this
    /// disagrees with the fields, and opens the selection once it agrees.
    ran: Option<Query>,
    remote_request: Option<RequestId>,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            text: TextField::new(FieldKind::Line),
            include: TextField::new(FieldKind::Line),
            field: Field::default(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            hidden: false,
            files: Vec::new(),
            hits: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            follow_selection: false,
            run: None,
            truncated: false,
            error: None,
            ran: None,
            remote_request: None,
        }
    }
}

impl Search {
    pub fn field(&self) -> Field {
        self.field
    }

    pub fn input(&self, field: Field) -> &TextField {
        match field {
            Field::Query => &self.text,
            Field::Include => &self.include,
        }
    }

    pub fn input_mut(&mut self, field: Field) -> &mut TextField {
        match field {
            Field::Query => &mut self.text,
            Field::Include => &mut self.include,
        }
    }

    /// What the fields and the toggles ask for, in the shape a run takes.
    pub fn query(&self) -> Query {
        Query {
            text: self.text.text(),
            include: self.include.text(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            hidden: self.hidden,
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn file(&self, index: usize) -> Option<&WorkbenchPath> {
        self.files.get(index)
    }

    pub fn hit(&self, index: usize) -> Option<&Hit> {
        self.hits.get(index)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some() || self.remote_request.is_some()
    }

    pub fn regex(&self) -> bool {
        self.regex
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    pub fn whole_word(&self) -> bool {
        self.whole_word
    }

    /// Whether the fields have moved on from the results below them. Asked
    /// on every frame, so it compares in place instead of building a query.
    pub fn is_stale(&self) -> bool {
        let Some(Query {
            text,
            include,
            regex,
            case_sensitive,
            whole_word,
            hidden,
        }) = &self.ran
        else {
            return true;
        };
        !self.text.holds(text)
            || !self.include.holds(include)
            || (*regex, *case_sensitive, *whole_word, *hidden)
                != (
                    self.regex,
                    self.case_sensitive,
                    self.whole_word,
                    self.hidden,
                )
    }

    pub fn has_results(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn next_field(&mut self) {
        self.field = match self.field {
            Field::Query => Field::Include,
            Field::Include => Field::Query,
        };
    }

    pub fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
    }

    pub fn toggle_word(&mut self) {
        self.whole_word = !self.whole_word;
    }

    pub fn toggle_regex(&mut self) {
        self.regex = !self.regex;
    }

    /// Starts a fresh walk, cancelling whatever the last one was still doing.
    /// An empty query clears the pane instead, so deleting the text does not
    /// leave stale results sitting under it.
    pub fn start(&mut self, root: &Path, hidden: bool) {
        self.hidden = hidden;
        self.clear();
        let query = self.query();
        self.ran = Some(query.clone());
        if query.text.is_empty() {
            return;
        }
        match Run::start(root, &query) {
            Ok(run) => self.run = Some(run),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn prepare_remote(&mut self) -> Option<(String, Option<String>)> {
        self.clear();
        let query = self.query();
        self.ran = Some(query.clone());
        if query.text.is_empty() {
            return None;
        }
        let include = (!query.include.trim().is_empty()).then_some(query.include);
        Some((query.text, include))
    }

    pub fn begin_remote(&mut self, request: RequestId) {
        self.remote_request = Some(request);
    }

    pub fn apply_remote(&mut self, request: RequestId, result: SearchResult) {
        if self.remote_request != Some(request) {
            return;
        }
        self.remote_request = None;
        self.truncated = result.truncated || result.incomplete;
        for hit in result.hits {
            self.push(Hit {
                path: hit.entry.path.clone(),
                resource: Some(hit.entry),
                line: u64::from(hit.line),
                text: hit.text,
                range: (0, 0),
            });
        }
    }

    pub fn fail_remote(&mut self, request: RequestId, error: String) {
        if self.remote_request == Some(request) {
            self.remote_request = None;
            self.error = Some(error);
        }
    }

    pub fn cancel_remote(&mut self) -> Option<RequestId> {
        self.remote_request.take()
    }

    /// Takes whatever the worker produced. Reports whether the pane changed, so
    /// the host only repaints when there is something new to see.
    pub fn tick(&mut self) -> bool {
        let Some(run) = self.run.as_mut() else {
            return false;
        };
        let events = run.drain();
        let finished = !run.is_running();
        if events.is_empty() && !finished {
            return false;
        }
        for event in events {
            match event {
                Event::Hit(hit) => self.push(hit),
                Event::Truncated => self.truncated = true,
                Event::Done => {}
            }
        }
        if finished {
            self.run = None;
        }
        true
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.follow_selection = true;
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.follow_selection = true;
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.follow_selection = true;
        self.selected = self.rows.len().saturating_sub(1);
    }

    pub fn select_index(&mut self, index: usize) {
        if index < self.rows.len() {
            self.selected = index;
            self.follow_selection = true;
        }
    }

    pub fn clamp_scroll(&mut self, viewport: usize) {
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.min(max);
        if viewport == 0 {
            return;
        }
        if self.follow_selection {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected.saturating_sub(self.scroll) >= viewport {
                self.scroll = self.selected + 1 - viewport;
            }
            self.scroll = self.scroll.min(max);
            self.follow_selection = false;
        }
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        self.follow_selection = false;
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn set_scroll(&mut self, top: usize, viewport: usize) {
        self.follow_selection = false;
        self.scroll = top.min(self.rows.len().saturating_sub(viewport));
    }

    /// Where the selected row points: a file, and the line to land on.
    pub fn selection(&self) -> Option<(WorkbenchPath, usize)> {
        match self.rows.get(self.selected)? {
            Row::File(index) => Some((self.files.get(*index)?.clone(), 1)),
            Row::Hit(index) => {
                let hit = self.hits.get(*index)?;
                Some((hit.path.clone(), usize::try_from(hit.line).unwrap_or(1)))
            }
        }
    }

    pub fn selected_resource(&self) -> Option<ResourceEntry> {
        let path = match self.rows.get(self.selected)? {
            Row::File(index) => self.files.get(*index)?,
            Row::Hit(index) => return self.hits.get(*index)?.resource.clone(),
        };
        self.hits
            .iter()
            .find(|hit| &hit.path == path)
            .and_then(|hit| hit.resource.clone())
    }

    pub fn counts(&self) -> (usize, usize) {
        (self.hits.len(), self.files.len())
    }

    fn clear(&mut self) {
        self.run = None;
        self.remote_request = None;
        self.files.clear();
        self.hits.clear();
        self.rows.clear();
        self.selected = 0;
        self.scroll = 0;
        self.follow_selection = false;
        self.truncated = false;
        self.error = None;
    }

    /// Hits arrive in walk order, so a change of path is a new group. Comparing
    /// against the last file alone keeps this O(1) per hit.
    fn push(&mut self, hit: Hit) {
        if self.files.last() != Some(&hit.path) {
            self.files.push(hit.path.clone());
            self.rows.push(Row::File(self.files.len() - 1));
        }
        self.hits.push(hit);
        self.rows.push(Row::Hit(self.hits.len() - 1));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use test_case::test_case;

    use super::{Field, Hit, Row, Search};

    const GROUPING_WRONG: &str = "hits must be grouped under one heading per file";
    const SELECTION_WRONG: &str = "the cursor is not on the row the test put it on";
    const STALE_WRONG: &str = "the pane disagrees about whether its results are current";
    const SCROLL_HITS: u64 = 20;
    const SCROLL_VIEWPORT: usize = 5;
    const MANUAL_SCROLL: usize = 8;
    const SCROLL_PATH: &str = "/scroll.rs";
    const SCROLL_WRONG: &str = "manual scrolling must persist until explicit selection navigation";
    const FOLLOW_WRONG: &str = "navigation must reveal selection once a viewport is available";

    fn type_query(search: &mut Search, text: &str) {
        search.input_mut(Field::Query).insert_text(text);
    }

    fn hit(path: &str, line: u64) -> Hit {
        Hit {
            path: PathBuf::from(path).into(),
            resource: None,
            line,
            text: format!("line {line}"),
            range: (0, 4),
        }
    }

    fn pane(hits: Vec<Hit>) -> Search {
        let mut search = Search::default();
        for hit in hits {
            search.push(hit);
        }
        search
    }

    fn scrolling_pane() -> Search {
        pane(
            (1..=SCROLL_HITS)
                .map(|line| hit(SCROLL_PATH, line))
                .collect(),
        )
    }

    #[test_case(false, SCROLL_VIEWPORT; "wheel")]
    #[test_case(true, SCROLL_VIEWPORT; "bar")]
    #[test_case(false, 0; "wheel_without_viewport")]
    #[test_case(true, 0; "bar_without_viewport")]
    fn manual_scroll_survives_redraws(bar: bool, viewport: usize) {
        let mut search = scrolling_pane();
        search.select_first();
        if bar {
            search.set_scroll(MANUAL_SCROLL, viewport);
        } else {
            search.scroll_by(MANUAL_SCROLL as isize, viewport);
        }
        search.clamp_scroll(viewport);
        search.clamp_scroll(SCROLL_VIEWPORT);
        search.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(search.scroll(), MANUAL_SCROLL, "{SCROLL_WRONG}");
        assert_eq!(search.selected_index(), 0, "{SELECTION_WRONG}");
    }

    #[test_case(Search::select_first, false; "home")]
    #[test_case(Search::select_last, true; "end")]
    #[test_case(|search: &mut Search| search.move_selection(-1), false; "up_at_start")]
    #[test_case(|search: &mut Search| search.move_selection(1), true; "down_at_end")]
    #[test_case(|search: &mut Search| search.select_index(search.selected_index()), false; "same_index")]
    fn boundary_navigation_follows_once(action: fn(&mut Search), last: bool) {
        let mut search = scrolling_pane();
        if last {
            search.select_last();
        }
        search.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        action(&mut search);
        search.clamp_scroll(SCROLL_VIEWPORT);
        let expected = if last {
            search.rows().len() - SCROLL_VIEWPORT
        } else {
            0
        };
        assert_eq!(search.scroll(), expected, "{FOLLOW_WRONG}");
        search.clamp_scroll(search.rows().len());
        search.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(search.scroll(), 0, "{SCROLL_WRONG}");
    }

    #[test_case(1; "single_row")]
    #[test_case(SCROLL_VIEWPORT; "several_rows")]
    fn zero_viewport_retains_navigation(viewport: usize) {
        let mut search = scrolling_pane();
        search.select_last();
        search.clamp_scroll(0);
        search.clamp_scroll(0);
        assert_eq!(search.scroll(), 0, "{FOLLOW_WRONG}");
        search.clamp_scroll(viewport);
        assert_eq!(
            search.scroll(),
            search.rows().len() - viewport,
            "{FOLLOW_WRONG}"
        );
    }

    #[test_case(SCROLL_VIEWPORT, MANUAL_SCROLL; "same_viewport")]
    #[test_case(SCROLL_HITS as usize, 1; "larger_viewport")]
    #[test_case(usize::MAX, 0; "oversized_viewport")]
    fn resizing_only_bounds_manual_scroll(viewport: usize, expected: usize) {
        let mut search = scrolling_pane();
        search.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        search.clamp_scroll(viewport);
        search.clamp_scroll(SCROLL_VIEWPORT);
        assert_eq!(search.scroll(), expected, "{SCROLL_WRONG}");
    }

    #[test_case(SCROLL_PATH; "same_file")]
    #[test_case("/another.rs"; "new_file")]
    fn result_growth_preserves_manual_scroll(path: &str) {
        let mut search = scrolling_pane();
        search.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        for line in 1..=SCROLL_HITS {
            search.push(hit(path, line));
            search.clamp_scroll(SCROLL_VIEWPORT);
            assert_eq!(search.scroll(), MANUAL_SCROLL, "{SCROLL_WRONG}");
        }
        assert_eq!(search.selected_index(), 0, "{SELECTION_WRONG}");
    }

    #[test_case(0; "hidden")]
    #[test_case(SCROLL_VIEWPORT; "visible")]
    fn clearing_results_resets_scroll_and_pending_follow(viewport: usize) {
        let mut search = scrolling_pane();
        search.set_scroll(MANUAL_SCROLL, SCROLL_VIEWPORT);
        search.select_last();
        search.clear();
        search.clamp_scroll(viewport);
        assert_eq!(search.scroll(), 0, "{SCROLL_WRONG}");
        assert_eq!(search.selected_index(), 0, "{SELECTION_WRONG}");
        assert!(!search.follow_selection, "{FOLLOW_WRONG}");
    }

    #[test]
    fn consecutive_hits_in_one_file_share_a_heading() {
        let search = pane(vec![hit("/a.rs", 1), hit("/a.rs", 9), hit("/b.rs", 3)]);
        assert_eq!(
            search.rows(),
            &[
                Row::File(0),
                Row::Hit(0),
                Row::Hit(1),
                Row::File(1),
                Row::Hit(2)
            ],
            "{GROUPING_WRONG}"
        );
    }

    #[test]
    fn a_file_row_points_at_the_top_of_its_file() {
        let mut search = pane(vec![hit("/a.rs", 7)]);
        search.select_first();
        assert_eq!(
            search.selection(),
            Some((PathBuf::from("/a.rs").into(), 1)),
            "{SELECTION_WRONG}"
        );
    }

    #[test]
    fn a_hit_row_points_at_its_line() {
        let mut search = pane(vec![hit("/a.rs", 7)]);
        search.move_selection(1);
        assert_eq!(
            search.selection(),
            Some((PathBuf::from("/a.rs").into(), 7)),
            "{SELECTION_WRONG}"
        );
    }

    #[test]
    fn the_cursor_stops_at_the_last_row() {
        let mut search = pane(vec![hit("/a.rs", 1)]);
        search.move_selection(50);
        assert_eq!(search.selected_index(), 1, "{SELECTION_WRONG}");
        search.move_selection(-50);
        assert_eq!(search.selected_index(), 0, "{SELECTION_WRONG}");
    }

    #[test]
    fn an_empty_pane_has_nothing_to_open() {
        let search = Search::default();
        assert!(search.selection().is_none(), "{SELECTION_WRONG}");
        assert!(!search.has_results(), "{SELECTION_WRONG}");
    }

    #[test]
    fn a_fresh_pane_is_stale_so_the_first_enter_searches() {
        let mut search = Search::default();
        assert!(search.is_stale(), "{STALE_WRONG}");

        type_query(&mut search, "x");
        search.start(&PathBuf::from("/nowhere-at-all"), false);
        assert!(!search.is_stale(), "{STALE_WRONG}");

        type_query(&mut search, "y");
        assert!(search.is_stale(), "{STALE_WRONG}");
    }

    #[test]
    fn a_toggle_makes_the_results_stale() {
        let mut search = Search::default();
        type_query(&mut search, "x");
        search.start(&PathBuf::from("/nowhere-at-all"), false);

        search.toggle_regex();
        assert!(search.is_stale(), "{STALE_WRONG}");
    }

    #[test]
    fn an_empty_query_clears_the_results_rather_than_searching() {
        let mut search = pane(vec![hit("/a.rs", 1)]);
        search.start(&PathBuf::from("/nowhere-at-all"), false);
        assert!(!search.has_results(), "{GROUPING_WRONG}");
        assert!(!search.is_running(), "{GROUPING_WRONG}");
    }

    #[test]
    fn an_invalid_pattern_is_reported_instead_of_run() {
        let mut search = Search::default();
        search.toggle_regex();
        type_query(&mut search, "a(");
        search.start(&PathBuf::from("/nowhere-at-all"), false);
        assert!(search.error().is_some(), "{STALE_WRONG}");
        assert!(!search.is_running(), "{STALE_WRONG}");
    }

    #[test]
    fn scrolling_keeps_the_cursor_in_view() {
        let hits = (1..=20).map(|line| hit("/a.rs", line)).collect();
        let mut search = pane(hits);
        search.select_last();
        search.clamp_scroll(5);
        let selected = search.selected_index();
        assert!(
            (search.scroll()..search.scroll() + 5).contains(&selected),
            "{SELECTION_WRONG}"
        );
    }
}
