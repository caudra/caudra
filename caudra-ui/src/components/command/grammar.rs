//! What a command takes after its name: subcommands, then positional
//! arguments. One walker reads it for every surface, so the palette, the
//! modal, the ghost text and the dispatcher agree on where the caret is.

use std::ops::Range;

use super::ChatScope;

pub const SIGNATURE_WIDTH: usize = 32;
const ELLIPSIS: char = '…';
const ALTERNATIVE: &str = "|";

#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    pub subcommands: &'a [Subcommand<'a>],
    pub args: &'a [Arg<'a>],
}

#[derive(Debug, Clone, Copy)]
pub struct Subcommand<'a> {
    pub name: &'a str,
    pub aliases: &'a [&'a str],
    pub description: &'a str,
    /// Overrides the parent's scope for this subcommand and what it leads to.
    pub scope: Option<ChatScope>,
    pub node: Node<'a>,
}

#[derive(Debug, Clone, Copy)]
pub struct Arg<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub required: bool,
    pub kind: ArgKind<'a>,
}

#[derive(Debug, Clone, Copy)]
pub enum ArgKind<'a> {
    /// One word out of a closed set.
    Word(&'a [Choice<'a>]),
    /// One word of any value, with suggestions when a source has them.
    Value(Option<Source>),
    /// The rest of the line, spaces and all.
    Text(Option<Source>),
}

#[derive(Debug, Clone, Copy)]
pub struct Choice<'a> {
    pub value: &'a str,
    pub aliases: &'a [&'a str],
    pub description: &'a str,
}

/// Where the values of an open slot come from. The palette answers the
/// asynchronous ones itself; the app supplies the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    ThinkingLevels,
    Tasks,
    TasksOrShells,
    WorkflowNames,
    WorkflowRuns,
    Automations,
    Directories,
    Files,
    DocsAddresses,
    PeerReviewed,
    PeerTopics,
    PeerJoinedGroups,
    McpPrompt,
    Lua,
}

impl Source {
    /// A closed source refuses what it does not list, short of
    /// [`Self::accepts_unlisted`].
    pub fn is_closed(self) -> bool {
        self == Self::ThinkingLevels
    }

    pub fn accepts_unlisted(self, token: &str) -> bool {
        match self {
            Self::ThinkingLevels => token.parse::<u32>().is_ok_and(|budget| budget > 0),
            _ => !self.is_closed(),
        }
    }

    /// Answered by the palette off the event loop rather than by the app.
    pub fn is_async(self) -> bool {
        matches!(
            self,
            Self::Directories | Self::Files | Self::McpPrompt | Self::Lua
        )
    }
}

impl<'a> Node<'a> {
    pub const fn args(args: &'a [Arg<'a>]) -> Self {
        Self {
            subcommands: &[],
            args,
        }
    }

    pub const fn with(subcommands: &'a [Subcommand<'a>], args: &'a [Arg<'a>]) -> Self {
        Self { subcommands, args }
    }

    pub fn is_leaf(&self) -> bool {
        self.subcommands.is_empty() && self.args.is_empty()
    }

    /// The most whitespace-separated words the node can take, which is how
    /// far the palette keeps a command matched while arguments are typed.
    pub fn max_args(&self) -> usize {
        let own = match self
            .args
            .iter()
            .any(|arg| matches!(arg.kind, ArgKind::Text(_)))
        {
            true => usize::MAX,
            false => self.args.len(),
        };
        self.subcommands
            .iter()
            .map(|sub| sub.node.max_args().saturating_add(1))
            .fold(own, usize::max)
    }

    pub fn find(&self, word: &str) -> Option<&'a Subcommand<'a>> {
        self.subcommands.iter().find(|sub| {
            sub.name.eq_ignore_ascii_case(word)
                || sub
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(word))
        })
    }

    /// One part per slot. Subcommands share the first part with the first
    /// argument, since either may stand there, so slot `i` is part `i`. That
    /// part is optional unless the first argument is required.
    pub fn parts(&self) -> Vec<String> {
        let mut args = self.args.iter();
        let mut parts = Vec::with_capacity(self.args.len().max(1));
        if !self.subcommands.is_empty() {
            let first = args.next();
            let mut alternatives: Vec<String> = self
                .subcommands
                .iter()
                .map(|sub| sub.name.to_owned())
                .collect();
            alternatives.extend(first.map(Arg::bare));
            let alternatives = alternatives.join(ALTERNATIVE);
            parts.push(match first.is_some_and(|arg| arg.required) {
                true => alternatives,
                false => format!("[{alternatives}]"),
            });
        }
        parts.extend(args.map(Arg::signature));
        parts
    }

    pub fn signature(&self) -> String {
        self.parts().join(" ")
    }
}

