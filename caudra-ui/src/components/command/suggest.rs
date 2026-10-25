//! The argument stage: what may stand where the caret is in a command's
//! arguments, and the values offered for it, fuzzy-filtered by what is typed.

use std::fs;
use std::ops::Range;
use std::path::Path;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use super::grammar::{ArgKind, Node, Source, locate};

const USAGE_PREFIX: &str = "Usage: ";
const PATH_SEPARATOR: &str = "/";
const HOME_PREFIX: &str = "~";
const HOME_DIRECTORY: &str = "~/";
/// More entries than a popup could ever show; a directory holding more is
/// narrowed by typing.
const MAX_PATHS: usize = 200;

/// A value a source offers for an open slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceValue {
    pub value: String,
    pub description: String,
}

impl SourceValue {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }
}

/// A row the reader can take into the slot under the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub value: String,
    pub description: String,
    /// Other words the handler reads as this one.
    pub aliases: Vec<String>,
    /// Chars of [`Self::value`] the typed token matched.
    pub indices: Vec<u32>,
    /// Taking it leaves more to type, so a space follows it.
    pub continues: bool,
}

impl Suggestion {
    fn new(value: &str, description: &str, aliases: &[&str], continues: bool) -> Self {
        Self {
            value: value.to_owned(),
            description: description.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            indices: Vec::new(),
            continues,
        }
    }

    pub fn matches(&self, word: &str) -> bool {
        self.value.eq_ignore_ascii_case(word)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(word))
    }
}

/// The dim row an open slot shows, so free text is discoverable too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    pub label: String,
    pub description: String,
}

/// A part of the signature line, and whether the caret's slot is that part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePart {
    pub text: String,
    pub current: bool,
}

/// Everything the palette shows and acts on for the slot under the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arguments {
    pub signature: Vec<SignaturePart>,
    pub suggestions: Vec<Suggestion>,
    pub placeholder: Option<Placeholder>,
    /// The word under the caret, in bytes of the whole input.
    pub token: Range<usize>,
    /// Only listed words belong here.
    pub closed: bool,
    pub source: Option<Source>,
    /// The slot's name, for the ghost of an empty slot.
    pub hint: Option<String>,
    /// The slot's argument name and the earlier slots' names and values,
    /// which a server completing it is told.
    pub argument: Option<String>,
    pub given: Vec<(String, String)>,
}

impl Arguments {
    pub fn usage(&self) -> String {
        let parts: Vec<&str> = self
            .signature
            .iter()
            .map(|part| part.text.as_str())
            .collect();
        format!("{USAGE_PREFIX}{}", parts.join(" "))
    }

    pub fn exact(&self, word: &str) -> Option<&Suggestion> {
        self.suggestions
            .iter()
            .find(|suggestion| suggestion.matches(word))
    }

    /// Whether the word typed here stands as it is, short of being one the
    /// slot lists.
    pub fn accepts_unlisted(&self, word: &str) -> bool {
        !self.closed
            || self
                .source
                .is_some_and(|source| source.accepts_unlisted(word))
    }
}

/// What stands at `caret` in the arguments of `command`, or `None` past
/// every slot it has. `args` starts `offset` bytes into the input, and
/// `caret` counts bytes of `args`. `values` are what the slot's source
/// offers, when it has any.
pub fn arguments(
    command: &str,
    node: Node<'_>,
    args: &str,
    offset: usize,
    caret: usize,
    values: &[SourceValue],
    matcher: &mut Matcher,
) -> Option<Arguments> {
    let position = locate(node, args, caret);
    let arg = position.arg();
    if arg.is_none() && position.subcommands().is_empty() {
        return None;
    }
    let continues = position.continues();
    let mut candidates: Vec<Suggestion> = position
        .subcommands()
        .iter()
        .map(|sub| Suggestion::new(sub.name, sub.description, sub.aliases, sub.continues()))
        .collect();
    let mut placeholder = None;
    if let Some(arg) = arg {
        match arg.kind {
            ArgKind::Word(choices) => candidates.extend(choices.iter().map(|choice| {
                Suggestion::new(choice.value, choice.description, choice.aliases, continues)
            })),
            ArgKind::Value(source) | ArgKind::Text(source) => {
                if source.is_some() {
                    candidates.extend(values.iter().map(|value| {
                        Suggestion::new(&value.value, &value.description, &[], continues)
                    }));
                }
                placeholder = Some(Placeholder {
                    label: format!("<{}>", arg.name),
                    description: arg.description.to_owned(),
                });
            }
        }
    }
    let token = &args[position.token.clone()];
    let parts = position.node.parts();
    let current = (position.head || arg.is_some()).then_some(position.slot);
    let signature = std::iter::once(SignaturePart {
        text: command.to_owned(),
        current: false,
    })
    .chain(position.path.iter().map(|name| SignaturePart {
        text: (*name).to_owned(),
        current: false,
    }))
    .chain(
        parts
            .into_iter()
            .enumerate()
            .map(|(index, text)| SignaturePart {
                text,
                current: current == Some(index),
            }),
    )
    .collect();
    Some(Arguments {
        signature,
        suggestions: filter(token, candidates, matcher),
        placeholder,
        token: offset + position.token.start..offset + position.token.end,
        closed: position.is_closed(),
        source: arg.and_then(|arg| arg.source()),
        hint: arg.map(|arg| arg.placeholder()),
        argument: arg.map(|arg| arg.name.to_owned()),
        given: position
            .node
            .args
            .iter()
            .zip(&position.values)
            .map(|(arg, range)| (arg.name.to_owned(), args[range.clone()].to_owned()))
            .collect(),
    })
}

