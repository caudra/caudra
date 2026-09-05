//! Syntax highlighting for a buffer that changes in the middle.
//!
//! `caudra_highlight::CodeHighlighter` only ever appends: it caches completed
//! lines and resumes past them, which is right for a streaming code block and
//! wrong for an editor, where an edit on line 4 invalidates every line after
//! it.
//!
//! Syntect's parse state is a running fold over the file, so line N cannot be
//! highlighted without having walked lines 0..N. Rather than re-walk the whole
//! file on every keystroke, this keeps a checkpoint of that state every
//! [`CHECKPOINT_STRIDE`] lines. An edit drops the checkpoints at or after it,
//! and rendering resumes from the nearest one still standing. A viewport low in
//! a large file therefore costs at most a stride of re-parsing, not a file.
//!
//! Colours are then kept in a band around the viewport, because parsing one
//! line of an ordinary grammar costs on the order of a hundred microseconds and
//! a frame that re-parsed its whole window would spend milliseconds redrawing
//! text that had not changed. Repainting the same window costs nothing, and
//! scrolling costs the rows that came into view.

use caudra_highlight::{Highlighter, StyledSegment, syntax_for_path};
use syntect::highlighting::HighlightState;
use syntect::parsing::ParseState;

const CHECKPOINT_STRIDE: usize = 100;
/// How far above the viewport a walk may start when no checkpoint is within
/// reach. Without a bound, jumping into a large file re-parses everything above
/// the target. A grammar resynchronises within a few dozen lines in practice,
/// so this is generous, and any line the walk did not truly fold over is only
/// at risk of being coloured as if a block comment or raw string above it had
/// closed.
const MAX_LOOKBACK: usize = 500;
/// How many highlighted lines to hold on to. A viewport is tens of rows, so
/// this covers a long scroll in either direction without keeping a second copy
/// of a large file in memory.
const MAX_CACHED_ROWS: usize = 1000;

#[derive(Clone)]
struct Checkpoint {
    line: usize,
    highlight: HighlightState,
    parse: ParseState,
}

/// Colours already worked out, covering `[first, first + rows.len())`, with the
/// state they left behind so the run can be continued downwards.
///
/// `exact` records whether the walk that built this started from a checkpoint
/// or the top of the file. A band resumed from [`MAX_LOOKBACK`] is a guess, and
/// must not leave checkpoints behind for a later walk to trust.
struct Band {
    first: usize,
    rows: Vec<Vec<StyledSegment>>,
    highlight: HighlightState,
    parse: ParseState,
    exact: bool,
}

pub struct ViewportHighlighter {
    path: String,
    checkpoints: Vec<Checkpoint>,
    band: Option<Band>,
    theme_generation: u64,
}

impl ViewportHighlighter {
    pub fn new(path: &str, theme_generation: u64) -> Self {
        Self {
            path: path.to_owned(),
            checkpoints: Vec::new(),
            band: None,
            theme_generation,
        }
    }

    /// Drops every checkpoint that folded over `line`, because the fold no
    /// longer describes the text.
    pub fn invalidate_from(&mut self, line: usize) {
        self.checkpoints.retain(|c| c.line <= line);
        if self.checkpoints.last().is_some_and(|c| c.line == line) {
            self.checkpoints.pop();
        }
        // Only the tail state is kept, so a band cut short cannot be continued.
        // Dropping it costs one window on the next frame and keeps the rule
        // simple: what is cached was parsed from the text as it stands.
        if self.band.as_ref().is_some_and(|band| band.covers(line)) {
            self.band = None;
        }
    }

    /// A theme change rewrites every colour, so nothing cached survives it.
    pub fn set_theme_generation(&mut self, generation: u64) {
        if self.theme_generation != generation {
            self.theme_generation = generation;
            self.checkpoints.clear();
            self.band = None;
        }
    }

    /// Makes sure the band covers `lines[first..last]`, extending it when the
    /// viewport only ran off the bottom and starting again when it did not.
    ///
    /// Reading is [`Self::cached`] rather than a return value here, so a caller
    /// holding the colours is not also holding a mutable borrow of the tab they
    /// belong to.
    pub fn fill(&mut self, lines: &[String], first: usize, last: usize) {
        let last = last.min(lines.len());
        if first >= last {
            return;
        }
        match self.band.as_ref().map(|band| band.holds(first, last)) {
            Some(true) => {}
            Some(false) if self.extendable(first, last) => self.extend(lines, last),
            _ => self.rebuild(lines, first, last),
        }
    }

