//! The only way the translator reads the tree. [`Parts::read`] hands an arm a node's children and,
//! once the arm returns, refuses the first one it left unclaimed unless that child is a comment or
//! punctuation. An arm that forgets a field, an `async`, a `?.` or a `for await` refuses instead of
//! translating it away.

use tree_sitter::Node;

use super::{Construct, Translation, Untranslated};

pub(super) const PROGRAM: &str = "program";
pub(super) const COMMENT: &str = "comment";
pub(super) const HTML_COMMENT: &str = "html_comment";
pub(super) const EXPRESSION_STATEMENT: &str = "expression_statement";
pub(super) const LEXICAL_DECLARATION: &str = "lexical_declaration";
pub(super) const VARIABLE_DECLARATION: &str = "variable_declaration";
pub(super) const VARIABLE_DECLARATOR: &str = "variable_declarator";
pub(super) const IF_STATEMENT: &str = "if_statement";
pub(super) const ELSE_CLAUSE: &str = "else_clause";
pub(super) const FOR_IN_STATEMENT: &str = "for_in_statement";
pub(super) const FOR_STATEMENT: &str = "for_statement";
pub(super) const WHILE_STATEMENT: &str = "while_statement";
pub(super) const BREAK_STATEMENT: &str = "break_statement";
pub(super) const CONTINUE_STATEMENT: &str = "continue_statement";
pub(super) const TRY_STATEMENT: &str = "try_statement";
pub(super) const CATCH_CLAUSE: &str = "catch_clause";
pub(super) const FINALLY_CLAUSE: &str = "finally_clause";
pub(super) const THROW_STATEMENT: &str = "throw_statement";
pub(super) const RETURN_STATEMENT: &str = "return_statement";
pub(super) const FUNCTION_DECLARATION: &str = "function_declaration";
pub(super) const STATEMENT_BLOCK: &str = "statement_block";
pub(super) const EMPTY_STATEMENT: &str = "empty_statement";
pub(super) const INTERFACE_DECLARATION: &str = "interface_declaration";
pub(super) const TYPE_ALIAS_DECLARATION: &str = "type_alias_declaration";
pub(super) const IDENTIFIER: &str = "identifier";
pub(super) const PROPERTY_IDENTIFIER: &str = "property_identifier";
pub(super) const SHORTHAND_PROPERTY_IDENTIFIER: &str = "shorthand_property_identifier";
pub(super) const SHORTHAND_PROPERTY_IDENTIFIER_PATTERN: &str =
    "shorthand_property_identifier_pattern";
pub(super) const STATEMENT_IDENTIFIER: &str = "statement_identifier";
pub(super) const NUMBER: &str = "number";
pub(super) const STRING: &str = "string";
pub(super) const STRING_FRAGMENT: &str = "string_fragment";
pub(super) const ESCAPE_SEQUENCE: &str = "escape_sequence";
pub(super) const TEMPLATE_STRING: &str = "template_string";
pub(super) const TEMPLATE_SUBSTITUTION: &str = "template_substitution";
pub(super) const TRUE: &str = "true";
pub(super) const FALSE: &str = "false";
pub(super) const NULL: &str = "null";
pub(super) const UNDEFINED: &str = "undefined";
pub(super) const ARRAY: &str = "array";
pub(super) const OBJECT: &str = "object";
pub(super) const PAIR: &str = "pair";
pub(super) const SPREAD_ELEMENT: &str = "spread_element";
pub(super) const COMPUTED_PROPERTY_NAME: &str = "computed_property_name";
pub(super) const MEMBER_EXPRESSION: &str = "member_expression";
pub(super) const SUBSCRIPT_EXPRESSION: &str = "subscript_expression";
pub(super) const CALL_EXPRESSION: &str = "call_expression";
pub(super) const ARGUMENTS: &str = "arguments";
pub(super) const AWAIT_EXPRESSION: &str = "await_expression";
pub(super) const UNARY_EXPRESSION: &str = "unary_expression";
pub(super) const BINARY_EXPRESSION: &str = "binary_expression";
pub(super) const TERNARY_EXPRESSION: &str = "ternary_expression";
pub(super) const PARENTHESIZED_EXPRESSION: &str = "parenthesized_expression";
pub(super) const AS_EXPRESSION: &str = "as_expression";
pub(super) const SATISFIES_EXPRESSION: &str = "satisfies_expression";
pub(super) const NON_NULL_EXPRESSION: &str = "non_null_expression";
pub(super) const NEW_EXPRESSION: &str = "new_expression";
pub(super) const ARROW_FUNCTION: &str = "arrow_function";
pub(super) const FORMAL_PARAMETERS: &str = "formal_parameters";
pub(super) const REQUIRED_PARAMETER: &str = "required_parameter";
pub(super) const OPTIONAL_PARAMETER: &str = "optional_parameter";
pub(super) const ASSIGNMENT_EXPRESSION: &str = "assignment_expression";
pub(super) const AUGMENTED_ASSIGNMENT_EXPRESSION: &str = "augmented_assignment_expression";
pub(super) const UPDATE_EXPRESSION: &str = "update_expression";
pub(super) const ARRAY_PATTERN: &str = "array_pattern";
pub(super) const OBJECT_PATTERN: &str = "object_pattern";
pub(super) const PAIR_PATTERN: &str = "pair_pattern";
pub(super) const OPTIONAL_CHAIN: &str = "optional_chain";
pub(super) const TYPE_ANNOTATION: &str = "type_annotation";
pub(super) const TYPE_ARGUMENTS: &str = "type_arguments";
pub(super) const TYPE_PARAMETERS: &str = "type_parameters";
pub(super) const ASSERTS_ANNOTATION: &str = "asserts_annotation";
pub(super) const TYPE_PREDICATE_ANNOTATION: &str = "type_predicate_annotation";
pub(super) const SWITCH_STATEMENT: &str = "switch_statement";
pub(super) const DO_STATEMENT: &str = "do_statement";
pub(super) const CLASS_DECLARATION: &str = "class_declaration";
pub(super) const ENUM_DECLARATION: &str = "enum_declaration";
pub(super) const LABELED_STATEMENT: &str = "labeled_statement";
pub(super) const GENERATOR_FUNCTION_DECLARATION: &str = "generator_function_declaration";

