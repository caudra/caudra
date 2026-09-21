//! What counts as one word, and what counts as one path component.
//!
//! Word boundaries answer the caret motions and the double click. Component
//! boundaries answer the delete chords, and follow fish's
//! `backward-kill-path-component` rather than a word delete: the run of
//! separators trailing a component belongs to it, so one press takes `bar/` out
//! of `/foo/bar/` instead of leaving the slash behind for a second press.

/// Separators end a component. This is fish's own set, which deliberately
/// leaves `.`, `-` and `_` out: `file.tar.gz` and `kebab-case` are each one
/// component, not three. `\` is ours, standing in for fish's shell escape so a
/// Windows path splits the way a POSIX one does.
const SEPARATORS: &str = "/={,}'\":@#|;<>&\\";

enum Class {
    Blank,
    Component,
    Separator,
}

/// Walking left, the states are named for what has been consumed so far.
#[derive(Clone, Copy)]
enum Left {
    Separators,
    Component,
    Space,
    SpaceSeparators,
}

#[derive(Clone, Copy)]
enum Right {
    Component,
    Separators,
    Blank,
}

/// What counts as one word to word motion and to a double click, so the two
/// never disagree about where a word ends.
pub(crate) fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Blank
    } else if SEPARATORS.contains(ch) {
        Class::Separator
    } else {
        Class::Component
    }
}

/// Where the component before `at` starts. Consumes trailing separators with
/// the component they trail, and leading whitespace with the component beyond
/// it, so a single press never leaves a fragment behind.
pub fn component_boundary_left(chars: &[char], at: usize) -> usize {
    let mut state = None;
    let mut index = at.min(chars.len());
    while index > 0 {
        let Some(next) = step_left(state, class(chars[index - 1])) else {
            break;
        };
        state = Some(next);
        index -= 1;
    }
    index
}

/// Where the component after `at` ends, the mirror of
/// [`component_boundary_left`].
pub fn component_boundary_right(chars: &[char], at: usize) -> usize {
    let mut state = None;
    let mut index = at.min(chars.len());
    while index < chars.len() {
        let Some(next) = step_right(state, class(chars[index])) else {
            break;
        };
        state = Some(next);
        index += 1;
    }
    index
}

fn step_left(state: Option<Left>, class: Class) -> Option<Left> {
    match state {
        None => Some(match class {
            Class::Blank => Left::Space,
            Class::Component => Left::Component,
            Class::Separator => Left::Separators,
        }),
        Some(Left::Separators) => match class {
            Class::Blank => None,
            Class::Component => Some(Left::Component),
            Class::Separator => Some(Left::Separators),
        },
        Some(Left::Component) => matches!(class, Class::Component).then_some(Left::Component),
        Some(Left::Space) => Some(match class {
            Class::Blank => Left::Space,
            Class::Component => Left::Component,
            Class::Separator => Left::SpaceSeparators,
        }),
        Some(Left::SpaceSeparators) => {
            matches!(class, Class::Separator).then_some(Left::SpaceSeparators)
        }
    }
}

fn step_right(state: Option<Right>, class: Class) -> Option<Right> {
    match state {
        None | Some(Right::Component) => Some(match class {
            Class::Blank => Right::Blank,
            Class::Component => Right::Component,
            Class::Separator => Right::Separators,
        }),
        Some(Right::Separators) => match class {
            Class::Blank => Some(Right::Blank),
            Class::Component => None,
            Class::Separator => Some(Right::Separators),
        },
        Some(Right::Blank) => matches!(class, Class::Blank).then_some(Right::Blank),
    }
}

#[cfg(test)]
mod tests {
    use super::{component_boundary_left, component_boundary_right};
    use test_case::test_case;

    const MARK: char = '^';
    const STOPPED_SHORT: &str = "a walk stopped somewhere the marks do not";

    /// Fish's own notation: every `^` is a place a press must stop, and the
    /// text is what is left once they are cut out.
    fn marked(line: &str) -> (Vec<char>, Vec<usize>) {
        let mut chars = Vec::new();
        let mut stops = Vec::new();
        for ch in line.chars() {
            if ch == MARK {
                stops.push(chars.len());
            } else {
                chars.push(ch);
            }
        }
        (chars, stops)
    }

    /// Fish's `Left, PathComponents` vectors, minus the ones that only exercise
    /// its backslash escaping, which we spend on Windows paths instead.
    #[test_case("^echo ^/^foo/^bar{^aaa,^bbb,^ccc}^bak/^"; "braces_and_slashes")]
    #[test_case("^echo ^bak ^///^"; "a_run_of_slashes")]
    #[test_case("^aaa ^@ ^@^aaa^"; "an_at_of_its_own")]
    #[test_case("^aaa ^a ^@^aaa^"; "a_letter_of_its_own")]
    #[test_case("^aaa ^@@@ ^@@^aa^"; "runs_of_ats")]
    #[test_case("^aa^@@  ^aa@@^a^"; "ats_inside_and_outside")]
    #[test_case("^C:\\^Users\\^me^"; "a_windows_path")]
    #[test_case("^file.tar.gz^"; "dots_do_not_separate")]
    #[test_case("^kebab-case_and_snake^"; "dashes_and_underscores_do_not_separate")]
    #[test_case("^@^src/^main.rs^"; "a_mention_of_a_path")]
    fn a_left_walk_stops_where_fish_stops(line: &str) {
        let (chars, stops) = marked(line);
        let mut index = *stops.last().expect("a trailing mark");
        for expected in stops.iter().rev().skip(1) {
            index = component_boundary_left(&chars, index);
            assert_eq!(index, *expected, "{STOPPED_SHORT}: {line}");
        }
        assert_eq!(index, 0, "{STOPPED_SHORT}: {line}");
    }

    /// Fish's `Right, PathComponents` vectors.
    #[test_case("^/^foo/^bar/^baz/^"; "slashes")]
    #[test_case("^echo ^--foo ^--bar^"; "flags_keep_their_dashes")]
    #[test_case("^echo ^hi ^> ^/^dev/^null^"; "a_redirect")]
    #[test_case("^echo ^/^foo/^bar{^aaa,^ccc}^bak/^"; "braces_and_slashes")]
    #[test_case("^echo ^bak ^///^"; "a_run_of_slashes")]
    #[test_case("^aaa ^@ ^@^aaa^"; "an_at_of_its_own")]
    #[test_case("^aa@@ ^aa@@^a^"; "ats_inside_and_outside")]
    fn a_right_walk_stops_where_fish_stops(line: &str) {
        let (chars, stops) = marked(line);
        let mut index = stops[0];
        for expected in stops.iter().skip(1) {
            index = component_boundary_right(&chars, index);
            assert_eq!(index, *expected, "{STOPPED_SHORT}: {line}");
        }
        assert_eq!(index, chars.len(), "{STOPPED_SHORT}: {line}");
    }
}
