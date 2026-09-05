//! The argument a file-mutating tool spends its whole stream writing.
//!
//! A call that rewrites a file is nearly all body: the path arrives in the
//! first fragment and everything after it is the content, the patch, or the
//! text a match is replaced with. Decoding that value as its fragments go past
//! is what lets a card show the change being written instead of a spinner and
//! a line count.
//!
//! The decode is resumable rather than a scan per fragment, which
//! [`super::tool_preview`] can afford only because it gives up after a few
//! kilobytes. A body is the opposite case: it is the long part, so rescanning
//! it once per token is quadratic. Resuming also solves the problem a scan
//! would have anyway, which is that a `\` escape or a `\uXXXX` sequence can
//! straddle two fragments.

use crate::types::{ToolBodyDelta, ToolBodyField};

use super::tool_preview::{candidates, same_key};

/// Stops carrying a runaway body once no reader could follow it. Matches the
/// cap on live shell output. The line count keeps going, so a huge write still
/// reports that it is making progress.
const LIVE_BODY_MAX_BYTES: usize = 64 * 1024;
/// Digits in a `\uXXXX` escape.
const UNICODE_ESCAPE_DIGITS: u8 = 4;

/// The arguments each tool streams, in the order a call writes them. Every
/// other tool leads with a path, a pattern or a query, and has nothing long
/// enough to be worth showing before it runs.
const BODY_FIELDS: &[(&str, &[ToolBodyField])] = &[
    ("file_write", &[ToolBodyField::Content]),
    (
        "file_edit",
        &[ToolBodyField::OldString, ToolBodyField::NewString],
    ),
    ("file_apply_patch", &[ToolBodyField::PatchText]),
];

/// The arguments `tool` streams, empty for a tool with no body to show. Names
/// are matched the way [`super::tool_preview`] matches them, so `mcp_File_write`
/// and `file_write` resolve alike.
fn body_fields(tool: &str) -> &'static [ToolBodyField] {
    candidates(tool)
        .find_map(|rest| {
            BODY_FIELDS
                .iter()
                .find(|(name, _)| same_key(name, rest))
                .map(|(_, fields)| *fields)
        })
        .unwrap_or_default()
}

/// One decoded character, or the fact that the escape it belongs to has not
/// produced one yet.
enum Piece {
    Char(char),
    Pending,
    /// The closing quote.
    End,
}

/// Decodes a JSON string body one character at a time, the opening quote
/// already consumed.
#[derive(Default)]
struct StringReader {
    escaped: bool,
    /// The code point built so far and how many of its digits have arrived.
    unicode: Option<(u32, u8)>,
}

impl StringReader {
    fn push(&mut self, c: char) -> Piece {
        if let Some((code, seen)) = self.unicode {
            // A lone surrogate or a truncated escape contributes nothing
            // rather than poisoning the body with a replacement character.
            let Some(digit) = c.to_digit(16) else {
                self.unicode = None;
                return Piece::Pending;
            };
            let (code, seen) = (code * 16 + digit, seen + 1);
            if seen < UNICODE_ESCAPE_DIGITS {
                self.unicode = Some((code, seen));
                return Piece::Pending;
            }
            self.unicode = None;
            return char::from_u32(code).map_or(Piece::Pending, Piece::Char);
        }
        if self.escaped {
            self.escaped = false;
            return match c {
                'n' => Piece::Char('\n'),
                't' => Piece::Char('\t'),
                'r' => Piece::Char('\r'),
                'b' => Piece::Char('\u{8}'),
                'f' => Piece::Char('\u{c}'),
                'u' => {
                    self.unicode = Some((0, 0));
                    Piece::Pending
                }
                // `"`, `\` and `/` stand for themselves.
                other => Piece::Char(other),
            };
        }
        match c {
            '"' => Piece::End,
            '\\' => {
                self.escaped = true;
                Piece::Pending
            }
            other => Piece::Char(other),
        }
    }
}

/// Where the reader is in the argument object.
#[derive(Clone, Copy)]
enum State {
    /// Before the opening brace.
    Open,
    /// Between members: a quote starts a key, a closing brace ends the object.
    Member,
    Key,
    /// A key has been read; waiting for its colon.
    Colon,
    /// Waiting for the first character of a value.
    Value,
    /// Decoding the value of a wanted key.
    Body(ToolBodyField),
    /// Discarding a string, which is either an unwanted value or one nested
    /// inside one, so its quotes and braces cannot be mistaken for structure.
    SkipString(usize),
    /// Discarding a non-string value, counting the containers still open.
    Skip(usize),
    /// The object closed.
    Done,
}

/// The body of one tool call as its argument fragments arrive, retaining only
/// what has not been handed to the caller yet.
pub(super) struct BodyStream {
    fields: &'static [ToolBodyField],
    state: State,
    string: StringReader,
    key: String,
    newlines: usize,
    /// Decoded bytes handed out so far, against [`LIVE_BODY_MAX_BYTES`].
    emitted: usize,
}