impl<'a> Subcommand<'a> {
    pub const fn new(name: &'a str, description: &'a str, node: Node<'a>) -> Self {
        Self {
            name,
            aliases: &[],
            description,
            scope: None,
            node,
        }
    }

    pub const fn aliases(mut self, aliases: &'a [&'a str]) -> Self {
        self.aliases = aliases;
        self
    }

    pub const fn main_only(mut self) -> Self {
        self.scope = Some(ChatScope::MainOnly);
        self
    }

    /// Whether completing it leaves more to type.
    pub fn continues(&self) -> bool {
        !self.node.is_leaf()
    }
}

impl<'a> Arg<'a> {
    pub const fn words(name: &'a str, description: &'a str, choices: &'a [Choice<'a>]) -> Self {
        Self {
            name,
            description,
            required: false,
            kind: ArgKind::Word(choices),
        }
    }

    pub const fn value(name: &'a str, description: &'a str, source: Option<Source>) -> Self {
        Self {
            name,
            description,
            required: false,
            kind: ArgKind::Value(source),
        }
    }

    pub const fn text(name: &'a str, description: &'a str, source: Option<Source>) -> Self {
        Self {
            name,
            description,
            required: false,
            kind: ArgKind::Text(source),
        }
    }

    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn source(&self) -> Option<Source> {
        match self.kind {
            ArgKind::Word(_) => None,
            ArgKind::Value(source) | ArgKind::Text(source) => source,
        }
    }

    /// Whether the slot accepts only the words it lists.
    pub fn is_closed(&self) -> bool {
        match self.kind {
            ArgKind::Word(_) => true,
            ArgKind::Value(source) | ArgKind::Text(source) => source.is_some_and(Source::is_closed),
        }
    }

    /// The slot as written inside an alternation: no brackets of its own.
    fn bare(&self) -> String {
        match self.kind {
            ArgKind::Word(choices) => choices
                .iter()
                .map(|choice| choice.value)
                .collect::<Vec<_>>()
                .join(ALTERNATIVE),
            ArgKind::Value(_) | ArgKind::Text(_) => format!("<{}>", self.name),
        }
    }

    pub fn signature(&self) -> String {
        match (self.kind, self.required) {
            (ArgKind::Word(_), true) => self.bare(),
            (ArgKind::Word(_), false) => format!("[{}]", self.bare()),
            (_, true) => format!("<{}>", self.name),
            (_, false) => format!("[{}]", self.name),
        }
    }

    /// What the composer shows in an empty slot.
    pub fn placeholder(&self) -> String {
        format!("⟨{}⟩", self.name)
    }
}

impl<'a> Choice<'a> {
    pub const fn new(value: &'a str, description: &'a str) -> Self {
        Self {
            value,
            aliases: &[],
            description,
        }
    }

    pub const fn aliases(mut self, aliases: &'a [&'a str]) -> Self {
        self.aliases = aliases;
        self
    }

    pub fn matches(&self, word: &str) -> bool {
        self.value.eq_ignore_ascii_case(word)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(word))
    }
}

/// Cuts `text` to `width` chars, marking the cut.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push(ELLIPSIS);
    cut
}

/// Where the caret stands in a command's arguments.
#[derive(Debug, Clone)]
pub struct Position<'a> {
    pub node: Node<'a>,
    /// The subcommands walked into, by their canonical names.
    pub path: Vec<&'a str>,
    pub scope: Option<ChatScope>,
    /// A subcommand of [`Self::node`] may stand here.
    pub head: bool,
    pub slot: usize,
    /// The word under the caret, in bytes of the arguments: the rest of the
    /// line for a text slot, and empty at the caret between words.
    pub token: Range<usize>,
    /// The words already given to the node's earlier slots.
    pub values: Vec<Range<usize>>,
}

impl<'a> Position<'a> {
    pub fn arg(&self) -> Option<&'a Arg<'a>> {
        self.node.args.get(self.slot)
    }

    /// Whether only listed words belong here.
    pub fn is_closed(&self) -> bool {
        match self.arg() {
            Some(arg) => arg.is_closed(),
            None => self.head && !self.node.subcommands.is_empty(),
        }
    }

    /// Whether a value given here leaves more slots after it.
    pub fn continues(&self) -> bool {
        self.slot + 1 < self.node.args.len()
    }

    pub fn subcommands(&self) -> &'a [Subcommand<'a>] {
        match self.head {
            true => self.node.subcommands,
            false => &[],
        }
    }
}

/// Byte ranges of the whitespace-separated words of `text`.
pub fn words(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut from = 0;
    std::iter::from_fn(move || {
        let start = from + text[from..].find(|c: char| !c.is_whitespace())?;
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| start + offset);
        from = end;
        Some(start..end)
    })
}

