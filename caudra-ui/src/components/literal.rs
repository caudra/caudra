//! A dict or a list as a snippet prints it, read back to a value so the `code` card can draw it as
//! a tree. `print(r)` writes Python's `repr` and `json.dumps(r)` writes JSON, so this reads both:
//! either quote, `True`/`true`, `None`/`null`, and a tuple as a list. Anything it cannot read
//! whole, such as a set, bytes or an object's `repr`, stays text.

use std::ops::Range;
use std::str::CharIndices;

use serde_json::{Map, Number, Value};

/// Deeper than any answer worth a tree, and shallow enough that a hostile print cannot exhaust
/// the stack.
const MAX_DEPTH: usize = 128;
const OPENERS: [char; 3] = ['{', '[', '('];
const SINGLE_QUOTE: char = '\'';
const DOUBLE_QUOTE: char = '"';
const ESCAPE: char = '\\';
const LINE_BREAK: char = '\n';
const ITEM_SEPARATOR: char = ',';
const KEY_SEPARATOR: char = ':';
const BYTE_DIGITS: usize = 2;
const BMP_DIGITS: usize = 4;
const WIDE_DIGITS: usize = 8;
const HEX_RADIX: u32 = 16;
/// What a number holds besides digits and letters: a sign, a point, an exponent's sign.
const NUMBER_PUNCTUATION: &str = "+-.";
const HIGH_SURROGATES: Range<u32> = 0xD800..0xDC00;
const LOW_SURROGATES: Range<u32> = 0xDC00..0xE000;
const SURROGATE_BASE: u32 = 0x1_0000;
const SURROGATE_SHIFT: u32 = 10;

/// `text` as a dict or a list with something in it, when that is all it holds.
pub(crate) fn tree(text: &str) -> Option<Value> {
    let text = text.trim();
    if !text.starts_with(OPENERS) {
        return None;
    }
    let mut parser = Parser {
        rest: text,
        depth: 0,
    };
    let value = parser.value()?;
    let whole = parser.rest.trim_start().is_empty();
    let filled = match &value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    };
    (whole && filled).then_some(value)
}

struct Parser<'a> {
    rest: &'a str,
    depth: usize,
}

impl Parser<'_> {
    fn eat(&mut self, token: char) -> bool {
        match self.rest.trim_start().strip_prefix(token) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.rest = self.rest.trim_start();
        match self.rest.chars().next()? {
            '{' => self.nested(Self::dict),
            '[' => self.nested(|parser| parser.sequence(']')),
            '(' => self.nested(|parser| parser.sequence(')')),
            SINGLE_QUOTE | DOUBLE_QUOTE => self.string().map(Value::String),
            _ => self.word(),
        }
    }

    fn nested(&mut self, read: impl FnOnce(&mut Self) -> Option<Value>) -> Option<Value> {
        if self.depth == MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        self.rest = &self.rest[1..];
        let value = read(self);
        self.depth -= 1;
        value
    }

    fn sequence(&mut self, close: char) -> Option<Value> {
        let mut items = Vec::new();
        loop {
            if self.eat(close) {
                return Some(Value::Array(items));
            }
            items.push(self.value()?);
            if !self.eat(ITEM_SEPARATOR) {
                return self.eat(close).then_some(Value::Array(items));
            }
        }
    }

    fn dict(&mut self) -> Option<Value> {
        let mut map = Map::new();
        loop {
            if self.eat('}') {
                return Some(Value::Object(map));
            }
            let key = self.key()?;
            if !self.eat(KEY_SEPARATOR) {
                return None;
            }
            map.insert(key, self.value()?);
            if !self.eat(ITEM_SEPARATOR) {
                return self.eat('}').then_some(Value::Object(map));
            }
        }
    }

    /// A string key as itself, and a number or a constant as Python wrote it, since JSON keys
    /// are strings.
    fn key(&mut self) -> Option<String> {
        self.rest = self.rest.trim_start();
        let before = self.rest;
        match self.value()? {
            Value::String(key) => Some(key),
            Value::Array(_) | Value::Object(_) => None,
            _ => Some(before[..before.len() - self.rest.len()].to_owned()),
        }
    }

    fn string(&mut self) -> Option<String> {
        let mut chars = self.rest.char_indices();
        let (_, quote) = chars.next()?;
        let mut text = String::new();
        while let Some((at, character)) = chars.next() {
            match character {
                character if character == quote => {
                    self.rest = &self.rest[at + character.len_utf8()..];
                    return Some(text);
                }
                ESCAPE => text.push(escaped(&mut chars)?),
                LINE_BREAK => return None,
                character => text.push(character),
            }
        }
        None
    }

    fn word(&mut self) -> Option<Value> {
        let end = self
            .rest
            .find(|character: char| {
                !(character.is_ascii_alphanumeric() || NUMBER_PUNCTUATION.contains(character))
            })
            .unwrap_or(self.rest.len());
        let (word, rest) = self.rest.split_at(end);
        let value = match word {
            "True" | "true" => Value::Bool(true),
            "False" | "false" => Value::Bool(false),
            "None" | "null" => Value::Null,
            number => Value::Number(number.parse::<Number>().ok()?),
        };
        self.rest = rest;
        Some(value)
    }
}

