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

use caudra_highlight::{Highlighter, StyledSegment, syntax_for_path};
use syntect::highlighting::HighlightState;
use syntect::parsing::ParseState;

const CHECKPOINT_STRIDE: usize = 100;

#[derive(Clone)]
struct Checkpoint {
    line: usize,
    highlight: HighlightState,
    parse: ParseState,
}

pub struct ViewportHighlighter {
    path: String,
    checkpoints: Vec<Checkpoint>,
    theme_generation: u64,
}

impl ViewportHighlighter {
    pub fn new(path: &str, theme_generation: u64) -> Self {
        Self {
            path: path.to_owned(),
            checkpoints: Vec::new(),
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
    }

    /// A theme change rewrites every colour, so nothing cached survives it.
    pub fn set_theme_generation(&mut self, generation: u64) {
        if self.theme_generation != generation {
            self.theme_generation = generation;
            self.checkpoints.clear();
        }
    }

    /// Highlights `lines[first..last]`, walking forward from the nearest
    /// checkpoint and laying down new ones as it passes them.
    pub fn segments(
        &mut self,
        lines: &[String],
        first: usize,
        last: usize,
    ) -> Vec<Vec<StyledSegment>> {
        let last = last.min(lines.len());
        if first >= last {
            return Vec::new();
        }

        let resume = self.resume_at(first);
        let mut highlighter = match resume {
            Some(checkpoint) => Highlighter::from_state(
                caudra_highlight::theme(),
                checkpoint.highlight.clone(),
                checkpoint.parse.clone(),
            ),
            None => Highlighter::for_syntax(syntax_for_path(&self.path)),
        };
        let mut cursor = resume.map_or(0, |c| c.line);

        while cursor < first {
            self.maybe_checkpoint(cursor, &highlighter);
            highlighter.advance(&with_newline(&lines[cursor]));
            cursor += 1;
        }

        let mut out = Vec::with_capacity(last - first);
        while cursor < last {
            self.maybe_checkpoint(cursor, &highlighter);
            out.push(highlighter.highlight_line(&with_newline(&lines[cursor])));
            cursor += 1;
        }
        out
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
}

fn with_newline(line: &str) -> String {
    let mut owned = String::with_capacity(line.len() + 1);
    owned.push_str(line);
    owned.push('\n');
    owned
}

#[cfg(test)]
mod tests {
    use super::{CHECKPOINT_STRIDE, ViewportHighlighter};
    use caudra_highlight::StyledSegment;

    const SAME_AS_COLD: &str =
        "a viewport resumed from a checkpoint must match one highlighted from the top";
    const DROPPED: &str = "an edit must drop every checkpoint that folded over it";
    const KEPT: &str = "an edit must keep the checkpoints before it, which are still true";
    const SPANS_WHOLE_LINE: &str = "every line must come back fully covered by segments";

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
        warm.segments(&lines, 0, 40);
        warm.segments(&lines, 100, 140);
        warm.segments(&lines, 200, 240);
        let warm_view = warm.segments(&lines, first, last);

        let mut cold = ViewportHighlighter::new("main.rs", 0);
        let cold_view = cold.segments(&lines, first, last);

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
        hl.segments(&lines, 300, 320);
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
        hl.segments(&lines, 450, 470);
        assert_eq!(hl.checkpoint_lines().len(), 4);

        hl.invalidate_from(250);
        assert_eq!(hl.checkpoint_lines(), vec![100, 200], "{DROPPED}");
        assert!(hl.checkpoint_lines().contains(&100), "{KEPT}");
    }

    #[test]
    fn an_edit_exactly_on_a_checkpoint_drops_it() {
        let lines = source(500);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        hl.segments(&lines, 450, 470);
        hl.invalidate_from(200);
        assert_eq!(hl.checkpoint_lines(), vec![100], "{DROPPED}");
    }

    #[test]
    fn a_theme_change_drops_everything() {
        let lines = source(300);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        hl.segments(&lines, 250, 270);
        assert!(!hl.checkpoint_lines().is_empty());

        hl.set_theme_generation(1);
        assert!(hl.checkpoint_lines().is_empty(), "{DROPPED}");
    }

    #[test]
    fn segments_cover_the_text_they_came_from() {
        let lines = source(20);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        let view = hl.segments(&lines, 0, 20);

        for (rendered, original) in text(&view).iter().zip(&lines) {
            assert_eq!(rendered.trim_end_matches('\n'), original, "{SPANS_WHOLE_LINE}");
        }
    }

    #[test]
    fn an_empty_range_highlights_nothing() {
        let lines = source(10);
        let mut hl = ViewportHighlighter::new("main.rs", 0);
        assert!(hl.segments(&lines, 5, 5).is_empty());
        assert!(hl.segments(&lines, 20, 30).is_empty());
    }
}