pub(super) const BODY_FIELD: &str = "body";
pub(super) const NAME_FIELD: &str = "name";
pub(super) const VALUE_FIELD: &str = "value";
pub(super) const KIND_FIELD: &str = "kind";
pub(super) const TYPE_FIELD: &str = "type";
pub(super) const CONDITION_FIELD: &str = "condition";
pub(super) const CONSEQUENCE_FIELD: &str = "consequence";
pub(super) const ALTERNATIVE_FIELD: &str = "alternative";
pub(super) const LEFT_FIELD: &str = "left";
pub(super) const RIGHT_FIELD: &str = "right";
pub(super) const OPERATOR_FIELD: &str = "operator";
pub(super) const INITIALIZER_FIELD: &str = "initializer";
pub(super) const INCREMENT_FIELD: &str = "increment";
pub(super) const HANDLER_FIELD: &str = "handler";
pub(super) const FINALIZER_FIELD: &str = "finalizer";
pub(super) const PARAMETER_FIELD: &str = "parameter";
pub(super) const PARAMETERS_FIELD: &str = "parameters";
pub(super) const RETURN_TYPE_FIELD: &str = "return_type";
pub(super) const TYPE_PARAMETERS_FIELD: &str = "type_parameters";
pub(super) const TYPE_ARGUMENTS_FIELD: &str = "type_arguments";
pub(super) const PATTERN_FIELD: &str = "pattern";
pub(super) const OBJECT_FIELD: &str = "object";
pub(super) const PROPERTY_FIELD: &str = "property";
pub(super) const INDEX_FIELD: &str = "index";
pub(super) const FUNCTION_FIELD: &str = "function";
pub(super) const ARGUMENTS_FIELD: &str = "arguments";
pub(super) const ARGUMENT_FIELD: &str = "argument";
pub(super) const KEY_FIELD: &str = "key";
pub(super) const OPTIONAL_CHAIN_FIELD: &str = "optional_chain";
pub(super) const CONSTRUCTOR_FIELD: &str = "constructor";

pub(super) const ASYNC: &str = "async";
pub(super) const AWAIT: &str = "await";
pub(super) const CONST: &str = "const";
pub(super) const LET: &str = "let";
pub(super) const VAR: &str = "var";
pub(super) const OF: &str = "of";
pub(super) const IN: &str = "in";
pub(super) const AS: &str = "as";
pub(super) const SATISFIES: &str = "satisfies";
pub(super) const NEW: &str = "new";
pub(super) const COMMA: &str = ",";
pub(super) const SEMICOLON: &str = ";";
pub(super) const OPTIONAL_TOKEN: &str = "?.";

