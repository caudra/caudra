//! The argument a file-mutating tool spends its whole stream writing.
//!
//! A call that rewrites a file is nearly all body: the path arrives in the
//! first fragment and everything after it is the content, the patch, or the
//! text a match is replaced with. Decoding that value as its fragments go past
//! is what lets the header say how much has arrived, and what lets a write show
//! the file being written instead of a spinner and a line count.
//!
//! Only a whole file is worth drawing half-arrived. A replacement and a patch
//! are legible as diffs and as nothing else, so their body is counted and
//! dropped rather than published. A patch keeps one thing on its way past: the
//! `*** Verb: path` lines of its envelope, which name the files it touches
//! long before it has finished describing what it does to them.
//!
//! The decode is resumable rather than a scan per fragment, which
//! [`super::tool_preview`] can afford only because it gives up after a few
//! kilobytes. A body is the opposite case: it is the long part, so rescanning
//! it once per token is quadratic. Resuming also solves the problem a scan
//! would have anyway, which is that a `\` escape or a `\uXXXX` sequence can
//! straddle two fragments.

use super::tool_preview::{candidates, same_key};
use crate::patch;

/// Stops carrying a runaway body once no reader could follow it. Matches the
/// cap on live shell output. The line count keeps going, so a huge write still
/// reports that it is making progress.
pub(super) const LIVE_BODY_MAX_BYTES: usize = 64 * 1024;
/// Room for the envelope lines a streaming patch is named by. A patch that
/// declares more files than this holds collapsed to a bare count long ago, and
/// `ToolStart` replaces whatever the header settled on.
const ENVELOPE_MAX_BYTES: usize = 4 * 1024;
/// Digits in a `\uXXXX` escape.
const UNICODE_ESCAPE_DIGITS: u8 = 4;

/// What a card can do with a body before the call runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Body {
    /// Drawn as it arrives: a whole file, and a script, are legible
    /// half-written.
    Drawn,
    /// Counted and dropped: a diff is legible as a diff and as nothing else.
    Counted,
    /// Counted, and read for the envelope lines that name the files it
    /// touches.
    Named,
}

/// The tool, the arguments it spends its whole stream writing, and what a card
/// can do with them before the call runs. Every other tool leads with a path,
/// a pattern or a query, and has nothing long enough to be worth showing or
/// counting.
///
/// A script belongs here for the same reason a whole file does: it is the
/// record of what ran, it is legible half-written, and its card draws it as
/// numbered lines either way. Nothing but its own length bounds it, so a
/// heredoc spends the whole stream arriving and the header alone can only
/// show a space-joined prefix of it.
///
/// An edit counts both of its sides. The count answers how long the wait is,
/// not how big the change is, and the side being replaced is half of that
/// wait: it arrives first, so counting only the new side leaves the header
/// empty until the last stretch of the stream, and empty for good on every
/// edit whose replacement is shorter than the floor `size_label` applies.
///
/// A note and a local document belong here for the reason a whole file does:
/// each is a document the call carries entire, legible half-written, and drawn
/// by its settled card as the document it is. A tool whose body is only one of
/// several commands still earns a row, because the reader that finds no such
/// argument decodes nothing and costs nothing.
///
/// An image prompt belongs for the same reason, minus the claim a script
/// makes: it does not name its own header, because the call is named by the
/// file it writes. A generation is a long wait, and the prompt is the only
/// thing worth reading during it.
const BODY_ARGS: &[(&str, &[&str], Body)] = &[
    ("file_write", &["content"], Body::Drawn),
    ("file_edit", &["oldString", "newString"], Body::Counted),
    ("file_apply_patch", &["patchText"], Body::Named),
    ("shell", &["command"], Body::Drawn),
    ("python_execution", &["code"], Body::Drawn),
    ("memory", &["content"], Body::Drawn),
    ("local_document_write", &["content"], Body::Drawn),
    ("task", &["prompt"], Body::Drawn),
    ("image_generate", &["prompt"], Body::Drawn),
];

