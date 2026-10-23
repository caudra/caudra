//! The text helpers upstream's `lib.mjs` shares between its scripts: label wrapping and joining,
//! Mermaid quoting, rule sentences, and the chart's colors.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::mem;
use std::sync::LazyLock;

use regex::Regex;

pub const LEGEND_ID: &str = "legend";
pub const FULL_LABELS: &str = "%% labels in full";
pub const PLAIN_FILL: &str = "#f4f6f7";
pub const STATUS_CLASSES: [&str; 6] = [
    "classDef works fill:#f4f6f7,stroke:#566573,color:#1b2631",
    "classDef wrong fill:#fdedec,stroke:#c0392b,color:#7b241c",
    "classDef fixed fill:#e9f7ef,stroke:#1e8449,color:#145a32",
    "classDef worksPath fill:#f4f6f7,stroke:#1f3a93,stroke-width:4px,color:#1b2631",
    "classDef wrongPath fill:#fdedec,stroke:#1f3a93,stroke-width:4px,color:#7b241c",
    "classDef fixedPath fill:#e9f7ef,stroke:#1f3a93,stroke-width:4px,color:#145a32",
];
pub const GROUP_PALETTE: [&str; 7] = [
    "#d6eaf8", "#fdebd0", "#e8daef", "#fcf3cf", "#d1f2eb", "#ede0d4", "#f6e0ef",
];
/// The label width upstream wraps state names at.
pub const STATE_WIDTH: usize = 26;
/// The label width upstream wraps step labels at.
pub const STEP_WIDTH: usize = 30;

const BREAK: &str = "<br/>";
const OPENS_A_PHRASE: [&str; 21] = [
    "a", "an", "the", "your", "my", "our", "their", "its", "this", "that", "to", "of", "in", "on",
    "at", "for", "with", "from", "by", "and", "or",
];
const NOT_A_COLOR: &str = r##"is not "#rgb" or "#rrggbb""##;
const TOO_DARK: &str = "is too dark for the status borders to show on it";
const DARKEST_FILL: f64 = 0.5;
const PLAIN_STROKE: &str = "#566573";
const PLAIN_TEXT: &str = "#1b2631";
const BORDERS: [(&str, &str, &str); 3] = [
    ("Wrong", "stroke:#c0392b,stroke-width:4px", "#7b241c"),
    ("Fixed", "stroke:#1e8449,stroke-width:4px", "#145a32"),
    ("Path", "stroke:#1f3a93,stroke-width:4px", "#1b2631"),
];
/// ASCII punctuation in the order of the root collation that JavaScript's `localeCompare` uses;
/// whitespace sorts before it, and digits and then letters after it.
const COLLATED_PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

static RULE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s*==").expect("valid regex"));
static COMMENT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\\\*").expect("valid regex"));
static COMMENT_MARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\\\*\s?").expect("valid regex"));

