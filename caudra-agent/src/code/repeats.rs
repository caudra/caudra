//! Whether what a snippet printed or ended on only repeats what one of its calls answered, so the
//! card can show that call's own card open beside the dump, which it draws folded to a tree. The
//! model's text is never changed.
//!
//! Only a dict or a list counts, printed as its `repr`, since that is what folds to a tree. Where
//! the `repr` here could differ from the interpreter's, it gives up, and the card stays folded.

use std::cmp::Reverse;
use std::iter;
use std::ops::Range;

use serde_json::{Map, Number, Value};

const NONE: &str = "None";
const TRUE: &str = "True";
const FALSE: &str = "False";
const SINGLE_QUOTE: char = '\'';
const DOUBLE_QUOTE: char = '"';
const ITEM_SEPARATOR: &str = ", ";
const KEY_SEPARATOR: &str = ": ";
const LINE_BREAK: char = '\n';
const INTEGRAL_FLOAT_SUFFIX: &str = ".0";
/// Python writes a float with an exponent outside this range.
const PLAIN_FLOAT_RANGE: Range<f64> = 1e-4..1e16;
const EXPONENT_MARK: char = 'e';
/// Python writes an exponent with a sign and at least two digits, as `1e-06`.
const EXPONENT_DIGITS: usize = 2;
const BYTE_ESCAPE_LIMIT: u32 = 0x100;
const BMP_ESCAPE_LIMIT: u32 = 0x1_0000;
/// Characters Python escapes in a `repr` that a cheap test cannot tell from printable ones.
const UNCERTAIN_RANGES: &[(char, char)] = &[
    ('\u{00A0}', '\u{00A0}'),
    ('\u{00AD}', '\u{00AD}'),
    ('\u{1680}', '\u{1680}'),
    ('\u{180E}', '\u{180E}'),
    ('\u{2000}', '\u{200F}'),
    ('\u{2028}', '\u{202F}'),
    ('\u{205F}', '\u{206F}'),
    ('\u{3000}', '\u{3000}'),
    ('\u{E000}', '\u{F8FF}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFF0}', '\u{FFFF}'),
    ('\u{E0000}', '\u{10FFFF}'),
];

/// What one call answered, by its index among the run's calls.
pub type Answer = (usize, Value);

/// The calls whose answers `stdout` prints and nothing else, in the order first printed. Empty
/// when any of it is something else.
pub fn printed_repeats(stdout: &str, answers: &[Answer]) -> Vec<usize> {
    let text = stdout.strip_suffix(LINE_BREAK).unwrap_or(stdout);
    if text.is_empty() {
        return Vec::new();
    }
    let mut forms: Vec<(usize, String)> = candidates(answers)
        .filter_map(|(call, value)| python_repr(value).map(|form| (call, form)))
        .collect();
    forms.sort_by_key(|(_, form)| Reverse(form.len()));
    let mut calls = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let Some((call, after)) = forms.iter().find_map(|(call, form)| {
            let after = rest.strip_prefix(form.as_str())?;
            match after.strip_prefix(LINE_BREAK) {
                Some(next) => Some((*call, next)),
                None if after.is_empty() => Some((*call, after)),
                None => None,
            }
        }) else {
            return Vec::new();
        };
        if !calls.contains(&call) {
            calls.push(call);
        }
        rest = after;
    }
    calls
}

/// The call whose answer the snippet ended on, when it ended on nothing else.
pub fn result_repeats(result: &Value, answers: &[Answer]) -> Option<usize> {
    candidates(answers)
        .find(|(_, value)| *value == result)
        .map(|(call, _)| call)
}

/// Each answer whole and each of an object's fields, as `hits = (...)["hits"]` keeps one, when it
/// is a dict or a list with something in it. A scalar print says something of its own.
fn candidates(answers: &[Answer]) -> impl Iterator<Item = (usize, &Value)> {
    answers
        .iter()
        .flat_map(|(call, value)| {
            let fields = value.as_object().into_iter().flat_map(Map::values);
            iter::once(value)
                .chain(fields)
                .map(move |value| (*call, value))
        })
        .filter(|(_, value)| is_tree(value))
}