/// The candidates `token` fuzzy-matches, best first; all of them, in their
/// own order, while nothing is typed. An alias that matches keeps its row.
pub fn filter(token: &str, candidates: Vec<Suggestion>, matcher: &mut Matcher) -> Vec<Suggestion> {
    if token.trim().is_empty() {
        return candidates;
    }
    let pattern = Pattern::parse(token, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, Suggestion)> = candidates
        .into_iter()
        .filter_map(|mut suggestion| {
            let mut indices = Vec::new();
            let score = pattern
                .indices(
                    Utf32Str::new(&suggestion.value, &mut buffer),
                    matcher,
                    &mut indices,
                )
                .or_else(|| {
                    indices.clear();
                    suggestion
                        .aliases
                        .iter()
                        .filter_map(|alias| {
                            pattern.score(Utf32Str::new(alias, &mut buffer), matcher)
                        })
                        .max()
                })?;
            indices.sort_unstable();
            indices.dedup();
            suggestion.indices = indices;
            Some((score, suggestion))
        })
        .collect();
    scored.sort_by(|(left, _), (right, _)| right.cmp(left));
    scored
        .into_iter()
        .map(|(_, suggestion)| suggestion)
        .collect()
}

/// The entries of the directory `token` names, completing its last part.
/// `token` is read from `base`, or from `home` when it starts with `~`; dot
/// entries are listed only once the last part starts with a dot.
pub fn list_paths(
    base: &Path,
    home: Option<&Path>,
    token: &str,
    directories_only: bool,
) -> Vec<SourceValue> {
    let (parent, prefix) = match token.rfind(PATH_SEPARATOR) {
        Some(index) => token.split_at(index + 1),
        None if token == HOME_PREFIX => (HOME_DIRECTORY, ""),
        None => ("", token),
    };
    let directory = match parent.strip_prefix(HOME_PREFIX) {
        Some(rest) => match home {
            Some(home) => home.join(rest.trim_start_matches(PATH_SEPARATOR)),
            None => return Vec::new(),
        },
        None => base.join(parent),
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    let hidden = prefix.starts_with('.');
    let mut values: Vec<SourceValue> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with('.') != hidden || !name.to_lowercase().starts_with(&prefix) {
                return None;
            }
            let is_dir = entry.path().is_dir();
            if directories_only && !is_dir {
                return None;
            }
            let separator = if is_dir { PATH_SEPARATOR } else { "" };
            Some(SourceValue::new(format!("{parent}{name}{separator}"), ""))
        })
        .collect();
    values.sort_by(|left, right| left.value.cmp(&right.value));
    values.truncate(MAX_PATHS);
    values
}

#[cfg(test)]
mod tests {
    use nucleo_matcher::Config;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::components::command::grammar::{Arg, Choice, Subcommand};

    const COMMAND: &str = "/demo";
    const LEVELS: &[Choice] = &[Choice::new("low", "Less"), Choice::new("high", "More")];
    const BRANCH: &[Arg] = &[Arg::value("branch", "A branch", Some(Source::Tasks))];
    const SUBS: &[Subcommand] = &[
        Subcommand::new("new", "Make one", Node::args(BRANCH)),
        Subcommand::new("clear", "Drop it", Node::args(&[])).aliases(&["stop"]),
    ];
    const ROOT_ARGS: &[Arg] = &[
        Arg::words("level", "", LEVELS),
        Arg::text("rest", "Anything", None),
    ];
    const ROOT: Node = Node::with(SUBS, ROOT_ARGS);
    const TEXT_ONLY: Node = Node::args(&[Arg::text("condition", "Until true", None)]);

    fn at(node: Node<'static>, args: &str, values: &[SourceValue]) -> Arguments {
        try_at(node, args, values).expect("a slot stands at the caret")
    }