    /// The colours for `[first, last)`, or nothing when the band does not reach
    /// them because [`Self::fill`] was not asked for this window.
    pub fn cached(&self, first: usize, last: usize) -> &[Vec<StyledSegment>] {
        match &self.band {
            Some(band) if band.holds(first, last) => {
                &band.rows[first - band.first..last - band.first]
            }
            _ => &[],
        }
    }

    /// Whether the band starts at or above `first` and only falls short below,
    /// which is a scroll downwards and can be answered by parsing on from its
    /// tail rather than starting again.
    fn extendable(&self, first: usize, last: usize) -> bool {
        self.band
            .as_ref()
            .is_some_and(|band| band.first <= first && last > band.end())
    }

    fn extend(&mut self, lines: &[String], last: usize) {
        let band = self.band.take().expect("an extendable band");
        let mut highlighter =
            Highlighter::from_state(caudra_highlight::theme(), band.highlight, band.parse);
        let end = band.first + band.rows.len();
        let mut rows = band.rows;
        rows.append(&mut self.walk(lines, &mut highlighter, end, last, band.exact));
        self.keep(rows, band.first, highlighter, band.exact, last);
    }

    fn rebuild(&mut self, lines: &[String], first: usize, last: usize) {
        let floor = first.saturating_sub(MAX_LOOKBACK);
        let resume = self.resume_at(first).filter(|c| c.line >= floor);
        // A walk from the top of the file is as true as one from a checkpoint,
        // and a file shorter than the lookback is always walked from the top.
        let exact = resume.is_some() || floor == 0;
        let mut highlighter = match resume {
            Some(checkpoint) => Highlighter::from_state(
                caudra_highlight::theme(),
                checkpoint.highlight.clone(),
                checkpoint.parse.clone(),
            ),
            None => Highlighter::for_syntax(syntax_for_path(&self.path)),
        };
        // The band starts where the walk starts rather than at the viewport, so
        // the rows it has to parse on the way down are kept instead of thrown
        // away, and scrolling back up is answered from memory.
        let start = resume.map_or(floor, |checkpoint| checkpoint.line);
        let rows = self.walk(lines, &mut highlighter, start, last, exact);
        self.keep(rows, start, highlighter, exact, last);
    }

    /// Parses `[from, last)` and keeps every row, laying a checkpoint on each
    /// stride it passes. Runs stop on those strides because a checkpoint has to
    /// be taken between lines, and because the theme's selector table is built
    /// once per run.
    fn walk(
        &mut self,
        lines: &[String],
        highlighter: &mut Highlighter,
        from: usize,
        last: usize,
        checkpoint: bool,
    ) -> Vec<Vec<StyledSegment>> {
        let mut rows = Vec::with_capacity(last.saturating_sub(from));
        let mut cursor = from;
        while cursor < last {
            if checkpoint {
                self.maybe_checkpoint(cursor, highlighter);
            }
            let stop = next_stride(cursor).min(last);
            rows.append(&mut highlighter.highlight_lines(lines[cursor..stop].iter().map(String::as_str)));
            cursor = stop;
        }
        rows
    }

    /// Stores a finished walk, dropping rows off the front once it outgrows
    /// [`MAX_CACHED_ROWS`]. The viewport is never trimmed away: it ends the
    /// band, and a window is far shorter than the cap.
    fn keep(
        &mut self,
        mut rows: Vec<Vec<StyledSegment>>,
        mut first: usize,
        highlighter: Highlighter,
        exact: bool,
        last: usize,
    ) {
        let excess = rows.len().saturating_sub(MAX_CACHED_ROWS);
        let excess = excess.min(last.saturating_sub(first));
        rows.drain(..excess);
        first += excess;
        let (highlight, parse) = highlighter.state();
        self.band = Some(Band {
            first,
            rows,
            highlight,
            parse,
            exact,
        });
    }

    fn resume_at(&self, line: usize) -> Option<&Checkpoint> {
        self.checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.line <= line)
    }

    fn maybe_checkpoint(&mut self, line: usize, highlighter: &Highlighter) {
        if line == 0 || !line.is_multiple_of(CHECKPOINT_STRIDE) {
            return;
        }
        if self.checkpoints.iter().any(|c| c.line == line) {
            return;
        }
        let (highlight, parse) = highlighter.snapshot();
        self.checkpoints.push(Checkpoint {
            line,
            highlight,
            parse,
        });
        self.checkpoints.sort_by_key(|c| c.line);
    }

    #[cfg(test)]
    fn checkpoint_lines(&self) -> Vec<usize> {
        self.checkpoints.iter().map(|c| c.line).collect()
    }

    #[cfg(test)]
    fn band_range(&self) -> Option<(usize, usize)> {
        self.band.as_ref().map(|band| (band.first, band.end()))
    }
}

