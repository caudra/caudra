//! Translates a JavaScript or TypeScript snippet into the Python it means, or names the first
//! construct whose Python form would not keep its meaning. A snippet is translated whole or not at
//! all: the parser must read it without an error, it must hold something only JavaScript writes,
//! and every node must pass a closed admission before any of it is lowered.

mod expressions;
mod library;
mod python;
mod scope;
mod statements;
mod syntax;
mod truthiness;

use std::fmt;

use tree_sitter::{Node, Parser, Tree};

use crate::repair::StubIndex;
use crate::types::MAX_CODE_BYTES;
use python::{CheckedModule, Statement};
use scope::Scopes;
use syntax::{
    ARGUMENTS, ARRAY, ARRAY_PATTERN, ARROW_FUNCTION, AS_EXPRESSION, ASSERTS_ANNOTATION,
    ASSIGNMENT_EXPRESSION, AUGMENTED_ASSIGNMENT_EXPRESSION, AWAIT_EXPRESSION, BINARY_EXPRESSION,
    BREAK_STATEMENT, CALL_EXPRESSION, CATCH_CLAUSE, CLASS_DECLARATION, COMMENT,
    COMPUTED_PROPERTY_NAME, CONTINUE_STATEMENT, DO_STATEMENT, ELSE_CLAUSE, EMPTY_STATEMENT,
    ENUM_DECLARATION, ESCAPE_SEQUENCE, EXPRESSION_STATEMENT, FALSE, FINALLY_CLAUSE,
    FOR_IN_STATEMENT, FOR_STATEMENT, FORMAL_PARAMETERS, FUNCTION_DECLARATION,
    GENERATOR_FUNCTION_DECLARATION, HTML_COMMENT, IDENTIFIER, IF_STATEMENT, INTERFACE_DECLARATION,
    KIND_FIELD, LEXICAL_DECLARATION, MEMBER_EXPRESSION, NEW_EXPRESSION, NON_NULL_EXPRESSION, NULL,
    NUMBER, OBJECT, OBJECT_PATTERN, OPTIONAL_CHAIN, OPTIONAL_PARAMETER, PAIR, PAIR_PATTERN,
    PARENTHESIZED_EXPRESSION, PROGRAM, PROPERTY_IDENTIFIER, REQUIRED_PARAMETER, RETURN_STATEMENT,
    SATISFIES_EXPRESSION, SHORTHAND_PROPERTY_IDENTIFIER, SHORTHAND_PROPERTY_IDENTIFIER_PATTERN,
    SPREAD_ELEMENT, STATEMENT_BLOCK, STRING, STRING_FRAGMENT, SUBSCRIPT_EXPRESSION,
    SWITCH_STATEMENT, TEMPLATE_STRING, TEMPLATE_SUBSTITUTION, TERNARY_EXPRESSION, THROW_STATEMENT,
    TRUE, TRY_STATEMENT, TYPE_ALIAS_DECLARATION, TYPE_ANNOTATION, TYPE_ARGUMENTS, TYPE_PARAMETERS,
    TYPE_PREDICATE_ANNOTATION, UNARY_EXPRESSION, UNDEFINED, UPDATE_EXPRESSION,
    VARIABLE_DECLARATION, VARIABLE_DECLARATOR, WHILE_STATEMENT, line,
};

