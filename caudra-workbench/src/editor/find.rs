//! Find within the open buffer.
//!
//! Matching is incremental: every keystroke in the find bar rescans, so the
//! query doubles as the input state. Case sensitivity is inferred rather than
//! toggled — a query typed in lower case matches anything, and the moment it
//! contains an upper-case character it means it.

use super::buffer::Cursor;

/// A match, in character columns, matching how [`Cursor::col`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

impl Match {
    fn at(&self) -> (usize, usize) {
        (self.line, self.start)
    }

    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.line, self.start)
    }
}

#[derive(Debug, Default)]
pub struct Find {
    query: String,
    matches: Vec<Match>,
    current: Option<usize>,
    open: bool,
}

impl Find {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    /// Leaves the query behind so reopening the bar resumes where it left off,
    /// but drops the matches so nothing stays highlighted.
    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.current = None;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn current(&self) -> Option<Match> {
        self.current.map(|index| self.matches[index])
    }

    /// One-based position of the current match and the total, for the counter
    /// the find bar shows.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.current.map(|index| (index + 1, self.matches.len()))
    }

    pub fn set_query(&mut self, query: String, lines: &[String], from: Cursor) {
        self.query = query;
        self.scan(lines);
        self.current = self.locate((from.line, from.col));
    }

    /// Rescans after the buffer changed, keeping the current match on the
    /// nearest surviving hit rather than jumping back to the top.
    pub fn refresh(&mut self, lines: &[String]) {
        let anchor = self.current().map(|found| found.at());
        self.scan(lines);
        self.current = self.locate(anchor.unwrap_or_default());
    }

    pub fn step(&mut self, delta: isize) -> Option<Match> {
        let len = isize::try_from(self.matches.len())
            .ok()
            .filter(|n| *n > 0)?;
        let current = isize::try_from(self.current.unwrap_or_default()).unwrap_or_default();
        let next = (current + delta).rem_euclid(len);
        self.current = usize::try_from(next).ok();
        self.current()
    }

    /// The matches on one line, for painting. Relies on `matches` being sorted
    /// by line and then column, which the scan produces by construction.
    pub fn on_line(&self, line: usize) -> &[Match] {
        let start = self.matches.partition_point(|found| found.line < line);
        let end = self.matches.partition_point(|found| found.line <= line);
        &self.matches[start..end]
    }

    fn locate(&self, at: (usize, usize)) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        Some(
            self.matches
                .iter()
                .position(|found| found.at() >= at)
                .unwrap_or_default(),
        )
    }

    fn scan(&mut self, lines: &[String]) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        let needle: Vec<char> = self.query.chars().collect();
        let sensitive = self.query.chars().any(char::is_uppercase);
        let mut haystack: Vec<char> = Vec::new();
        for (line, text) in lines.iter().enumerate() {
            haystack.clear();
            haystack.extend(text.chars());
            let mut start = 0;
            while start + needle.len() <= haystack.len() {
                if starts_with(&haystack[start..], &needle, sensitive) {
                    let end = start + needle.len();
                    self.matches.push(Match { line, start, end });
                    start = end;
                } else {
                    start += 1;
                }
            }
        }
    }
}

fn starts_with(haystack: &[char], needle: &[char], sensitive: bool) -> bool {
    haystack
        .iter()
        .zip(needle)
        .all(|(found, wanted)| same(*found, *wanted, sensitive))
}

