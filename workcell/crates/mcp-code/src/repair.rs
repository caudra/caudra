//! Repairs a hosted snippet before it runs, where what it meant has only one reading.
//!
//! A snippet's calls reach real servers, so a repair that guessed could run something its author
//! never wrote. Each one here keeps every line where it was written, so errors still point at the
//! author's own lines, and anything the parser cannot read whole is left exactly as it is.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;

use ruff_python_ast::PySourceType;
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_parser::{parse_module, parse_unchecked_source};
use ruff_text_size::{Ranged, TextRange};
use tree_sitter::{Node, Parser, Tree};

use crate::subset::SUBSET_MODULES;

const FENCE: &str = "```";
const FENCE_LANGUAGES: [&str; 4] = ["", "python", "py", "python3"];
const IDENTIFIER: &str = "identifier";
const ATTRIBUTE: &str = "attribute";
const CALL: &str = "call";
const AWAIT: &str = "await";
const OBJECT_FIELD: &str = "object";
const FUNCTION_FIELD: &str = "function";
const ATTRIBUTE_FIELD: &str = "attribute";
const ARGUMENTS_FIELD: &str = "arguments";
const NAME_FIELD: &str = "name";
const VALUE_FIELD: &str = "value";
const ARGUMENT_LIST: &str = "argument_list";
const KEYWORD_ARGUMENT: &str = "keyword_argument";
const LIST_SPLAT: &str = "list_splat";
const DICTIONARY_SPLAT: &str = "dictionary_splat";
const COMMA: &str = ",";
const RETURN_STATEMENT: &str = "return_statement";
const TRUE: &str = "true";
const FALSE: &str = "false";
const ASYNCIO: &str = "asyncio";
const GATHER: &str = "gather";
const RETURN_EXCEPTIONS: &str = "return_exceptions";
const NONE: &str = "None";
/// Text no Python statement contains.
const JAVASCRIPT_MARKERS: [&str; 4] = ["=>", "===", "Promise.", "console.log("];
const JAVASCRIPT_DECLARATIONS: [&str; 4] = ["const ", "let ", "var ", "function "];
/// What JavaScript writes before a name it assigns, and Python never does.
const ASSIGNING_KEYWORDS: [&str; 3] = ["const", "let", "var"];
const STATEMENT_SEPARATOR: &str = ";";
const STRING: &str = "string";
const DICTIONARY: &str = "dictionary";
const PAIR: &str = "pair";
const KEY_FIELD: &str = "key";
const KEYWORD_ASSIGN: &str = "=";
// MONTY-GATHER-SHIM: temporary, until Monty's `asyncio.gather` takes `return_exceptions`.
// Monty 1.1.0 declares the keyword in `monty-typeshed/custom/asyncio.pyi`, so the checker passes
// it, but `monty/src/modules/asyncio.rs` rejects every gather keyword at runtime
// (`kwargs_not_supported_yet`). Once a pinned Monty release accepts it, delete everything tagged
// MONTY-GATHER-SHIM (`grep -rn MONTY-GATHER-SHIM workcell`): `SETTLED_HELPER`, `Repair::Settled`,
// `settled_gather`, `Repaired::settles`, and `Prelude::settles` in hosted.rs. The gather cases in
// `tests/hosted.rs` must then still pass with no repair listed.
/// Defined before a snippet whose `asyncio.gather(..., return_exceptions=True)` was rewritten: the
/// interpreter's `gather` takes no keyword arguments, so each awaitable settles on its own instead.
/// The checker sees earlier turns through `from ... import *`, which skips `_`-prefixed names.
pub(crate) const SETTLED_HELPER: &str = "async def caudra_settled(awaitable):\n    try:\n        return await awaitable\n    except Exception as error:\n        return error\n";
const SETTLED_OPEN: &str = "caudra_settled(";
const SETTLED_CLOSE: &str = ")";
const SETTLED_SPLAT_OPEN: &str = "[caudra_settled(_caudra_item) for _caudra_item in ";
const SETTLED_SPLAT_CLOSE: &str = "]";
const FUNCTION_DEFINITION: &str = "function_definition";
const CLASS_DEFINITION: &str = "class_definition";
const EXPRESSION_STATEMENT: &str = "expression_statement";
const ASSIGNMENT: &str = "assignment";
const ASYNC: &str = "async";
const BODY_FIELD: &str = "body";
const LEFT_FIELD: &str = "left";
const RIGHT_FIELD: &str = "right";
const TYPE_FIELD: &str = "type";
const INDENT: u8 = b' ';

/// One change made to a snippet before it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// A Markdown fence around the whole snippet was blanked out.
    Unfenced,
    /// JavaScript's `const`, `let` or `var` was dropped before an assignment.
    Undeclared(&'static str),
    /// A line indented deeper than its block, and the lines under it, were lined up with the block.
    Dedented,
    /// JavaScript's single object argument to a host call was passed as keyword arguments.
    Unpacked(String),
    /// A module the snippet used without importing it was imported before it ran.
    Imported(&'static str),
    /// `await` before a call to a host function that answers at once was dropped.
    Unawaited(String),
    /// `return` was dropped from the snippet's last statement, which then ends on its value.
    FinalReturn,
    /// `asyncio.gather(..., return_exceptions=...)` was rewritten to run without the keyword.
    // MONTY-GATHER-SHIM: remove with the rest of the shim; see `SETTLED_HELPER`.
    Settled,
}

impl Repair {
    /// What the model should write next time, for a repair that undid its slip. `None` when what
    /// it wrote was fine and only the interpreter needed the change.
    #[must_use]
    pub fn lesson(&self) -> Option<String> {
        match self {
            Self::Unfenced => Some("send plain Python, without a Markdown fence".to_owned()),
            Self::Undeclared(keyword) => Some(format!(
                "snippets are Python, so assign with `name = value`, without `{keyword}`"
            )),
            Self::Dedented => Some(
                "start each line at its block's indentation: only a line after one ending in `:` goes deeper"
                    .to_owned(),
            ),
            Self::Unpacked(function) => Some(format!(
                "calls take Python arguments, so write `{function}(name=value)` rather than one JavaScript object"
            )),
            Self::Imported(module) => Some(format!(
                "`{module}` was used without `import {module}`, so it was imported for this run"
            )),
            Self::Unawaited(function) => Some(format!(
                "`{function}()` answers at once, so the `await` before it was dropped"
            )),
            Self::FinalReturn => Some(
                "a snippet is module code, so end on the expression rather than `return` it"
                    .to_owned(),
            ),
            Self::Settled => None,
        }
    }
}

impl fmt::Display for Repair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unfenced => formatter.write_str("removed the Markdown fence around the code"),
            Self::Undeclared(keyword) => {
                write!(
                    formatter,
                    "dropped JavaScript's `{keyword}` before an assignment"
                )
            }
            Self::Dedented => formatter.write_str("lined up an over-indented line with its block"),
            Self::Unpacked(function) => {
                write!(
                    formatter,
                    "passed the object to `{function}()` as keyword arguments"
                )
            }
            Self::Imported(module) => write!(formatter, "imported `{module}`"),
            Self::Unawaited(function) => {
                write!(formatter, "dropped `await` before `{function}()`")
            }
            Self::FinalReturn => formatter.write_str("dropped `return` from the last line"),
            Self::Settled => {
                formatter.write_str("ran `asyncio.gather` without `return_exceptions`")
            }
        }
    }
}