/// The character an escape after a backslash stands for, in Python's spelling or JSON's.
fn escaped(chars: &mut CharIndices) -> Option<char> {
    let (_, kind) = chars.next()?;
    let character = match kind {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'v' => '\u{b}',
        'x' => char::from_u32(hex(chars, BYTE_DIGITS)?)?,
        'U' => char::from_u32(hex(chars, WIDE_DIGITS)?)?,
        'u' => {
            let code = hex(chars, BMP_DIGITS)?;
            match HIGH_SURROGATES.contains(&code) {
                true => paired(chars, code)?,
                false => char::from_u32(code)?,
            }
        }
        ESCAPE | SINGLE_QUOTE | DOUBLE_QUOTE | '/' => kind,
        _ => return None,
    };
    Some(character)
}

/// JSON writes a character beyond the first plane as two escapes, and the second must follow.
fn paired(chars: &mut CharIndices, high: u32) -> Option<char> {
    let (_, slash) = chars.next()?;
    let (_, kind) = chars.next()?;
    if (slash, kind) != (ESCAPE, 'u') {
        return None;
    }
    let low = hex(chars, BMP_DIGITS).filter(|low| LOW_SURROGATES.contains(low))?;
    let code = SURROGATE_BASE
        + ((high - HIGH_SURROGATES.start) << SURROGATE_SHIFT)
        + (low - LOW_SURROGATES.start);
    char::from_u32(code)
}

fn hex(chars: &mut CharIndices, digits: usize) -> Option<u32> {
    let digits: String = chars.take(digits).map(|(_, digit)| digit).collect();
    u32::from_str_radix(&digits, HEX_RADIX).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test_case("{'runtime': 'workcell', 'ok': False, 'commands': [{'id': 'bash', 'version': None}]}", json!({"runtime": "workcell", "ok": false, "commands": [{"id": "bash", "version": null}]}) ; "a_python_repr")]
    #[test_case("{\n  \"a\": true,\n  \"b\": [1, 2.5, -3e+20]\n}", json!({"a": true, "b": [1, 2.5, -3e20]}) ; "indented_json")]
    #[test_case("[(1, 2), (3,)]", json!([[1, 2], [3]]) ; "tuples_read_as_lists")]
    #[test_case("{1: 'one', None: 'none'}", json!({"1": "one", "None": "none"}) ; "keys_as_python_wrote_them")]
    #[test_case(r#"["it's", 'say "hi"', 'a\'b']"#, json!(["it's", "say \"hi\"", "a'b"]) ; "either_quote")]
    #[test_case(r"['a\nb\tc\\', '\x07\u00e9\U0001f600']", json!(["a\nb\tc\\", "\u{7}é😀"]) ; "python_escapes")]
    #[test_case(r#"["\ud83d\ude00", "a\/b"]"#, json!(["😀", "a/b"]) ; "json_escapes")]
    #[test_case("  [1, 2,]\n", json!([1, 2]) ; "a_trailing_comma_and_space")]
    #[test_case("{'café': '界'}", json!({"café": "界"}) ; "unicode_stays")]
    fn a_printed_value_reads_back(text: &str, expected: Value) {
        assert_eq!(tree(text), Some(expected));
    }

    #[test_case("plain words" ; "text")]
    #[test_case("42" ; "a_scalar")]
    #[test_case("[]" ; "an_empty_list")]
    #[test_case("{}" ; "an_empty_dict")]
    #[test_case("{1, 2}" ; "a_set")]
    #[test_case("[b'raw']" ; "bytes")]
    #[test_case("[datetime.date(2026, 1, 1)]" ; "an_objects_repr")]
    #[test_case("[1, 2] and more" ; "trailing_words")]
    #[test_case("[1, 2]\n[3]" ; "two_values")]
    #[test_case("[float('inf'), nan]" ; "not_a_number")]
    #[test_case("['open" ; "an_unclosed_string")]
    #[test_case("['a\nb']" ; "a_raw_line_break_in_a_string")]
    #[test_case(r#"["\ud83d"]"#; "a_lone_surrogate")]
    fn anything_else_stays_text(text: &str) {
        assert_eq!(tree(text), None);
    }

    #[test]
    fn a_print_too_deep_stays_text() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert_eq!(tree(&deep), None);
    }
}
