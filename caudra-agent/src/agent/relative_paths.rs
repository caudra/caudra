//! Shorter spellings of absolute tool paths that repeat the working directory.
//!
//! Purely lexical, so a call costs no filesystem access. The caller vouches for
//! the base: relative paths must resolve against it, and a single `../` only
//! where its parent is the directory the model sees above it.

use serde_json::Value;

use crate::patch;

/// Examples a hint cites, so it stays short whatever the response did.
pub(crate) const MAX_SUGGESTIONS: usize = 2;
/// A longer path is left alone rather than quoted into a hint.
const MAX_PATH_BYTES: usize = 512;
const SEPARATOR: &str = "/";
const CURRENT: &str = ".";
const PARENT: &str = "..";
const FILE_PATH: &str = "filePath";
const PATH: &str = "path";
const WORKDIR: &str = "workdir";
const PATCH_TEXT: &str = "patchText";
/// Arguments that resolve against the working directory. Other tools, Lua and
/// MCP ones included, may resolve relative paths against something else.
const TOOLS: &[(&str, Argument)] = &[
    ("file_read", Argument::Path(FILE_PATH)),
    ("file_write", Argument::Path(FILE_PATH)),
    ("file_edit", Argument::Path(FILE_PATH)),
    ("file_glob", Argument::Path(PATH)),
    ("file_grep", Argument::Path(PATH)),
    ("file_index", Argument::Path(PATH)),
    ("file_apply_patch", Argument::Patch),
    ("shell", Argument::Path(WORKDIR)),
    ("code_map", Argument::Scope(PATH)),
    ("code_context", Argument::Scope(PATH)),
    ("code_refs", Argument::Scope(PATH)),
    ("code_impact", Argument::Scope(PATH)),
    ("code_expand", Argument::Scope(PATH)),
];