/// Tokens that only mark where a construct's parts are. Every other token, `async`, `await`,
/// `?.`, `in`, `of` and every operator among them, must be claimed by the arm that reads its node.
const DELIMITERS: [&str; 30] = [
    "(", ")", "[", "]", "{", "}", ",", ";", ":", ".", "=", "=>", "?", "...", "${", "`", "\"", "'",
    "if", "else", "for", "while", "return", "throw", "try", "catch", "finally", "function",
    "break", "continue",
];
/// How a refused kind is named where its leading token would not say what it is.
const NAMED_KINDS: [(&str, &str); 16] = [
    ("arrow_function", "a function used as a value"),
    ("function_expression", "a function used as a value"),
    ("regex", "a regular expression"),
    ("sequence_expression", "the comma operator"),
    (
        "assignment_expression",
        "an assignment inside an expression",
    ),
    (
        "augmented_assignment_expression",
        "an assignment inside an expression",
    ),
    ("update_expression", "`++` or `--` inside an expression"),
    (LABELED_STATEMENT, "a label"),
    (STATEMENT_IDENTIFIER, "a label"),
    ("generator_function_declaration", "a generator"),
    ("generator_function", "a generator"),
    ("rest_pattern", "`...` in a pattern"),
    ("assignment_pattern", "a default in a pattern"),
    ("object_assignment_pattern", "a default in a pattern"),
    ("decorator", "a decorator"),
    ("hash_bang_line", "a `#!` line"),
];
const MAX_CONSTRUCT_CHARS: usize = 40;
const ELLIPSIS: char = '…';
const KIND_WORD_SEPARATOR: char = '_';
const LINE_CONTINUATIONS: [&str; 3] = ["\r\n", "\n", "\r"];
const ESCAPE: char = '\\';
const HEX_RADIX: u32 = 16;
const LONE_SURROGATE: &str = "a string with a lone surrogate";

pub(super) struct Parts<'tree, 'code> {
    node: Node<'tree>,
    source: &'code str,
    parts: Vec<Part<'tree>>,
}

struct Part<'tree> {
    node: Node<'tree>,
    field: Option<&'tree str>,
    claimed: bool,
}

impl<'tree, 'code> Parts<'tree, 'code> {
    /// Runs `arm` over `node`'s children, then refuses the first child it left unclaimed. The
    /// only constructor, so no arm skips the audit.
    pub(super) fn read<T>(
        node: Node<'tree>,
        source: &'code str,
        arm: impl FnOnce(&mut Self) -> Translation<T>,
    ) -> Translation<T> {
        let mut cursor = node.walk();
        let mut parts = Vec::new();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                parts.push(Part {
                    node: child,
                    field: cursor.field_name(),
                    claimed: child.is_extra(),
                });
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        let mut parts = Self {
            node,
            source,
            parts,
        };
        let value = arm(&mut parts)?;
        parts.finish()?;
        Ok(value)
    }