const FENCE: &str = "```";
const JAVASCRIPT_FENCES: [&str; 4] = ["js", "javascript", "ts", "typescript"];
const PYTHON_FENCES: [&str; 4] = ["", "python", "py", "python3"];
const MAX_DEPTH: usize = 128;
const FIRST_LINE: usize = 1;
/// Kinds only JavaScript or TypeScript writes, any one of which marks a snippet as JavaScript.
const EVIDENCE_KINDS: [&str; 11] = [
    LEXICAL_DECLARATION,
    VARIABLE_DECLARATION,
    ARROW_FUNCTION,
    FUNCTION_DECLARATION,
    GENERATOR_FUNCTION_DECLARATION,
    TEMPLATE_STRING,
    SWITCH_STATEMENT,
    DO_STATEMENT,
    CLASS_DECLARATION,
    INTERFACE_DECLARATION,
    ENUM_DECLARATION,
];
const EVIDENCE_TOKENS: [&str; 2] = ["===", "!=="];
/// Subtrees that only say what type something has: erased whole, so nothing in them runs.
const TYPE_KINDS: [&str; 7] = [
    TYPE_ANNOTATION,
    TYPE_ARGUMENTS,
    TYPE_PARAMETERS,
    ASSERTS_ANNOTATION,
    TYPE_PREDICATE_ANNOTATION,
    INTERFACE_DECLARATION,
    TYPE_ALIAS_DECLARATION,
];
/// Wrappers whose first named child is the expression and the rest its type.
const TYPED_KINDS: [&str; 2] = [AS_EXPRESSION, SATISFIES_EXPRESSION];
/// Every other kind the translator lowers. A kind not here refuses before anything is lowered.
const RUNTIME_KINDS: [&str; 65] = [
    PROGRAM,
    COMMENT,
    EMPTY_STATEMENT,
    EXPRESSION_STATEMENT,
    LEXICAL_DECLARATION,
    VARIABLE_DECLARATION,
    VARIABLE_DECLARATOR,
    STATEMENT_BLOCK,
    IF_STATEMENT,
    ELSE_CLAUSE,
    FOR_IN_STATEMENT,
    FOR_STATEMENT,
    WHILE_STATEMENT,
    BREAK_STATEMENT,
    CONTINUE_STATEMENT,
    TRY_STATEMENT,
    CATCH_CLAUSE,
    FINALLY_CLAUSE,
    THROW_STATEMENT,
    RETURN_STATEMENT,
    FUNCTION_DECLARATION,
    FORMAL_PARAMETERS,
    REQUIRED_PARAMETER,
    OPTIONAL_PARAMETER,
    IDENTIFIER,
    PROPERTY_IDENTIFIER,
    SHORTHAND_PROPERTY_IDENTIFIER,
    SHORTHAND_PROPERTY_IDENTIFIER_PATTERN,
    NUMBER,
    STRING,
    STRING_FRAGMENT,
    ESCAPE_SEQUENCE,
    TEMPLATE_STRING,
    TEMPLATE_SUBSTITUTION,
    TRUE,
    FALSE,
    NULL,
    UNDEFINED,
    ARRAY,
    OBJECT,
    PAIR,
    SPREAD_ELEMENT,
    COMPUTED_PROPERTY_NAME,
    ARRAY_PATTERN,
    OBJECT_PATTERN,
    PAIR_PATTERN,
    MEMBER_EXPRESSION,
    SUBSCRIPT_EXPRESSION,
    OPTIONAL_CHAIN,
    CALL_EXPRESSION,
    ARGUMENTS,
    AWAIT_EXPRESSION,
    UNARY_EXPRESSION,
    BINARY_EXPRESSION,
    TERNARY_EXPRESSION,
    PARENTHESIZED_EXPRESSION,
    AS_EXPRESSION,
    SATISFIES_EXPRESSION,
    NON_NULL_EXPRESSION,
    NEW_EXPRESSION,
    ARROW_FUNCTION,
    ASSIGNMENT_EXPRESSION,
    AUGMENTED_ASSIGNMENT_EXPRESSION,
    UPDATE_EXPRESSION,
    HTML_COMMENT,
];

pub(crate) type Translation<T> = Result<T, Untranslated>;

/// Why a snippet was not translated, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Untranslated {
    pub(crate) construct: Construct,
    /// 1-based, in the snippet as written.
    pub(crate) line: usize,
}

impl Untranslated {
    pub(crate) fn is_javascript(&self) -> bool {
        self.construct != Construct::NotJavascript
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Construct {
    /// Nothing in the snippet is only JavaScript, so it is left to the Python diagnostics.
    NotJavascript,
    /// JavaScript the parser could not read whole.
    Unparsed,
    /// The Python would be longer than a snippet may be.
    TooLarge,
    /// Nested deeper than the translator follows.
    TooDeep,
    /// A construct with no Python form, named as the reader would know it.
    Syntax(String),
    /// A condition on a value whose truthiness differs between the two languages, as written.
    Condition(String),
    /// A deferred call no `await` or `Promise.all` consumes, as written.
    Unawaited(String),
}

impl fmt::Display for Construct {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJavascript => formatter.write_str("code that is not JavaScript"),
            Self::Unparsed => formatter.write_str("JavaScript that does not parse"),
            Self::TooLarge => formatter.write_str("a snippet whose Python is too long"),
            Self::TooDeep => write!(formatter, "nesting deeper than {MAX_DEPTH} levels"),
            Self::Syntax(named) => formatter.write_str(named),
            Self::Condition(written) => write!(formatter, "the condition `{written}`"),
            Self::Unawaited(written) => write!(formatter, "`{written}` without `await`"),
        }
    }
}

