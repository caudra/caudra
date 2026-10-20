//! The comments an upgrade leaves above each line it changed. A line's own
//! comments move below the note, and the blank lines that set it apart stay
//! in front of it.

use super::Stamp;

const COMMENT: &str = "# ";
const SEPARATOR: &str = "#";
const NEWLINE: char = '\n';

pub(super) fn upgraded(stamp: &Stamp, what: &str, why: Option<&str>) -> String {
    let Stamp { by, on } = stamp;
    match why {
        Some(why) => format!("Upgraded by {by} on {on}: {what}, because {why}."),
        None => format!("Upgraded by {by} on {on}: {what}."),
    }
}

pub(super) fn added(stamp: &Stamp, what: &str) -> String {
    let Stamp { by, on } = stamp;
    format!("Added by {by} on {on}: {what}.")
}

/// `prefix` with `note` placed above the comments it held.
pub(super) fn annotate(prefix: &str, note: &str) -> String {
    let parts = Prefix::split(prefix);
    parts.noted(note, None) + parts.indent
}

/// `prefix` kept whole, then `note`: for a line inserted below it.
pub(super) fn below(prefix: &str, note: &str) -> String {
    format!("{prefix}{COMMENT}{note}{NEWLINE}")
}

/// [`annotate`], followed by `entry` commented out: an entry retired from
/// the inline table on this line.
pub(super) fn annotate_retired(prefix: &str, note: &str, entry: &str) -> String {
    let parts = Prefix::split(prefix);
    parts.noted(note, Some(entry)) + parts.indent
}

/// The lines that replace a retired line whose decor was `prefix`: the
/// note, then the line itself commented out.
pub(super) fn retired(prefix: &str, note: &str, line: &str) -> String {
    Prefix::split(prefix).noted(note, Some(line))
}

/// `prefix` cut after its last blank line: the comment block that leads the
/// file, and what belongs to the item it precedes.
pub(super) fn split_leading(prefix: &str) -> (&str, &str) {
    let end = prefix
        .split_inclusive(NEWLINE)
        .scan(0, |offset, line| {
            *offset += line.len();
            Some((*offset, line))
        })
        .filter(|(_, line)| is_blank(line))
        .map(|(offset, _)| offset)
        .last()
        .unwrap_or(0);
    prefix.split_at(end)
}

/// `prefix` set apart from the line above by a blank line.
pub(super) fn separated(prefix: &str) -> String {
    match prefix.split_inclusive(NEWLINE).next() {
        Some(line) if is_blank(line) => prefix.to_owned(),
        _ => format!("{NEWLINE}{prefix}"),
    }
}

fn is_blank(line: &str) -> bool {
    line.ends_with(NEWLINE) && line.trim().is_empty()
}

/// A decor prefix: blank lines, then comment lines, then the indent of the
/// line it decorates.
struct Prefix<'a> {
    blank: &'a str,
    comments: &'a str,
    indent: &'a str,
}

impl<'a> Prefix<'a> {
    fn split(prefix: &'a str) -> Self {
        let (lines, indent) = prefix
            .rfind(NEWLINE)
            .map_or(("", prefix), |end| prefix.split_at(end + 1));
        let blank_end = lines
            .split_inclusive(NEWLINE)
            .take_while(|line| is_blank(line))
            .map(str::len)
            .sum();
        let (blank, comments) = lines.split_at(blank_end);
        Self {
            blank,
            comments,
            indent,
        }
    }

    fn noted(&self, note: &str, retired: Option<&str>) -> String {
        let Self {
            blank,
            comments,
            indent,
        } = self;
        let mut noted = format!("{blank}{indent}{COMMENT}{note}{NEWLINE}");
        if !comments.is_empty() {
            noted.push_str(&format!("{indent}{SEPARATOR}{NEWLINE}{comments}"));
        }
        for line in retired.into_iter().flat_map(str::lines) {
            let marker = if line.is_empty() { SEPARATOR } else { COMMENT };
            noted.push_str(&format!("{indent}{marker}{line}{NEWLINE}"));
        }
        noted
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{annotate, retired, separated, split_leading};

    const NOTE: &str = "Upgraded: renamed";

    #[test_case("", "# Upgraded: renamed\n" ; "no_decor")]
    #[test_case("\n\n", "\n\n# Upgraded: renamed\n" ; "blank_lines_stay_first")]
    #[test_case("# mine\n", "# Upgraded: renamed\n#\n# mine\n" ; "own_comment_goes_below_a_separator")]
    #[test_case("\n# mine\n\n# more\n", "\n# Upgraded: renamed\n#\n# mine\n\n# more\n" ; "blank_lines_before_comments_stay_first")]
    #[test_case("\n  # mine\n  ", "\n  # Upgraded: renamed\n  #\n  # mine\n  " ; "indented_line")]
    fn annotate_places_the_note_above_the_lines_own_comments(prefix: &str, expected: &str) {
        assert_eq!(annotate(prefix, NOTE), expected);
    }

    #[test_case("x = [\n  1,\n\n]", "# Upgraded: renamed\n# x = [\n#   1,\n#\n# ]\n" ; "multiline_value")]
    #[test_case("x = 1", "# Upgraded: renamed\n# x = 1\n" ; "single_line")]
    fn retired_comments_out_every_line(line: &str, expected: &str) {
        assert_eq!(retired("", NOTE, line), expected);
    }

    #[test_case("# head\n\n# about a\n", "# head\n\n", "# about a\n" ; "block_ends_at_the_last_blank_line")]
    #[test_case("# about a\n", "", "# about a\n" ; "no_blank_line_means_no_block")]
    #[test_case("# head\n\n  ", "# head\n\n", "  " ; "indent_is_not_a_blank_line")]
    fn split_leading_cuts_after_the_last_blank_line(prefix: &str, head: &str, tail: &str) {
        assert_eq!(split_leading(prefix), (head, tail));
    }

    #[test_case("", "\n" ; "empty")]
    #[test_case("\n# a\n", "\n# a\n" ; "already_separated")]
    #[test_case("# a\n", "\n# a\n" ; "comment")]
    fn separated_starts_with_a_blank_line(prefix: &str, expected: &str) {
        assert_eq!(separated(prefix), expected);
    }
}