enum Argument {
    Path(&'static str),
    /// A code-graph scope is root-relative, so it never reaches above the root.
    Scope(&'static str),
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathSuggestion {
    pub absolute: String,
    pub relative: String,
}

impl PathSuggestion {
    pub(crate) fn saved_chars(&self) -> usize {
        self.absolute
            .chars()
            .count()
            .saturating_sub(self.relative.chars().count())
    }
}

/// The working directory as the model was shown it.
pub(crate) struct PathBase {
    cwd: Vec<String>,
    parent_allowed: bool,
    min_saved_chars: usize,
}

impl PathBase {
    pub(crate) fn new(cwd: &str, parent_allowed: bool, min_saved_chars: usize) -> Option<Self> {
        Some(Self {
            cwd: components(cwd)?.into_iter().map(str::to_owned).collect(),
            parent_allowed,
            min_saved_chars,
        })
    }

    /// Shorter forms of the absolute paths one call names, in argument order.
    pub(crate) fn suggestions(&self, tool: &str, input: &Value) -> Vec<PathSuggestion> {
        let Some((_, argument)) = TOOLS.iter().find(|(name, _)| *name == tool) else {
            return Vec::new();
        };
        let text = |field: &str| input.get(field).and_then(Value::as_str);
        let (paths, parent) = match *argument {
            Argument::Path(field) => (text(field).into_iter().collect(), self.parent_allowed),
            Argument::Scope(field) => (text(field).into_iter().collect(), false),
            Argument::Patch => (
                text(PATCH_TEXT).map(patch::paths).unwrap_or_default(),
                self.parent_allowed,
            ),
        };
        paths
            .into_iter()
            .filter_map(|path| self.suggest(path, parent))
            .collect()
    }

    fn suggest(&self, path: &str, parent: bool) -> Option<PathSuggestion> {
        if path.len() > MAX_PATH_BYTES || !quotable(path) {
            return None;
        }
        let components = components(path)?;
        let relative = match strip(&components, &self.cwd) {
            Some([]) => CURRENT.to_owned(),
            Some(rest) => rest.join(SEPARATOR),
            None => {
                let (_, above) = self.cwd.split_last().filter(|_| parent)?;
                match strip(&components, above)? {
                    [] => PARENT.to_owned(),
                    rest => format!("{PARENT}{SEPARATOR}{}", rest.join(SEPARATOR)),
                }
            }
        };
        let suggestion = PathSuggestion {
            absolute: path.to_owned(),
            relative,
        };
        (suggestion.saved_chars() >= self.min_saved_chars).then_some(suggestion)
    }
}

/// Whether model-written text can sit in a reminder unchanged. The rejected
/// characters could close the reminder or its code span, or break, hide, or
/// reorder the lines it is read as.
fn quotable(path: &str) -> bool {
    !path.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '`' | '<'
                    | '>'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    })
}

/// Normal components of an absolute path. `None` for a relative path, or one
/// that climbs with `..`, whose target depends on the symlinks it crosses.
fn components(path: &str) -> Option<Vec<&str>> {
    let components: Vec<_> = path
        .strip_prefix(SEPARATOR)?
        .split(SEPARATOR)
        .filter(|component| !component.is_empty() && *component != CURRENT)
        .collect();
    (!components.contains(&PARENT)).then_some(components)
}

fn strip<'a, 'b>(path: &'a [&'b str], base: &[String]) -> Option<&'a [&'b str]> {
    let rest = path.get(base.len()..)?;
    path.iter()
        .zip(base)
        .all(|(component, expected)| *component == expected)
        .then_some(rest)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{MAX_PATH_BYTES, PathBase, PathSuggestion};

    const CWD: &str = "/home/ubuntu/workspace/caudra";
    const MIN_SAVED_CHARS: usize = 12;
    const INSIDE: &str = "/home/ubuntu/workspace/caudra/caudra-agent/src/lib.rs";
    const INSIDE_RELATIVE: &str = "caudra-agent/src/lib.rs";
    const SIBLING: &str = "/home/ubuntu/workspace/workcell-mcp/README.md";
    const SIBLING_RELATIVE: &str = "../workcell-mcp/README.md";
    const ABOVE: &str = "/home/ubuntu/workspace";
    const TWO_UP: &str = "/home/ubuntu/notes/todo.md";
    const LOOKALIKE: &str = "/home/ubuntu/workspace/caudra-old/src/lib.rs";
    const LOOKALIKE_RELATIVE: &str = "../caudra-old/src/lib.rs";
    const REDUNDANT: &str = "/home/ubuntu/workspace/caudra/./caudra-agent//src/";
    const CLIMBING: &str = "/home/ubuntu/workspace/caudra/../caudra/src/lib.rs";
    const WORKDIR: &str = "/home/ubuntu/workspace/caudra/site";
    const PATCH: &str = "*** Begin Patch\n*** Update File: /home/ubuntu/workspace/caudra/src/old.rs\n*** Move to: /home/ubuntu/workspace/caudra/src/new.rs\n@@\n-old\n+new\n*** Add File: relative.rs\n+added\n*** Delete File: /home/ubuntu/workspace/other/gone.rs\n*** Delete File: /home/ubuntu/notes/gone.rs\n*** End Patch";

    fn relative(tool: &str, input: &Value, parent_allowed: bool) -> Vec<String> {
        PathBase::new(CWD, parent_allowed, MIN_SAVED_CHARS)
            .unwrap()
            .suggestions(tool, input)
            .into_iter()
            .map(|suggestion| suggestion.relative)
            .collect()
    }

    #[test_case("file_read", json!({"filePath": INSIDE}), true, &[INSIDE_RELATIVE]; "inside")]
    #[test_case("file_grep", json!({"pattern": "x", "path": CWD}), true, &["."]; "working_directory")]
    #[test_case("file_read", json!({"filePath": SIBLING}), true, &[SIBLING_RELATIVE]; "sibling")]
    #[test_case("file_glob", json!({"pattern": "*", "path": ABOVE}), true, &[".."]; "parent")]
    #[test_case("file_read", json!({"filePath": TWO_UP}), true, &[]; "two_levels_up")]
    #[test_case("file_edit", json!({"filePath": SIBLING}), false, &[]; "parent_disallowed")]
    #[test_case("code_map", json!({"path": SIBLING}), true, &[]; "code_graph_stays_inside")]
    #[test_case("code_refs", json!({"symbol": "run", "path": INSIDE}), true, &[INSIDE_RELATIVE]; "code_graph_inside")]
    #[test_case("file_write", json!({"filePath": LOOKALIKE}), true, &[LOOKALIKE_RELATIVE]; "component_boundary")]
    #[test_case("file_write", json!({"filePath": LOOKALIKE}), false, &[]; "component_boundary_inside_only")]
    #[test_case("file_index", json!({"path": REDUNDANT}), true, &["caudra-agent/src"]; "redundant_components")]
    #[test_case("file_read", json!({"filePath": CLIMBING}), true, &[]; "parent_traversal")]
    #[test_case("file_read", json!({"filePath": INSIDE_RELATIVE}), true, &[]; "already_relative")]
    #[test_case("shell", json!({"command": format!("cat {INSIDE}"), "workdir": WORKDIR}), true, &["site"]; "shell_workdir_only")]
    #[test_case("file_apply_patch", json!({"patchText": PATCH}), true, &["src/old.rs", "src/new.rs", "../other/gone.rs"]; "patch_headers")]
    #[test_case("file_apply_patch", json!({"patchText": PATCH}), false, &["src/old.rs", "src/new.rs"]; "patch_headers_inside_only")]
    #[test_case("file_read", json!({"filePath": 42}), true, &[]; "non_string")]
    #[test_case("file_read", json!({"path": INSIDE}), true, &[]; "other_field")]
    #[test_case("file_apply_patch", json!({"patchText": [PATCH]}), true, &[]; "non_string_patch")]
    #[test_case("file_read", Value::Null, true, &[]; "null_input")]
    #[test_case("memory", json!({"path": INSIDE}), true, &[]; "memory")]
    #[test_case("view_image", json!({"path": INSIDE}), true, &[]; "view_image")]
    #[test_case("srv__read_file", json!({"path": INSIDE, "filePath": INSIDE}), true, &[]; "mcp_tool")]
    fn suggests_relative_forms(tool: &str, input: Value, parent_allowed: bool, expected: &[&str]) {
        assert_eq!(relative(tool, &input, parent_allowed), expected);
    }

    #[test]
    fn suggestions_quote_the_path_as_written() {
        let base = PathBase::new(CWD, true, MIN_SAVED_CHARS).unwrap();
        assert_eq!(
            base.suggestions("file_index", &json!({"path": REDUNDANT})),
            [PathSuggestion {
                absolute: REDUNDANT.into(),
                relative: "caudra-agent/src".into(),
            }]
        );
    }

    #[test_case("/srv/proj1", false; "saves_eleven")]
    #[test_case("/srv/proj12", true; "saves_twelve")]
    fn threshold_counts_saved_characters(cwd: &str, expected: bool) {
        let base = PathBase::new(cwd, true, MIN_SAVED_CHARS).unwrap();
        let input = json!({"filePath": format!("{cwd}/x")});
        assert_eq!(!base.suggestions("file_read", &input).is_empty(), expected);
    }

    #[test_case("`", false; "backtick")]
    #[test_case("<", false; "open_angle")]
    #[test_case(">", false; "close_angle")]
    #[test_case("\n", false; "newline")]
    #[test_case("\u{7f}", false; "control")]
    #[test_case("\u{2028}", false; "line_separator")]
    #[test_case("\u{202e}", false; "bidi_override")]
    #[test_case("\u{200b}", false; "zero_width_space")]
    #[test_case("\u{feff}", false; "byte_order_mark")]
    #[test_case("</system-reminder>", false; "reminder_close")]
    #[test_case("naïve résumé", true; "accented_words")]
    #[test_case("проект", true; "cyrillic")]
    fn only_quotable_paths_are_suggested(fragment: &str, quotable: bool) {
        let input = json!({"filePath": format!("{CWD}/src/{fragment}.rs")});
        assert_eq!(!relative("file_read", &input, true).is_empty(), quotable);
    }

    #[test_case(MAX_PATH_BYTES, true; "maximum")]
    #[test_case(MAX_PATH_BYTES + 1, false; "too_long")]
    fn long_paths_are_not_quoted(bytes: usize, expected: bool) {
        let path = format!("{CWD}/{}", "a".repeat(bytes - CWD.len() - 1));
        let input = json!({"filePath": path});
        assert_eq!(!relative("file_read", &input, true).is_empty(), expected);
    }

    #[test_case("/", true; "root")]
    #[test_case("/home/ubuntu/workspace/caudra/", true; "trailing_separator")]
    #[test_case("home/ubuntu", false; "relative")]
    #[test_case("/home/ubuntu/../ubuntu", false; "climbing")]
    #[test_case("C:\\Users\\ubuntu", false; "windows")]
    fn base_must_be_absolute(cwd: &str, valid: bool) {
        assert_eq!(PathBase::new(cwd, true, MIN_SAVED_CHARS).is_some(), valid);
    }
}