/// What lowering carries from statement to statement: the scopes, and the callbacks lifted to a
/// `def` that wait to be placed before the statement that uses them.
struct Translator<'code> {
    source: &'code str,
    stubs: &'code StubIndex,
    scopes: Scopes,
    lifted: Vec<Statement>,
    callbacks: usize,
    temporaries: usize,
    /// The snippet's last statement, when it is a `return`: it becomes the value the snippet
    /// ends on.
    final_return: Option<usize>,
}

/// `code` as Python, when it is JavaScript that has one.
pub(crate) fn translated(code: &str, stubs: &StubIndex) -> Translation<String> {
    let (source, fenced) = unfenced(code);
    let tree = parse(&source).ok_or(Untranslated {
        construct: Construct::Unparsed,
        line: FIRST_LINE,
    })?;
    let root = tree.root_node();
    if root.has_error() {
        let construct = if fenced || has_evidence(root) {
            Construct::Unparsed
        } else {
            Construct::NotJavascript
        };
        return Err(Untranslated {
            construct,
            line: first_error(root),
        });
    }
    if !fenced && !has_evidence(root) {
        return Err(Untranslated {
            construct: Construct::NotJavascript,
            line: FIRST_LINE,
        });
    }
    admitted(root, &source)?;
    let mut translator = Translator {
        source: &source,
        stubs,
        scopes: Scopes::default(),
        lifted: Vec::new(),
        callbacks: 0,
        temporaries: 0,
        final_return: None,
    };
    let module = translator.module(root)?;
    let python = python::printed(&CheckedModule::consumed(module)?);
    if python.len() > MAX_CODE_BYTES {
        return Err(Untranslated {
            construct: Construct::TooLarge,
            line: FIRST_LINE,
        });
    }
    Ok(python)
}

/// `code` with a Markdown fence around all of it blanked line for line, and whether the fence
/// named JavaScript or TypeScript.
fn unfenced(code: &str) -> (String, bool) {
    let mut lines: Vec<&str> = code.lines().collect();
    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return (code.to_owned(), false);
    };
    let language = lines[first].trim().strip_prefix(FENCE);
    let Some(language) = language.filter(|_| last > first && lines[last].trim() == FENCE) else {
        return (code.to_owned(), false);
    };
    let javascript = JAVASCRIPT_FENCES.contains(&language);
    if !javascript && !PYTHON_FENCES.contains(&language) {
        return (code.to_owned(), false);
    }
    lines[first] = "";
    lines[last] = "";
    (lines.join("\n"), javascript)
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    parser.parse(source, None)
}

/// Every node in source order with its depth, without recursing, so no tree overflows the stack.
fn each_node<'tree>(root: Node<'tree>, mut visit: impl FnMut(Node<'tree>, usize) -> Descend) {
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        let descend = visit(cursor.node(), depth);
        if descend == Descend::Into && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
            depth -= 1;
        }
    }
}

#[derive(PartialEq, Eq)]
enum Descend {
    Into,
    Over,
}

/// Whether anything outside a parse error is something only JavaScript writes, a `for` that
/// declares its variable included. A string or a comment never is.
fn has_evidence(root: Node<'_>) -> bool {
    let mut found = false;
    each_node(root, |node, _| {
        if node.is_error() {
            return Descend::Over;
        }
        found |= if node.is_named() {
            EVIDENCE_KINDS.contains(&node.kind())
                || (node.kind() == FOR_IN_STATEMENT
                    && node.child_by_field_name(KIND_FIELD).is_some())
        } else {
            EVIDENCE_TOKENS.contains(&node.kind())
        };
        Descend::Into
    });
    found
}

fn first_error(root: Node<'_>) -> usize {
    let mut found = None;
    each_node(root, |node, _| {
        if found.is_none() && (node.is_error() || node.is_missing()) {
            found = Some(line(node));
        }
        if node.has_error() {
            Descend::Into
        } else {
            Descend::Over
        }
    });
    found.unwrap_or(FIRST_LINE)
}