pub(crate) struct Repaired {
    pub(crate) code: String,
    pub(crate) repairs: Vec<Repair>,
    /// The snippet with only the changes that undid the model's own slips, line for line with what
    /// it wrote, so it can see what it should have written. `None` when no such change touched it.
    pub(crate) taught: Option<String>,
}

impl Repaired {
    pub(crate) fn imports(&self) -> Vec<&'static str> {
        self.repairs
            .iter()
            .filter_map(|repair| match repair {
                Repair::Imported(module) => Some(*module),
                Repair::Unfenced
                | Repair::Undeclared(_)
                | Repair::Dedented
                | Repair::Unpacked(_)
                | Repair::Unawaited(_)
                | Repair::FinalReturn
                | Repair::Settled => None,
            })
            .collect()
    }

    /// Whether the snippet calls the helper [`SETTLED_HELPER`] defines.
    // MONTY-GATHER-SHIM: remove with the rest of the shim; see `SETTLED_HELPER`.
    pub(crate) fn settles(&self) -> bool {
        self.repairs.contains(&Repair::Settled)
    }
}

/// Repairs `code`, whose host declares its functions in `stubs`. Every repair finds its slip in
/// the parser's tree, and the parser reads the result again: a stage whose result it cannot read
/// whole is dropped, so a repair never leaves a snippet worse than it came.
pub(crate) fn repair(code: String, stubs: &str) -> Repaired {
    let stubs = StubIndex::of(stubs);
    let (code, mut repairs) = syntax_repaired(code, &stubs);
    let syntax_taught = (!repairs.is_empty()).then(|| code.clone());
    let unedited = |code: String, repairs: Vec<Repair>| Repaired {
        code,
        repairs,
        taught: syntax_taught.clone(),
    };
    let Some(tree) = parse(&code).filter(|tree| !tree.root_node().has_error()) else {
        return unedited(code, repairs);
    };
    let root = tree.root_node();
    let uses = Uses::of(root, &code, &stubs);
    repairs.extend(
        SUBSET_MODULES
            .into_iter()
            .filter(|module| uses.qualified.contains(module) && !uses.elsewhere.contains(module))
            .map(Repair::Imported),
    );
    let mut edits = Vec::new();
    let mut edited = Vec::new();
    let mut note = |repair: Repair| {
        if !edited.contains(&repair) {
            edited.push(repair);
        }
    };
    for awaited in uses.awaits {
        if uses.elsewhere.contains(awaited.root.as_str()) {
            continue;
        }
        edits.push(Edit::remove(awaited.start, awaited.end));
        note(Repair::Unawaited(awaited.function));
    }
    if let Some(edit) = final_return(root, &code) {
        edits.push(edit);
        note(Repair::FinalReturn);
    }
    let names = names_outside_keys(root, &code);
    walk(root, |node| {
        if let Some((function, fields)) = object_arguments(node, &code, &stubs.namespaces, &names) {
            edits.extend(fields);
            note(Repair::Unpacked(function));
        }
    });
    let mut shim = Vec::new();
    walk(root, |node| {
        if let Some(gather) = settled_gather(node, &code) {
            shim.extend(gather);
        }
    });
    if !shim.is_empty() {
        note(Repair::Settled);
    }
    if edits.is_empty() && shim.is_empty() {
        return unedited(code, repairs);
    }
    let taught = (!edits.is_empty()).then(|| applied(&code, edits.clone()));
    let repaired = applied(&code, edits.into_iter().chain(shim).collect());
    if !reads_as_python(&repaired) || !taught.as_deref().is_none_or(reads_as_python) {
        return unedited(code, repairs);
    }
    repairs.extend(edited);
    Repaired {
        code: repaired,
        repairs,
        taught: taught.or(syntax_taught),
    }
}

/// A snippet Python cannot read, with its Markdown fence blanked, JavaScript's declarations dropped,
/// and over-indented blocks lined up: kept only when together they leave one Python reads whole,
/// else returned as it came.
fn syntax_repaired(code: String, stubs: &StubIndex) -> (String, Vec<Repair>) {
    if reads_as_python(&code) {
        return (code, Vec::new());
    }
    let mut repairs = Vec::new();
    let mut candidate = None;
    if let Some(unfenced) = unfenced(&code) {
        repairs.push(Repair::Unfenced);
        candidate = Some(unfenced);
    }
    if let Some((undeclared, keywords)) = undeclared(candidate.as_deref().unwrap_or(&code)) {
        repairs.extend(keywords.into_iter().map(Repair::Undeclared));
        candidate = Some(undeclared);
    }
    if let Some(dedented) = dedented(candidate.as_deref().unwrap_or(&code), stubs) {
        repairs.push(Repair::Dedented);
        candidate = Some(dedented);
    }
    match candidate.filter(|candidate| reads_as_python(candidate)) {
        Some(candidate) => (candidate, repairs),
        None => (code, Vec::new()),
    }
}

/// `code` with `edits` made, from the end so earlier offsets stay valid.
fn applied(code: &str, mut edits: Vec<Edit>) -> String {
    let mut code = code.to_owned();
    edits.sort_by_key(|edit| (edit.start, edit.end));
    for edit in edits.iter().rev() {
        code.replace_range(edit.start..edit.end, edit.text);
    }
    code
}

/// The tree's leaves in source order: every token the parser read, never a skipped character.
fn tokens(tree: &Tree) -> Vec<Node<'_>> {
    let mut tokens = Vec::new();
    walk(tree.root_node(), |node| {
        if node.child_count() == 0 && !node.is_missing() {
            tokens.push(node);
        }
    });
    tokens
}

/// Whether the parser could not read the node: it is, or sits inside, an error.
fn within_error(node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        if node.is_error() {
            return true;
        }
        current = node.parent();
    }
    false
}

/// The rows the parser could not read: those an error or a missing token touches.
fn rejected_rows(tree: &Tree) -> HashSet<usize> {
    let mut rows = HashSet::new();
    walk(tree.root_node(), |node| {
        if node.is_error() || node.is_missing() {
            rows.extend(node.start_position().row..=node.end_position().row);
        }
    });
    rows
}

/// Whether code Python cannot parse reads as JavaScript, as a model used to a JavaScript code tool
/// writes it.
pub(crate) fn looks_like_javascript(code: &str) -> bool {
    parse(code).is_some_and(|tree| tree.root_node().has_error())
        && (JAVASCRIPT_MARKERS
            .iter()
            .any(|marker| code.contains(marker))
            || code.lines().any(|line| {
                let line = line.trim_start();
                JAVASCRIPT_DECLARATIONS
                    .iter()
                    .any(|declaration| line.starts_with(declaration))
            }))
}

/// Replaces `start..end` with `text`. An insertion is empty, so it sorts before a removal at the
/// same place and lands in front of what is left there.
#[derive(Clone)]
struct Edit {
    start: usize,
    end: usize,
    text: &'static str,
}