    pub(super) fn kind(&self) -> &'tree str {
        self.node.kind()
    }

    /// Claims the child in `field`.
    pub(super) fn field(&mut self, field: &str) -> Option<Node<'tree>> {
        self.claim(|part| part.field == Some(field))
    }

    /// Claims the child in `field`, whose absence refuses the node.
    pub(super) fn required(&mut self, field: &str) -> Translation<Node<'tree>> {
        self.field(field).ok_or_else(|| self.refused())
    }

    /// Claims every child in `field`, in source order.
    pub(super) fn fields(&mut self, field: &str) -> Vec<Node<'tree>> {
        std::iter::from_fn(|| self.field(field)).collect()
    }

    /// Claims the anonymous `token`, reporting whether it was there.
    pub(super) fn token(&mut self, token: &str) -> bool {
        self.claim(|part| !part.node.is_named() && part.node.kind() == token)
            .is_some()
    }

    /// Claims every named child without a field, in source order: a list's items.
    pub(super) fn named(&mut self) -> Vec<Node<'tree>> {
        std::iter::from_fn(|| self.claim(|part| part.node.is_named() && part.field.is_none()))
            .collect()
    }

    /// Claims the one named child without a field: an expression statement's expression.
    pub(super) fn only(&mut self) -> Translation<Node<'tree>> {
        match self.named()[..] {
            [only] => Ok(only),
            _ => Err(self.refused()),
        }
    }

    /// Claims every child, comments included, in source order: a block's statements.
    pub(super) fn statements(&mut self) -> Vec<Node<'tree>> {
        let mut statements = Vec::new();
        for part in &mut self.parts {
            if part.node.is_named() && part.field.is_none() {
                part.claimed = true;
                statements.push(part.node);
            }
        }
        statements
    }

    /// The anonymous tokens and named children in source order, claiming them all: an array's
    /// items with the commas between them.
    pub(super) fn sequence(&mut self) -> Vec<Node<'tree>> {
        self.parts
            .iter_mut()
            .filter(|part| !part.node.is_extra() && part.field.is_none())
            .map(|part| {
                part.claimed = true;
                part.node
            })
            .collect()
    }

    /// The node itself refused, as [`construct`] names it.
    pub(super) fn refused(&self) -> Untranslated {
        refused(self.node, self.source)
    }

    fn claim(&mut self, matches: impl Fn(&Part<'tree>) -> bool) -> Option<Node<'tree>> {
        let part = self
            .parts
            .iter_mut()
            .find(|part| !part.claimed && matches(part))?;
        part.claimed = true;
        Some(part.node)
    }

    fn finish(self) -> Translation<()> {
        match self.parts.iter().find(|part| {
            !part.claimed && (part.node.is_named() || !DELIMITERS.contains(&part.node.kind()))
        }) {
            Some(part) => Err(refused(part.node, self.source)),
            None => Ok(()),
        }
    }
}

/// 1-based, in the snippet as written: a fence is blanked line for line.
pub(super) fn line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

pub(super) fn refused(node: Node<'_>, source: &str) -> Untranslated {
    Untranslated {
        construct: construct(node, source),
        line: line(node),
    }
}

/// A refused node's name: [`NAMED_KINDS`], else its leading token or its own text in backticks.
pub(super) fn construct(node: Node<'_>, source: &str) -> Construct {
    if let Some((_, named)) = NAMED_KINDS.iter().find(|(kind, _)| *kind == node.kind()) {
        return Construct::Syntax((*named).to_owned());
    }
    if !node.is_named() {
        return Construct::Syntax(format!("`{}`", node.kind()));
    }
    if node.child_count() == 0 {
        return Construct::Syntax(format!("`{}`", written(node, source)));
    }
    match node.child(0).filter(|first| !first.is_named()) {
        Some(token) => Construct::Syntax(format!("`{}`", token.kind())),
        None => Construct::Syntax(node.kind().replace(KIND_WORD_SEPARATOR, " ")),
    }
}

/// `node`'s source on one line, cut to [`MAX_CONSTRUCT_CHARS`], for a construct's text.
pub(super) fn written(node: Node<'_>, source: &str) -> String {
    let text = source[node.byte_range()]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match text.char_indices().nth(MAX_CONSTRUCT_CHARS) {
        Some((end, _)) => format!("{}{ELLIPSIS}", &text[..end]),
        None => text,
    }
}

/// The text of a string or a template run as UTF-16, so a surrogate pair written as two escapes
/// joins into one character and a lone surrogate refuses.
#[derive(Default)]
pub(super) struct Text(Vec<u16>);

impl Text {
    pub(super) fn fragment(&mut self, text: &str) {
        self.0.extend(text.encode_utf16());
    }

    /// Adds one escape sequence's value. A legacy octal escape refuses, as strict mode does.
    pub(super) fn escape(&mut self, node: Node<'_>, source: &str) -> Translation<()> {
        let written = &source[node.byte_range()];
        let refuse = || refused(node, source);
        let escaped = written.strip_prefix(ESCAPE).ok_or_else(refuse)?;
        if LINE_CONTINUATIONS.contains(&escaped) {
            return Ok(());
        }
        let mut chars = escaped.chars();
        let first = chars.next().ok_or_else(refuse)?;
        let rest = chars.as_str();
        let unit = |hex: &str| u32::from_str_radix(hex, HEX_RADIX).map_err(|_| refuse());
        match first {
            'n' => self.fragment("\n"),
            't' => self.fragment("\t"),
            'r' => self.fragment("\r"),
            'b' => self.fragment("\u{8}"),
            'f' => self.fragment("\u{c}"),
            'v' => self.fragment("\u{b}"),
            '0' if rest.is_empty() => self.fragment("\0"),
            '0'..='9' => return Err(refuse()),
            'x' if rest.len() == 2 => self.push_point(unit(rest)?, node, source)?,
            'u' => match rest.strip_prefix('{').and_then(|hex| hex.strip_suffix('}')) {
                Some(hex) => self.push_point(unit(hex)?, node, source)?,
                None if rest.len() == 4 => {
                    self.0
                        .push(u16::try_from(unit(rest)?).map_err(|_| refuse())?);
                }
                None => return Err(refuse()),
            },
            _ if rest.is_empty() => self.fragment(escaped),
            _ => return Err(refuse()),
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn finished(self, node: Node<'_>) -> Translation<String> {
        String::from_utf16(&self.0).map_err(|_| Untranslated {
            construct: Construct::Syntax(LONE_SURROGATE.to_owned()),
            line: line(node),
        })
    }

    fn push_point(&mut self, point: u32, node: Node<'_>, source: &str) -> Translation<()> {
        let character = char::from_u32(point).ok_or_else(|| refused(node, source))?;
        self.fragment(character.encode_utf8(&mut [0; 4]));
        Ok(())
    }
}

/// A string literal's value, its escapes decoded.
pub(super) fn decoded(node: Node<'_>, source: &str) -> Translation<String> {
    Parts::read(node, source, |parts| {
        let mut text = Text::default();
        for child in parts.named() {
            match child.kind() {
                STRING_FRAGMENT => text.fragment(&source[child.byte_range()]),
                ESCAPE_SEQUENCE => text.escape(child, source)?,
                _ => return Err(refused(child, source)),
            }
        }
        text.finished(node)
    })
}