/// The arguments `tool` writes and what becomes of them, `None` for a tool
/// with no body. Names are matched the way [`super::tool_preview`] matches
/// them, so `mcp_File_write` and `file_write` resolve alike.
fn body_arg(tool: &str) -> Option<(&'static [&'static str], Body)> {
    candidates(tool).find_map(|rest| {
        BODY_ARGS
            .iter()
            .find(|(name, ..)| same_key(name, rest))
            .map(|(_, keys, body)| (*keys, *body))
    })
}

/// A patch's `*** Verb: path` lines as they are decoded, so its header can
/// name files while the diff itself is still arriving.
///
/// Only those lines are kept. Everything else is dropped on the character that
/// rules it out, which for an ordinary content line is its first, so the cost
/// is the envelope rather than the patch.
#[derive(Default)]
struct Envelope {
    lines: String,
    /// Where the line being decoded starts, so a line that turns out not to
    /// belong is dropped whole.
    line_start: usize,
    /// The line being decoded can no longer become an envelope line.
    dropped: bool,
    /// [`Self::lines`] gained a character since the header was last asked for.
    grown: bool,
}

impl Envelope {
    fn push(&mut self, c: char) {
        if c == '\n' {
            match self.line().starts_with(patch::MARKER) {
                true => {
                    self.lines.push('\n');
                    self.line_start = self.lines.len();
                }
                false => self.lines.truncate(self.line_start),
            }
            self.dropped = false;
            return;
        }
        if self.dropped || self.lines.len() >= ENVELOPE_MAX_BYTES {
            return;
        }
        self.lines.push(c);
        // The line so far and the marker stay prefixes of one another until
        // the line proves otherwise, which is what admits `*`, `**` and `***`
        // on their way to a header line the fragment has not finished.
        let line = self.line();
        if line.starts_with(patch::MARKER) || patch::MARKER.starts_with(line) {
            self.grown = true;
            return;
        }
        self.dropped = true;
        self.lines.truncate(self.line_start);
    }

    fn line(&self) -> &str {
        self.lines[self.line_start..].trim_start()
    }

    /// The header these lines name, `None` unless they grew since the last
    /// call and name a file. A patch that has named none yet is left blank
    /// rather than filled with the placeholder a finished header falls back
    /// to and then replaced by the first path.
    fn header(&mut self) -> Option<String> {
        let grown = std::mem::take(&mut self.grown);
        (grown && !patch::paths(&self.lines).is_empty()).then(|| patch::header(&self.lines))
    }
}

/// One decoded character, or the fact that the escape it belongs to has not
/// produced one yet.
pub(super) enum Piece {
    Char(char),
    Pending,
    /// The closing quote.
    End,
}

/// Decodes a JSON string body one character at a time, the opening quote
/// already consumed.
#[derive(Default)]
pub(super) struct StringReader {
    escaped: bool,
    /// The code point built so far and how many of its digits have arrived.
    unicode: Option<(u32, u8)>,
}

impl StringReader {
    pub(super) fn push(&mut self, c: char) -> Piece {
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
    /// Decoding the body.
    Body,
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
    keys: &'static [&'static str],
    body: Body,
    state: State,
    string: StringReader,
    /// The member name being read, against [`Self::keys`].
    member: String,
    newlines: usize,
    /// Decoded bytes handed out so far, against [`LIVE_BODY_MAX_BYTES`].
    emitted: usize,
    envelope: Envelope,
}

impl BodyStream {
    /// `None` for a tool with no body worth reading.
    pub(super) fn new(tool: &str) -> Option<Self> {
        let (keys, body) = body_arg(tool)?;
        Some(Self {
            keys,
            body,
            state: State::Open,
            string: StringReader::default(),
            member: String::new(),
            newlines: 0,
            emitted: 0,
            envelope: Envelope::default(),
        })
    }