impl Edit {
    fn remove(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            text: "",
        }
    }

    fn insert(at: usize, text: &'static str) -> Self {
        Self {
            start: at,
            end: at,
            text,
        }
    }
}

/// Drops `return` from the snippet's last top-level statement. A bare `return` becomes `None`.
fn final_return(root: Node<'_>, code: &str) -> Option<Edit> {
    let mut cursor = root.walk();
    let last = root
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .last()?;
    if last.kind() != RETURN_STATEMENT {
        return None;
    }
    let keyword = last.child(0)?;
    let mut cursor = last.walk();
    let value = last
        .named_children(&mut cursor)
        .find(|node| !node.is_extra());
    let Some(value) = value else {
        return Some(Edit {
            start: keyword.start_byte(),
            end: keyword.end_byte(),
            text: NONE,
        });
    };
    (!code[keyword.start_byte()..value.start_byte()].contains('\n'))
        .then(|| Edit::remove(keyword.start_byte(), value.start_byte()))
}

// MONTY-GATHER-SHIM: remove with the rest of the shim; see `SETTLED_HELPER`.
/// Rewrites `asyncio.gather(..., return_exceptions=True)` so each awaitable settles on its own,
/// and drops `return_exceptions=False`. A call with any other keyword, a `**` argument, or a value
/// that is not a literal is left alone.
fn settled_gather(call: Node<'_>, code: &str) -> Option<Vec<Edit>> {
    if call.kind() != CALL {
        return None;
    }
    let function = call.child_by_field_name(FUNCTION_FIELD)?;
    let object = function.child_by_field_name(OBJECT_FIELD)?;
    let attribute = function.child_by_field_name(ATTRIBUTE_FIELD)?;
    if function.kind() != ATTRIBUTE
        || object.kind() != IDENTIFIER
        || &code[object.byte_range()] != ASYNCIO
        || &code[attribute.byte_range()] != GATHER
    {
        return None;
    }
    let arguments = call
        .child_by_field_name(ARGUMENTS_FIELD)
        .filter(|arguments| arguments.kind() == ARGUMENT_LIST)?;
    let mut cursor = arguments.walk();
    let children: Vec<Node<'_>> = arguments.named_children(&mut cursor).collect();
    let mut keyword = None;
    for child in &children {
        match child.kind() {
            KEYWORD_ARGUMENT => {
                let name = child.child_by_field_name(NAME_FIELD)?;
                if keyword.is_some() || &code[name.byte_range()] != RETURN_EXCEPTIONS {
                    return None;
                }
                keyword = Some(*child);
            }
            DICTIONARY_SPLAT => return None,
            _ => {}
        }
    }
    let keyword = keyword?;
    let settles = match keyword.child_by_field_name(VALUE_FIELD)?.kind() {
        TRUE => true,
        FALSE => false,
        _ => return None,
    };
    let comma = keyword
        .next_sibling()
        .filter(|next| next.kind() == COMMA)
        .filter(|next| !code[keyword.end_byte()..next.start_byte()].contains('\n'));
    let mut edits = vec![Edit::remove(
        keyword.start_byte(),
        comma.map_or(keyword.end_byte(), |comma| comma.end_byte()),
    )];
    if !settles {
        return Some(edits);
    }
    for child in children.into_iter().filter(|child| !child.is_extra()) {
        match child.kind() {
            KEYWORD_ARGUMENT => {}
            LIST_SPLAT => {
                let spread = child.named_child(0)?;
                edits.push(Edit::insert(spread.start_byte(), SETTLED_SPLAT_OPEN));
                edits.push(Edit::insert(spread.end_byte(), SETTLED_SPLAT_CLOSE));
            }
            _ => {
                edits.push(Edit::insert(child.start_byte(), SETTLED_OPEN));
                edits.push(Edit::insert(child.end_byte(), SETTLED_CLOSE));
            }
        }
    }
    Some(edits)
}