impl BodyStream {
    /// `None` for a tool with no body worth streaming.
    pub(super) fn new(tool: &str) -> Option<Self> {
        let fields = body_fields(tool);
        (!fields.is_empty()).then(|| Self {
            fields,
            state: State::Open,
            string: StringReader::default(),
            key: String::new(),
            newlines: 0,
            emitted: 0,
        })
    }

    /// What this fragment added, per argument. Usually empty or one entry; a
    /// fragment that closes one argument and opens the next carries both.
    pub(super) fn absorb(&mut self, delta: &str) -> Vec<ToolBodyDelta> {
        let mut decoded = Vec::new();
        for c in delta.chars() {
            self.push(c, &mut decoded);
        }
        decoded
    }

    /// Lines of body decoded so far. A body whose last line has no newline of
    /// its own still occupies a line, which is why this is one more than the
    /// newline count.
    pub(super) fn lines(&self) -> usize {
        self.newlines + 1
    }

    fn push(&mut self, c: char, decoded: &mut Vec<ToolBodyDelta>) {
        self.state = match self.state {
            State::Done => State::Done,
            State::Open => match c {
                '{' => State::Member,
                _ => State::Open,
            },
            State::Member => match c {
                '"' => {
                    self.key.clear();
                    self.string = StringReader::default();
                    State::Key
                }
                '}' => State::Done,
                _ => State::Member,
            },
            State::Key => match self.string.push(c) {
                Piece::Char(c) => {
                    self.key.push(c);
                    State::Key
                }
                Piece::Pending => State::Key,
                Piece::End => State::Colon,
            },
            State::Colon => match c {
                ':' => State::Value,
                _ => State::Colon,
            },
            State::Value if c.is_whitespace() => State::Value,
            State::Value => {
                self.string = StringReader::default();
                match c {
                    '"' => match self.wanted() {
                        Some(field) => State::Body(field),
                        None => State::SkipString(0),
                    },
                    '{' | '[' => State::Skip(1),
                    _ => State::Skip(0),
                }
            }
            State::Body(field) => match self.string.push(c) {
                Piece::Char(c) => {
                    self.newlines += usize::from(c == '\n');
                    if self.emitted < LIVE_BODY_MAX_BYTES {
                        self.emitted += c.len_utf8();
                        extend(decoded, field, c);
                    }
                    State::Body(field)
                }
                Piece::Pending => State::Body(field),
                Piece::End => State::Skip(0),
            },
            State::SkipString(depth) => match self.string.push(c) {
                Piece::End => State::Skip(depth),
                _ => State::SkipString(depth),
            },
            // Depth zero means the value is over: what follows is either the
            // comma before the next member or the brace closing the object.
            State::Skip(0) => match c {
                ',' => State::Member,
                '}' => State::Done,
                _ => State::Skip(0),
            },
            State::Skip(depth) => match c {
                '"' => {
                    self.string = StringReader::default();
                    State::SkipString(depth)
                }
                '{' | '[' => State::Skip(depth + 1),
                '}' | ']' => State::Skip(depth - 1),
                _ => State::Skip(depth),
            },
        };
    }

    fn wanted(&self) -> Option<ToolBodyField> {
        self.fields
            .iter()
            .copied()
            .find(|field| same_key(field.key(), &self.key))
    }
}

