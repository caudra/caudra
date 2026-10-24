use crate::SnapshotLine;

pub const OUTPUT_SO_FAR_HEADING: &str = "Output so far";
const OUTPUT_SO_FAR_LINES: usize = 20;
const OUTPUT_SO_FAR_BYTES: usize = 2048;

/// The last of what a command has printed, after `reduce` gives it the
/// reduction its final result would get, as a block that ends a note to the
/// model. `None` when nothing is left to show.
pub fn output_so_far(
    lines: &[SnapshotLine],
    reduce: impl FnOnce(String) -> String,
) -> Option<String> {
    let tail = tail(&reduce(plain_text(lines)));
    if tail.is_empty() {
        return None;
    }
    let shown = tail.lines().count();
    Some(format!(
        "{OUTPUT_SO_FAR_HEADING} (last {shown} lines):\n```\n{tail}\n```"
    ))
}

fn plain_text(lines: &[SnapshotLine]) -> String {
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

/// At most [`OUTPUT_SO_FAR_LINES`] lines and [`OUTPUT_SO_FAR_BYTES`] bytes,
/// cut at a line, or failing that a character, boundary.
fn tail(text: &str) -> String {
    let text = text.trim_end();
    let start = text
        .rmatch_indices('\n')
        .nth(OUTPUT_SO_FAR_LINES - 1)
        .map_or(0, |(newline, _)| newline + 1);
    let text = &text[start..];
    if text.len() <= OUTPUT_SO_FAR_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - OUTPUT_SO_FAR_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let kept = &text[start..];
    kept.split_once('\n')
        .map_or(kept, |(_, whole_lines)| whole_lines)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PRINTED: &str = "compiling";
    const FILTERED: &str = "[filtered: progress]";
    const WIDE_CHAR: &str = "é";

    fn printed(count: usize) -> Vec<SnapshotLine> {
        (0..count)
            .map(|line| SnapshotLine::plain(format!("line {line}")))
            .collect()
    }

    #[test_case(0, 0; "no_output")]
    #[test_case(3, 3; "short_output_whole")]
    #[test_case(OUTPUT_SO_FAR_LINES + 5, OUTPUT_SO_FAR_LINES; "long_output_keeps_the_last_lines")]
    fn the_tail_keeps_the_last_lines(count: usize, kept: usize) {
        let tail = tail(&plain_text(&printed(count)));
        assert_eq!(tail.lines().count(), kept);
        if kept > 0 {
            assert_eq!(tail.lines().last().unwrap(), format!("line {}", count - 1));
        }
    }

    #[test]
    fn nothing_printed_shows_nothing() {
        assert_eq!(output_so_far(&printed(0), |text| text), None);
    }

    #[test]
    fn what_is_shown_is_what_the_filter_leaves() {
        let shown = output_so_far(&[SnapshotLine::plain(PRINTED.into())], |_| {
            FILTERED.to_owned()
        })
        .unwrap();
        assert!(shown.starts_with(OUTPUT_SO_FAR_HEADING), "{shown}");
        assert!(shown.contains(FILTERED), "{shown}");
        assert!(!shown.contains(PRINTED), "{shown}");
    }

    #[test]
    fn the_tail_cuts_at_a_char_boundary_within_its_byte_budget() {
        let tail = tail(&WIDE_CHAR.repeat(OUTPUT_SO_FAR_BYTES));
        assert!(tail.len() <= OUTPUT_SO_FAR_BYTES);
        assert!(!tail.is_empty());
        assert!(tail.chars().all(|char| char.to_string() == WIDE_CHAR));
    }
}
