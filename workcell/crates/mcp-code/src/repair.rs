//! Repairs a hosted snippet before it runs, where what it meant has only one reading.
//!
//! A snippet's calls reach real servers, so a repair that guessed could run something its author
//! never wrote. Each one here keeps every line where it was written, so errors still point at the
//! author's own lines, and anything the parser cannot read whole is left exactly as it is.

use std::collections::HashSet;
use std::fmt;

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
/// How a stub declares a top-level function that answers at once. `async def` and indented methods
/// do not start with it.
const IMMEDIATE_STUB: &str = "def ";

/// One change made to a snippet before it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// A Markdown fence around the whole snippet was blanked out.
    Unfenced,
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
}

impl Repaired {
    pub(crate) fn imports(&self) -> Vec<&'static str> {
        self.repairs
            .iter()
            .filter_map(|repair| match repair {
                Repair::Imported(module) => Some(*module),
                Repair::Unfenced | Repair::Unawaited(_) | Repair::FinalReturn | Repair::Settled => {
                    None
                }
            })
            .collect()
    }

    /// Whether the snippet calls the helper [`SETTLED_HELPER`] defines.
    // MONTY-GATHER-SHIM: remove with the rest of the shim; see `SETTLED_HELPER`.
    pub(crate) fn settles(&self) -> bool {
        self.repairs.contains(&Repair::Settled)
    }
}

/// Repairs `code`, whose host declares its functions in `stubs`.
pub(crate) fn repair(code: String, stubs: &str) -> Repaired {
    let mut repairs = Vec::new();
    let mut code = match unfenced(&code) {
        Some(unfenced) => {
            repairs.push(Repair::Unfenced);
            unfenced
        }
        None => code,
    };
    let Some(tree) = parse(&code).filter(|tree| !tree.root_node().has_error()) else {
        return Repaired { code, repairs };
    };
    let root = tree.root_node();
    let immediate: HashSet<&str> = immediate_functions(stubs).collect();
    let uses = Uses::of(root, &code, &immediate);
    repairs.extend(
        SUBSET_MODULES
            .into_iter()
            .filter(|module| uses.qualified.contains(module) && !uses.elsewhere.contains(module))
            .map(Repair::Imported),
    );
    let mut edits = Vec::new();
    for awaited in uses.awaits {
        if uses.elsewhere.contains(awaited.function.as_str()) {
            continue;
        }
        edits.push(Edit::remove(awaited.start, awaited.end));
        let repair = Repair::Unawaited(awaited.function);
        if !repairs.contains(&repair) {
            repairs.push(repair);
        }
    }
    if let Some(edit) = final_return(root, &code) {
        edits.push(edit);
        repairs.push(Repair::FinalReturn);
    }
    let mut settled = false;
    walk(root, |node| {
        if let Some(gather) = settled_gather(node, &code) {
            edits.extend(gather);
            settled = true;
        }
    });
    if settled {
        repairs.push(Repair::Settled);
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));
    for edit in edits.iter().rev() {
        code.replace_range(edit.start..edit.end, edit.text);
    }
    Repaired { code, repairs }
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

/// The snippet with a fence around all of it blanked out, line for line.
fn unfenced(code: &str) -> Option<String> {
    let mut lines: Vec<&str> = code.lines().collect();
    let first = lines.iter().position(|line| !line.trim().is_empty())?;
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    let language = lines[first].trim().strip_prefix(FENCE)?;
    if last == first || lines[last].trim() != FENCE || !FENCE_LANGUAGES.contains(&language) {
        return None;
    }
    lines[first] = "";
    lines[last] = "";
    Some(lines.join("\n"))
}

fn parse(code: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    parser.parse(code, None)
}

fn immediate_functions(stubs: &str) -> impl Iterator<Item = &str> {
    stubs.lines().filter_map(|line| {
        line.strip_prefix(IMMEDIATE_STUB)?
            .split_once('(')
            .map(|(name, _)| name.trim())
    })
}

/// An `await` on a call to a function that answers at once: the bytes to drop, up to the call.
struct Await {
    start: usize,
    end: usize,
    function: String,
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
    fn of(root: Node<'_>, code: &'code str, immediate: &HashSet<&str>) -> Self {
        let mut uses = Self::default();
        walk(root, |node| {
            if node.kind() == IDENTIFIER {
                uses.note(node, &code[node.byte_range()], code, immediate);
            }
        });
        uses
    }

    fn note(&mut self, node: Node<'_>, name: &'code str, code: &str, immediate: &HashSet<&str>) {
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
        if !immediate.contains(name) {
            return;
        }
        if !is_field(CALL, FUNCTION_FIELD) {
            self.elsewhere.insert(name);
            return;
        }
        let Some(call) = parent else {
            return;
        };
        if let Some(awaited) = call.parent().filter(|outer| outer.kind() == AWAIT)
            && !code[awaited.start_byte()..call.start_byte()].contains('\n')
        {
            self.awaits.push(Await {
                start: awaited.start_byte(),
                end: call.start_byte(),
                function: name.to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

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
    #[test_case("await gh.nope()", "await gh.nope()", &[] ; "a_namespace_method")]
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

    #[test_case("```js\nx = 1\n```" ; "another_language")]
    #[test_case("x = '```'" ; "a_fence_inside_a_string")]
    #[test_case("```python\nx = 1" ; "unclosed")]
    fn other_fences_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(!repaired.repairs.contains(&Repair::Unfenced));
    }
}
