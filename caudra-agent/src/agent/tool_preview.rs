//! The one argument worth showing while a tool call is still being written.
//!
//! Providers stream a tool's arguments as JSON fragments, so for most of a
//! call's life the only thing on hand is a prefix of an object that does not
//! parse yet. This module pulls a single scalar out of that prefix so the card
//! and the status line can say *which* file is being edited before the model
//! has finished saying it.

use crate::tools::relative_path;

/// Wide enough for a path or a short command, short enough that a header row
/// never becomes the payload.
const PREVIEW_MAX_CHARS: usize = 160;
/// A key that has not appeared by here is not going to be the headline, and
/// rescanning a growing file body on every delta is what this bound exists to
/// prevent.
const PREVIEW_SCAN_CAP: usize = 8 * 1024;
/// What separates a server or namespace from the tool it qualifies.
const QUALIFIER: [char; 3] = ['_', '.', '-'];
const ELLIPSIS: char = '…';

/// The argument each tool leads with. Matched ignoring case and underscores,
/// so one spelling covers Workcell's camelCase wire names and the snake_case
/// the native tools use.
const PREVIEW_KEYS: &[(&str, &str)] = &[
    ("file_read", "filePath"),
    ("file_write", "filePath"),
    ("file_edit", "filePath"),
    ("file_index", "path"),
    ("view_image", "path"),
    ("file_glob", "pattern"),
    ("file_grep", "pattern"),
    ("shell", "command"),
    ("memory", "command"),
    ("websearch", "query"),
    ("webfetch", "url"),
    ("task", "description"),
    ("skill", "name"),
    ("workflow", "action"),
    ("code_map", "path"),
    ("code_context", "task"),
    ("code_refs", "symbol"),
    ("code_impact", "symbol"),
    ("code_expand", "symbol"),
];

/// Tools that lead with a blob or an aggregate. There is nothing short to show
/// mid-stream, and the finished header says it better, so they stay bare.
///
/// A patch is bare only as far as this scanner reaches. Its files are named
/// from the envelope [`super::tool_body`] decodes, which keeps the newlines
/// this scanner collapses into spaces.
const NO_PREVIEW: &[&str] = &[
    "python_execution",
    "file_apply_patch",
    "batch",
    "todo_write",
    "question",
    "execution_environment",
];

/// Keys an unknown tool must never be previewed by: a file body or a prompt
/// would fill the row with the payload the header exists to summarise.
const BLOB_KEYS: &[&str] = &[
    "content",
    "code",
    "body",
    "text",
    "patchText",
    "oldString",
    "newString",
    "prompt",
];

/// Keys whose value is a path, shortened the way the finished header shortens
/// it so the preview does not jump when the real header replaces it.
const PATH_KEYS: &[&str] = &["filePath", "path"];

/// Below this there is no wait to narrate, and a counter that appears and
/// vanishes is worse than none.
const SIZE_MIN_LINES: usize = 5;
/// Coarse enough that the digits stay readable and the row is rebuilt a fifth
/// as often. The exact prefix length is not information; that it grows is.
const SIZE_STEP_LINES: usize = 5;
/// Honest about being a floor: the body is still being written.
const SIZE_SUFFIX: &str = "+ lines";

enum Rule {
    Key(&'static str),
    Suppressed,
    Generic,
}

pub(crate) struct Preview {
    pub(crate) text: String,
    /// The value's closing quote arrived, so it will not grow again and the
    /// caller can stop scanning.
    pub(crate) complete: bool,
}

/// Compares tool and argument names the way the wire spells them: `filePath`,
/// `file_path`, and `FilePath` are one key.
pub(super) fn same_key(left: &str, right: &str) -> bool {
    let normalized = |key: &str| {
        key.chars()
            .filter(|c| *c != '_')
            .flat_map(char::to_lowercase)
            .collect::<Vec<_>>()
    };
    normalized(left) == normalized(right)
}

/// Every name a tool answers to, most qualified first. A call wrapped by an
/// MCP server arrives as `mcp_File_read`, so leading qualifier segments are
/// dropped one at a time rather than the name matched as a bare suffix: the
/// qualifier has to end where the tool name begins, and an unrelated
/// `myfile_read` cannot pass for `file_read`.
pub(super) fn candidates(tool: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(tool), |rest| {
        rest.split_once(QUALIFIER).map(|(_, tail)| tail)
    })
}

fn rule(tool: &str) -> Rule {
    for rest in candidates(tool) {
        if let Some((_, key)) = PREVIEW_KEYS.iter().find(|(name, _)| same_key(name, rest)) {
            return Rule::Key(key);
        }
        if NO_PREVIEW.iter().any(|name| same_key(name, rest)) {
            return Rule::Suppressed;
        }
    }
    Rule::Generic
}