/// Visits every node under `root`, parents before their children.
fn walk<'tree>(root: Node<'tree>, mut visit: impl FnMut(Node<'tree>)) {
    let mut cursor = root.walk();
    loop {
        visit(cursor.node());
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// The snippet with a fence around all of it blanked out, line for line: the first and last lines
/// it writes, which the parser rejected and which no string spans.
fn unfenced(code: &str) -> Option<String> {
    let tree = parse(code).filter(|tree| tree.root_node().has_error())?;
    let mut lines: Vec<&str> = code.lines().collect();
    let first = lines.iter().position(|line| !line.trim().is_empty())?;
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    let language = lines[first].trim().strip_prefix(FENCE)?;
    if last == first || lines[last].trim() != FENCE || !FENCE_LANGUAGES.contains(&language) {
        return None;
    }
    let rejected = rejected_rows(&tree);
    if !rejected.contains(&first) || !rejected.contains(&last) {
        return None;
    }
    let mut in_string = false;
    walk(tree.root_node(), |node| {
        in_string |= node.kind() == STRING
            && !within_error(node)
            && [first, last]
                .iter()
                .any(|row| (node.start_position().row..=node.end_position().row).contains(row));
    });
    if in_string {
        return None;
    }
    lines[first] = "";
    lines[last] = "";
    Some(lines.join("\n"))
}

/// The snippet with JavaScript's `const`, `let` or `var` dropped wherever the parser read one as a
/// name that starts a statement and is followed by a name and `=`, when only that kept it from
/// being Python: `const r = await gh.list(); r` reads as `r = await gh.list(); r`. Python never
/// writes two names in a row, and text in a string or comment is never a name. The keywords
/// dropped come back once each, in the order first seen; the caller has the parser read the result.
fn undeclared(code: &str) -> Option<(String, Vec<&'static str>)> {
    let tree = parse(code).filter(|tree| tree.root_node().has_error())?;
    let tokens = tokens(&tree);
    let mut edits = Vec::new();
    let mut keywords = Vec::new();
    for (index, window) in tokens.windows(3).enumerate() {
        let [keyword, name, assign] = window else {
            continue;
        };
        let starts_statement = index
            .checked_sub(1)
            .map(|previous| tokens[previous])
            .is_none_or(|previous| {
                previous.end_position().row < keyword.start_position().row
                    || previous.kind() == STATEMENT_SEPARATOR
            });
        let Some(written) = ASSIGNING_KEYWORDS
            .into_iter()
            .find(|written| *written == &code[keyword.byte_range()])
        else {
            continue;
        };
        if !(starts_statement
            && keyword.kind() == IDENTIFIER
            && name.kind() == IDENTIFIER
            && assign.kind() == KEYWORD_ASSIGN)
        {
            continue;
        }
        edits.push(Edit::remove(keyword.start_byte(), name.start_byte()));
        if !keywords.contains(&written) {
            keywords.push(written);
        }
    }
    (!keywords.is_empty()).then(|| (applied(code, edits), keywords))
}

/// The snippet with each over-indented block lined up with the block around it, as the parser
/// Monty runs reads it: Python goes deeper only after a line ending in `:`, so the block around a
/// stray indent is the only place it can belong. Each pass lines up the block whose stray indent
/// the first error sits on, and the snippet is read again; any other first error, a tab, or a pass
/// that leaves nothing to change returns `None`. The lexer never starts a statement inside
/// brackets, a string, a comment, or after `\`, so those lines keep their indentation. A stray
/// block that names a host function or namespace returns `None` too: its indent may be all that
/// is left of a dropped `if` or loop header, and lining it up would make its calls unconditional.
fn dedented(code: &str, stubs: &StubIndex) -> Option<String> {
    let mut current = code.to_owned();
    for _ in 0..=code.lines().count() {
        let parsed = parse_unchecked_source(&current, PySourceType::Python);
        let Some(first) = parsed
            .errors()
            .iter()
            .min_by_key(|error| error.location.start())
        else {
            return (current != code).then_some(current);
        };
        if !is_stray_indent(parsed.tokens(), first.location) {
            return None;
        }
        let edits = lined_up(
            &current,
            parsed.tokens(),
            first.location.start().to_usize(),
            stubs,
        )?;
        current = applied(&current, edits);
    }
    None
}

/// Whether an error sits on an `Indent` that no line ending in `:` opened. The parser names it
/// `Unexpected indentation`, or by what it expected there instead, as a definition after a
/// decorator or `except` after `try`.
fn is_stray_indent(tokens: &[Token], location: TextRange) -> bool {
    let Some(indent) = tokens
        .iter()
        .position(|token| token.kind() == TokenKind::Indent && token.range() == location)
    else {
        return false;
    };
    tokens[..indent].iter().map(Token::kind).rfind(|kind| {
        !matches!(
            kind,
            TokenKind::Newline | TokenKind::NonLogicalNewline | TokenKind::Comment
        )
    }) != Some(TokenKind::Colon)
}

/// The edits taking the stray statement whose indentation starts at `at`, and the statements
/// after it as deep or deeper, back to the indentation of the statement before it: the deepest
/// block open there, since everything before the first error is valid. `None` when the block
/// names a host function or namespace.
fn lined_up(code: &str, tokens: &[Token], at: usize, stubs: &StubIndex) -> Option<Vec<Edit>> {
    let starts = statement_starts(code, tokens)?;
    let first = starts.iter().position(|start| start.token >= at)?;
    let enclosing = first
        .checked_sub(1)
        .map_or(0, |previous| starts[previous].width);
    let written = starts[first].width;
    let shift = written.checked_sub(enclosing).filter(|shift| *shift > 0)?;
    let block = starts[first..]
        .iter()
        .take_while(|start| start.width >= written)
        .count();
    let end = starts
        .get(first + block)
        .map_or(code.len(), |next| next.line);
    if names_host(code, tokens, starts[first].line..end, stubs) {
        return None;
    }
    Some(
        starts[first..first + block]
            .iter()
            .map(|start| Edit::remove(start.line, start.line + shift))
            .collect(),
    )
}

/// Whether a name token within `span` is one the host declares, read as a name rather than as an
/// attribute after `.`.
fn names_host(code: &str, tokens: &[Token], span: Range<usize>, stubs: &StubIndex) -> bool {
    tokens.iter().enumerate().any(|(index, token)| {
        let after_dot = index
            .checked_sub(1)
            .is_some_and(|previous| tokens[previous].kind() == TokenKind::Dot);
        token.kind() == TokenKind::Name
            && !after_dot
            && span.contains(&token.start().to_usize())
            && stubs.is_host(&code[token.range()])
    })
}

/// A token that starts a statement on its own line.
struct StatementStart {
    /// The byte its line starts at.
    line: usize,
    /// Its indentation, in spaces.
    width: usize,
    /// The byte the token starts at.
    token: usize,
}

/// Every token after a `Newline` that is neither indentation nor a comment, with its line's
/// indentation: an indentation the lexer rejected comes as an `Unknown` token, so the width is
/// read from the line. `None` when one is indented with anything but spaces.
fn statement_starts(code: &str, tokens: &[Token]) -> Option<Vec<StatementStart>> {
    let mut starts = Vec::new();
    let mut at_line_start = true;
    for token in tokens {
        match token.kind() {
            TokenKind::Newline => at_line_start = true,
            TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::Comment
            | TokenKind::NonLogicalNewline
            | TokenKind::EndOfFile => {}
            _ if at_line_start => {
                at_line_start = false;
                let token = token.start().to_usize();
                let line = code[..token].rfind('\n').map_or(0, |newline| newline + 1);
                let indentation = code[line..]
                    .bytes()
                    .take_while(u8::is_ascii_whitespace)
                    .take_while(|byte| !matches!(byte, b'\n' | b'\r'));
                let mut width = 0;
                for byte in indentation {
                    if byte != INDENT {
                        return None;
                    }
                    width += 1;
                }
                starts.push(StatementStart { line, width, token });
            }
            _ => {}
        }
    }
    Some(starts)
}

/// `gh.list({state: "open"})`, JavaScript's single object argument to a host call, as
/// `gh.list(state="open")`: the call's name and the edits. Only when every key is a bare name
/// the snippet uses nowhere else, so the object cannot be a dict keyed by the snippet's variables,
/// and each value starts on its key's line, so the lines stay where they were.
fn object_arguments(
    call: Node<'_>,
    code: &str,
    namespaces: &HashSet<String>,
    names: &HashSet<&str>,
) -> Option<(String, Vec<Edit>)> {
    if call.kind() != CALL {
        return None;
    }
    let function = call
        .child_by_field_name(FUNCTION_FIELD)
        .filter(|function| function.kind() == ATTRIBUTE)?;
    let object = function
        .child_by_field_name(OBJECT_FIELD)
        .filter(|object| object.kind() == IDENTIFIER)?;
    if !namespaces.contains(&code[object.byte_range()]) {
        return None;
    }
    let arguments = call
        .child_by_field_name(ARGUMENTS_FIELD)
        .filter(|arguments| arguments.kind() == ARGUMENT_LIST)?;
    let mut cursor = arguments.walk();
    let [object_literal] = arguments
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>()[..]
    else {
        return None;
    };
    if object_literal.kind() != DICTIONARY {
        return None;
    }
    let mut cursor = object_literal.walk();
    let pairs: Vec<Node<'_>> = object_literal
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .collect();
    if pairs.is_empty() {
        return None;
    }
    let mut edits = vec![
        Edit::remove(object_literal.start_byte(), object_literal.start_byte() + 1),
        Edit::remove(object_literal.end_byte() - 1, object_literal.end_byte()),
    ];
    for pair in pairs {
        if pair.kind() != PAIR {
            return None;
        }
        let key = pair
            .child_by_field_name(KEY_FIELD)
            .filter(|key| key.kind() == IDENTIFIER)?;
        let value = pair.child_by_field_name(VALUE_FIELD)?;
        let separator = key.end_byte()..value.start_byte();
        if names.contains(&code[key.byte_range()]) || code[separator.clone()].contains('\n') {
            return None;
        }
        edits.push(Edit {
            start: separator.start,
            end: separator.end,
            text: KEYWORD_ASSIGN,
        });
    }
    Some((code[function.byte_range()].to_owned(), edits))
}

/// Every name the snippet reads or binds, leaving out the bare keys of its dict literals.
fn names_outside_keys<'code>(root: Node<'_>, code: &'code str) -> HashSet<&'code str> {
    let mut names = HashSet::new();
    walk(root, |node| {
        let is_key = node.parent().is_some_and(|parent| {
            parent.kind() == PAIR && parent.child_by_field_name(KEY_FIELD) == Some(node)
        });
        if node.kind() == IDENTIFIER && !is_key {
            names.insert(&code[node.byte_range()]);
        }
    });
    names
}

/// Whether the parser Monty runs a snippet with reads it whole. Tree-sitter, which the repairs walk,
/// accepts some code that one rejects, such as an over-indented line.
pub(crate) fn reads_as_python(code: &str) -> bool {
    parse_module(code).is_ok()
}

fn parse(code: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    parser.parse(code, None)
}

/// What the host's stubs declare, as the parser reads them: the names a snippet starts with, and
/// the calls among them that answer at once, as `help` or `mcp.search`.
#[derive(Default)]
struct StubIndex {
    /// Top-level functions and namespaces.
    names: HashSet<String>,
    /// Namespaces, as `gh` in `gh: _Gh`.
    namespaces: HashSet<String>,
    /// Calls that answer at once, as `help` or `gh.nope`: a plain `def`, never an `async def`.
    immediate: HashSet<String>,
}

impl StubIndex {
    fn is_host(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    fn of(stubs: &str) -> Self {
        let mut index = Self::default();
        let Some(tree) = parse(stubs) else {
            return index;
        };
        let root = tree.root_node();
        let mut methods: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut typed = Vec::new();
        let mut cursor = root.walk();
        for node in root.named_children(&mut cursor) {
            match node.kind() {
                FUNCTION_DEFINITION => {
                    let Some(name) = defined_name(node, stubs) else {
                        continue;
                    };
                    index.names.insert(name.to_owned());
                    if !is_async(node) {
                        index.immediate.insert(name.to_owned());
                    }
                }
                CLASS_DEFINITION => {
                    let (Some(class), Some(body)) = (
                        defined_name(node, stubs),
                        node.child_by_field_name(BODY_FIELD),
                    ) else {
                        continue;
                    };
                    let mut cursor = body.walk();
                    methods.entry(class).or_default().extend(
                        body.named_children(&mut cursor)
                            .filter(|method| {
                                method.kind() == FUNCTION_DEFINITION && !is_async(*method)
                            })
                            .filter_map(|method| defined_name(method, stubs)),
                    );
                }
                EXPRESSION_STATEMENT => typed.extend(annotated_name(node, stubs)),
                _ => {}
            }
        }
        for (namespace, class) in typed {
            index.names.insert(namespace.to_owned());
            index.namespaces.insert(namespace.to_owned());
            index.immediate.extend(
                methods
                    .get(class)
                    .into_iter()
                    .flatten()
                    .map(|method| format!("{namespace}.{method}")),
            );
        }
        index
    }
}

fn defined_name<'source>(definition: Node<'_>, source: &'source str) -> Option<&'source str> {
    definition
        .child_by_field_name(NAME_FIELD)
        .map(|name| &source[name.byte_range()])
}

fn is_async(definition: Node<'_>) -> bool {
    definition
        .child(0)
        .is_some_and(|keyword| keyword.kind() == ASYNC)
}

/// `gh: _Gh` as `("gh", "_Gh")`.
fn annotated_name<'source>(
    statement: Node<'_>,
    source: &'source str,
) -> Option<(&'source str, &'source str)> {
    let assignment = statement
        .named_child(0)
        .filter(|assignment| assignment.kind() == ASSIGNMENT)?;
    let name = assignment
        .child_by_field_name(LEFT_FIELD)
        .filter(|name| name.kind() == IDENTIFIER)?;
    let annotation = assignment.child_by_field_name(TYPE_FIELD)?;
    if assignment.child_by_field_name(RIGHT_FIELD).is_some() {
        return None;
    }
    Some((&source[name.byte_range()], &source[annotation.byte_range()]))
}

/// An `await` on a call that answers at once: the bytes to drop, up to the call.
struct Await {
    start: usize,
    end: usize,
    /// As written, `help` or `mcp.search`.
    function: String,
    /// The name it starts from, `help` or `mcp`.
    root: String,
}

/// How the snippet uses the names a repair may touch.
#[derive(Default)]
struct Uses<'code> {
    /// Modules read as the object of an attribute, as `json` in `json.dumps(value)`.
    qualified: HashSet<&'code str>,
    /// Names used any other way, including being bound: those are the snippet's own.
    elsewhere: HashSet<&'code str>,
    awaits: Vec<Await>,
}

impl<'code> Uses<'code> {
    fn of(root: Node<'_>, code: &'code str, stubs: &StubIndex) -> Self {
        let mut uses = Self::default();
        walk(root, |node| {
            if node.kind() == IDENTIFIER {
                uses.note(node, &code[node.byte_range()], code, stubs);
            }
        });
        uses
    }

    fn note(&mut self, node: Node<'_>, name: &'code str, code: &str, stubs: &StubIndex) {
        let parent = node.parent();
        let is_field = |kind: &str, field: &str| {
            parent.is_some_and(|parent| {
                parent.kind() == kind && parent.child_by_field_name(field) == Some(node)
            })
        };
        if SUBSET_MODULES.contains(&name) {
            if is_field(ATTRIBUTE, OBJECT_FIELD) {
                self.qualified.insert(name);
            } else {
                self.elsewhere.insert(name);
            }
        }
        if !stubs.names.contains(name) {
            return;
        }
        let callee = if is_field(CALL, FUNCTION_FIELD) {
            parent.map(|call| (name.to_owned(), call))
        } else if is_field(ATTRIBUTE, OBJECT_FIELD) {
            parent.and_then(|attribute| {
                let member = attribute.child_by_field_name(ATTRIBUTE_FIELD)?;
                let call = attribute.parent().filter(|call| {
                    call.kind() == CALL
                        && call.child_by_field_name(FUNCTION_FIELD) == Some(attribute)
                })?;
                Some((format!("{name}.{}", &code[member.byte_range()]), call))
            })
        } else {
            self.elsewhere.insert(name);
            return;
        };
        let Some((function, call)) =
            callee.filter(|(function, _)| stubs.immediate.contains(function))
        else {
            return;
        };
        if let Some(awaited) = call.parent().filter(|outer| outer.kind() == AWAIT)
            && !code[awaited.start_byte()..call.start_byte()].contains('\n')
        {
            self.awaits.push(Await {
                start: awaited.start_byte(),
                end: call.start_byte(),
                function,
                root: name.to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const CORPUS_VARIABLE: &str = "WORKCELL_PYTHON_CORPUS";
    const MAX_STRAYS_PER_FILE: usize = 40;
    const STRAY_WIDTHS: [usize; 2] = [1, 4];
    const STUBS: &str = "class _Gh:\n    async def list(self) -> list[str]: ...\n    def nope(self) -> int: ...\ngh: _Gh\ndef search(query: str) -> list[str]: ...\ndef servers() -> list[str]: ...\nasync def resources(server: str) -> list[str]: ...\n";

    fn repaired(code: &str) -> Repaired {
        repair(code.to_owned(), STUBS)
    }

    #[test_case("results = await asyncio.gather(gh.list(), gh.list())", &["asyncio"] ; "gather")]
    #[test_case("print(json.dumps(re.findall('a', 'b')))", &["json", "re"] ; "several_in_list_order")]
    #[test_case("f'{json.dumps(1)}'", &["json"] ; "inside_an_f_string")]
    #[test_case("import asyncio\nawait asyncio.sleep(0)", &[] ; "already_imported")]
    #[test_case("from json import dumps\njson.loads('1')", &[] ; "imported_from")]
    #[test_case("json = await gh.list()\njson.count('a')", &[] ; "bound_by_the_snippet")]
    #[test_case("for re in items:\n    re.strip()", &[] ; "bound_by_a_loop")]
    #[test_case("def f(time):\n    return time.monotonic()", &[] ; "a_parameter")]
    #[test_case("value.json.dumps(1)", &[] ; "an_attribute_of_something_else")]
    #[test_case("statistics.mean([1])", &[] ; "a_module_the_sandbox_lacks")]
    #[test_case("asyncio.gather(", &[] ; "code_that_does_not_parse")]
    fn imports_only_modules_read_through_an_attribute_and_never_bound(
        code: &str,
        expected: &[&str],
    ) {
        let repaired = repaired(code);
        assert_eq!(repaired.imports(), expected);
        assert_eq!(repaired.code, code);
    }

    #[test_case("hits = await search('x')", "hits = search('x')", &["search"] ; "search")]
    #[test_case("[await servers(), await search('x')]", "[servers(), search('x')]", &["servers", "search"] ; "both")]
    #[test_case("a = await search('x')\nb = await search('y')", "a = search('x')\nb = search('y')", &["search"] ; "named_once")]
    #[test_case("await resources('gh')", "await resources('gh')", &[] ; "an_async_helper")]
    #[test_case("await gh.nope()", "gh.nope()", &["gh.nope"] ; "a_namespace_method")]
    #[test_case("await gh.list()", "await gh.list()", &[] ; "an_async_namespace_method")]
    #[test_case("gh = other\nawait gh.nope()", "gh = other\nawait gh.nope()", &[] ; "a_namespace_the_snippet_rebinds")]
    #[test_case("x = '''await gh.nope()'''", "x = '''await gh.nope()'''", &[] ; "inside_a_string")]
    #[test_case("async def search(q):\n    return q\nawait search('x')", "async def search(q):\n    return q\nawait search('x')", &[] ; "the_snippets_own_function")]
    #[test_case("await \\\n    search('x')", "await \\\n    search('x')", &[] ; "across_lines")]
    fn drops_await_only_before_helpers_that_answer_at_once(
        code: &str,
        expected: &str,
        functions: &[&str],
    ) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        let unawaited: Vec<Repair> = functions
            .iter()
            .map(|function| Repair::Unawaited((*function).to_owned()))
            .collect();
        assert_eq!(repaired.repairs, unawaited);
    }

    #[test_case("```python\nx = 1\n```", "\nx = 1\n" ; "python")]
    #[test_case("\n```\nx = 1\n```\n", "\n\nx = 1\n" ; "bare_with_blank_lines")]
    #[test_case("```py\njson.dumps(1)\n```", "\njson.dumps(1)\n" ; "then_repaired_further")]
    fn a_fence_around_the_whole_snippet_is_blanked_line_for_line(code: &str, expected: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        assert_eq!(repaired.repairs.first(), Some(&Repair::Unfenced));
    }

    #[test_case("x = 1\nreturn x", "x = 1\nx" ; "a_value")]
    #[test_case("return {\n    'a': 1,\n}", "{\n    'a': 1,\n}" ; "across_lines")]
    #[test_case("x = 1\nreturn", "x = 1\nNone" ; "bare")]
    #[test_case("return x  # done\n# end", "x  # done\n# end" ; "before_comments")]
    fn a_final_return_ends_on_its_value(code: &str, expected: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        assert_eq!(repaired.repairs, [Repair::FinalReturn]);
    }

    #[test_case("if x:\n    return 1\n2" ; "not_the_last_statement")]
    #[test_case("def f():\n    return 1" ; "inside_a_function")]
    #[test_case("return \\\n    1" ; "value_on_the_next_line")]
    fn other_returns_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(repaired.repairs.is_empty(), "{:?}", repaired.repairs);
    }

    #[test_case(
        "await asyncio.gather(gh.list(), gh.list(), return_exceptions=True)",
        "await asyncio.gather(caudra_settled(gh.list()), caudra_settled(gh.list()), )" ;
        "positional"
    )]
    #[test_case(
        "await asyncio.gather(\n    gh.list(),\n    return_exceptions=True,\n)",
        "await asyncio.gather(\n    caudra_settled(gh.list()),\n    \n)" ;
        "across_lines_with_a_trailing_comma"
    )]
    #[test_case(
        "await asyncio.gather(*calls, return_exceptions=True)",
        "await asyncio.gather(*[caudra_settled(_caudra_item) for _caudra_item in calls], )" ;
        "a_spread"
    )]
    #[test_case(
        "return await asyncio.gather(await search('x'), return_exceptions=True)",
        "await asyncio.gather(caudra_settled(search('x')), )" ;
        "with_the_other_repairs"
    )]
    fn gather_settles_each_awaitable_instead_of_returning_exceptions(code: &str, expected: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        assert!(repaired.settles());
        assert_eq!(repaired.code.lines().count(), code.lines().count());
    }

    #[test]
    fn gather_without_exceptions_returned_only_loses_the_keyword() {
        let repaired = repaired("await asyncio.gather(gh.list(), return_exceptions=False)");
        assert_eq!(repaired.code, "await asyncio.gather(gh.list(), )");
        assert_eq!(
            repaired.repairs,
            [Repair::Imported("asyncio"), Repair::Settled]
        );
    }

    #[test_case("await asyncio.gather(gh.list(), return_exceptions=flag)" ; "a_value_that_is_not_a_literal")]
    #[test_case("await asyncio.gather(gh.list(), return_exceptions=True, loop=None)" ; "another_keyword")]
    #[test_case("await asyncio.gather(gh.list(), **options)" ; "a_keyword_spread")]
    #[test_case("await other.gather(gh.list(), return_exceptions=True)" ; "another_module")]
    fn other_gathers_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(!repaired.settles());
    }

    /// Only a slip teaches the model something. `return_exceptions` is declared by the
    /// interpreter's own stubs, so using it was not one.
    #[test_case(Repair::Imported("json"), true ; "imported")]
    #[test_case(Repair::FinalReturn, true ; "final_return")]
    #[test_case(Repair::Dedented, true ; "dedented")]
    #[test_case(Repair::Settled, false ; "settled")]
    fn only_slips_carry_a_lesson(repair: Repair, teaches: bool) {
        assert_eq!(repair.lesson().is_some(), teaches);
    }

    #[test_case("const results = await Promise.all([gh.list()]); return results;", true ; "promise_all")]
    #[test_case("items.map(item => item.name)", true ; "an_arrow")]
    #[test_case("let total = 0\ntotal += 1;", true ; "a_declaration")]
    #[test_case("const = 1", false ; "python_naming_a_variable_const")]
    #[test_case("results = await gh.list(\n", false ; "broken_python")]
    #[test_case("constant = 'a => b'", false ; "valid_python_quoting_javascript")]
    fn javascript_is_recognised_only_where_python_fails(code: &str, javascript: bool) {
        assert_eq!(looks_like_javascript(code), javascript);
    }

    #[test_case("const r = await gh.list(); r", "r = await gh.list(); r", &["const"] ; "a_declaration_ended_by_a_semicolon")]
    #[test_case("let a = 1\nif a:\n    var b = 2\nconst c = a", "a = 1\nif a:\n    b = 2\nc = a", &["let", "var", "const"] ; "each_keyword_once_in_order")]
    #[test_case("const  r=search('x')\nconst s = 1", "r=search('x')\ns = 1", &["const"] ; "spacing_kept_after_the_name")]
    #[test_case("x = '''\nlet a = 1\n'''\nconst y = 2", "x = '''\nlet a = 1\n'''\ny = 2", &["const"] ; "a_string_is_never_touched")]
    #[test_case("const a = 1  # let b = 2\nlet c = a", "a = 1  # let b = 2\nc = a", &["const", "let"] ; "a_comment_is_never_touched")]
    fn javascript_declarations_become_python_assignments(
        code: &str,
        expected: &str,
        keywords: &[&'static str],
    ) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        let undeclared: Vec<Repair> = keywords.iter().copied().map(Repair::Undeclared).collect();
        assert_eq!(&repaired.repairs[..undeclared.len()], undeclared);
        assert_eq!(repaired.code.lines().count(), code.lines().count());
    }

    #[test_case("const = 1" ; "python_naming_a_variable_const")]
    #[test_case("letter = 1\nlet = letter" ; "names_that_start_like_keywords")]
    #[test_case("const r = items.map(item => item.name)" ; "still_javascript_after")]
    #[test_case("const a = 1\nconst b = (" ; "still_broken_after")]
    #[test_case("x = '''\nlet a = 1\n'''\nconst y = 2 +" ; "broken_beside_a_string")]
    #[test_case("if const == 1:\n    pass" ; "a_comparison")]
    #[test_case("x = 1; const == x\ny = (" ; "a_comparison_beside_broken_code")]
    fn declarations_are_dropped_only_where_that_alone_makes_python(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(
            !repaired
                .repairs
                .iter()
                .any(|repair| matches!(repair, Repair::Undeclared(_))),
            "{:?}",
            repaired.repairs
        );
    }

    #[test_case("r = await gh.list({state: \"open\", limit: 5})", "r = await gh.list(state=\"open\", limit=5)", &[] ; "keys_become_keywords")]
    #[test_case("const r = await gh.list({prompt:\"x\"}); r", "r = await gh.list(prompt=\"x\"); r", &["const"] ; "with_a_declaration")]
    #[test_case("await gh.list({\n    state: \"open\",\n})", "await gh.list(\n    state=\"open\",\n)", &[] ; "across_lines")]
    fn a_javascript_object_to_a_host_call_becomes_keyword_arguments(
        code: &str,
        expected: &str,
        keywords: &[&'static str],
    ) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        let mut expected_repairs: Vec<Repair> =
            keywords.iter().copied().map(Repair::Undeclared).collect();
        expected_repairs.push(Repair::Unpacked("gh.list".to_owned()));
        assert_eq!(repaired.repairs, expected_repairs);
        assert_eq!(repaired.code.lines().count(), code.lines().count());
        assert!(reads_as_python(&repaired.code), "{}", repaired.code);
        assert_eq!(repaired.taught.as_deref(), Some(expected));
    }

    #[test]
    fn javascript_awaiting_a_namespace_method_that_answers_at_once_runs() {
        let code = "const r = await gh.nope({query:\"x\"}); r";
        let expected = "r = gh.nope(query=\"x\"); r";
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        assert_eq!(
            repaired.repairs,
            vec![
                Repair::Undeclared("const"),
                Repair::Unawaited("gh.nope".to_owned()),
                Repair::Unpacked("gh.nope".to_owned()),
            ]
        );
        assert_eq!(repaired.taught.as_deref(), Some(expected));
    }

    /// The model is shown what it should have written, never the interpreter's own workaround.
    #[test_case("```python\nconst r = 1\n```", Some("\nr = 1\n") ; "syntax_repairs_alone")]
    #[test_case("const r = await asyncio.gather(gh.list(), return_exceptions=True)", Some("r = await asyncio.gather(gh.list(), return_exceptions=True)") ; "beside_the_gather_shim")]
    #[test_case("await asyncio.gather(gh.list(), return_exceptions=True)", None ; "the_gather_shim_alone")]
    #[test_case("json.dumps(1)", None ; "an_import_alone")]
    fn what_is_taught_leaves_out_the_shim(code: &str, taught: Option<&str>) {
        assert_eq!(repaired(code).taught.as_deref(), taught);
    }

    #[test_case("state = 1\nawait gh.list({state: 2})" ; "a_key_the_snippet_binds")]
    #[test_case("await gh.list({\"state\": \"open\"})" ; "a_string_key")]
    #[test_case("await other.list({state: 1})" ; "not_a_host_namespace")]
    #[test_case("await gh.list({state: 1}, 2)" ; "beside_another_argument")]
    #[test_case("await gh.list({})" ; "an_empty_object")]
    #[test_case("await gh.list({state:\n    1})" ; "a_value_on_the_next_line")]
    #[test_case("await gh.list({**rest})" ; "a_spread")]
    fn other_dict_arguments_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(repaired.repairs.is_empty(), "{:?}", repaired.repairs);
    }

    #[test_case("import datetime\n datetime.datetime.now()", "import datetime\ndatetime.datetime.now()" ; "a_stray_space")]
    #[test_case("  x = 1\n  x", "x = 1\nx" ; "the_whole_snippet_indented")]
    #[test_case("x = 1\n    if x:\n        y = 2\n    z = 3\nx", "x = 1\nif x:\n    y = 2\nz = 3\nx" ; "a_block_under_the_stray_line")]
    #[test_case("for i in range(2):\n    a = i\n      b = i\n    c = i", "for i in range(2):\n    a = i\n    b = i\n    c = i" ; "deeper_inside_a_block")]
    #[test_case("x = 1\n    # note\n  y = 2", "x = 1\n    # note\ny = 2" ; "a_comment_line_keeps_its_place")]
    #[test_case("x = 1\n  y = [\n      1,\n  ]", "x = 1\ny = [\n      1,\n  ]" ; "lines_inside_brackets_keep_theirs")]
    #[test_case("x = 1\n  y = '''\n    z = 1\n'''", "x = 1\ny = '''\n    z = 1\n'''" ; "a_string_is_never_touched")]
    #[test_case("x = 1\n  y = 1 + \\\n    2", "x = 1\ny = 1 + \\\n    2" ; "a_continued_line_keeps_its_indentation")]
    #[test_case("```python\nx = 1\n  x\n```", "\nx = 1\nx\n" ; "inside_a_fence")]
    #[test_case("x = 1\n  y = 2\n    z = 3", "x = 1\ny = 2\nz = 3" ; "a_stray_inside_a_stray")]
    #[test_case("x = 1\n  if x:\n      y = 2\nz = 3", "x = 1\nif x:\n    y = 2\nz = 3" ; "a_stray_block_header")]
    #[test_case("x = 1\n    y = 2\n  z = 3", "x = 1\ny = 2\nz = 3" ; "a_shallower_stray_after_a_deeper_one")]
    #[test_case("import datetime\r\n datetime.datetime.now()", "import datetime\r\ndatetime.datetime.now()" ; "crlf")]
    #[test_case("@a\n @b\nclass C: pass", "@a\n@b\nclass C: pass" ; "between_decorators")]
    #[test_case("try: a()\n except OSError: pass", "try: a()\nexcept OSError: pass" ; "a_clause_after_a_one_line_block")]
    #[test_case("x = 1\n  y = x.search", "x = 1\ny = x.search" ; "a_host_name_read_as_an_attribute")]
    #[test_case("r = await gh.list()\n  n = len(r)", "r = await gh.list()\nn = len(r)" ; "a_host_call_before_the_stray")]
    fn an_over_indented_line_is_lined_up_with_its_block(code: &str, expected: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, expected);
        assert!(
            repaired.repairs.contains(&Repair::Dedented),
            "{:?}",
            repaired.repairs
        );
        assert_eq!(repaired.taught.as_deref(), Some(expected));
    }

    #[test_case("if x:\n    y = 1\nelif z:\n    y = 2\nelse:\n    y = 3" ; "branches")]
    #[test_case("def f():\n    '''doc\n  more'''\n    return 1" ; "a_docstring_less_indented")]
    #[test_case("x = (1,\n        2)\nd = {\n 'a': 1,\n}" ; "brackets")]
    #[test_case("class A:\n    @staticmethod\n    def f():\n        pass\n\n        # note\n    def g(self): pass" ; "a_class")]
    #[test_case("match x:\n    case 1:\n        pass" ; "a_match")]
    #[test_case("if x: y = 1\nz = 2" ; "a_one_line_block")]
    #[test_case("try:\n    a()\nexcept Exception as error:\n    b(error)\nfinally:\n    c()" ; "a_try")]
    #[test_case("x = 1 + \\\n        2\ny = f'''{x}\n   {x}'''" ; "continued_lines")]
    #[test_case("if x:\n\ty = 1\n\tz = 2" ; "tabs")]
    fn valid_python_keeps_its_indentation(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(repaired.repairs.is_empty(), "{:?}", repaired.repairs);
    }

    #[test_case("if x:\n    a = 1\n  b = 2" ; "an_unindent_that_matches_no_block")]
    #[test_case("if x:\ny = 1" ; "a_block_left_empty")]
    #[test_case("x = 1\n\ty = 2" ; "a_tab")]
    #[test_case("if x:\n\ty = 1\n  \tz = 2" ; "a_stray_among_tabs")]
    #[test_case("if x:\n    if y:\n        a = 1\n      b = 2" ; "a_dedent_between_levels")]
    #[test_case("x = 1\n  y = (" ; "still_broken_after")]
    #[test_case("x = '''\n  a\n'''" ; "a_string_alone")]
    #[test_case("x = 1\n    await gh.list()" ; "a_stray_host_call")]
    #[test_case("x = 1\n  y = 2\n  hits = search('a')" ; "a_host_call_later_in_the_stray_block")]
    #[test_case("x = 1\n  for q in qs:\n      print(f'{servers()}')" ; "a_host_call_inside_an_f_string")]
    #[test_case("x = 1\n  y = gh" ; "a_host_namespace_named")]
    #[test_case("```python\nx = 1\n  await gh.list()\n```" ; "inside_a_fence")]
    fn other_indentation_is_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(repaired.repairs.is_empty(), "{:?}", repaired.repairs);
    }

    /// Over real Python: `WORKCELL_PYTHON_CORPUS=/usr/lib/python3.12 cargo nextest run -p
    /// workcell-mcp-code --run-ignored only corpus`. Every file the parser reads is left alone, and
    /// every statement after one as deep and before one no deeper, pushed right, comes back as it
    /// was written. Only those: pushed after a deeper block, a statement lands on or between that
    /// block's levels, where it reads as part of it or is ambiguous.
    #[test]
    #[ignore = "reads a Python corpus named by WORKCELL_PYTHON_CORPUS"]
    fn corpus_strays_come_back_as_written() {
        let corpus = std::env::var_os(CORPUS_VARIABLE).expect(CORPUS_VARIABLE);
        let mut pending = vec![std::path::PathBuf::from(corpus)];
        let (mut files, mut cases, mut failures) = (0, 0, Vec::new());
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    std::fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                continue;
            }
            let Ok(code) = std::fs::read_to_string(&path) else {
                continue;
            };
            if path.extension().is_none_or(|extension| extension != "py") || !reads_as_python(&code)
            {
                continue;
            }
            files += 1;
            if syntax_repaired(code.clone(), &StubIndex::default()) != (code.clone(), Vec::new()) {
                failures.push(format!("{}: valid file changed", path.display()));
            }
            let parsed = parse_unchecked_source(&code, PySourceType::Python);
            let Some(starts) = statement_starts(&code, parsed.tokens()) else {
                continue;
            };
            let movable: Vec<&StatementStart> = starts
                .iter()
                .enumerate()
                .filter(|(index, start)| {
                    let previous = index.checked_sub(1).map(|previous| &starts[previous]);
                    previous.is_none_or(|previous| previous.width == start.width)
                        && starts
                            .get(index + 1)
                            .is_none_or(|next| next.width <= start.width)
                })
                .map(|(_, start)| start)
                .collect();
            let step = movable.len().div_ceil(MAX_STRAYS_PER_FILE).max(1);
            for start in movable.into_iter().step_by(step) {
                for spaces in STRAY_WIDTHS {
                    cases += 1;
                    let mut stray = code.clone();
                    stray.insert_str(start.line, &" ".repeat(spaces));
                    let (repaired, repairs) = syntax_repaired(stray, &StubIndex::default());
                    if repaired != code || repairs != [Repair::Dedented] {
                        let line = code[..start.line].lines().count() + 1;
                        failures.push(format!("{}:{line} +{spaces}: {repairs:?}", path.display()));
                    }
                }
            }
        }
        eprintln!("{files} files, {cases} strays, {} failures", failures.len());
        assert!(
            failures.is_empty(),
            "{}",
            failures[..failures.len().min(30)].join("\n")
        );
    }

    #[test_case("```js\nx = 1\n```" ; "another_language")]
    #[test_case("x = '```'" ; "a_fence_inside_a_string")]
    #[test_case("```python\nx = 1" ; "unclosed")]
    #[test_case("```python\nx = (\n```" ; "around_code_still_broken")]
    #[test_case("x = '''\n```\n'''\ny = (\n```" ; "a_fence_line_inside_a_string")]
    fn other_fences_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(!repaired.repairs.contains(&Repair::Unfenced));
    }
}