impl Band {
    fn end(&self) -> usize {
        self.first + self.rows.len()
    }

    fn holds(&self, first: usize, last: usize) -> bool {
        self.first <= first && last <= self.end()
    }

    fn covers(&self, line: usize) -> bool {
        line < self.end()
    }
}

/// The next line a checkpoint belongs on, which is where a run has to stop.
fn next_stride(line: usize) -> usize {
    line / CHECKPOINT_STRIDE * CHECKPOINT_STRIDE + CHECKPOINT_STRIDE
}

#[cfg(test)]
mod tests {
    use super::{CHECKPOINT_STRIDE, MAX_CACHED_ROWS, MAX_LOOKBACK, ViewportHighlighter};
    use caudra_highlight::StyledSegment;

    const SAME_AS_COLD: &str =
        "a viewport resumed from a checkpoint must match one highlighted from the top";
    const DROPPED: &str = "an edit must drop every checkpoint that folded over it";
    const KEPT: &str = "an edit must keep the checkpoints before it, which are still true";
    const SPANS_WHOLE_LINE: &str = "every line must come back fully covered by segments";
    const NOT_CACHED: &str = "a window already worked out must be answered from the band";
    const WRONG_BAND: &str = "the band does not cover what the walk was asked for";
    const NOT_BOUNDED: &str = "a jump must not walk further than the lookback allows";