/// A string's length as JavaScript counts it, in UTF-16 units.
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.split(/\s+/)`: a leading or trailing run of whitespace leaves an empty word.
pub fn split_spaces(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (at, character) in text.char_indices() {
        if character.is_whitespace() {
            if !in_space {
                words.push(&text[start..at]);
                in_space = true;
            }
        } else if in_space {
            start = at;
            in_space = false;
        }
    }
    words.push(if in_space { "" } else { &text[start..] });
    words
}

/// `text.replace(/\s+/g, " ").trim()`.
pub fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Breaks `text` into rows of at most `width` characters where it can, joined by `<br/>`.
pub fn wrap(text: &str, width: usize) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for word in split_spaces(text) {
        if !row.is_empty() && js_len(&row) + 1 + js_len(word) > width {
            rows.push(mem::replace(&mut row, word.to_owned()));
        } else if row.is_empty() {
            word.clone_into(&mut row);
        } else {
            row.push(' ');
            row.push_str(word);
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.join(BREAK)
}

pub fn escape_quotes(text: &str) -> String {
    text.replace('"', "#quot;")
}

/// The first character in upper case.
pub fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// "a and b", or "a, b, and c".
pub fn join_and(items: &[String]) -> String {
    match items {
        [.., last] if items.len() >= 3 => {
            format!("{}, and {last}", items[..items.len() - 1].join(", "))
        }
        _ => items.join(" and "),
    }
}

/// Joins labels by "or", writing the leading words they all share once: "you tap Pay or Back".
pub fn join_labels(labels: &[String]) -> String {
    if labels.len() < 2 {
        return labels.join(" or ");
    }
    let words: Vec<Vec<&str>> = labels
        .iter()
        .map(|label| label.split(' ').collect())
        .collect();
    let first = &words[0];
    let mut shared = 0;
    while words
        .iter()
        .all(|label| shared + 1 < label.len() && label[shared] == first[shared])
    {
        shared += 1;
    }
    while shared > 0 && OPENS_A_PHRASE.contains(&first[shared - 1]) {
        shared -= 1;
    }
    if shared < 2 {
        return labels.join(" or ");
    }
    let rest: Vec<String> = words
        .iter()
        .map(|label| label[shared..].join(" "))
        .collect();
    let list = match rest.as_slice() {
        [one, two] => format!("{one} or {two}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
        [] => String::new(),
    };
    format!("{} {list}", first[..shared].join(" "))
}

/// Each rule's sentence: the `\*` comment lines right above its definition.
pub fn rule_sentences(tla: &str) -> HashMap<String, String> {
    let lines: Vec<&str> = tla.split('\n').collect();
    let mut sentences = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(name) = RULE_NAME.captures(line).and_then(|found| found.get(1)) else {
            continue;
        };
        let said: Vec<String> = lines[..index]
            .iter()
            .rev()
            .take_while(|above| COMMENT_LINE.is_match(above))
            .map(|above| COMMENT_MARK.replace(above, "").trim().to_owned())
            .filter(|words| !words.is_empty())
            .collect();
        if !said.is_empty() {
            let sentence: Vec<String> = said.into_iter().rev().collect();
            sentences.insert(name.as_str().to_owned(), sentence.join(" "));
        }
    }
    sentences
}

/// Why `color` cannot fill a group, or `None` when it can.
pub fn group_color_problem(color: &str) -> Option<&'static str> {
    let Some(channels) = rgb(color) else {
        return Some(NOT_A_COLOR);
    };
    (relative_luminance(channels) < DARKEST_FILL).then_some(TOO_DARK)
}

/// The class for states of one fill with one border: "", "Wrong", "Fixed" or "Path".
pub fn group_class_def(base: &str, fill: &str, border: &str) -> String {
    let (stroke, color) = match BORDERS.iter().find(|(name, ..)| *name == border) {
        Some((_, stroke, color)) => ((*stroke).to_owned(), *color),
        None if fill == PLAIN_FILL => (format!("stroke:{PLAIN_STROKE}"), PLAIN_TEXT),
        None => (format!("stroke:{}", darker(fill)), PLAIN_TEXT),
    };
    format!("classDef {base}{border} fill:{fill},{stroke},color:{color}")
}

/// JavaScript's `a.localeCompare(b)` for the labels a chart holds: letters compare without case
/// first, punctuation sorts before digits, and case breaks a tie with lower case first.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    let primary = |text: &str| text.chars().map(collation_weight).collect::<Vec<_>>();
    primary(a).cmp(&primary(b)).then_with(|| {
        let case = |text: &str| text.chars().map(char::is_uppercase).collect::<Vec<_>>();
        case(a).cmp(&case(b))
    })
}

fn collation_weight(character: char) -> (u8, u32) {
    if character.is_whitespace() {
        return (0, character as u32);
    }
    if let Some(at) = COLLATED_PUNCTUATION.find(character) {
        return (1, at as u32);
    }
    if character.is_ascii_digit() {
        return (2, character as u32);
    }
    if character.is_ascii_alphabetic() {
        return (3, character.to_ascii_lowercase() as u32);
    }
    let folded = character.to_lowercase().next().unwrap_or(character);
    (4, folded as u32)
}

fn rgb(color: &str) -> Option<[u8; 3]> {
    let hex = color.strip_prefix('#')?;
    if !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    let full: String = match hex.len() {
        3 => hex.chars().flat_map(|digit| [digit, digit]).collect(),
        6 => hex.to_owned(),
        _ => return None,
    };
    let channel = |at: usize| u8::from_str_radix(&full[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

fn relative_luminance(channels: [u8; 3]) -> f64 {
    let [red, green, blue] = channels.map(|channel| {
        let share = f64::from(channel) / 255.0;
        if share <= 0.03928 {
            share / 12.92
        } else {
            ((share + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * red + 0.7152 * green + 0.0722 * blue
}

fn darker(fill: &str) -> String {
    let channels = rgb(fill).unwrap_or_default();
    let halved: String = channels
        .iter()
        .map(|channel| format!("{:02x}", channel.div_ceil(2)))
        .collect();
    format!("#{halved}")
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_rules_sentence_is_the_comment_lines_right_above_it() {
        let sentences = rule_sentences(
            "---- MODULE M ----\n\\* Not about a rule.\n\n\\* You never see \"Saved\"\n\\* while the note is a draft.\nNoFalseSaved == TRUE\nBare == TRUE\n====",
        );

        assert_eq!(
            sentences,
            HashMap::from([(
                "NoFalseSaved".to_owned(),
                "You never see \"Saved\" while the note is a draft.".to_owned()
            )])
        );
    }

    #[test_case(&["you tap Pay", "you tap Back", "you tap Home"] => "you tap Pay, Back, or Home"; "three share their leading words")]
    #[test_case(&["you open the thread", "you open the phone app"] => "you open the thread or the phone app"; "a phrase keeps its article")]
    #[test_case(&["the network drops", "you edit a note"] => "the network drops or you edit a note"; "nothing shared")]
    #[test_case(&["you tap", "you tap Pay"] => "you tap or you tap Pay"; "one label is all shared words")]
    fn labels_join(labels: &[&str]) -> String {
        join_labels(&owned(labels))
    }

    #[test_case("Uploading.Checking the file", 26 => "Uploading.Checking the<br/>file"; "breaks at the width")]
    #[test_case("you edit a note", 30 => "you edit a note"; "fits")]
    #[test_case("averyveryveryverylongwordthatoverflows x", 10 => "averyveryveryverylongwordthatoverflows<br/>x"; "a long word keeps its row")]
    #[test_case(" a", 26 => "a"; "leading space")]
    fn wraps(text: &str, width: usize) -> String {
        wrap(text, width)
    }

    #[test_case(&["a"] => "a"; "one")]
    #[test_case(&["a", "b"] => "a and b"; "two")]
    #[test_case(&["a", "b", "c"] => "a, b, and c"; "three")]
    fn joins_with_and(items: &[&str]) -> String {
        join_and(&owned(items))
    }

    #[test_case("#d1f2eb" => None; "light")]
    #[test_case("#123" => Some(TOO_DARK); "dark")]
    #[test_case("red" => Some(NOT_A_COLOR); "a name")]
    #[test_case("#12345" => Some(NOT_A_COLOR); "five digits")]
    fn group_colors(color: &str) -> Option<&'static str> {
        group_color_problem(color)
    }

    #[test_case("group0", "#d1f2eb", "" => "classDef group0 fill:#d1f2eb,stroke:#697976,color:#1b2631"; "darker stroke")]
    #[test_case("plain", PLAIN_FILL, "" => "classDef plain fill:#f4f6f7,stroke:#566573,color:#1b2631"; "plain")]
    #[test_case("plain", PLAIN_FILL, "Wrong" => "classDef plainWrong fill:#f4f6f7,stroke:#c0392b,stroke-width:4px,color:#7b241c"; "wrong border")]
    fn class_defs(base: &str, fill: &str, border: &str) -> String {
        group_class_def(base, fill, border)
    }

    #[test_case("Before any run", "chart of the spec" => Ordering::Less; "case does not sort first")]
    #[test_case("Clean(1)", "Clean(2)" => Ordering::Less; "digits")]
    #[test_case("a b", "a(b" => Ordering::Less; "space before punctuation")]
    #[test_case("you tap", "You tap" => Ordering::Less; "lower case first on a tie")]
    fn collates_as_javascript(a: &str, b: &str) -> Ordering {
        locale_cmp(a, b)
    }
}