    /// What this fragment added to the body, `None` when it added nothing a
    /// reader can use: a fragment outside the body, one that only advanced an
    /// escape, or any fragment at all of a body that is counted rather than
    /// drawn.
    pub(super) fn absorb(&mut self, delta: &str) -> Option<String> {
        let mut decoded = String::new();
        for c in delta.chars() {
            self.push(c, &mut decoded);
        }
        (!decoded.is_empty()).then_some(decoded)
    }

    /// Lines of body decoded so far. A body whose last line has no newline of
    /// its own still occupies a line, which is why this is one more than the
    /// newline count.
    pub(super) fn lines(&self) -> usize {
        self.newlines + 1
    }

    /// The files a patch has named so far, when that changed since the last
    /// call. Always `None` for a body nothing reads an envelope out of.
    pub(super) fn header(&mut self) -> Option<String> {
        self.envelope.header()
    }

    fn push(&mut self, c: char, decoded: &mut String) {
        self.state = match self.state {
            State::Done => State::Done,
            State::Open => match c {
                '{' => State::Member,
                _ => State::Open,
            },
            State::Member => match c {
                '"' => {
                    self.member.clear();
                    self.string = StringReader::default();
                    State::Key
                }
                '}' => State::Done,
                _ => State::Member,
            },
            State::Key => match self.string.push(c) {
                Piece::Char(c) => {
                    self.member.push(c);
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
                    '"' => match self.keys.iter().any(|key| same_key(key, &self.member)) {
                        true => State::Body,
                        false => State::SkipString(0),
                    },
                    '{' | '[' => State::Skip(1),
                    _ => State::Skip(0),
                }
            }
            State::Body => match self.string.push(c) {
                Piece::Char(c) => {
                    self.newlines += usize::from(c == '\n');
                    match self.body {
                        Body::Drawn if self.emitted < LIVE_BODY_MAX_BYTES => {
                            self.emitted += c.len_utf8();
                            decoded.push(c);
                        }
                        Body::Named => self.envelope.push(c),
                        Body::Drawn | Body::Counted => {}
                    }
                    State::Body
                }
                Piece::Pending => State::Body,
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
}

#[cfg(test)]
mod tests {
    use super::{Body, BodyStream, ENVELOPE_MAX_BYTES, LIVE_BODY_MAX_BYTES, body_arg};
    use test_case::test_case;

    const WRITE: &str = "file_write";
    const EDIT: &str = "file_edit";
    const PATCH: &str = "file_apply_patch";
    const SHELL: &str = "shell";
    const MEMORY: &str = "memory";
    const TASK: &str = "task";
    const IMAGE: &str = "image_generate";
    const CONTENT_KEYS: &[&str] = &["content"];
    const PROMPT_KEYS: &[&str] = &["prompt"];
    const EDIT_KEYS: &[&str] = &["oldString", "newString"];
    const PATCH_TEXT_KEYS: &[&str] = &["patchText"];
    const COMMAND_KEYS: &[&str] = &["command"];
    const CODE_KEYS: &[&str] = &["code"];
    /// What a header reads once it stops naming files one by one.
    const COUNTED_FILES: &str = " files";
    const EXPECT_NAMED: &str = "a patch that declares files earns a header";
    const EXPECT_PROMPT: &str = "a delegation draws the brief it sends, whole";
    const EXPECT_IMAGE_PROMPT: &str =
        "a generation draws the prompt it sends, whole, while the model is still writing it";

    /// Everything the fragments published, in arrival order.
    fn published(tool: &str, fragments: &[&str]) -> String {
        let mut stream = BodyStream::new(tool).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment))
            .collect()
    }

    /// The stream after every fragment, for the counting a reader never sees.
    fn counted(tool: &str, fragments: &[&str]) -> BodyStream {
        let mut stream = BodyStream::new(tool).unwrap();
        for fragment in fragments {
            stream.absorb(fragment);
        }
        stream
    }

    /// Every header the fragments produce, in order, without the repeats a
    /// reader drops.
    fn headers(tool: &str, fragments: &[&str]) -> Vec<String> {
        let mut stream = BodyStream::new(tool).unwrap();
        let mut published: Vec<String> = Vec::new();
        for fragment in fragments {
            stream.absorb(fragment);
            if let Some(header) = stream.header()
                && published.last() != Some(&header)
            {
                published.push(header);
            }
        }
        published
    }

    #[test_case(WRITE, Some((CONTENT_KEYS, Body::Drawn)) ; "a_write_publishes_its_content")]
    #[test_case("mcp_File_write", Some((CONTENT_KEYS, Body::Drawn)) ; "a_qualified_name_resolves")]
    #[test_case(EDIT, Some((EDIT_KEYS, Body::Counted)) ; "an_edit_reads_both_sides")]
    #[test_case(PATCH, Some((PATCH_TEXT_KEYS, Body::Named)) ; "a_patch_reads_its_envelope")]
    #[test_case(SHELL, Some((COMMAND_KEYS, Body::Drawn)) ; "a_command_is_drawn")]
    #[test_case("python_execution", Some((CODE_KEYS, Body::Drawn)) ; "a_script_is_drawn")]
    #[test_case(MEMORY, Some((CONTENT_KEYS, Body::Drawn)) ; "a_note_is_drawn")]
    #[test_case("local_document_write", Some((CONTENT_KEYS, Body::Drawn)) ; "a_local_document_is_drawn")]
    #[test_case(TASK, Some((PROMPT_KEYS, Body::Drawn)) ; "a_delegation_draws_its_prompt")]
    #[test_case(IMAGE, Some((PROMPT_KEYS, Body::Drawn)) ; "a_generation_draws_its_prompt")]
    #[test_case("file_read", None ; "a_tool_with_no_body")]
    fn a_tools_body_arguments(tool: &str, expected: Option<(&[&str], Body)>) {
        assert_eq!(body_arg(tool), expected);
        assert_eq!(BodyStream::new(tool).is_some(), expected.is_some());
    }

    /// The description names the call; the prompt is the call. A card that drew
    /// the first would be summarising a summary, and the row already carries it.
    #[test]
    fn a_delegation_draws_the_prompt_and_not_the_description() {
        let decoded = published(
            TASK,
            &[
                r#"{"description": "Find auth", "prompt": "Search for "#,
                r#"the middleware."}"#,
            ],
        );
        assert_eq!(decoded, "Search for the middleware.", "{EXPECT_PROMPT}");
    }

    /// A prompt is the one body whose escapes routinely land on a token
    /// boundary, because the model writes it as prose with newlines in it.
    #[test]
    fn a_prompt_escape_split_across_fragments_still_decodes() {
        let decoded = published(TASK, &[r#"{"prompt": "first\"#, r#"nsecond"}"#]);
        assert_eq!(decoded, "first\nsecond", "{EXPECT_PROMPT}");
    }

    /// The reported bug: a generation showed nothing at all while the model
    /// wrote the prompt, and a generation is a long wait with nothing else to
    /// read. The output path arrives after the prompt and must not take it.
    #[test]
    fn a_generation_draws_the_prompt_and_not_the_path() {
        let decoded = published(
            IMAGE,
            &[
                r#"{"prompt": "A wide cinematic"#,
                r#" shot", "out": "assets/hero.png"}"#,
            ],
        );
        assert_eq!(decoded, "A wide cinematic shot", "{EXPECT_IMAGE_PROMPT}");
    }

    #[test]
    fn a_body_is_decoded_across_fragments() {
        let decoded = published(
            WRITE,
            &[r#"{"filePath": "a.rs", "content": "fn "#, r#"x() {}"}"#],
        );
        assert_eq!(decoded, "fn x() {}");
    }

    #[test]
    fn a_note_is_decoded_past_the_arguments_that_precede_it() {
        let decoded = published(
            MEMORY,
            &[
                r##"{"command": "write", "path": "a.md", "tags": ["x"], "content": "# T"##,
                r#"itle\nbody"}"#,
            ],
        );
        assert_eq!(decoded, "# Title\nbody");
    }

    /// The reader is built for every command of a tool that has one body
    /// argument, and the commands without it decode nothing.
    #[test]
    fn a_command_with_no_body_publishes_nothing() {
        let mut stream = BodyStream::new(MEMORY).unwrap();
        assert_eq!(stream.absorb(r#"{"command": "list", "tags": ["x"]}"#), None);
        assert_eq!(stream.lines(), 1);
    }

    /// A heredoc is the case the header could never show: the newlines it is
    /// made of are what the header collapses into spaces.
    #[test]
    fn a_command_keeps_the_newlines_its_header_collapses() {
        let decoded = published(
            SHELL,
            &[
                r#"{"command": "python3 - <<'PY'\nimp"#,
                r#"ort re\nprint(re)\nPY", "timeoutSec": 1}"#,
            ],
        );
        assert_eq!(decoded, "python3 - <<'PY'\nimport re\nprint(re)\nPY");
    }

    #[test]
    fn an_escape_split_across_fragments_is_one_character() {
        let decoded = published(WRITE, &[r#"{"content": "a\"#, r#"nb"}"#]);
        assert_eq!(decoded, "a\nb");
    }

    #[test]
    fn an_escaped_backslash_is_not_a_newline() {
        let decoded = published(WRITE, &[r#"{"content": "a\\nb"}"#]);
        assert_eq!(decoded, r"a\nb");
    }

    #[test]
    fn a_unicode_escape_split_across_fragments_is_one_character() {
        let decoded = published(WRITE, &[r#"{"content": "a\u00"#, r#"e9b"}"#]);
        assert_eq!(decoded, "aéb");
    }

    #[test]
    fn a_quote_inside_the_body_does_not_end_it() {
        let decoded = published(WRITE, &[r#"{"content": "say \"hi\" now"}"#]);
        assert_eq!(decoded, r#"say "hi" now"#);
    }

    /// A fragment that only advances an escape has nothing for a reader, and
    /// an empty repaint is worse than none.
    #[test]
    fn a_fragment_that_decodes_nothing_publishes_nothing() {
        let mut stream = BodyStream::new(WRITE).unwrap();
        assert_eq!(stream.absorb(r#"{"filePath":"#), None);
        assert_eq!(
            stream.absorb(r#" "a.rs", "content": "a\"#),
            Some("a".into())
        );
        assert_eq!(stream.absorb("n"), Some("\n".into()));
    }

    #[test]
    fn an_unwanted_value_is_skipped_whole() {
        let decoded = published(
            WRITE,
            &[
                r#"{"opts": {"content": "decoy", "n": [1, 2]}, "#,
                r#""overwrite": true, "content": "real"}"#,
            ],
        );
        assert_eq!(decoded, "real");
    }

    #[test]
    fn a_brace_inside_a_skipped_string_does_not_break_depth() {
        let decoded = published(WRITE, &[r#"{"opts": {"re": "a}b{c"}, "content": "real"}"#]);
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

    /// The header is the only thing an edit shows while it streams, so the
    /// count has to run even though nothing is published.
    #[test]
    fn an_edit_counts_lines_without_publishing_them() {
        let fragments = [r#"{"oldString": "gone", "newString": "one"#, r"\ntwo"];
        assert!(published(EDIT, &fragments).is_empty());
        assert_eq!(counted(EDIT, &fragments).lines(), 2);
    }

    /// The side being replaced is half the wait, and it arrives first. An edit
    /// counting only its new side reports nothing until the last stretch of
    /// the stream, and nothing at all when the replacement is short: this is
    /// the case that left the header empty on an ordinary edit.
    #[test]
    fn an_edits_two_sides_are_counted_together() {
        let old = counted(EDIT, &[r#"{"oldString": "one\ntwo\nthree"#]);
        assert_eq!(old.lines(), 3);

        let both = counted(
            EDIT,
            &[
                r#"{"oldString": "one\ntwo\nthree""#,
                r#", "newString": "1\n2\n3"}"#,
            ],
        );
        assert_eq!(both.lines(), 5);
    }

    #[test]
    fn a_patch_is_counted_without_being_published() {
        let fragments = [r#"{"patchText": "*** Begin Patch\n+one"#];
        assert!(published(PATCH, &fragments).is_empty());
        assert_eq!(counted(PATCH, &fragments).lines(), 2);
    }

    /// The header is the only thing a patch shows while it streams, and the
    /// files it declares are the part of it worth reading early.
    #[test]
    fn a_patch_names_its_files_as_they_arrive() {
        let fragments = [
            r#"{"patchText": "*** Begin Patch\n*** Update File: a.rs\n"#,
            r"-one\n+two\n",
            r#"*** Delete File: b.rs\n*** End Patch"}"#,
        ];
        assert_eq!(headers(PATCH, &fragments), ["a.rs", "a.rs, b.rs"]);
    }

    /// A path grows in the header the way a write's does, rather than the row
    /// staying empty until the line it is on ends.
    #[test]
    fn a_path_is_named_before_its_line_ends() {
        let fragments = [r#"{"patchText": "*** Begin Patch\n*** Update File: src/ap"#];
        assert_eq!(headers(PATCH, &fragments), ["src/ap"]);
    }

    #[test]
    fn an_envelope_line_split_across_fragments_is_still_named() {
        let fragments = [r#"{"patchText": "*** Upda"#, r"te File: a.rs\n"];
        assert_eq!(headers(PATCH, &fragments), ["a.rs"]);
    }

    /// A patch body can quote the envelope it lives in, so the line has to
    /// start with the marker rather than merely contain it.
    #[test]
    fn a_content_line_quoting_the_marker_names_nothing() {
        let fragments =
            [r#"{"patchText": "*** Update File: a.rs\n+*** Add File: decoy.rs\n*** End Patch"}"#];
        assert_eq!(headers(PATCH, &fragments), ["a.rs"]);
    }

    /// The placeholder a finished header falls back to would be a worse row
    /// than none, and it would be replaced by the first path anyway.
    #[test]
    fn a_patch_that_has_named_nothing_shows_no_header() {
        assert!(headers(PATCH, &[r#"{"patchText": "*** Begin Patch\n"#]).is_empty());
    }

    #[test_case(WRITE, r#"{"content": "*** Update File: a.rs\n"}"# ; "a_write_names_nothing")]
    #[test_case(EDIT, r#"{"oldString": "*** Update File: a.rs\n"}"# ; "an_edit_names_nothing")]
    fn only_a_patch_is_read_for_an_envelope(tool: &str, json: &str) {
        assert!(headers(tool, &[json]).is_empty());
    }

    /// The count is what the header falls back to, so it has to outlive the
    /// point where carrying the paths stops being useful.
    #[test]
    fn a_patch_past_the_envelope_cap_stops_naming_but_keeps_counting() {
        const DECLARED: usize = 200;
        let line = format!(r"*** Update File: {}.rs\n", "x".repeat(40));
        let mut stream = BodyStream::new(PATCH).unwrap();
        stream.absorb(r#"{"patchText": ""#);
        let mut header = None;
        for _ in 0..DECLARED {
            stream.absorb(&line);
            header = stream.header().or(header);
        }

        // The cap is checked before a character is kept, and a committed line
        // adds its newline afterwards.
        assert!(stream.envelope.lines.len() <= ENVELOPE_MAX_BYTES + 1);
        assert_eq!(stream.lines(), DECLARED + 1);
        let header = header.expect(EXPECT_NAMED);
        assert!(header.ends_with(COUNTED_FILES), "{header}");
        assert_ne!(header, format!("{DECLARED}{COUNTED_FILES}"));
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
            .map(|_| stream.absorb(&line).map_or(0, |text| text.len()))
            .sum();
        assert_eq!(carried, LIVE_BODY_MAX_BYTES);
        assert_eq!(stream.lines(), lines + 1);
    }
}
