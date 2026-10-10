//! What counts as one path component, the way fish reads one.
//!
//! The delete chords follow fish's `backward-kill-path-component` rather than
//! a word delete: the run of separators trailing a component belongs to it, so
//! one press takes `bar/` out of `/foo/bar/` instead of leaving the slash
//! behind for a second press. Permission scopes cut a word at the same places,
//! so a rung like `repos/tensorninja/*` stops exactly where a press would.

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

pub fn is_separator(ch: char) -> bool {
    SEPARATORS.contains(ch)
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Blank
    } else if is_separator(ch) {
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

/// Every proper prefix of `word` that ends where a rightward press stops just
/// past a run of separators, shortest first: `repos/a/b` gives `repos/` and
/// `repos/a/`, while `file.tar.gz` and `src/` give none.
pub fn component_prefixes(word: &str) -> Vec<&str> {
    let chars: Vec<char> = word.chars().collect();
    let mut prefixes = Vec::new();
    let (mut at, mut bytes) = (0, 0);
    loop {
        let next = component_boundary_right(&chars, at);
        if next == at || next == chars.len() {
            return prefixes;
        }
        bytes += chars[at..next]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum::<usize>();
        if is_separator(chars[next - 1]) {
            prefixes.push(&word[..bytes]);
        }
        at = next;
    }
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
    use super::{component_boundary_left, component_boundary_right, component_prefixes};
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

    #[test_case("repos/a/b", &["repos/", "repos/a/"]; "a_relative_path")]
    #[test_case("/etc/hosts", &["/", "/etc/"]; "an_absolute_path")]
    #[test_case("tcp://host", &["tcp://"]; "a_run_of_separators_stays_whole")]
    #[test_case("--repo=a/b", &["--repo=", "--repo=a/"]; "a_flag_value")]
    #[test_case("user@host:dir", &["user@", "user@host:"]; "a_remote_path")]
    #[test_case("a b/c", &["a b/"]; "a_blank_is_no_separator")]
    #[test_case("ä/ö", &["ä/"]; "multibyte_components")]
    #[test_case("file.tar.gz", &[]; "dots_do_not_separate")]
    #[test_case("src/", &[]; "a_trailing_separator_has_nothing_after_it")]
    #[test_case("", &[]; "an_empty_word")]
    fn a_word_is_cut_after_each_run_of_separators(word: &str, expected: &[&str]) {
        assert_eq!(component_prefixes(word), expected);
    }
}