    fn source(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| match i % 4 {
                0 => format!("fn function_{i}() -> usize {{"),
                1 => format!("    let value = {i}; // a comment"),
                2 => "    value".to_owned(),
                _ => "}".to_owned(),
            })
            .collect()
    }

    /// Fills and reads in one step, which is what a frame does.
    fn view(
        hl: &mut ViewportHighlighter,
        lines: &[String],
        first: usize,
        last: usize,
    ) -> Vec<Vec<StyledSegment>> {
        hl.fill(lines, first, last);
        hl.cached(first, last.min(lines.len())).to_vec()
    }

    fn text(segments: &[Vec<StyledSegment>]) -> Vec<String> {
        segments
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn a_resumed_viewport_matches_a_cold_one() {
        let lines = source(400);
        let first = 350;
        let last = 380;

        let mut warm = ViewportHighlighter::new("main.rs", 0);
        view(&mut warm, &lines, 0, 40);
        view(&mut warm, &lines, 100, 140);
        view(&mut warm, &lines, 200, 240);
        let warm_view = view(&mut warm, &lines, first, last);

        let mut cold = ViewportHighlighter::new("main.rs", 0);
        let cold_view = view(&mut cold, &lines, first, last);

        assert_eq!(warm_view.len(), cold_view.len(), "{SAME_AS_COLD}");
        assert_eq!(text(&warm_view), text(&cold_view), "{SAME_AS_COLD}");
        for (warm_line, cold_line) in warm_view.iter().zip(&cold_view) {
            assert_eq!(warm_line, cold_line, "{SAME_AS_COLD}");
        }
    }

    #[test]
    fn walking_a_file_lays_down_checkpoints_on_the_stride() {
        let lines = source(350);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 300, 320);
        assert_eq!(
            hl.checkpoint_lines(),
            vec![
                CHECKPOINT_STRIDE,
                CHECKPOINT_STRIDE * 2,
                CHECKPOINT_STRIDE * 3
            ]
        );
    }

    #[test]
    fn an_edit_drops_only_the_checkpoints_that_saw_it() {
        let lines = source(500);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 450, 470);
        assert_eq!(hl.checkpoint_lines().len(), 4);

        hl.invalidate_from(250);
        assert_eq!(hl.checkpoint_lines(), vec![100, 200], "{DROPPED}");
        assert!(hl.checkpoint_lines().contains(&100), "{KEPT}");
    }

    #[test]
    fn an_edit_exactly_on_a_checkpoint_drops_it() {
        let lines = source(500);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 450, 470);
        hl.invalidate_from(200);
        assert_eq!(hl.checkpoint_lines(), vec![100], "{DROPPED}");
    }

    #[test]
    fn a_theme_change_drops_everything() {
        let lines = source(300);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 250, 270);
        assert!(!hl.checkpoint_lines().is_empty());

        hl.set_theme_generation(1);
        assert!(hl.checkpoint_lines().is_empty(), "{DROPPED}");
        assert_eq!(hl.band_range(), None, "{DROPPED}");
    }

    #[test]
    fn segments_cover_the_text_they_came_from() {
        let lines = source(20);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        let view = view(&mut hl, &lines, 0, 20);

        for (rendered, original) in text(&view).iter().zip(&lines) {
            assert_eq!(
                rendered.trim_end_matches('\n'),
                original,
                "{SPANS_WHOLE_LINE}"
            );
        }
    }

    #[test]
    fn an_empty_range_highlights_nothing() {
        let lines = source(10);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        assert!(view(&mut hl, &lines, 5, 5).is_empty());
        assert!(view(&mut hl, &lines, 20, 30).is_empty());
    }

    #[test]
    fn a_window_already_worked_out_is_not_parsed_again() {
        let lines = source(200);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        let first = view(&mut hl, &lines, 40, 80);
        let band = hl.band_range();

        let again = view(&mut hl, &lines, 40, 80);

        assert_eq!(hl.band_range(), band, "{NOT_CACHED}");
        assert_eq!(text(&first), text(&again), "{NOT_CACHED}");
    }

    #[test]
    fn scrolling_down_keeps_what_was_already_parsed_and_adds_the_rest() {
        let lines = source(200);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 0, 40);
        assert_eq!(hl.band_range(), Some((0, 40)), "{WRONG_BAND}");

        view(&mut hl, &lines, 3, 43);

        assert_eq!(hl.band_range(), Some((0, 43)), "{WRONG_BAND}");
    }

    /// The walk has to reach the viewport from somewhere above it, so the band
    /// starts where the walk started rather than at the viewport. That makes
    /// scrolling back up free until it passes the walk's own start.
    #[test]
    fn scrolling_up_is_answered_from_the_rows_the_walk_kept() {
        let lines = source(400);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 300, 340);
        assert_eq!(hl.band_range(), Some((0, 340)), "{WRONG_BAND}");

        let scrolled = view(&mut hl, &lines, 250, 290);

        assert_eq!(hl.band_range(), Some((0, 340)), "{NOT_CACHED}");
        assert_eq!(scrolled.len(), 40, "{WRONG_BAND}");
    }

    /// Above the lookback there are no kept rows to scroll back into, so the
    /// band is rebuilt, and it again reaches far enough above the viewport to
    /// absorb the scrolling that follows.
    #[test]
    fn scrolling_up_past_the_band_rebuilds_it_with_room_above() {
        let lines = source(4000);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 3000, 3040);
        assert_eq!(hl.band_range(), Some((3000 - MAX_LOOKBACK, 3040)), "{WRONG_BAND}");

        view(&mut hl, &lines, 2400, 2440);

        assert_eq!(hl.band_range(), Some((2400 - MAX_LOOKBACK, 2440)), "{WRONG_BAND}");
    }

    #[test]
    fn a_jump_past_the_lookback_starts_within_it() {
        let lines = source(4000);
        let mut hl = ViewportHighlighter::new("main.rs", 0);

        view(&mut hl, &lines, 3000, 3040);

        assert_eq!(hl.band_range(), Some((3000 - MAX_LOOKBACK, 3040)), "{NOT_BOUNDED}");
        assert!(hl.checkpoint_lines().is_empty(), "{NOT_BOUNDED}");
    }

    #[test]
    fn a_guessed_walk_matches_a_true_one_where_the_grammar_has_resynchronised() {
        let lines = source(4000);
        let mut guessed = ViewportHighlighter::new("main.rs", 0);
        let jumped = view(&mut guessed, &lines, 3000, 3040);

        let mut walked = ViewportHighlighter::new("main.rs", 0);
        for first in (0..3000).step_by(40) {
            view(&mut walked, &lines, first, first + 40);
        }
        let scrolled = view(&mut walked, &lines, 3000, 3040);

        assert_eq!(jumped, scrolled, "{SAME_AS_COLD}");
    }

    #[test]
    fn a_band_grown_past_the_cap_gives_up_its_oldest_rows() {
        let lines = source(MAX_CACHED_ROWS * 2);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 0, 40);

        let last = MAX_CACHED_ROWS + 200;
        view(&mut hl, &lines, last - 40, last);

        let (first, end) = hl.band_range().expect("a band");
        assert_eq!(end, last, "{WRONG_BAND}");
        assert!(end - first <= MAX_CACHED_ROWS, "{WRONG_BAND}");
        assert!(first <= last - 40, "{WRONG_BAND}");
    }

    #[test]
    fn an_edit_inside_the_band_drops_it() {
        let lines = source(200);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 0, 40);

        hl.invalidate_from(20);

        assert_eq!(hl.band_range(), None, "{DROPPED}");
    }

    #[test]
    fn an_edit_below_the_band_leaves_it_alone() {
        let lines = source(200);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        view(&mut hl, &lines, 0, 40);

        hl.invalidate_from(120);

        assert_eq!(hl.band_range(), Some((0, 40)), "{KEPT}");
    }
}
