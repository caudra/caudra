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
}

impl fmt::Display for Repair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unfenced => formatter.write_str("removed the Markdown fence around the code"),
            Self::Imported(module) => write!(formatter, "imported `{module}`"),
            Self::Unawaited(function) => {
                write!(formatter, "dropped `await` before `{function}()`")
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
                Repair::Unfenced | Repair::Unawaited(_) => None,
            })
            .collect()
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
    let immediate: HashSet<&str> = immediate_functions(stubs).collect();
    let uses = Uses::of(tree.root_node(), &code, &immediate);
    repairs.extend(
        SUBSET_MODULES
            .into_iter()
            .filter(|module| uses.qualified.contains(module) && !uses.elsewhere.contains(module))
            .map(Repair::Imported),
    );
    let awaits: Vec<Await> = uses
        .awaits
        .into_iter()
        .filter(|awaited| !uses.elsewhere.contains(awaited.function.as_str()))
        .collect();
    for awaited in awaits.iter().rev() {
        code.replace_range(awaited.start..awaited.end, "");
    }
    for awaited in awaits {
        let repair = Repair::Unawaited(awaited.function);
        if !repairs.contains(&repair) {
            repairs.push(repair);
        }
    }
    Repaired { code, repairs }
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
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if node.kind() == IDENTIFIER {
                uses.note(node, &code[node.byte_range()], code, immediate);
            }
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    return uses;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
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

    #[test_case("```js\nx = 1\n```" ; "another_language")]
    #[test_case("x = '```'" ; "a_fence_inside_a_string")]
    #[test_case("```python\nx = 1" ; "unclosed")]
    fn other_fences_are_left_alone(code: &str) {
        let repaired = repaired(code);
        assert_eq!(repaired.code, code);
        assert!(!repaired.repairs.contains(&Repair::Unfenced));
    }
}