/// Walks `args` up to `caret`, a byte offset into them: a word that names a
/// subcommand descends into it, and every other word fills the next slot,
/// a text slot taking the rest of the line.
pub fn locate<'a>(root: Node<'a>, args: &str, caret: usize) -> Position<'a> {
    let mut position = Position {
        node: root,
        path: Vec::new(),
        scope: None,
        head: true,
        slot: 0,
        token: caret..caret,
        values: Vec::new(),
    };
    let takes_rest =
        |position: &Position| matches!(position.arg().map(|arg| arg.kind), Some(ArgKind::Text(_)));
    for word in words(args) {
        if word.start > caret {
            break;
        }
        if caret <= word.end {
            position.token = match takes_rest(&position) {
                true => word.start..args.len(),
                false => word,
            };
            return position;
        }
        if position.head
            && let Some(sub) = position.node.find(&args[word.clone()])
        {
            position.node = sub.node;
            position.path.push(sub.name);
            position.scope = sub.scope.or(position.scope);
            position.slot = 0;
            position.values.clear();
            continue;
        }
        position.head = false;
        if takes_rest(&position) {
            position.token = word.start..args.len();
            return position;
        }
        position.values.push(word);
        position.slot += 1;
    }
    position.token = caret..caret;
    position
}

/// The scope a command line runs in: the innermost subcommand's, when one
/// of its leading words names one.
pub fn scope_of(root: Node<'_>, args: &str) -> Option<ChatScope> {
    let mut node = root;
    let mut scope = None;
    for word in words(args) {
        let Some(sub) = node.find(&args[word]) else {
            break;
        };
        scope = sub.scope.or(scope);
        node = sub.node;
    }
    scope
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const LEVELS: &[Choice] = &[Choice::new("low", ""), Choice::new("high", "")];
    const NEW_ARGS: &[Arg] = &[Arg::value("branch", "", None)];
    const SUBS: &[Subcommand] = &[
        Subcommand::new("new", "", Node::args(NEW_ARGS)).main_only(),
        Subcommand::new("close", "", Node::args(&[])).aliases(&["remove"]),
    ];
    const ROOT_ARGS: &[Arg] = &[Arg::words("level", "", LEVELS), Arg::text("rest", "", None)];
    const ROOT: Node = Node::with(SUBS, ROOT_ARGS);

    fn at(args: &str) -> Position<'static> {
        locate(ROOT, args, args.len())
    }

    #[test_case("", true, 0, 0..0 ; "empty")]
    #[test_case("ne", true, 0, 0..2 ; "typing_a_subcommand")]
    #[test_case("NEW ", true, 0, 4..4 ; "descends_ignoring_case")]
    #[test_case("new ma", true, 0, 4..6 ; "a_value_inside_the_subcommand")]
    #[test_case("high ", false, 1, 5..5 ; "positional_order")]
    #[test_case("high fix the bug", false, 1, 5..16 ; "text_takes_the_rest")]
    fn the_walker_finds_the_slot(args: &str, head: bool, slot: usize, token: Range<usize>) {
        let position = at(args);
        assert_eq!(
            (position.head, position.slot, position.token),
            (head, slot, token)
        );
    }

    #[test]
    fn an_alias_descends_into_its_subcommand() {
        assert_eq!(at("remove ").path, ["close"]);
    }

    #[test]
    fn the_caret_inside_a_word_takes_the_whole_word() {
        let position = locate(ROOT, "new main", 5);
        assert_eq!(position.token, 4..8);
    }

    #[test_case("new", Some(ChatScope::MainOnly) ; "subcommand_scope")]
    #[test_case("close", None ; "inherits")]
    #[test_case("high", None ; "positional")]
    fn scope_follows_the_subcommand(args: &str, expected: Option<ChatScope>) {
        assert_eq!(scope_of(ROOT, args), expected);
    }

    #[test]
    fn max_args_counts_the_deepest_path() {
        const FLAT: Node = Node::args(NEW_ARGS);
        const NESTED: Node = Node::with(SUBS, &[]);
        assert_eq!(FLAT.max_args(), 1);
        assert_eq!(NESTED.max_args(), 2);
        assert_eq!(ROOT.max_args(), usize::MAX);
    }

    #[test]
    fn the_signature_merges_subcommands_with_the_first_slot() {
        assert_eq!(ROOT.signature(), "[new|close|low|high] [rest]");
        assert_eq!(Node::args(NEW_ARGS).signature(), "[branch]");
    }

    #[test_case("short", 10, "short" ; "fits")]
    #[test_case("a much longer one", 6, "a muc…" ; "cut")]
    fn truncation_marks_the_cut(text: &str, width: usize, expected: &str) {
        assert_eq!(truncate(text, width), expected);
    }
}