fn is_tree(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

/// Python's `repr` of `value` as the interpreter builds it from JSON, or `None` where this could
/// write something else.
pub fn python_repr(value: &Value) -> Option<String> {
    let mut out = String::new();
    write_repr(&mut out, value)?;
    Some(out)
}

fn write_repr(out: &mut String, value: &Value) -> Option<()> {
    match value {
        Value::Null => out.push_str(NONE),
        Value::Bool(true) => out.push_str(TRUE),
        Value::Bool(false) => out.push_str(FALSE),
        Value::Number(number) => out.push_str(&number_repr(number)?),
        Value::String(text) => write_str_repr(out, text)?,
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(ITEM_SEPARATOR);
                }
                write_repr(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(ITEM_SEPARATOR);
                }
                write_str_repr(out, key)?;
                out.push_str(KEY_SEPARATOR);
                write_repr(out, item)?;
            }
            out.push('}');
        }
    }
    Some(())
}

fn number_repr(number: &Number) -> Option<String> {
    if number.is_i64() || number.is_u64() {
        return Some(number.to_string());
    }
    let float = number.as_f64()?;
    let magnitude = float.abs();
    if float.fract() == 0.0 && magnitude < PLAIN_FLOAT_RANGE.end {
        return Some(format!("{float}{INTEGRAL_FLOAT_SUFFIX}"));
    }
    if PLAIN_FLOAT_RANGE.contains(&magnitude) {
        return Some(format!("{float}"));
    }
    let scientific = format!("{float:e}");
    let (mantissa, exponent) = scientific.split_once(EXPONENT_MARK)?;
    let exponent: i32 = exponent.parse().ok()?;
    let sign = match exponent < 0 {
        true => '-',
        false => '+',
    };
    let digits = exponent.unsigned_abs();
    Some(format!(
        "{mantissa}{EXPONENT_MARK}{sign}{digits:0EXPONENT_DIGITS$}"
    ))
}

fn write_str_repr(out: &mut String, text: &str) -> Option<()> {
    let quote = match text.contains(SINGLE_QUOTE) && !text.contains(DOUBLE_QUOTE) {
        true => DOUBLE_QUOTE,
        false => SINGLE_QUOTE,
    };
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character == quote => {
                out.push('\\');
                out.push(character);
            }
            character if character.is_control() => out.push_str(&escaped(character)),
            character if is_uncertain(character) => return None,
            character => out.push(character),
        }
    }
    out.push(quote);
    Some(())
}

fn escaped(character: char) -> String {
    let code = u32::from(character);
    match code {
        code if code < BYTE_ESCAPE_LIMIT => format!("\\x{code:02x}"),
        code if code < BMP_ESCAPE_LIMIT => format!("\\u{code:04x}"),
        code => format!("\\U{code:08x}"),
    }
}