    fn try_at(node: Node<'static>, args: &str, values: &[SourceValue]) -> Option<Arguments> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let offset = COMMAND.len() + 1;
        arguments(
            COMMAND,
            node,
            args,
            offset,
            args.len(),
            values,
            &mut matcher,
        )
    }

    #[test_case("clear " ; "after_a_leaf_subcommand")]
    #[test_case("new main " ; "after_the_last_slot")]
    fn nothing_stands_past_the_last_slot(args: &str) {
        assert_eq!(try_at(ROOT, args, &[]), None);
    }

    fn offered(arguments: &Arguments) -> Vec<&str> {
        arguments
            .suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect()
    }

    #[test]
    fn an_empty_head_offers_subcommands_then_choices() {
        let arguments = at(ROOT, "", &[]);
        assert_eq!(offered(&arguments), ["new", "clear", "low", "high"]);
        assert!(arguments.closed);
        assert_eq!(arguments.placeholder, None);
    }

    #[test_case("hg", "high" ; "a_typo_still_finds_the_word")]
    #[test_case("sto", "clear" ; "an_alias_keeps_its_row")]
    fn the_typed_token_filters(token: &str, first: &str) {
        assert_eq!(offered(&at(ROOT, token, &[]))[0], first);
    }

    #[test]
    fn a_value_slot_lists_its_source_and_a_placeholder() {
        let values = [SourceValue::new("main", ""), SourceValue::new("dev", "")];
        let arguments = at(ROOT, "new ", &values);
        assert_eq!(offered(&arguments), ["main", "dev"]);
        assert!(!arguments.closed);
        assert_eq!(arguments.placeholder.unwrap().label, "<branch>");
        assert_eq!(arguments.hint.as_deref(), Some("⟨branch⟩"));
    }

    #[test]
    fn a_text_slot_takes_the_rest_of_the_line() {
        let input = "fix the bug";
        let arguments = at(TEXT_ONLY, input, &[]);
        let offset = COMMAND.len() + 1;
        assert_eq!(arguments.token, offset..offset + input.len());
        assert!(arguments.suggestions.is_empty());
        assert!(!arguments.closed);
    }

    #[test]
    fn the_signature_marks_the_current_slot() {
        let arguments = at(ROOT, "new ", &[]);
        let current: Vec<_> = arguments
            .signature
            .iter()
            .filter(|part| part.current)
            .map(|part| part.text.as_str())
            .collect();
        assert_eq!(current, ["[branch]"]);
        assert_eq!(arguments.usage(), "Usage: /demo new [branch]");
    }

    #[test_case("STOP", Some("clear") ; "alias_ignoring_case")]
    #[test_case("High", Some("high") ; "value_ignoring_case")]
    #[test_case("hig", None ; "a_prefix_is_not_exact")]
    fn exact_matches_ignore_case(word: &str, expected: Option<&str>) {
        let arguments = at(ROOT, word, &[]);
        assert_eq!(
            arguments
                .exact(word)
                .map(|suggestion| suggestion.value.as_str()),
            expected
        );
    }

    #[test]
    fn a_continuing_subcommand_says_so() {
        let arguments = at(ROOT, "", &[]);
        let continues: Vec<bool> = arguments
            .suggestions
            .iter()
            .map(|suggestion| suggestion.continues)
            .collect();
        assert_eq!(continues, [true, false, true, true]);
    }

    fn tree() -> TempDir {
        let root = TempDir::new().unwrap();
        for directory in ["work", "Wolf", ".hidden", "work/inner"] {
            fs::create_dir(root.path().join(directory)).unwrap();
        }
        fs::write(root.path().join("words.txt"), "").unwrap();
        root
    }

    fn listed(root: &TempDir, token: &str, directories_only: bool) -> Vec<String> {
        list_paths(root.path(), Some(root.path()), token, directories_only)
            .into_iter()
            .map(|value| value.value)
            .collect()
    }

    #[test_case("wo", true, &["Wolf/", "work/"] ; "directories_by_prefix_ignoring_case")]
    #[test_case("wo", false, &["Wolf/", "words.txt", "work/"] ; "files_too")]
    #[test_case("work/", true, &["work/inner/"] ; "inside_a_directory")]
    #[test_case(".", true, &[".hidden/"] ; "dot_entries_once_asked_for")]
    #[test_case("~/wor", true, &["~/work/"] ; "from_home")]
    #[test_case("missing/", true, &[] ; "a_missing_directory_offers_nothing")]
    fn paths_complete_the_last_part(token: &str, directories_only: bool, expected: &[&str]) {
        let root = tree();
        assert_eq!(listed(&root, token, directories_only), expected);
    }
}