/// Refuses the first node, in source order, of a kind the translator does not lower, and any
/// nesting past [`MAX_DEPTH`]. Type subtrees are passed over whole: they are erased.
fn admitted(root: Node<'_>, source: &str) -> Translation<()> {
    let mut refusal = None;
    each_node(root, |node, depth| {
        if refusal.is_some() || TYPE_KINDS.contains(&node.kind()) {
            return Descend::Over;
        }
        if depth > MAX_DEPTH {
            refusal = Some(Untranslated {
                construct: Construct::TooDeep,
                line: line(node),
            });
            return Descend::Over;
        }
        if node.is_named() && !RUNTIME_KINDS.contains(&node.kind()) {
            if !is_typed_type(node) {
                refusal = Some(syntax::refused(node, source));
            }
            return Descend::Over;
        }
        Descend::Into
    });
    refusal.map_or(Ok(()), Err)
}

/// Whether `node` is the type in `x as T` or `x satisfies T`, after the expression.
fn is_typed_type(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        TYPED_KINDS.contains(&parent.kind())
            && parent
                .named_child(0)
                .is_some_and(|expression| expression.id() != node.id())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{HashMap, HashSet};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::{env, fs};

    use test_case::test_case;

    use super::{Construct, MAX_DEPTH, Untranslated, translated};
    use crate::repair::{StubIndex, reads_as_python};
    use crate::types::MAX_CODE_BYTES;

    const CORPUS_VARIABLE: &str = "WORKCELL_JAVASCRIPT_CORPUS";
    const CORPUS_EXTENSIONS: [&str; 5] = ["js", "mjs", "cjs", "ts", "txt"];
    const SNIPPET_SEPARATOR: &str = "\n----\n";
    const MOST_REFUSED: usize = 15;
    const MAX_LISTED_FAILURES: usize = 30;

    const FIELD_JAVASCRIPT: &str = include_str!("../../tests/fixtures/field_javascript.js");
    const STUBS: &str = "from typing import Any\nclass _Gh:\n    async def list(self, *, owner: str = '') -> Any: ...\n    async def get(self, *, id: Any = None) -> Any: ...\n    def nope(self) -> Any: ...\ngh: _Gh\nclass _Executor:\n    async def search(self, *, query: str = '', namespace: str = '', limit: int = 0) -> Any: ...\nexecutor: _Executor\ndef search(query: str) -> list[str]: ...\nasync def fetch(url: str) -> Any: ...\n";
    const FIELD_PYTHON: &str = include_str!("../../tests/fixtures/field_javascript.py");
    const SWITCH: &str = "`switch`";
    const CLASS: &str = "`class`";
    const LABEL: &str = "a label";
    const DECORATOR: &str = "a decorator";
    const AWAIT: &str = "`await`";
    const OPTIONAL_CALL: &str = "`?.()`";
    const REGEX: &str = "a regular expression";
    const LONE_SURROGATE: &str = "a string with a lone surrogate";
    const HOISTED_CALL: &str = "`f()`";

    /// The Python `js` translates to, which must read as Python.
    pub(crate) fn python(js: &str) -> String {
        let python = translated(js, &StubIndex::of(STUBS)).unwrap_or_else(|refusal| {
            panic!(
                "{js:?} refused: {} on line {}",
                refusal.construct, refusal.line
            )
        });
        assert!(reads_as_python(&python), "{python}");
        python
    }

    /// Why `js` is not translated.
    pub(crate) fn refused(js: &str) -> Untranslated {
        match translated(js, &StubIndex::of(STUBS)) {
            Ok(python) => panic!("{js:?} translated to {python:?}"),
            Err(refusal) => refusal,
        }
    }

    #[test]
    fn field_javascript_translates() {
        assert_eq!(python(FIELD_JAVASCRIPT), FIELD_PYTHON.trim_end());
    }

    #[test_case("```js\nconst a = 1;\na\n```", "a = 1\na" ; "javascript_fence")]
    #[test_case("```ts\nlet a: number = 1;\na\n```", "a = 1\na" ; "typescript_fence")]
    #[test_case("```python\nconst a = 1;\na\n```", "a = 1\na" ; "python_fence")]
    #[test_case("```js\nconsole.log(1);;\n```", "print(1)" ; "fence_is_evidence")]
    fn fenced_snippet_translates(js: &str, expected: &str) {
        assert_eq!(python(js), expected);
    }

    #[test_case("for (const x of [1, 2]) console.log(x);", "for x in [1, 2]:\n    print(x)" ; "for_of_is_evidence")]
    #[test_case("const a = [\"x\", \"y\"] as const;\na", "a = [\"x\", \"y\"]\na" ; "as_const")]
    #[test_case("const café = 1;\ncafé", "café = 1\ncafé" ; "non_ascii_name")]
    fn unfenced_snippet_translates(js: &str, expected: &str) {
        assert_eq!(python(js), expected);
    }

    #[test_case("x = 1;;" ; "assignment")]
    #[test_case("print(\"x\");;" ; "call")]
    #[test_case("x = \"const a = () => 1\";;" ; "evidence_in_a_string")]
    #[test_case("// const a = 1\nx = 1;;" ; "evidence_in_a_comment")]
    #[test_case("results = await gh.list(" ; "unparsed_python")]
    fn snippet_without_evidence_is_not_javascript(code: &str) {
        assert_eq!(refused(code).construct, Construct::NotJavascript);
    }

    #[test_case("const a = 1;\nswitch (a) { case 1: break; }", SWITCH, 2 ; "switch")]
    #[test_case("const a = 1;\nclass B {}", CLASS, 2 ; "class")]
    #[test_case("const a = 1;\nouter: for (const b of a) { break outer; }", LABEL, 2 ; "label")]
    #[test_case("const a = 1;\nwhile (a > 0) { break outer; }", LABEL, 2 ; "break_label")]
    #[test_case("const a = 1;\nfunction f(@d b) {}", DECORATOR, 2 ; "decorator")]
    #[test_case("const a = [];\nfor await (const b of a) {}", AWAIT, 2 ; "for_await")]
    #[test_case("const f = 1;\n\nf?.();", OPTIONAL_CALL, 3 ; "optional_call")]
    #[test_case("const a = [1].map(x => x.replace(/a/, \"b\"));", REGEX, 1 ; "regex_under_a_supported_parent")]
    #[test_case("const a = 1;\nconst s = \"\\uD800\";", LONE_SURROGATE, 2 ; "lone_surrogate")]
    #[test_case("const xs = [1];\nf();\nfunction f() { console.log(1); }", HOISTED_CALL, 2 ; "function_called_before_its_declaration")]
    fn construct_refuses(js: &str, construct: &str, line: usize) {
        let refusal = refused(js);
        assert_eq!(
            (refusal.construct.to_string().as_str(), refusal.line),
            (construct, line)
        );
    }

    #[test]
    fn nesting_past_the_limit_refuses() {
        let nested = format!(
            "const a = {}1{};",
            "(".repeat(MAX_DEPTH),
            ")".repeat(MAX_DEPTH)
        );
        assert_eq!(refused(&nested).construct, Construct::TooDeep);
    }

    /// The deepest nesting admission lets through is lowered, printed and parsed again within a
    /// test thread's stack, which is no larger than the runtime's.
    #[test_case(|n| format!("const a = {}1{};", "(".repeat(n), ")".repeat(n)) ; "parentheses")]
    #[test_case(|n| format!("const a = {}1{};", "[".repeat(n), "]".repeat(n)) ; "arrays")]
    #[test_case(|n| format!("const a = {}1{};", "{a: ".repeat(n), "}".repeat(n)) ; "objects")]
    #[test_case(|n| format!("const a = {}1{};", "String(".repeat(n), ")".repeat(n)) ; "calls")]
    #[test_case(|n| format!("const b = 1;\nconst a = {}2;", "b > 0 ? 1 : ".repeat(n)) ; "ternaries")]
    #[test_case(|n| format!("const b = 1;\nconst a = {}b;", "b + ".repeat(n)) ; "sums")]
    #[test_case(|n| format!("const b = \"1\";\nconst a = `${{b}}` + {}b;", "`x` + b + ".repeat(n)) ; "strings")]
    #[test_case(|n| format!("const o = {{}};\nconst a = o{};", ".b".repeat(n)) ; "members")]
    #[test_case(|n| format!("const a = 1;\n{}{}", "if (a > 0) {".repeat(n), "}".repeat(n)) ; "blocks")]
    #[test_case(|n| format!("const a = 1;\nif ({}a > 0) {{}}", "a > 0 && ".repeat(n)) ; "conditions")]
    #[test_case(|n| format!("const xs = [1];\nconst a = {}1{};", (0..n).map(|i| format!("xs.map(x{i} => ")).collect::<String>(), ")".repeat(n)) ; "callbacks")]
    fn nesting_at_the_limit_never_overflows(nested: impl Fn(usize) -> String) {
        let stubs = StubIndex::of(STUBS);
        let deepest = (1..)
            .find(|n| {
                translated(&nested(*n), &stubs)
                    .is_err_and(|refusal| refusal.construct == Construct::TooDeep)
            })
            .expect("refused past the limit");
        let admitted = nested(deepest - 1);
        assert!(deepest > 1, "{admitted}");
        let translation = translated(&admitted, &stubs);
        assert!(translation.is_ok(), "{translation:?}");
    }

    #[test]
    fn python_past_the_size_limit_refuses() {
        let long = format!(
            "const a = {{}};\nconst b = [{}];",
            "a?.b, ".repeat(MAX_CODE_BYTES / 16)
        );
        assert_eq!(refused(&long).construct, Construct::TooLarge);
    }

    /// Over real JavaScript: `WORKCELL_JAVASCRIPT_CORPUS=/usr/share/javascript cargo test --release
    /// -p workcell-mcp-code -- --ignored corpus_javascript`. Every snippet refuses or translates to
    /// Python the parser reads, and none panics. A file holds one snippet, or several separated by
    /// `----` lines as review probe files do.
    #[test]
    #[ignore = "reads a JavaScript corpus named by WORKCELL_JAVASCRIPT_CORPUS"]
    fn corpus_javascript_translates_or_refuses() {
        let corpus = env::var_os(CORPUS_VARIABLE).expect(CORPUS_VARIABLE);
        let stubs = StubIndex::of(STUBS);
        let mut pending = vec![PathBuf::from(corpus)];
        let (mut snippets, mut translations) = (0, 0);
        let mut refusals: HashMap<String, usize> = HashMap::new();
        let mut failures = Vec::new();
        let mut visited = HashSet::new();
        while let Some(path) = pending.pop() {
            if !fs::canonicalize(&path).is_ok_and(|canonical| visited.insert(canonical)) {
                continue;
            }
            if path.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| entry.path()),
                );
                continue;
            }
            let is_javascript = path.extension().is_some_and(|extension| {
                CORPUS_EXTENSIONS
                    .iter()
                    .any(|candidate| extension == *candidate)
            });
            let Some(code) = is_javascript
                .then(|| fs::read_to_string(&path).ok())
                .flatten()
            else {
                continue;
            };
            for (index, snippet) in code.split(SNIPPET_SEPARATOR).enumerate() {
                snippets += 1;
                let at = format!("{}#{index}", path.display());
                match catch_unwind(AssertUnwindSafe(|| translated(snippet, &stubs))) {
                    Ok(Ok(python)) if reads_as_python(&python) => translations += 1,
                    Ok(Ok(python)) => failures.push(format!("{at}: unreadable\n{python}")),
                    Ok(Err(refusal)) => {
                        *refusals.entry(refusal.construct.to_string()).or_default() += 1
                    }
                    Err(_) => failures.push(format!("{at}: panicked")),
                }
            }
        }
        let mut most_refused: Vec<(String, usize)> = refusals.into_iter().collect();
        let refused: usize = most_refused.iter().map(|(_, count)| count).sum();
        most_refused.sort_by(|(a, a_count), (b, b_count)| b_count.cmp(a_count).then(a.cmp(b)));
        eprintln!("{snippets} snippets, {translations} translated, {refused} refused");
        for (construct, count) in most_refused.iter().take(MOST_REFUSED) {
            eprintln!("{count:>6} {construct}");
        }
        assert!(
            failures.is_empty(),
            "{}",
            failures[..failures.len().min(MAX_LISTED_FAILURES)].join("\n")
        );
    }

    #[test]
    fn unparsed_javascript_is_javascript() {
        assert_eq!(
            refused("const a = 1;\nconst b = (").construct,
            Construct::Unparsed
        );
    }
}