/// Keeps one entry per contiguous run of an argument, so a fragment wholly
/// inside one body is a single push and a single allocation.
fn extend(decoded: &mut Vec<ToolBodyDelta>, field: ToolBodyField, c: char) {
    match decoded.last_mut() {
        Some(last) if last.field == field => last.text.push(c),
        _ => decoded.push(ToolBodyDelta {
            field,
            text: c.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{BodyStream, LIVE_BODY_MAX_BYTES, body_fields};
    use crate::types::ToolBodyField;
    use test_case::test_case;

    const WRITE: &str = "file_write";
    const EDIT: &str = "file_edit";
    const PATCH: &str = "file_apply_patch";

    /// Everything the fragments decode into, per argument, in arrival order.
    fn decode(tool: &str, fragments: &[&str]) -> Vec<(ToolBodyField, String)> {
        let mut stream = BodyStream::new(tool).unwrap();
        let mut out: Vec<(ToolBodyField, String)> = Vec::new();
        for fragment in fragments {
            for delta in stream.absorb(fragment) {
                match out.last_mut() {
                    Some(last) if last.0 == delta.field => last.1.push_str(&delta.text),
                    _ => out.push((delta.field, delta.text)),
                }
            }
        }
        out
    }

    fn content(tool: &str, fragments: &[&str]) -> String {
        decode(tool, fragments)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    #[test_case(WRITE, &[ToolBodyField::Content] ; "write_streams_its_content")]
    #[test_case("mcp_File_write", &[ToolBodyField::Content] ; "a_qualified_name_resolves")]
    #[test_case(EDIT, &[ToolBodyField::OldString, ToolBodyField::NewString] ; "edit_streams_both_sides")]
    #[test_case(PATCH, &[ToolBodyField::PatchText] ; "patch_streams_its_envelope")]
    #[test_case("shell", &[] ; "a_tool_with_no_body")]
    fn a_tools_streamed_arguments(tool: &str, expected: &[ToolBodyField]) {
        assert_eq!(body_fields(tool), expected);
        assert_eq!(BodyStream::new(tool).is_some(), !expected.is_empty());
    }

    #[test]
    fn a_body_is_decoded_across_fragments() {
        let decoded = content(
            WRITE,
            &[r#"{"filePath": "a.rs", "content": "fn "#, r#"x() {}"}"#],
        );
        assert_eq!(decoded, "fn x() {}");
    }

    #[test]
    fn an_escape_split_across_fragments_is_one_character() {
        let decoded = content(WRITE, &[r#"{"content": "a\"#, r#"nb"}"#]);
        assert_eq!(decoded, "a\nb");
    }

    #[test]
    fn an_escaped_backslash_is_not_a_newline() {
        let decoded = content(WRITE, &[r#"{"content": "a\\nb"}"#]);
        assert_eq!(decoded, r"a\nb");
    }

    #[test]
    fn a_unicode_escape_split_across_fragments_is_one_character() {
        let decoded = content(WRITE, &[r#"{"content": "a\u00"#, r#"e9b"}"#]);
        assert_eq!(decoded, "aéb");
    }

    #[test]
    fn a_quote_inside_the_body_does_not_end_it() {
        let decoded = content(WRITE, &[r#"{"content": "say \"hi\" now"}"#]);
        assert_eq!(decoded, r#"say "hi" now"#);
    }

    #[test]
    fn the_two_sides_of_an_edit_arrive_in_order() {
        let decoded = decode(
            EDIT,
            &[
                r#"{"filePath": "a.rs", "oldString": "one"#,
                r#"", "newString": "two"#,
                r#""}"#,
            ],
        );
        assert_eq!(
            decoded,
            [
                (ToolBodyField::OldString, "one".to_owned()),
                (ToolBodyField::NewString, "two".to_owned()),
            ]
        );
    }

    /// One fragment can close an argument and open the next, so a single delta
    /// is not enough to carry a fragment.
    #[test]
    fn a_fragment_that_crosses_arguments_carries_both() {
        let mut stream = BodyStream::new(EDIT).unwrap();
        stream.absorb(r#"{"oldString": "a"#);
        let crossing = stream.absorb(r#"b", "newString": "cd"#);
        let carried: Vec<_> = crossing
            .into_iter()
            .map(|delta| (delta.field, delta.text))
            .collect();
        assert_eq!(
            carried,
            [
                (ToolBodyField::OldString, "b".to_owned()),
                (ToolBodyField::NewString, "cd".to_owned()),
            ]
        );
    }

    #[test]
    fn an_unwanted_value_is_skipped_whole() {
        let decoded = content(
            EDIT,
            &[
                r#"{"opts": {"newString": "decoy", "n": [1, 2]}, "#,
                r#""replaceAll": true, "newString": "real"}"#,
            ],
        );
        assert_eq!(decoded, "real");
    }

    #[test]
    fn a_brace_inside_a_skipped_string_does_not_break_depth() {
        let decoded = content(WRITE, &[r#"{"opts": {"re": "a}b{c"}, "content": "real"}"#]);
        assert_eq!(decoded, "real");
    }

    #[test]
    fn the_line_count_follows_the_body() {
        let mut stream = BodyStream::new(WRITE).unwrap();
        stream.absorb(r#"{"content": "one"#);
        assert_eq!(stream.lines(), 1);
        stream.absorb(r"\ntwo\nthree");
        assert_eq!(stream.lines(), 3);
    }

    /// The count is what the header shows, so it has to outlive the point
    /// where carrying the text stops being useful.
    #[test]
    fn a_body_past_the_cap_stops_being_carried_but_keeps_counting() {
        let mut stream = BodyStream::new(WRITE).unwrap();
        stream.absorb(r#"{"content": ""#);
        let line = format!(r"{}\n", "x".repeat(63));
        let lines = LIVE_BODY_MAX_BYTES / 64 + 8;
        let carried: usize = (0..lines)
            .map(|_| {
                stream
                    .absorb(&line)
                    .iter()
                    .map(|delta| delta.text.len())
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(carried, LIVE_BODY_MAX_BYTES);
        assert_eq!(stream.lines(), lines + 1);
    }
}