fn same(found: char, wanted: char, sensitive: bool) -> bool {
    if sensitive || found == wanted {
        return found == wanted;
    }
    found.to_lowercase().eq(wanted.to_lowercase())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Cursor, Find, Match};

    const WRONG_MATCHES: &str = "matches do not cover the expected columns";
    const WRONG_CURRENT: &str = "the current match is not the expected one";
    const WRONG_POSITION: &str = "the match counter is wrong";
    /// More lines than any fixture here has, so nothing is missed when the
    /// matches are gathered a line at a time.
    const LINES: usize = 16;

    fn lines(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_owned()).collect()
    }

    fn found(find: &Find) -> Vec<(usize, usize, usize)> {
        (0..LINES)
            .flat_map(|line| find.on_line(line))
            .map(|Match { line, start, end }| (*line, *start, *end))
            .collect()
    }

    fn search(query: &str, rows: &[&str]) -> Find {
        let mut find = Find::default();
        find.set_query(query.to_owned(), &lines(rows), Cursor::default());
        find
    }

    #[test_case("fn", &["fn a() {}", "  fn b() {}"], &[(0, 0, 2), (1, 2, 4)] ; "every line is scanned")]
    #[test_case("aa", &["aaaa"], &[(0, 0, 2), (0, 2, 4)] ; "matches do not overlap")]
    #[test_case("", &["anything"], &[] ; "an empty query matches nothing")]
    #[test_case("zz", &["anything"], &[] ; "a missing query matches nothing")]
    #[test_case("é", &["café éclair"], &[(0, 3, 4), (0, 5, 6)] ; "columns count characters not bytes")]
    fn scan_reports_character_columns(
        query: &str,
        rows: &[&str],
        expected: &[(usize, usize, usize)],
    ) {
        assert_eq!(found(&search(query, rows)), expected, "{WRONG_MATCHES}");
    }

    #[test_case("tab", &[(0, 0, 3), (0, 4, 7)] ; "lower case ignores case")]
    #[test_case("Tab", &[(0, 4, 7)] ; "an upper case character demands an exact match")]
    fn case_sensitivity_follows_the_query(query: &str, expected: &[(usize, usize, usize)]) {
        let find = search(query, &["tab Tab"]);
        assert_eq!(found(&find), expected, "{WRONG_MATCHES}");
    }

    #[test]
    fn the_first_match_at_or_after_the_cursor_is_selected() {
        let rows = lines(&["a", "a", "a"]);
        let mut find = Find::default();
        find.set_query("a".to_owned(), &rows, Cursor::new(1, 0));

        assert_eq!(
            find.current(),
            Some(Match {
                line: 1,
                start: 0,
                end: 1
            }),
            "{WRONG_CURRENT}"
        );
        assert_eq!(find.position(), Some((2, 3)), "{WRONG_POSITION}");
    }

    #[test]
    fn selection_wraps_to_the_top_when_the_cursor_is_past_the_last_match() {
        let rows = lines(&["a", "b"]);
        let mut find = Find::default();
        find.set_query("a".to_owned(), &rows, Cursor::new(1, 0));

        assert_eq!(
            find.current(),
            Some(Match {
                line: 0,
                start: 0,
                end: 1
            }),
            "{WRONG_CURRENT}"
        );
    }

    #[test_case(1, &[1, 2, 0] ; "forward wraps past the end")]
    #[test_case(-1, &[2, 1, 0] ; "backward wraps past the start")]
    fn stepping_cycles_through_every_match(delta: isize, expected: &[usize]) {
        let mut find = search("a", &["a", "a", "a"]);
        let walked: Vec<usize> = expected
            .iter()
            .map(|_| find.step(delta).expect(WRONG_CURRENT).line)
            .collect();

        assert_eq!(walked, expected, "{WRONG_CURRENT}");
    }

    #[test]
    fn stepping_without_matches_does_nothing() {
        let mut find = search("zz", &["a"]);
        assert_eq!(find.step(1), None, "{WRONG_CURRENT}");
    }

    #[test]
    fn refreshing_keeps_the_current_match_in_place() {
        let mut find = search("a", &["a", "a", "a"]);
        find.step(1);

        find.refresh(&lines(&["x", "a", "a"]));

        assert_eq!(
            find.current(),
            Some(Match {
                line: 1,
                start: 0,
                end: 1
            }),
            "{WRONG_CURRENT}"
        );
        assert_eq!(find.position(), Some((1, 2)), "{WRONG_POSITION}");
    }

    #[test]
    fn refreshing_after_the_current_match_disappears_falls_back_to_the_first() {
        let mut find = search("a", &["x", "a"]);
        find.refresh(&lines(&["a", "x"]));

        assert_eq!(
            find.current(),
            Some(Match {
                line: 0,
                start: 0,
                end: 1
            }),
            "{WRONG_CURRENT}"
        );
    }

    #[test]
    fn matches_are_grouped_by_line() {
        let find = search("a", &["aa", "b", "a"]);

        assert_eq!(find.on_line(0).len(), 2, "{WRONG_MATCHES}");
        assert!(find.on_line(1).is_empty(), "{WRONG_MATCHES}");
        assert_eq!(find.on_line(2).len(), 1, "{WRONG_MATCHES}");
        assert!(find.on_line(9).is_empty(), "{WRONG_MATCHES}");
    }

    #[test]
    fn closing_keeps_the_query_but_drops_the_highlights() {
        let mut find = search("a", &["a"]);
        find.open();
        find.close();

        assert!(!find.is_open(), "{WRONG_MATCHES}");
        assert_eq!(find.query(), "a", "{WRONG_MATCHES}");
        assert!(found(&find).is_empty(), "{WRONG_MATCHES}");
        assert_eq!(find.position(), None, "{WRONG_POSITION}");
    }
}