/// What to show for a body of `lines` so far, or `None` while there is too
/// little of it for the count to be worth a row.
pub(crate) fn size_label(lines: usize) -> Option<String> {
    (lines >= SIZE_MIN_LINES).then(|| format!("{}{SIZE_SUFFIX}", lines - lines % SIZE_STEP_LINES))
}

/// The preview for `tool` given everything of its argument JSON that has
/// arrived. `None` while the headline key is still unwritten, and for every
/// tool that has no headline to show.
pub(crate) fn preview_for(tool: &str, json: &str) -> Option<Preview> {
    let wanted = match rule(tool) {
        Rule::Suppressed => return None,
        Rule::Key(key) => Some(key),
        Rule::Generic => None,
    };
    let matches = |found: &str| match wanted {
        Some(key) => same_key(found, key),
        None => !BLOB_KEYS.iter().any(|blob| same_key(blob, found)),
    };

    let (key, value, complete) = Scanner::new(capped(json)).find_string(matches)?;
    // Shortened even while it is still being typed: `strip_prefix` matches
    // whole components, so a half-written path either shortens correctly or
    // stays as it came.
    let value = match PATH_KEYS.iter().any(|path| same_key(path, &key)) {
        true => relative_path(&value),
        false => value,
    };
    let text = tidy(&value);
    (!text.is_empty()).then_some(Preview { text, complete })
}

/// Whether the caller can stop feeding a buffer that has produced no preview.
pub(crate) fn past_scan_cap(len: usize) -> bool {
    len > PREVIEW_SCAN_CAP
}

/// The value of the first top-level member named `key`, with whether its
/// closing quote arrived. `None` while the member is unwritten, or once an
/// earlier value stops the scan short of it.
pub(super) fn string_member(json: &str, key: &str) -> Option<(String, bool)> {
    let (_, value, complete) = Scanner::new(json).find_string(|found| same_key(found, key))?;
    Some((value, complete))
}

/// Everything that has arrived of the object value of the first top-level
/// member named `key`, from its opening brace. `None` while the member is
/// unwritten or its value is not an object.
pub(super) fn object_member<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let start = Scanner::new(json).find_object(|found| same_key(found, key))?;
    Some(&json[start..])
}

/// Truncating mid-character is fine: the scanner tolerates any tail, and past
/// the cap the answer is "give up" either way.
fn capped(json: &str) -> &str {
    if json.len() <= PREVIEW_SCAN_CAP {
        return json;
    }
    let mut end = PREVIEW_SCAN_CAP;
    while !json.is_char_boundary(end) {
        end -= 1;
    }
    &json[..end]
}

/// One line, single-spaced, short enough for a header. A value still being
/// written is a prefix, which is the point; a value that outgrows the row is
/// cut with an ellipsis.
fn tidy(value: &str) -> String {
    let mut out = String::new();
    for word in value.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    match out.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((cut, _)) => {
            out.truncate(cut);
            out.push(ELLIPSIS);
            out
        }
        None => out,
    }
}