fn is_uncertain(character: char) -> bool {
    UNCERTAIN_RANGES
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&character))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const ENVIRONMENT: usize = 0;
    const SEARCH: usize = 1;
    const NOT_A_REPEAT: &str = "the print says something the calls did not answer";

    #[test_case(json!(null), "None" ; "none")]
    #[test_case(json!(true), "True" ; "true")]
    #[test_case(json!(-3), "-3" ; "integer")]
    #[test_case(json!(1.0), "1.0" ; "integral_float")]
    #[test_case(json!(0.5), "0.5" ; "fraction")]
    #[test_case(json!(1e-6), "1e-06" ; "tiny_float")]
    #[test_case(json!(-2.5e20), "-2.5e+20" ; "huge_float")]
    #[test_case(json!(1e100), "1e+100" ; "three_exponent_digits")]
    #[test_case(json!(0.0001), "0.0001" ; "smallest_plain_float")]
    #[test_case(json!("it's"), "\"it's\"" ; "single_quote_takes_double_quotes")]
    #[test_case(json!("say \"hi\" it's"), "'say \"hi\" it\\'s'" ; "both_quotes_escape_the_single")]
    #[test_case(json!("a\nb\tc\\"), "'a\\nb\\tc\\\\'" ; "named_escapes")]
    #[test_case(json!("\u{7}\u{85}"), "'\\x07\\x85'" ; "controls_escape")]
    #[test_case(json!("café 界"), "'café 界'" ; "printable_unicode_stays")]
    #[test_case(json!("«retry»"), "'«retry»'" ; "latin_quotes_stay")]
    #[test_case(json!([]), "[]" ; "empty_list")]
    #[test_case(
        json!({"runtime": "workcell", "ok": false, "commands": [{"id": "bash", "version": null}]}),
        "{'runtime': 'workcell', 'ok': False, 'commands': [{'id': 'bash', 'version': None}]}" ;
        "nested_in_key_order"
    )]
    fn a_value_reprs_as_python_writes_it(value: Value, expected: &str) {
        assert_eq!(python_repr(&value).as_deref(), Some(expected));
    }

    #[test_case(json!("no\u{a0}break") ; "uncertain_space")]
    #[test_case(json!({"k": "zero\u{200b}width"}) ; "uncertain_inside_a_dict")]
    fn an_uncertain_repr_gives_up(value: Value) {
        assert_eq!(python_repr(&value), None);
    }

    fn answers() -> Vec<Answer> {
        vec![
            (
                ENVIRONMENT,
                json!({"runtime": "workcell", "commands": [{"id": "bash"}]}),
            ),
            (
                SEARCH,
                json!({"summary": "1 hit", "hits": [{"ref": "a", "snippet": "x"}]}),
            ),
        ]
    }

    #[test_case("{'runtime': 'workcell', 'commands': [{'id': 'bash'}]}\n", vec![ENVIRONMENT] ; "a_whole_answer")]
    #[test_case("[{'ref': 'a', 'snippet': 'x'}]\n", vec![SEARCH] ; "a_field_of_one")]
    #[test_case(
        "[{'id': 'bash'}]\n{'summary': '1 hit', 'hits': [{'ref': 'a', 'snippet': 'x'}]}\n",
        vec![ENVIRONMENT, SEARCH] ;
        "two_prints_in_order"
    )]
    #[test_case(
        "[{'id': 'bash'}]\n[{'id': 'bash'}]",
        vec![ENVIRONMENT] ;
        "a_repeat_counts_once"
    )]
    fn a_print_of_answers_repeats_them(stdout: &str, expected: Vec<usize>) {
        assert_eq!(printed_repeats(stdout, &answers()), expected);
    }

    #[test_case("1 hit\n" ; "a_scalar_field")]
    #[test_case("found [{'ref': 'a', 'snippet': 'x'}]\n" ; "a_formatted_print")]
    #[test_case("[{'ref': 'a', 'snippet': 'x'}]\ndone\n" ; "trailing_words")]
    #[test_case("2\n" ; "a_count")]
    #[test_case("" ; "nothing")]
    fn anything_else_is_no_repeat(stdout: &str) {
        assert!(
            printed_repeats(stdout, &answers()).is_empty(),
            "{NOT_A_REPEAT}"
        );
    }

    #[test]
    fn a_text_answer_is_no_repeat() {
        let answers = vec![(ENVIRONMENT, json!("line one\nline 'two'"))];
        assert!(
            printed_repeats("line one\nline 'two'\n", &answers).is_empty(),
            "{NOT_A_REPEAT}"
        );
    }

    #[test_case(json!([{"ref": "a", "snippet": "x"}]), Some(SEARCH) ; "a_field")]
    #[test_case(json!({"runtime": "workcell", "commands": [{"id": "bash"}]}), Some(ENVIRONMENT) ; "a_whole_answer")]
    #[test_case(json!("1 hit"), None ; "a_scalar")]
    #[test_case(json!(null), None ; "none")]
    fn an_ending_value_repeats_an_answer(result: Value, expected: Option<usize>) {
        assert_eq!(result_repeats(&result, &answers()), expected);
    }
}
