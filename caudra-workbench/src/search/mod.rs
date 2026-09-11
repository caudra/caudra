//! The Search pane: two fields, three toggles, and a two-level result list.
//!
//! Unlike source control, this one is genuinely slow, so the walk runs on a
//! worker and the pane grows as answers arrive. Everything here is the part
//! that can be reasoned about without a thread: what was typed, what came back,
//! and where the cursor is in it.

pub mod engine;

use std::path::{Path, PathBuf};

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

#[derive(Default)]
pub struct Search {
    query: Query,
    field: Field,
    files: Vec<PathBuf>,
    hits: Vec<Hit>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    run: Option<Run>,
    truncated: bool,
    error: Option<String>,
    /// The query the listed results came from. Enter runs the search while this
    /// disagrees with the fields, and opens the selection once it agrees.
    ran: Option<Query>,
}

impl Search {
    pub fn field(&self) -> Field {
        self.field
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn file(&self, index: usize) -> Option<&Path> {
        self.files.get(index).map(PathBuf::as_path)
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
        self.run.is_some()
    }

    /// Whether the fields have moved on from the results below them.
    pub fn is_stale(&self) -> bool {
        self.ran.as_ref() != Some(&self.query)
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

    pub fn push_char(&mut self, ch: char) {
        self.focused_field().push(ch);
    }

    pub fn pop_char(&mut self) {
        self.focused_field().pop();
    }

    pub fn toggle_case(&mut self) {
        self.query.case_sensitive = !self.query.case_sensitive;
    }

    pub fn toggle_word(&mut self) {
        self.query.whole_word = !self.query.whole_word;
    }

    pub fn toggle_regex(&mut self) {
        self.query.regex = !self.query.regex;
    }

    /// Starts a fresh walk, cancelling whatever the last one was still doing.
    /// An empty query clears the pane instead, so deleting the text does not
    /// leave stale results sitting under it.
    pub fn start(&mut self, root: &Path, hidden: bool) {
        self.query.hidden = hidden;
        self.clear();
        self.ran = Some(self.query.clone());
        if self.query.text.is_empty() {
            return;
        }
        match Run::start(root, &self.query) {
            Ok(run) => self.run = Some(run),
            Err(error) => self.error = Some(error.to_string()),
        }
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
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    pub fn select_index(&mut self, index: usize) {
        if index < self.rows.len() {
            self.selected = index;
        }
    }

    pub fn clamp_scroll(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + viewport {
            self.scroll = self.selected + 1 - viewport;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(viewport));
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let max = self.rows.len().saturating_sub(viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn set_scroll(&mut self, top: usize, viewport: usize) {
        self.scroll = top.min(self.rows.len().saturating_sub(viewport));
    }

    /// Where the selected row points: a file, and the line to land on.
    pub fn selection(&self) -> Option<(PathBuf, usize)> {
        match self.rows.get(self.selected)? {
            Row::File(index) => Some((self.files.get(*index)?.clone(), 1)),
            Row::Hit(index) => {
                let hit = self.hits.get(*index)?;
                Some((hit.path.clone(), usize::try_from(hit.line).unwrap_or(1)))
            }
        }
    }

    pub fn counts(&self) -> (usize, usize) {
        (self.hits.len(), self.files.len())
    }

    fn focused_field(&mut self) -> &mut String {
        match self.field {
            Field::Query => &mut self.query.text,
            Field::Include => &mut self.query.include,
        }
    }

    fn clear(&mut self) {
        self.run = None;
        self.files.clear();
        self.hits.clear();
        self.rows.clear();
        self.selected = 0;
        self.scroll = 0;
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

    use super::{Field, Hit, Row, Search};

    const GROUPING_WRONG: &str = "hits must be grouped under one heading per file";
    const SELECTION_WRONG: &str = "the cursor is not on the row the test put it on";
    const FIELD_WRONG: &str = "typing went into the wrong field";
    const STALE_WRONG: &str = "the pane disagrees about whether its results are current";

    fn hit(path: &str, line: u64) -> Hit {
        Hit {
            path: PathBuf::from(path),
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
            Some((PathBuf::from("/a.rs"), 1)),
            "{SELECTION_WRONG}"
        );
    }

    #[test]
    fn a_hit_row_points_at_its_line() {
        let mut search = pane(vec![hit("/a.rs", 7)]);
        search.move_selection(1);
        assert_eq!(
            search.selection(),
            Some((PathBuf::from("/a.rs"), 7)),
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
    fn typing_lands_in_whichever_field_holds_the_caret() {
        let mut search = Search::default();
        search.push_char('a');
        search.next_field();
        search.push_char('b');
        assert_eq!(search.query().text, "a", "{FIELD_WRONG}");
        assert_eq!(search.query().include, "b", "{FIELD_WRONG}");
        assert_eq!(search.field(), Field::Include, "{FIELD_WRONG}");

        search.pop_char();
        assert!(search.query().include.is_empty(), "{FIELD_WRONG}");
    }

    #[test]
    fn a_fresh_pane_is_stale_so_the_first_enter_searches() {
        let mut search = Search::default();
        assert!(search.is_stale(), "{STALE_WRONG}");

        search.push_char('x');
        search.start(&PathBuf::from("/nowhere-at-all"), false);
        assert!(!search.is_stale(), "{STALE_WRONG}");

        search.push_char('y');
        assert!(search.is_stale(), "{STALE_WRONG}");
    }

    #[test]
    fn a_toggle_makes_the_results_stale() {
        let mut search = Search::default();
        search.push_char('x');
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
        for ch in "a(".chars() {
            search.push_char(ch);
        }
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