/// A forward-only reader over a JSON object that may stop anywhere: mid-key,
/// mid-value, or mid-escape. Nothing here reports malformed input, because a
/// prefix of valid JSON is indistinguishable from it.
struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.pos += expected.len_utf8();
            return true;
        }
        false
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    /// Leaves the reader on the value of the next top-level member and returns
    /// its key. `None` once the object closes, or once the prefix stops before
    /// a value begins.
    fn next_key(&mut self) -> Option<String> {
        loop {
            self.skip_ws();
            match self.peek()? {
                ',' => {
                    self.bump();
                    continue;
                }
                '"' => self.bump(),
                _ => return None,
            };
            let mut key = String::new();
            if !self.read_string(&mut key) {
                return None;
            }
            self.skip_ws();
            if !self.eat(':') {
                return None;
            }
            self.skip_ws();
            return Some(key);
        }
    }

    /// The first top-level `"key": "value"` whose key satisfies `matches`,
    /// with whether the value's closing quote arrived. Scanning stops at the
    /// first string value that is both unwanted and unfinished, because
    /// nothing can follow it yet.
    fn find_string(&mut self, matches: impl Fn(&str) -> bool) -> Option<(String, String, bool)> {
        self.skip_ws();
        if !self.eat('{') {
            return None;
        }
        while let Some(key) = self.next_key() {
            if self.peek()? == '"' {
                self.bump();
                let mut value = String::new();
                let complete = self.read_string(&mut value);
                if matches(&key) {
                    return Some((key, value, complete));
                }
                if !complete {
                    return None;
                }
            } else {
                self.skip_value();
            }
        }
        None
    }

    /// Where the object value of the first member satisfying `matches` opens.
    /// A member of that name whose value is not an object is skipped like any
    /// other, so the shape is part of what is being matched.
    fn find_object(&mut self, matches: impl Fn(&str) -> bool) -> Option<usize> {
        self.skip_ws();
        if !self.eat('{') {
            return None;
        }
        while let Some(key) = self.next_key() {
            if self.peek()? == '{' && matches(&key) {
                return Some(self.pos);
            }
            self.skip_value();
        }
        None
    }

    /// Decodes a string body, the opening quote already consumed. Returns
    /// whether the closing quote arrived. Control escapes collapse to a space
    /// because a preview is one line by construction.
    fn read_string(&mut self, out: &mut String) -> bool {
        while let Some(c) = self.bump() {
            match c {
                '"' => return true,
                '\\' => match self.bump() {
                    Some('n' | 't' | 'r' | 'b' | 'f') => out.push(' '),
                    Some('u') => self.read_unicode_escape(out),
                    Some(escaped) => out.push(escaped),
                    None => return false,
                },
                _ => out.push(c),
            }
        }
        false
    }

    /// A lone surrogate or a truncated escape contributes nothing rather than
    /// poisoning the row with a replacement character.
    fn read_unicode_escape(&mut self, out: &mut String) {
        let mut code = 0u32;
        for _ in 0..4 {
            let Some(digit) = self.peek().and_then(|c| c.to_digit(16)) else {
                return;
            };
            self.bump();
            code = code * 16 + digit;
        }
        if let Some(c) = char::from_u32(code) {
            out.push(c);
        }
    }

    /// Consumes one non-string value, including a nested container and any
    /// strings inside it, so their braces cannot be mistaken for structure.
    fn skip_value(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => return,
                '"' => {
                    self.bump();
                    let mut discarded = String::new();
                    if !self.read_string(&mut discarded) {
                        return;
                    }
                    continue;
                }
                _ => {}
            }
            self.bump();
            if depth == 0 {
                // A bare scalar ends at the next delimiter, which the loop
                // head above already refuses to consume.
                if matches!(self.peek(), Some(',' | '}' | ']')) {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PREVIEW_SCAN_CAP, Preview, preview_for, size_label};
    use test_case::test_case;

    const EDIT: &str = "file_edit";
    const SHELL: &str = "shell";

    fn text_of(tool: &str, json: &str) -> Option<String> {
        preview_for(tool, json).map(|p| p.text)
    }

    #[test_case(EDIT, r#"{"filePath": "src/app.rs""#, Some("src/app.rs") ; "truncated_before_closing_brace")]
    #[test_case(EDIT, r#"{"filePath": "src/ap"#, Some("src/ap") ; "truncated_mid_value")]
    #[test_case(EDIT, r#"{"filePa"#, None ; "truncated_mid_key")]
    #[test_case(EDIT, r#"{"filePath": ""#, None ; "value_not_started")]
    #[test_case(EDIT, r#"{"file_path": "a.rs""#, Some("a.rs") ; "snake_case_key")]
    #[test_case(EDIT, r#"{"FILEPATH": "a.rs""#, Some("a.rs") ; "upper_case_key")]
    #[test_case(EDIT, "{", None ; "object_only")]
    #[test_case(EDIT, "", None ; "empty_buffer")]
    #[test_case(SHELL, r#"{"command": "echo hi", "timeout": 5}"#, Some("echo hi") ; "value_before_scalar")]
    #[test_case(SHELL, r#"{"timeout": 5, "command": "ls"#, Some("ls") ; "scalar_before_value")]
    #[test_case(SHELL, r#"{"command": "a\nb""#, Some("a b") ; "escape_collapses_to_space")]
    #[test_case(SHELL, r#"{"command": "a\"#, Some("a") ; "truncated_mid_escape")]
    #[test_case(SHELL, r#"{"command": "sp   aced""#, Some("sp aced") ; "whitespace_collapsed")]
    #[test_case("python_execution", r#"{"code": "print(1)""#, None ; "blob_tool_suppressed")]
    #[test_case("batch", r#"{"tool_calls": []}"#, None ; "aggregate_tool_suppressed")]
    #[test_case("mcp_File_read", r#"{"filePath": "a.rs""#, Some("a.rs") ; "mcp_qualified_name")]
    #[test_case("some_plugin_tool", r#"{"target": "thing""#, Some("thing") ; "unknown_tool_fallback")]
    #[test_case("some_plugin_tool", r#"{"content": "a whole file""#, None ; "unknown_tool_skips_blob_key")]
    #[test_case("file_glob", r#"{"pattern": "**/*.rs""#, Some("**/*.rs") ; "glob_pattern")]
    fn preview_text(tool: &str, json: &str, expected: Option<&str>) {
        assert_eq!(text_of(tool, json).as_deref(), expected);
    }

    #[test]
    fn a_nested_object_before_the_key_is_skipped_whole() {
        let json = r#"{"opts": {"filePath": "decoy.rs", "n": [1, 2]}, "command": "real""#;
        assert_eq!(text_of(SHELL, json).as_deref(), Some("real"));
    }

    #[test]
    fn a_brace_inside_a_nested_string_does_not_break_depth() {
        let json = r#"{"opts": {"re": "a}b{c"}, "command": "real""#;
        assert_eq!(text_of(SHELL, json).as_deref(), Some("real"));
    }

    #[test]
    fn an_unterminated_value_hides_a_later_key() {
        let json = r#"{"other": "still writing"#;
        assert_eq!(text_of(SHELL, json), None);
    }

    #[test]
    fn an_absolute_path_shortens_before_it_is_finished() {
        let cwd = std::env::current_dir().unwrap();
        let json = format!(r#"{{"filePath": "{}/src/ap"#, cwd.display());
        assert_eq!(text_of(EDIT, &json).as_deref(), Some("src/ap"));
    }

    #[test]
    fn completeness_tracks_the_closing_quote() {
        let growing = preview_for(SHELL, r#"{"command": "ec"#).unwrap();
        assert!(!growing.complete);
        let done = preview_for(SHELL, r#"{"command": "echo""#).unwrap();
        assert!(done.complete);
    }

    #[test]
    fn a_key_past_the_scan_cap_is_given_up_on() {
        let filler = "x".repeat(PREVIEW_SCAN_CAP);
        let json = format!(r#"{{"note": "{filler}", "command": "ls""#);
        assert_eq!(text_of(SHELL, &json), None);
    }

    #[test_case(4, None ; "one_line_below_the_threshold")]
    #[test_case(5, Some("5+ lines") ; "exactly_at_the_threshold")]
    #[test_case(9, Some("5+ lines") ; "floored_back_to_the_step")]
    #[test_case(10, Some("10+ lines") ; "the_next_step")]
    fn a_streamed_size_is_a_floor(lines: usize, expected: Option<&str>) {
        assert_eq!(size_label(lines).as_deref(), expected);
    }

    #[test_case(r#"{"tool": "shell", "parameters": {}}"#, Some(("shell", true)) ; "a_closed_value")]
    #[test_case(r#"{"tool": "she"#, Some(("she", false)) ; "a_value_still_arriving")]
    #[test_case(r#"{"parameters": {"a": 1}, "tool": "shell""#, Some(("shell", true)) ; "after_a_nested_object")]
    #[test_case(r#"{"tool": {"nested": 1}"#, None ; "a_value_of_the_wrong_shape")]
    #[test_case(r#"{"other": 1"#, None ; "an_absent_member")]
    fn a_string_member_is_read_with_its_completeness(json: &str, expected: Option<(&str, bool)>) {
        let found = super::string_member(json, "tool");
        assert_eq!(
            found.as_ref().map(|(value, done)| (value.as_str(), *done)),
            expected
        );
    }

    #[test_case(r#"{"tool": "shell", "parameters": {"command": "ls"#, Some(r#"{"command": "ls"#) ; "an_object_still_arriving")]
    #[test_case(r#"{"parameters": {"a": {"b": 1}}, "tool": "x""#, Some(r#"{"a": {"b": 1}}, "tool": "x""#) ; "everything_from_the_brace")]
    #[test_case(r#"{"tool": "shell", "command": "ls""#, None ; "the_flat_shape_has_none")]
    #[test_case(r#"{"parameters": "not an object""#, None ; "a_member_of_the_wrong_shape")]
    fn an_object_member_is_returned_from_its_brace(json: &str, expected: Option<&str>) {
        assert_eq!(super::object_member(json, "parameters"), expected);
    }

    #[test]
    fn an_overlong_value_is_cut_with_an_ellipsis() {
        let json = format!(r#"{{"command": "{}""#, "a".repeat(500));
        let Preview { text, .. } = preview_for(SHELL, &json).unwrap();
        assert!(text.ends_with('…'), "{text}");
        assert_eq!(text.chars().count(), super::PREVIEW_MAX_CHARS + 1);
    }
}
