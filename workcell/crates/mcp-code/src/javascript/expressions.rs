//! Expressions: each lowered with what its value is known to be, callbacks lifted or turned into
//! comprehensions, and the proof that every deferred call is consumed.

use std::collections::HashSet;
use std::ops::RangeInclusive;

use tree_sitter::Node;

use super::library::{self, Form, Mapping, Shape};
use super::python::{
    Argument, Builtin, Call, Callee, Clause, Collected, Comparison, Comprehension, Constant, Def,
    Entry, Expression, Gather, Gathered, Import, Item, Logic, Name, Operator, Parameter, Piece,
    Statement, Target, each_statement, walk,
};
use super::scope::{Binding, BindingKind, Frame};
use super::statements::mutated;
use super::syntax::{
    ALTERNATIVE_FIELD, ARGUMENT_FIELD, ARGUMENTS, ARGUMENTS_FIELD, ARRAY, ARRAY_PATTERN,
    ARROW_FUNCTION, AS, AS_EXPRESSION, ASYNC, AWAIT, AWAIT_EXPRESSION, BINARY_EXPRESSION,
    BODY_FIELD, CALL_EXPRESSION, COMMA, COMPUTED_PROPERTY_NAME, CONDITION_FIELD, CONSEQUENCE_FIELD,
    CONST, ESCAPE_SEQUENCE, FALSE, FORMAL_PARAMETERS, FUNCTION_FIELD, IDENTIFIER, INDEX_FIELD,
    KEY_FIELD, LEFT_FIELD, MEMBER_EXPRESSION, NON_NULL_EXPRESSION, NULL, NUMBER, OBJECT,
    OBJECT_FIELD, OPERATOR_FIELD, OPTIONAL_CHAIN_FIELD, OPTIONAL_PARAMETER, OPTIONAL_TOKEN, PAIR,
    PARAMETER_FIELD, PARAMETERS_FIELD, PARENTHESIZED_EXPRESSION, PATTERN_FIELD, PROPERTY_FIELD,
    PROPERTY_IDENTIFIER, Parts, REQUIRED_PARAMETER, RETURN_TYPE_FIELD, RIGHT_FIELD, SATISFIES,
    SATISFIES_EXPRESSION, SHORTHAND_PROPERTY_IDENTIFIER, SPREAD_ELEMENT, STATEMENT_BLOCK, STRING,
    STRING_FRAGMENT, SUBSCRIPT_EXPRESSION, TEMPLATE_STRING, TEMPLATE_SUBSTITUTION,
    TERNARY_EXPRESSION, TRUE, TYPE_ARGUMENTS_FIELD, TYPE_FIELD, TYPE_PARAMETERS_FIELD, Text,
    UNARY_EXPRESSION, UNDEFINED, VALUE_FIELD, decoded, line, refused, written,
};
use super::truthiness::{SafeTest, safe_test};
use super::{Construct, Translation, Translator, Untranslated};

const NOT: &str = "!";
const NEGATE: &str = "-";
const AND: &str = "&&";
const OR: &str = "||";
const NULLISH: &str = "??";
const EQUALITIES: [&str; 2] = ["===", "=="];
const INEQUALITIES: [&str; 2] = ["!==", "!="];
const STRICT: [&str; 2] = ["===", "!=="];
const ORDERINGS: [(&str, Comparison); 4] = [
    ("<", Comparison::Lt),
    (">", Comparison::Gt),
    ("<=", Comparison::LtE),
    (">=", Comparison::GtE),
];
const ARITHMETIC: [(&str, Operator); 5] = [
    ("+", Operator::Add),
    ("-", Operator::Sub),
    ("*", Operator::Mult),
    ("/", Operator::Div),
    ("%", Operator::Mod),
];
const LENGTH: &str = "length";
const GET: &str = "get";
const VALUES: &str = "values";
const ITEMS: &str = "items";
const JOIN: &str = "join";
const REPLACE: &str = "replace";
const SPLIT: &str = "split";
const DUMPS: &str = "dumps";
const INDENT: &str = "indent";
const SEPARATORS: &str = "separators";
const COMPACT_SEPARATORS: [&str; 2] = [",", ":"];
const ENSURE_ASCII: &str = "ensure_ascii";
/// The widths `JSON.stringify()` indents by as written; it cuts a wider one to 10.
const MAX_INDENT: RangeInclusive<u64> = 1..=10;
const PERCENT: char = '%';
const LIST: &str = "list";
const DEFAULT_SEPARATOR: &str = ",";
/// What `trim()` removes: JavaScript's white space and line terminators, where Python's `strip()`
/// also takes `\x1c`-`\x1f` and `\x85` but leaves `\ufeff`.
const JAVASCRIPT_WHITESPACE: &str = "\t\n\u{b}\u{c}\r \u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
const DECIMAL_RADIX: &str = "10";
const ONE: &str = "1";
const REPLACEMENT_TOKEN: char = '$';
const RADIXES: [(&str, u32); 3] = [("0x", 16), ("0o", 8), ("0b", 2)];
const DIGIT_SEPARATOR: char = '_';
const MAX_EXACT_WHOLE: u64 = 1 << 53;
const BIGINT_SUFFIX: char = 'n';
const FRACTION_MARKS: [char; 3] = ['.', 'e', 'E'];
const NON_NULL: &str = "!";
const GATHER_CALL: &str = "Promise.all()";
const MAX_CALLBACK_PARAMETERS: usize = 2;
const ENTRY_NAMES: usize = 2;

const OR_VALUE: &str = "`||` outside a condition";
const UNDECLARED: &str = "the undeclared name";
const AS_VALUE: &str = "used as a value";
const AWAIT_VALUE: &str = "`await` on something other than a call";
const AWAIT_HERE: &str = "`await` outside an async function";
const AWAIT_IMMEDIATE: &str = "`await` on a call that returns at once, inside a function";
const OPTIONAL_AGAIN: &str = "`?.` after a call";
const OPTIONAL_CALL: &str = "`?.()`";
const NULLISH_AGAIN: &str = "`??` after a call";
const NULLISH_CHAINED: &str = "`??` after another `??`";
const UNDEFINED_OPERAND: &str = "which may be `undefined`, compared or computed";
const NOT_NUMBERS: &str = "of a value not known to be a number";
const NOT_NUMBERS_OR_STRINGS: &str = "of values not known to be two numbers or two strings";
const DIVISOR: &str = "`/` by something other than a number literal that is not zero";
const CHAINED: &str = "a comparison of a comparison";
const UNDEFINED_VALUE: &str = "`undefined` as a value";
const LOOSE_EQUALITY: &str = "`==` or `!=` against something other than `null`";
const STRICT_NULL: &str = "`===` or `!==` against `null` or `undefined`";
const STRICT_UNKNOWN: &str =
    "`===` or `!==` with no string, number or boolean literal on either side";
const BOOLEAN_NUMBER: &str = "`===` or `!==` of `0` or `1` and a value that may be a boolean";
const REORDERED: &str = "with calls both in what it is called on and in its argument";
const INCLUDED: &str = "`.includes()` of something other than a string";
const CONCATENATED: &str =
    "`.concat()` of something other than an array, or of a string by other than strings";
const NOT_WHOLE_NUMBERS: &str = "of values not known to be whole numbers";
const LENGTHLESS: &str = "`.length` of a number, a boolean or an object";
const OBJECT_TEXT: &str = "an object written into a string";
const FORMAT: &str = "with a `%` format";
const SPREAD_OBJECT: &str = "`...` of an object in an array";
const CALLBACK_MUTATION: &str = "a callback that pushes or assigns a property";
const INDEX: &str = "the index";
const MODULO: &str = "`%` on a value that may be negative";
const KEY: &str = "the computed key";
const HOLE: &str = "a hole in an array";
const NUMBER_LITERAL: &str = "the number";
const SPREAD: &str = "`...` in this call";
const STATEMENT_ONLY: &str = "inside an expression";
const ASYNC_CALLBACK: &str = "an `async` callback to";
const NESTED_CALLBACK: &str = "a callback with a body inside a one-line callback";
const DESTRUCTURED_ITEM: &str = "a destructured parameter here";
const FLATTENED: &str = "`.flatMap()` of a value that may not be an array";
const REPLACEMENT: &str = "a replacement that is not a plain string";
const SUBSTRING: &str = "`.substring()` bounds that may be negative or reversed";
const AT: &str = "`.at()` of something other than a whole number";
const ASSIGN_TARGET: &str = "`Object.assign()` onto an object other than `{}`";
const REPLACER: &str = "`JSON.stringify()` with a replacer";
const INDENT_WIDTH: &str = "a `JSON.stringify()` indent other than a whole number from 1 to 10";
const SPLIT_SEPARATOR: &str = "`.split()` by something other than a string literal";
const GATHER_PLACE: &str = "`await Promise.all()` inside a larger expression or statement";
const GATHERED_ARGUMENT: &str =
    "gathered with an argument other than a literal, a name, or a read of the loop's own item";
const ENTRY_PATTERN: &str = "an array pattern other than `[key, value]` over `Object.entries()`";
const KEYWORDS_SPREAD: &str =
    "`...`, a computed key or a repeated key in the object a tool is called with";
const RADIX: &str = "`parseInt()` with a radix other than 10";
const ONE_VALUE: &str = "of one value";
const ARGUMENT_COUNT: &str = "with this many arguments";
const DEFAULT: &str = "a parameter default that is not a literal";
const GATHERED: &str =
    "`Promise.all()` of something other than calls written in it or pushed just before it";

/// What a lowered value is known to be, which decides where it may stand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Fact {
    #[default]
    Unknown,
    /// May be `undefined`: read through `?.`, or `find`'s result.
    Optional,
    Bool,
    /// A number that may be fractional, infinite or NaN.
    Number,
    /// A whole number that may be negative: never NaN, so safe to test.
    Integer,
    /// A whole number that is never negative: a safe index, and safe on both sides of `%`.
    Count,
    Str,
    /// An object literal, which JavaScript neither iterates, sizes, nor writes out as Python does.
    Object,
}

impl Fact {
    pub(super) fn is_numeric(self) -> bool {
        matches!(self, Self::Number | Self::Integer | Self::Count)
    }

    pub(super) fn is_whole(self) -> bool {
        matches!(self, Self::Integer | Self::Count)
    }

    fn merged(self, other: Self) -> Self {
        if self == other {
            self
        } else if self == Self::Optional || other == Self::Optional {
            Self::Optional
        } else if self.is_whole() && other.is_whole() {
            Self::Integer
        } else if self.is_numeric() && other.is_numeric() {
            Self::Number
        } else {
            Self::Unknown
        }
    }

    /// What survives a member read on a value of this kind: only the taint.
    fn member(self) -> Self {
        if self == Self::Optional {
            Self::Optional
        } else {
            Self::Unknown
        }
    }
}

pub(super) struct Lowered {
    pub(super) expression: Expression,
    pub(super) fact: Fact,
}

impl Lowered {
    pub(super) fn new(expression: Expression, fact: Fact) -> Self {
        Self { expression, fact }
    }
}

/// A member chain lowered so far: the receivers `?.` checks, and the value read past them.
pub(super) struct Chain {
    checks: Vec<Expression>,
    lowered: Lowered,
}

impl Chain {
    fn plain(lowered: Lowered) -> Self {
        Self {
            checks: Vec::new(),
            lowered,
        }
    }

    /// Short-circuits the rest of the chain when the value read so far is `null`.
    fn check(&mut self, node: Node<'_>) -> Translation<()> {
        if !self.lowered.expression.is_repeatable() {
            return Err(syntax(OPTIONAL_AGAIN, node));
        }
        self.checks.push(self.lowered.expression.clone());
        Ok(())
    }

    pub(super) fn closed(self) -> Lowered {
        if self.checks.is_empty() {
            return self.lowered;
        }
        Lowered::new(
            Expression::Guarded {
                checks: self.checks,
                value: Box::new(self.lowered.expression),
                fallback: Box::new(Expression::Constant(Constant::None)),
            },
            Fact::Optional,
        )
    }
}

/// A call lowered where it stands: a value in a chain, or a statement only a statement may be.
pub(super) enum Called {
    Chain(Chain),
    Statement(Statement),
}

/// An arrow function given to an array method.
struct Callback {
    /// What each item binds: the parameter, or a destructured pair.
    item: Target,
    /// The second parameter, the item's index.
    index: Option<Name>,
    body: Lowered,
}

impl Callback {
    /// `for item in receiver`, or `for index, item in enumerate(receiver)`.
    fn clause(&self, receiver: Expression, node: Node<'_>) -> Translation<Clause> {
        let Some(index) = &self.index else {
            return Ok(Clause::For {
                target: self.item.clone(),
                iterable: receiver,
            });
        };
        let Target::Name(item) = &self.item else {
            return Err(syntax(DESTRUCTURED_ITEM, node));
        };
        Ok(Clause::For {
            target: Target::Tuple(vec![index.clone(), item.clone()]),
            iterable: Expression::call(
                Callee::Builtin(Builtin::Enumerate),
                vec![receiver],
                line(node),
            ),
        })
    }

    fn item_name(&self, node: Node<'_>) -> Translation<Name> {
        match &self.item {
            Target::Name(name) => Ok(name.clone()),
            Target::Tuple(_) | Target::Subscript { .. } => Err(syntax(DESTRUCTURED_ITEM, node)),
        }
    }
}

pub(super) fn syntax(named: impl Into<String>, node: Node<'_>) -> Untranslated {
    Untranslated {
        construct: Construct::Syntax(named.into()),
        line: line(node),
    }
}

impl Translator<'_> {
    pub(super) fn value(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        match node.kind() {
            IDENTIFIER => self.identifier(node),
            NUMBER => number(node, source),
            STRING => Ok(Lowered::new(
                Expression::string(decoded(node, source)?),
                Fact::Str,
            )),
            TEMPLATE_STRING => self.template(node),
            TRUE => Ok(Lowered::new(
                Expression::Constant(Constant::True),
                Fact::Bool,
            )),
            FALSE => Ok(Lowered::new(
                Expression::Constant(Constant::False),
                Fact::Bool,
            )),
            NULL | UNDEFINED => Ok(Lowered::new(
                Expression::Constant(Constant::None),
                Fact::Unknown,
            )),
            ARRAY => self.array(node),
            OBJECT => self.object(node),
            MEMBER_EXPRESSION | SUBSCRIPT_EXPRESSION | CALL_EXPRESSION | NON_NULL_EXPRESSION => {
                Ok(self.chain(node)?.closed())
            }
            AWAIT_EXPRESSION => self.awaited(node),
            UNARY_EXPRESSION => self.unary(node),
            BINARY_EXPRESSION => self.binary(node),
            TERNARY_EXPRESSION => self.ternary(node),
            PARENTHESIZED_EXPRESSION => Parts::read(node, source, |parts| {
                parts.only().and_then(|inner| self.value(inner))
            }),
            AS_EXPRESSION | SATISFIES_EXPRESSION => self.typed(node),
            _ => Err(refused(node, source)),
        }
    }

    /// `node` as a condition: `!`, `&&` and `||` over conditions, or a value safe to test.
    pub(super) fn test(&mut self, node: Node<'_>) -> Translation<SafeTest> {
        let source = self.source;
        let operator = node
            .child_by_field_name(OPERATOR_FIELD)
            .map(|operator| operator.kind());
        match (node.kind(), operator) {
            (PARENTHESIZED_EXPRESSION, _) => Parts::read(node, source, |parts| {
                parts.only().and_then(|inner| self.test(inner))
            }),
            (UNARY_EXPRESSION, Some(NOT)) => Parts::read(node, source, |parts| {
                parts.required(OPERATOR_FIELD)?;
                let argument = parts.required(ARGUMENT_FIELD)?;
                Ok(SafeTest::not(self.test(argument)?))
            }),
            (BINARY_EXPRESSION, Some(operator @ (AND | OR))) => {
                Parts::read(node, source, |parts| {
                    let left = parts.required(LEFT_FIELD)?;
                    parts.required(OPERATOR_FIELD)?;
                    let right = parts.required(RIGHT_FIELD)?;
                    let logic = if operator == AND {
                        Logic::And
                    } else {
                        Logic::Or
                    };
                    Ok(SafeTest::logical(
                        self.test(left)?,
                        logic,
                        self.test(right)?,
                    ))
                })
            }
            _ => {
                let lowered = self.value(node)?;
                safe_test(lowered, &written(node, source), line(node))
            }
        }
    }

    fn identifier(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let text = &self.source[node.byte_range()];
        match self.scopes.resolve(text) {
            Some(Binding {
                kind: BindingKind::Function { .. },
                ..
            }) => Err(syntax(format!("`{text}` {AS_VALUE}"), node)),
            Some(binding) => {
                let fact = binding.fact;
                let name = Name::parse(text).ok_or_else(|| refused(node, self.source))?;
                Ok(Lowered::new(Expression::Name(name), fact))
            }
            None if self.stubs.is_host(text)
                || library::is_global_object(text)
                || library::global(text).is_some() =>
            {
                Err(syntax(format!("`{text}` {AS_VALUE}"), node))
            }
            None => Err(syntax(format!("{UNDECLARED} `{text}`"), node)),
        }
    }

    fn template(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let mut pieces = Vec::new();
            let mut text = Text::default();
            for child in parts.named() {
                match child.kind() {
                    STRING_FRAGMENT => text.fragment(&source[child.byte_range()]),
                    ESCAPE_SEQUENCE => text.escape(child, source)?,
                    TEMPLATE_SUBSTITUTION => {
                        if !text.is_empty() {
                            pieces.push(Piece::Text(std::mem::take(&mut text).finished(node)?));
                        }
                        let inner = Parts::read(child, source, |parts| parts.only())?;
                        let value = self.value(inner)?;
                        if value.fact == Fact::Object {
                            return Err(syntax(OBJECT_TEXT, child));
                        }
                        pieces.push(Piece::Value(value.expression));
                    }
                    _ => return Err(refused(child, source)),
                }
            }
            if !text.is_empty() {
                pieces.push(Piece::Text(text.finished(node)?));
            }
            Ok(Lowered::new(formatted(pieces), Fact::Str))
        })
    }

    fn array(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let mut items = Vec::new();
            let mut fact = Fact::Unknown;
            let mut expecting = true;
            for part in parts.sequence() {
                match part.kind() {
                    COMMA if expecting => return Err(syntax(HOLE, part)),
                    COMMA => expecting = true,
                    SPREAD_ELEMENT => {
                        let spread = self.spread(part)?;
                        if spread.fact == Fact::Object {
                            return Err(syntax(SPREAD_OBJECT, part));
                        }
                        items.push(Item::Spread(spread.expression));
                        expecting = false;
                    }
                    _ if part.is_named() => {
                        let lowered = self.value(part)?;
                        fact = fact.merged(lowered.fact).member();
                        items.push(Item::One(lowered.expression));
                        expecting = false;
                    }
                    _ => {}
                }
            }
            Ok(Lowered::new(Expression::List(items), fact))
        })
    }

    fn spread(&mut self, node: Node<'_>) -> Translation<Lowered> {
        Parts::read(node, self.source, |parts| {
            parts.only().and_then(|inner| self.value(inner))
        })
    }

    fn object(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let mut entries = Vec::new();
            let mut fact = Fact::Object;
            for child in parts.named() {
                match child.kind() {
                    PAIR => {
                        let (key, value) = Parts::read(child, source, |pair| {
                            let key = pair.required(KEY_FIELD)?;
                            let value = pair.required(VALUE_FIELD)?;
                            Ok((self.key(key)?, self.defined(value)?))
                        })?;
                        if value.fact == Fact::Optional {
                            fact = Fact::Optional;
                        }
                        entries.push(Entry::Pair(key, value.expression));
                    }
                    SHORTHAND_PROPERTY_IDENTIFIER => {
                        let value = self.identifier(child)?;
                        if value.fact == Fact::Optional {
                            fact = Fact::Optional;
                        }
                        entries.push(Entry::Pair(
                            Expression::string(&source[child.byte_range()]),
                            value.expression,
                        ));
                    }
                    SPREAD_ELEMENT => entries.push(Entry::Spread(self.spread(child)?.expression)),
                    _ => return Err(refused(child, source)),
                }
            }
            Ok(Lowered::new(Expression::Dict(entries), fact))
        })
    }

    /// An object key as the string JavaScript makes of it.
    fn key(&mut self, node: Node<'_>) -> Translation<Expression> {
        let source = self.source;
        match node.kind() {
            PROPERTY_IDENTIFIER => Ok(Expression::string(&source[node.byte_range()])),
            STRING => Ok(Expression::string(decoded(node, source)?)),
            NUMBER
                if number(node, source)?.fact == Fact::Count
                    && node
                        .utf8_text(source.as_bytes())
                        .is_ok_and(|text| text.bytes().all(|byte| byte.is_ascii_digit())) =>
            {
                Ok(Expression::string(&source[node.byte_range()]))
            }
            COMPUTED_PROPERTY_NAME => Parts::read(node, source, |parts| {
                let inner = parts.only()?;
                let key = self.value(inner)?;
                if key.fact != Fact::Str {
                    return Err(syntax(format!("{KEY} `{}`", written(inner, source)), inner));
                }
                Ok(key.expression)
            }),
            _ => Err(refused(node, source)),
        }
    }

    fn awaited(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            parts.token(AWAIT);
            let inner = parts.only()?;
            if !self.scopes.awaits() {
                return Err(syntax(AWAIT_HERE, node));
            }
            let lowered = self.value(inner)?;
            let awaitable = match &lowered.expression {
                Expression::Call(Call {
                    callee:
                        Callee::Host {
                            deferred: false, ..
                        },
                    ..
                }) if self.scopes.frame() != Frame::Module => {
                    return Err(syntax(AWAIT_IMMEDIATE, node));
                }
                Expression::Call(call) => {
                    call.is_deferred() || matches!(call.callee, Callee::Host { .. })
                }
                Expression::Gather(_) => true,
                _ => false,
            };
            if !awaitable {
                return Err(syntax(AWAIT_VALUE, node));
            }
            Ok(Lowered::new(
                Expression::Await(Box::new(lowered.expression)),
                Fact::Unknown,
            ))
        })
    }

    fn unary(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let operator = parts.required(OPERATOR_FIELD)?;
            let argument = parts.required(ARGUMENT_FIELD)?;
            match operator.kind() {
                NOT => Ok(Lowered::new(
                    Expression::Not(Box::new(self.test(argument)?)),
                    Fact::Bool,
                )),
                NEGATE => {
                    let value = self.operand(argument)?;
                    let fact = match value.fact {
                        fact if fact.is_whole() => Fact::Integer,
                        Fact::Number => Fact::Number,
                        _ => return Err(syntax(format!("`{NEGATE}` {NOT_NUMBERS}"), operator)),
                    };
                    Ok(Lowered::new(
                        Expression::Negative(Box::new(value.expression)),
                        fact,
                    ))
                }
                _ => Err(refused(operator, source)),
            }
        })
    }

    /// An operand of a comparison or arithmetic, which `undefined` would not survive alike.
    pub(super) fn operand(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let lowered = self.value(node)?;
        if lowered.fact == Fact::Optional {
            return Err(syntax(
                format!("`{}`, {UNDEFINED_OPERAND}", written(node, self.source)),
                node,
            ));
        }
        Ok(lowered)
    }

    /// Both operands of arithmetic or an ordering, which must be known numbers: where JavaScript
    /// converts a string, `null` or a list, Python raises or compares otherwise.
    fn numbers(
        &mut self,
        left: Node<'_>,
        right: Node<'_>,
        operator: Node<'_>,
    ) -> Translation<(Lowered, Lowered)> {
        let left = self.operand(left)?;
        let right = self.operand(right)?;
        if !left.fact.is_numeric() || !right.fact.is_numeric() {
            return Err(syntax(
                format!("`{}` {NOT_NUMBERS}", operator.kind()),
                operator,
            ));
        }
        Ok((left, right))
    }

    /// A value in an object literal or an argument, which `undefined` may not be: JavaScript
    /// drops the key where Python passes `None`.
    fn defined(&mut self, node: Node<'_>) -> Translation<Lowered> {
        if node.kind() == UNDEFINED {
            return Err(syntax(UNDEFINED_VALUE, node));
        }
        self.value(node)
    }

    fn binary(&mut self, node: Node<'_>) -> Translation<Lowered> {
        let source = self.source;
        if node
            .child_by_field_name(OPERATOR_FIELD)
            .is_some_and(|operator| operator.kind() == AND)
        {
            return Ok(Lowered::new(self.test(node)?.into_expression(), Fact::Bool));
        }
        Parts::read(node, source, |parts| {
            let left = parts.required(LEFT_FIELD)?;
            let operator = parts.required(OPERATOR_FIELD)?;
            let right = parts.required(RIGHT_FIELD)?;
            let symbol = operator.kind();
            if symbol == OR {
                return Err(syntax(OR_VALUE, operator));
            }
            if symbol == NULLISH {
                return self.nullish(left, right, operator);
            }
            let equality = EQUALITIES.contains(&symbol) || INEQUALITIES.contains(&symbol);
            let ordering = ORDERINGS.iter().find(|(written, _)| *written == symbol);
            if (equality || ordering.is_some()) && (is_comparison(left) || is_comparison(right)) {
                return Err(syntax(CHAINED, operator));
            }
            if equality {
                return self.equality(
                    left,
                    right,
                    INEQUALITIES.contains(&symbol),
                    STRICT.contains(&symbol),
                    operator,
                );
            }
            if let Some((_, comparison)) = ordering {
                let (left, right) = self.numbers(left, right, operator)?;
                return Ok(Lowered::new(
                    Expression::Compare {
                        left: Box::new(left.expression),
                        comparison: *comparison,
                        right: Box::new(right.expression),
                    },
                    Fact::Bool,
                ));
            }
            let Some((_, operator_kind)) =
                ARITHMETIC.iter().find(|(written, _)| *written == symbol)
            else {
                return Err(refused(operator, source));
            };
            let left = self.operand(left)?;
            let right = self.operand(right)?;
            let fact = arithmetic(left.fact, *operator_kind, &right, operator)?;
            if fact == Fact::Str {
                let pieces = [left.expression, right.expression]
                    .into_iter()
                    .flat_map(pieces)
                    .collect();
                return Ok(Lowered::new(formatted(pieces), Fact::Str));
            }
            Ok(Lowered::new(
                Expression::Binary {
                    left: Box::new(left.expression),
                    operator: *operator_kind,
                    right: Box::new(right.expression),
                },
                fact,
            ))
        })
    }

    /// `a ?? b`, where `a` is read twice and so must not call anything.
    fn nullish(
        &mut self,
        left: Node<'_>,
        right: Node<'_>,
        operator: Node<'_>,
    ) -> Translation<Lowered> {
        let mut chain = self.chain(left)?;
        if !chain.lowered.expression.is_repeatable() {
            let chained = left.kind() == BINARY_EXPRESSION
                && left
                    .child_by_field_name(OPERATOR_FIELD)
                    .is_some_and(|inner| inner.kind() == NULLISH);
            let named = if chained {
                NULLISH_CHAINED
            } else {
                NULLISH_AGAIN
            };
            return Err(syntax(named, operator));
        }
        let value = lenient(chain.lowered.expression);
        chain.checks.push(value.clone());
        let fallback = self.value(right)?;
        let fact = match chain.lowered.fact {
            Fact::Optional => Fact::Unknown,
            known if known == fallback.fact => known,
            _ => Fact::Unknown,
        };
        Ok(Lowered::new(
            Expression::Guarded {
                checks: chain.checks,
                value: Box::new(value),
                fallback: Box::new(fallback.expression),
            },
            fact,
        ))
    }

    /// `a == null` as `a is None`, which holds for a missing key as `== null` does for
    /// `undefined`; `a === true` as `a is True`; and `a === "x"` or `a === 1` as `==`, which no
    /// list, object or boolean on the other side can make hold. Every other equality refuses.
    fn equality(
        &mut self,
        left: Node<'_>,
        right: Node<'_>,
        negated: bool,
        strict: bool,
        operator: Node<'_>,
    ) -> Translation<Lowered> {
        let is_nullish = |node: Node<'_>| matches!(node.kind(), NULL | UNDEFINED);
        let is_boolean = |node: Node<'_>| matches!(node.kind(), TRUE | FALSE);
        let (identity, equal) = if negated {
            (Comparison::IsNot, Comparison::NotEq)
        } else {
            (Comparison::Is, Comparison::Eq)
        };
        let nullish = is_nullish(left) || is_nullish(right);
        let (left, comparison, right) = match (strict, nullish) {
            (false, true) => {
                let value = if is_nullish(right) { left } else { right };
                (
                    lenient(self.value(value)?.expression),
                    identity,
                    Expression::Constant(Constant::None),
                )
            }
            (false, false) => return Err(syntax(LOOSE_EQUALITY, operator)),
            (true, true) => return Err(syntax(STRICT_NULL, operator)),
            (true, false) => {
                let left_value = self.value(left)?;
                let right_value = self.value(right)?;
                let boolean = is_boolean(left) || is_boolean(right);
                let literal = is_scalar_literal(left) || is_scalar_literal(right);
                let may_be_boolean = |number: &Lowered, other: &Lowered| {
                    equals_a_boolean(&number.expression) && !other.fact.is_numeric()
                };
                let comparison = match (boolean, literal) {
                    (true, false) => identity,
                    (false, true)
                        if may_be_boolean(&left_value, &right_value)
                            || may_be_boolean(&right_value, &left_value) =>
                    {
                        return Err(syntax(BOOLEAN_NUMBER, operator));
                    }
                    (false, true) => equal,
                    _ => return Err(syntax(STRICT_UNKNOWN, operator)),
                };
                (left_value.expression, comparison, right_value.expression)
            }
        };
        Ok(Lowered::new(
            Expression::Compare {
                left: Box::new(left),
                comparison,
                right: Box::new(right),
            },
            Fact::Bool,
        ))
    }

    fn ternary(&mut self, node: Node<'_>) -> Translation<Lowered> {
        Parts::read(node, self.source, |parts| {
            let condition = parts.required(CONDITION_FIELD)?;
            let consequence = parts.required(CONSEQUENCE_FIELD)?;
            let alternative = parts.required(ALTERNATIVE_FIELD)?;
            let test = self.test(condition)?;
            let body = self.value(consequence)?;
            let orelse = self.value(alternative)?;
            Ok(Lowered::new(
                Expression::IfElse {
                    test: Box::new(test),
                    body: Box::new(body.expression),
                    orelse: Box::new(orelse.expression),
                },
                body.fact.merged(orelse.fact),
            ))
        })
    }

    /// `x as T`, `x as const` and `x satisfies T`, whose type the admission already passed over.
    fn typed(&mut self, node: Node<'_>) -> Translation<Lowered> {
        Parts::read(node, self.source, |parts| {
            parts.token(AS);
            parts.token(SATISFIES);
            let constant = parts.token(CONST);
            match parts.named()[..] {
                [expression, _] => self.value(expression),
                [expression] if constant => self.value(expression),
                _ => Err(parts.refused()),
            }
        })
    }

    pub(super) fn chain(&mut self, node: Node<'_>) -> Translation<Chain> {
        let source = self.source;
        match node.kind() {
            MEMBER_EXPRESSION => self.member(node),
            SUBSCRIPT_EXPRESSION => self.subscript(node),
            CALL_EXPRESSION => match self.call(node, false)? {
                Called::Chain(chain) => Ok(chain),
                Called::Statement(_) => Err(refused(node, source)),
            },
            NON_NULL_EXPRESSION => Parts::read(node, source, |parts| {
                parts.token(NON_NULL);
                parts.only().and_then(|inner| self.chain(inner))
            }),
            _ => Ok(Chain::plain(self.value(node)?)),
        }
    }

    fn member(&mut self, node: Node<'_>) -> Translation<Chain> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let object = parts.required(OBJECT_FIELD)?;
            let optional = parts.field(OPTIONAL_CHAIN_FIELD).is_some();
            let property = parts.required(PROPERTY_FIELD)?;
            let name = &source[property.byte_range()];
            let global = &source[object.byte_range()];
            if object.kind() == IDENTIFIER
                && self.scopes.resolve(global).is_none()
                && library::is_global_object(global)
            {
                return Err(syntax(format!("`{global}.{name}`"), node));
            }
            let mut chain = self.chain(object)?;
            if optional {
                chain.check(node)?;
            }
            let receiver = std::mem::replace(
                &mut chain.lowered,
                Lowered::new(Expression::Constant(Constant::None), Fact::Unknown),
            );
            chain.lowered = match name {
                LENGTH
                    if receiver.fact.is_numeric()
                        || matches!(receiver.fact, Fact::Bool | Fact::Object) =>
                {
                    return Err(syntax(LENGTHLESS, node));
                }
                LENGTH => Lowered::new(
                    Expression::call(
                        Callee::Builtin(Builtin::Len),
                        vec![receiver.expression],
                        line(node),
                    ),
                    if receiver.fact == Fact::Optional {
                        Fact::Optional
                    } else {
                        Fact::Count
                    },
                ),
                _ => Lowered::new(
                    Expression::Subscript {
                        value: Box::new(receiver.expression),
                        index: Box::new(Expression::string(name)),
                    },
                    receiver.fact.member(),
                ),
            };
            Ok(chain)
        })
    }

    fn subscript(&mut self, node: Node<'_>) -> Translation<Chain> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let object = parts.required(OBJECT_FIELD)?;
            let optional = parts.field(OPTIONAL_CHAIN_FIELD).is_some();
            let index = parts.required(INDEX_FIELD)?;
            let mut chain = self.chain(object)?;
            if optional {
                chain.check(node)?;
            }
            let key = self.value(index)?;
            if !matches!(key.fact, Fact::Count | Fact::Str) && !is_last_index(&key.expression) {
                return Err(syntax(
                    format!("{INDEX} `{}`", written(index, source)),
                    index,
                ));
            }
            let receiver = std::mem::replace(
                &mut chain.lowered,
                Lowered::new(Expression::Constant(Constant::None), Fact::Unknown),
            );
            chain.lowered = Lowered::new(
                Expression::Subscript {
                    value: Box::new(receiver.expression),
                    index: Box::new(key.expression),
                },
                receiver.fact.member(),
            );
            Ok(chain)
        })
    }

    /// A call, `statement` when nothing reads its value, so `push` and `forEach` may stand.
    pub(super) fn call(&mut self, node: Node<'_>, statement: bool) -> Translation<Called> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            if parts.token(OPTIONAL_TOKEN) {
                return Err(syntax(OPTIONAL_CALL, node));
            }
            let function = parts.required(FUNCTION_FIELD)?;
            parts.field(TYPE_ARGUMENTS_FIELD);
            let arguments = parts.required(ARGUMENTS_FIELD)?;
            if arguments.kind() != ARGUMENTS {
                return Err(refused(arguments, source));
            }
            let arguments = Parts::read(arguments, source, |parts| Ok(parts.named()))?;
            match function.kind() {
                IDENTIFIER => self.function_call(function, &arguments, node),
                MEMBER_EXPRESSION => self.member_call(function, &arguments, node, statement),
                _ => Err(refused(function, source)),
            }
        })
    }

    fn function_call(
        &mut self,
        function: Node<'_>,
        arguments: &[Node<'_>],
        call: Node<'_>,
    ) -> Translation<Called> {
        let source = self.source;
        let written = &source[function.byte_range()];
        let callee = match self.scopes.resolve(written).map(|binding| binding.kind) {
            Some(BindingKind::Function { is_async }) => Callee::Local {
                name: Name::parse(written).ok_or_else(|| refused(function, source))?,
                is_async,
            },
            Some(_) => return Err(syntax(format!("`{written}()`"), function)),
            None if self.stubs.is_host(written) && !self.stubs.is_namespace(written) => {
                return self.host_call(None, written, function, arguments, call);
            }
            None => {
                return match library::global(written) {
                    Some(mapping) => self.mapped(mapping, None, arguments, call, false),
                    None => Err(syntax(format!("`{written}()`"), function)),
                };
            }
        };
        let arguments = self.arguments(arguments, true)?;
        Ok(Called::Chain(Chain::plain(Lowered::new(
            Expression::Call(Call {
                callee,
                arguments,
                line: line(call),
            }),
            Fact::Unknown,
        ))))
    }

    fn member_call(
        &mut self,
        member: Node<'_>,
        arguments: &[Node<'_>],
        call: Node<'_>,
        statement: bool,
    ) -> Translation<Called> {
        let source = self.source;
        Parts::read(member, source, |parts| {
            let object = parts.required(OBJECT_FIELD)?;
            let optional = parts.field(OPTIONAL_CHAIN_FIELD).is_some();
            let property = parts.required(PROPERTY_FIELD)?;
            let method = &source[property.byte_range()];
            if object.kind() == IDENTIFIER && !optional {
                let receiver = &source[object.byte_range()];
                if self.stubs.is_namespace(receiver) {
                    return self.host_call(Some(receiver), method, property, arguments, call);
                }
                if self.scopes.resolve(receiver).is_none() && library::is_global_object(receiver) {
                    return match library::global(&format!("{receiver}.{method}")) {
                        Some(mapping) => self.mapped(mapping, None, arguments, call, statement),
                        None => Err(syntax(format!("`{receiver}.{method}()`"), property)),
                    };
                }
            }
            let mut chain = self.chain(object)?;
            if optional {
                chain.check(member)?;
            }
            let mapping = library::method(method)
                .ok_or_else(|| syntax(format!("`.{method}()`"), property))?;
            let receiver = std::mem::replace(
                &mut chain.lowered,
                Lowered::new(Expression::Constant(Constant::None), Fact::Unknown),
            );
            match self.mapped(mapping, Some(receiver), arguments, call, statement)? {
                Called::Chain(inner) => {
                    chain.checks.extend(inner.checks);
                    chain.lowered = inner.lowered;
                    Ok(Called::Chain(chain))
                }
                Called::Statement(lowered) if chain.checks.is_empty() => {
                    Ok(Called::Statement(lowered))
                }
                Called::Statement(_) => Err(syntax(OPTIONAL_AGAIN, member)),
            }
        })
    }

    fn host_call(
        &mut self,
        namespace: Option<&str>,
        function: &str,
        node: Node<'_>,
        arguments: &[Node<'_>],
        call: Node<'_>,
    ) -> Translation<Called> {
        let source = self.source;
        let qualified = match namespace {
            Some(namespace) => format!("{namespace}.{function}"),
            None => function.to_owned(),
        };
        let name = |written: &str| Name::parse(written).ok_or_else(|| refused(node, source));
        let callee = Callee::Host {
            namespace: namespace.map(name).transpose()?,
            name: name(function)?,
            deferred: !self.stubs.is_immediate(&qualified),
        };
        let arguments = match arguments {
            [object] if object.kind() == OBJECT => {
                let lowered = self.object(*object)?;
                keywords(lowered.expression).ok_or_else(|| syntax(KEYWORDS_SPREAD, *object))?
            }
            _ => self.arguments(arguments, true)?,
        };
        Ok(Called::Chain(Chain::plain(Lowered::new(
            Expression::Call(Call {
                callee,
                arguments,
                line: line(call),
            }),
            Fact::Unknown,
        ))))
    }

    fn arguments(&mut self, nodes: &[Node<'_>], spreads: bool) -> Translation<Vec<Argument>> {
        nodes
            .iter()
            .map(|node| match node.kind() {
                SPREAD_ELEMENT if spreads => Ok(Argument::Spread(self.spread(*node)?.expression)),
                SPREAD_ELEMENT => Err(syntax(SPREAD, *node)),
                _ => Ok(Argument::Positional(self.defined(*node)?.expression)),
            })
            .collect()
    }

    fn positional(&mut self, nodes: &[Node<'_>]) -> Translation<Vec<Lowered>> {
        nodes
            .iter()
            .map(|node| match node.kind() {
                SPREAD_ELEMENT => Err(syntax(SPREAD, *node)),
                _ => self.defined(*node),
            })
            .collect()
    }

    fn mapped(
        &mut self,
        mapping: &Mapping,
        receiver: Option<Lowered>,
        arguments: &[Node<'_>],
        call: Node<'_>,
        statement: bool,
    ) -> Translation<Called> {
        let source = self.source;
        if !mapping.arity.contains(&arguments.len()) {
            return Err(syntax(
                format!("`{}()` {ARGUMENT_COUNT}", mapping.written),
                call,
            ));
        }
        let at = line(call);
        if let (Form::Builtin(Builtin::Print), Some(first)) = (mapping.form, arguments.first())
            && first.kind() == STRING
            && decoded(*first, source)?.contains(PERCENT)
        {
            return Err(syntax(format!("`{}()` {FORMAT}", mapping.written), *first));
        }
        let expression = match (mapping.form, receiver) {
            (Form::Builtin(builtin), None) => Expression::Call(Call {
                callee: Callee::Builtin(builtin),
                arguments: self.arguments(arguments, builtin == Builtin::Print)?,
                line: at,
            }),
            (Form::Imported(import, function), None) => Expression::Call(Call {
                callee: Callee::Imported(import, function),
                arguments: self.arguments(arguments, false)?,
                line: at,
            }),
            (Form::Method(method), Some(receiver)) => Expression::Call(Call {
                callee: Callee::Method {
                    receiver: Box::new(receiver.expression),
                    name: method,
                },
                arguments: self.arguments(arguments, false)?,
                line: at,
            }),
            (Form::Shaped(shape), receiver) => {
                return self.shaped(shape, mapping, receiver, arguments, call, statement);
            }
            _ => return Err(refused(call, source)),
        };
        Ok(Called::Chain(Chain::plain(Lowered::new(
            expression,
            mapping.returns,
        ))))
    }

    /// The one match over every row whose Python takes its arguments in another shape.
    #[allow(clippy::too_many_lines)]
    fn shaped(
        &mut self,
        shape: Shape,
        mapping: &Mapping,
        receiver: Option<Lowered>,
        arguments: &[Node<'_>],
        call: Node<'_>,
        statement: bool,
    ) -> Translation<Called> {
        let source = self.source;
        let at = line(call);
        let value = |lowered: Lowered| {
            Ok(Called::Chain(Chain::plain(Lowered::new(
                lowered.expression,
                mapping.returns,
            ))))
        };
        let built = |expression: Expression| value(Lowered::new(expression, mapping.returns));
        let builtin = |builtin: Builtin, arguments: Vec<Expression>| {
            Expression::call(Callee::Builtin(builtin), arguments, at)
        };
        let method = |receiver: Expression, name: &'static str, arguments: Vec<Expression>| {
            Expression::call(
                Callee::Method {
                    receiver: Box::new(receiver),
                    name,
                },
                arguments,
                at,
            )
        };
        let statement_only = || syntax(format!("`.{}()` {STATEMENT_ONLY}", mapping.written), call);
        if let Some(receiver) = receiver {
            let receiver_expression = receiver.expression;
            return match shape {
                Shape::Map
                | Shape::Filter
                | Shape::Find
                | Shape::Some
                | Shape::Every
                | Shape::FlatMap => {
                    let lowered = self.iterated(shape, receiver_expression, arguments[0], call)?;
                    value(lowered)
                }
                Shape::ForEach if statement => Ok(Called::Statement(
                    self.for_each(receiver_expression, arguments[0])?,
                )),
                Shape::Push if statement => Ok(Called::Statement(Statement::Append {
                    list: receiver_expression,
                    value: self.positional(arguments)?.remove(0).expression,
                })),
                Shape::ForEach | Shape::Push => Err(statement_only()),
                Shape::Includes => {
                    let needle = self.positional(arguments)?.remove(0);
                    if needle.fact != Fact::Str {
                        return Err(syntax(INCLUDED, call));
                    }
                    if receiver_expression.has_call() && needle.expression.has_call() {
                        return Err(syntax(format!("`.includes()` {REORDERED}"), call));
                    }
                    built(Expression::Compare {
                        left: Box::new(needle.expression),
                        comparison: Comparison::In,
                        right: Box::new(receiver_expression),
                    })
                }
                Shape::Slice => {
                    let mut bounds = self
                        .positional(arguments)?
                        .into_iter()
                        .map(|bound| Box::new(bound.expression));
                    built(Expression::Slice {
                        value: Box::new(receiver_expression),
                        lower: bounds.next(),
                        upper: bounds.next(),
                    })
                }
                Shape::Join => {
                    let separator = match self.positional(arguments)?.pop() {
                        Some(separator) => separator.expression,
                        None => Expression::string(DEFAULT_SEPARATOR),
                    };
                    if receiver_expression.has_call() && separator.has_call() {
                        return Err(syntax(format!("`.join()` {REORDERED}"), call));
                    }
                    let item = Expression::Name(Name::item());
                    let joined = Expression::Comprehension(Box::new(Comprehension {
                        collected: Collected::Generator,
                        element: builtin(Builtin::Str, vec![item]),
                        clauses: vec![Clause::For {
                            target: Target::Name(Name::item()),
                            iterable: receiver_expression,
                        }],
                    }));
                    built(method(separator, JOIN, vec![joined]))
                }
                Shape::Concat => {
                    let parts = self.positional(arguments)?;
                    let joinable = |part: &Lowered| match receiver.fact {
                        Fact::Str => part.fact == Fact::Str,
                        _ => {
                            matches!(part.expression, Expression::List(_))
                                || (part.fact == Fact::Unknown
                                    && !matches!(
                                        part.expression,
                                        Expression::Dict(_) | Expression::Constant(_)
                                    ))
                        }
                    };
                    if !parts.iter().all(joinable) {
                        return Err(syntax(CONCATENATED, call));
                    }
                    let joined = parts.into_iter().fold(receiver_expression, |left, right| {
                        Expression::Binary {
                            left: Box::new(left),
                            operator: Operator::Add,
                            right: Box::new(right.expression),
                        }
                    });
                    built(joined)
                }
                Shape::At => {
                    let index = self.positional(arguments)?.remove(0);
                    if !is_whole(&index.expression) {
                        return Err(syntax(AT, call));
                    }
                    built(Expression::Subscript {
                        value: Box::new(receiver_expression),
                        index: Box::new(index.expression),
                    })
                }
                Shape::Replace | Shape::ReplaceAll => {
                    let replacement = arguments[1];
                    let plain = replacement.kind() == STRING
                        && !decoded(replacement, source)?.contains(REPLACEMENT_TOKEN);
                    if !plain {
                        return Err(syntax(REPLACEMENT, replacement));
                    }
                    let mut replaced: Vec<Expression> = self
                        .positional(arguments)?
                        .into_iter()
                        .map(|argument| argument.expression)
                        .collect();
                    if shape == Shape::Replace {
                        replaced.push(Expression::Constant(Constant::Number(ONE.to_owned())));
                    }
                    built(method(receiver_expression, REPLACE, replaced))
                }
                Shape::Split => {
                    let separator = arguments[0];
                    if separator.kind() != STRING {
                        return Err(syntax(SPLIT_SEPARATOR, separator));
                    }
                    let text = decoded(separator, source)?;
                    built(if text.is_empty() {
                        builtin(Builtin::List, vec![receiver_expression])
                    } else {
                        method(receiver_expression, SPLIT, vec![Expression::string(text)])
                    })
                }
                Shape::Trim(name) => built(method(
                    receiver_expression,
                    name,
                    vec![Expression::string(JAVASCRIPT_WHITESPACE)],
                )),
                Shape::Substring => {
                    let bounds = self.positional(arguments)?;
                    let ordered = match &bounds[..] {
                        [start] => start.fact == Fact::Count,
                        [start, end] => match (&start.expression, &end.expression) {
                            (
                                Expression::Constant(Constant::Number(start)),
                                Expression::Constant(Constant::Number(end)),
                            ) => whole(start)
                                .zip(whole(end))
                                .is_some_and(|(start, end)| start <= end),
                            _ => false,
                        },
                        _ => false,
                    };
                    if !ordered {
                        return Err(syntax(SUBSTRING, call));
                    }
                    let mut bounds = bounds.into_iter().map(|bound| Box::new(bound.expression));
                    built(Expression::Slice {
                        value: Box::new(receiver_expression),
                        lower: bounds.next(),
                        upper: bounds.next(),
                    })
                }
                _ => Err(refused(call, source)),
            };
        }
        match shape {
            Shape::Dumps => {
                let (first, rest) = arguments
                    .split_first()
                    .ok_or_else(|| refused(call, source))?;
                let mut dumped = vec![Argument::Positional(self.defined(*first)?.expression)];
                if let Some(replacer) = rest.first()
                    && !matches!(replacer.kind(), NULL | UNDEFINED)
                {
                    return Err(syntax(REPLACER, *replacer));
                }
                dumped.push(match rest.get(1) {
                    Some(indent) => {
                        let spaces = (indent.kind() == NUMBER)
                            .then(|| whole(&source[indent.byte_range()]))
                            .flatten()
                            .filter(|spaces| MAX_INDENT.contains(spaces))
                            .ok_or_else(|| syntax(INDENT_WIDTH, *indent))?;
                        Argument::Keyword(
                            Name::fixed(INDENT),
                            Expression::Constant(Constant::Number(spaces.to_string())),
                        )
                    }
                    None => Argument::Keyword(
                        Name::fixed(SEPARATORS),
                        Expression::Tuple(
                            COMPACT_SEPARATORS
                                .into_iter()
                                .map(Expression::string)
                                .collect(),
                        ),
                    ),
                });
                dumped.push(Argument::Keyword(
                    Name::fixed(ENSURE_ASCII),
                    Expression::Constant(Constant::False),
                ));
                built(Expression::Call(Call {
                    callee: Callee::Imported(Import::Json, DUMPS),
                    arguments: dumped,
                    line: at,
                }))
            }
            Shape::Extremum(extremum) => match arguments {
                [only] if only.kind() != SPREAD_ELEMENT => {
                    Err(syntax(format!("`{}()` {ONE_VALUE}", mapping.written), call))
                }
                _ => {
                    let values = self.positional(arguments)?;
                    if !values.iter().all(|value| value.fact.is_whole()) {
                        return Err(syntax(
                            format!("`{}()` {NOT_WHOLE_NUMBERS}", mapping.written),
                            call,
                        ));
                    }
                    built(builtin(
                        extremum,
                        values.into_iter().map(|value| value.expression).collect(),
                    ))
                }
            },
            Shape::Values | Shape::Entries => {
                let object = self.positional(arguments)?.remove(0);
                let read = if shape == Shape::Values {
                    VALUES
                } else {
                    ITEMS
                };
                built(builtin(
                    Builtin::List,
                    vec![method(object.expression, read, Vec::new())],
                ))
            }
            Shape::Assign => {
                let (target, sources) = arguments
                    .split_first()
                    .ok_or_else(|| refused(call, source))?;
                let empty = target.kind() == OBJECT && target.named_child_count() == 0;
                if !empty {
                    return Err(syntax(ASSIGN_TARGET, *target));
                }
                let merged = self
                    .positional(sources)?
                    .into_iter()
                    .map(|source| Entry::Spread(source.expression))
                    .collect();
                built(Expression::Dict(merged))
            }
            Shape::IsArray => {
                let tested = self.positional(arguments)?.remove(0);
                built(builtin(
                    Builtin::Isinstance,
                    vec![tested.expression, Expression::Name(Name::fixed(LIST))],
                ))
            }
            Shape::ParseInt => {
                if let Some(radix) = arguments.get(1)
                    && &source[radix.byte_range()] != DECIMAL_RADIX
                {
                    return Err(syntax(RADIX, *radix));
                }
                let parsed = self.positional(&arguments[..1])?.remove(0);
                built(builtin(Builtin::Int, vec![parsed.expression]))
            }
            Shape::Gather => {
                let argument = arguments[0];
                let items = if argument.kind() == ARRAY {
                    let Expression::List(items) = self.array(argument)?.expression else {
                        return Err(refused(argument, source));
                    };
                    let elements = items
                        .into_iter()
                        .map(|item| match item {
                            Item::One(element) => Ok(element),
                            Item::Spread(_) => Err(syntax(SPREAD, argument)),
                        })
                        .collect::<Translation<Vec<_>>>()?;
                    Gathered::Elements(elements)
                } else {
                    Gathered::Spread(Box::new(self.value(argument)?.expression))
                };
                built(Expression::Gather(Gather { items, line: at }))
            }
            _ => Err(refused(call, source)),
        }
    }

    /// `map`, `filter`, `find`, `some`, `every` and `flatMap` as comprehensions.
    fn iterated(
        &mut self,
        shape: Shape,
        receiver: Expression,
        callback: Node<'_>,
        call: Node<'_>,
    ) -> Translation<Lowered> {
        let predicate = matches!(
            shape,
            Shape::Filter | Shape::Find | Shape::Some | Shape::Every
        );
        let lowered = self.callback(callback, predicate, shape == Shape::Map, &receiver)?;
        let clause = lowered.clause(receiver, callback)?;
        let at = line(call);
        let test = |lowered: &Callback| {
            safe_test(
                Lowered::new(lowered.body.expression.clone(), lowered.body.fact),
                &written(callback, self.source),
                at,
            )
        };
        let comprehension = |collected, element, clauses| {
            Expression::Comprehension(Box::new(Comprehension {
                collected,
                element,
                clauses,
            }))
        };
        let expression = match shape {
            Shape::Map => comprehension(Collected::List, lowered.body.expression, vec![clause]),
            Shape::Filter => comprehension(
                Collected::List,
                Expression::Name(lowered.item_name(callback)?),
                vec![clause, Clause::If(test(&lowered)?)],
            ),
            Shape::Find => Expression::call(
                Callee::Builtin(Builtin::Next),
                vec![
                    comprehension(
                        Collected::Generator,
                        Expression::Name(lowered.item_name(callback)?),
                        vec![clause, Clause::If(test(&lowered)?)],
                    ),
                    Expression::Constant(Constant::None),
                ],
                at,
            ),
            Shape::Some | Shape::Every => Expression::call(
                Callee::Builtin(if shape == Shape::Some {
                    Builtin::Any
                } else {
                    Builtin::All
                }),
                vec![comprehension(
                    Collected::Generator,
                    test(&lowered)?.into_expression(),
                    vec![clause],
                )],
                at,
            ),
            Shape::FlatMap => {
                let listed = match &lowered.body.expression {
                    Expression::List(_) => true,
                    Expression::Comprehension(inner) => inner.collected == Collected::List,
                    _ => false,
                };
                if !listed {
                    return Err(syntax(FLATTENED, callback));
                }
                comprehension(
                    Collected::List,
                    Expression::Name(Name::item()),
                    vec![
                        clause,
                        Clause::For {
                            target: Target::Name(Name::item()),
                            iterable: lowered.body.expression,
                        },
                    ],
                )
            }
            _ => return Err(refused(call, self.source)),
        };
        Ok(Lowered::new(expression, Fact::Unknown))
    }

    /// An arrow function an array method calls once per item. A one-line callback becomes the
    /// comprehension's element; one with a body, or an `async` one (only `map` takes it), is
    /// lifted to a `def` placed before the statement.
    fn callback(
        &mut self,
        node: Node<'_>,
        predicate: bool,
        allows_async: bool,
        receiver: &Expression,
    ) -> Translation<Callback> {
        let source = self.source;
        if node.kind() != ARROW_FUNCTION {
            return Err(refused(node, source));
        }
        Parts::read(node, source, |parts| {
            let is_async = parts.token(ASYNC);
            parts.field(TYPE_PARAMETERS_FIELD);
            parts.field(RETURN_TYPE_FIELD);
            let parameters = self.parameters(parts)?;
            let body = parts.required(BODY_FIELD)?;
            if is_async && !allows_async {
                return Err(syntax(ASYNC_CALLBACK.to_owned(), node));
            }
            let (item_node, index_node) = match parameters[..] {
                [(item, None)] => (item, None),
                [(item, None), (index, None)] if index.kind() == IDENTIFIER => (item, Some(index)),
                _ => return Err(refused(node, source)),
            };
            if !is_async && body.kind() != STATEMENT_BLOCK {
                return self.framed(Frame::Comprehension, |translator| {
                    let item = translator.target(item_node, BindingKind::Parameter, receiver)?;
                    let index = index_node
                        .map(|index| translator.declare(index, BindingKind::Const, Fact::Count))
                        .transpose()?;
                    let body = if predicate {
                        Lowered::new(translator.test(body)?.into_expression(), Fact::Bool)
                    } else {
                        translator.value(body)?
                    };
                    Ok(Callback { item, index, body })
                });
            }
            if self.scopes.frame() == Frame::Comprehension {
                return Err(syntax(NESTED_CALLBACK, node));
            }
            let name = Name::callback(self.callbacks);
            self.callbacks += 1;
            let frame = if predicate {
                Frame::Predicate
            } else {
                Frame::Function { is_async }
            };
            let (item, index, statements) = self.framed(frame, |translator| {
                let item = match item_node.kind() {
                    IDENTIFIER => {
                        translator.declare(item_node, BindingKind::Parameter, Fact::Unknown)?
                    }
                    _ => return Err(syntax(DESTRUCTURED_ITEM, item_node)),
                };
                let index = index_node
                    .map(|index| translator.declare(index, BindingKind::Const, Fact::Count))
                    .transpose()?;
                let statements = translator.function_body(body, is_async)?;
                Ok((item, index, statements))
            })?;
            self.contained(&statements, node)?;
            let mut parameters = vec![Parameter {
                name: item.clone(),
                default: None,
            }];
            let mut passed = vec![Expression::Name(item.clone())];
            if let Some(index) = &index {
                parameters.push(Parameter {
                    name: index.clone(),
                    default: None,
                });
                passed.push(Expression::Name(index.clone()));
            }
            self.lifted.push(Statement::Def(Def {
                name: name.clone(),
                is_async,
                parameters,
                body: statements,
            }));
            let called = Expression::call(Callee::Local { name, is_async }, passed, line(node));
            Ok(Callback {
                item: Target::Name(item),
                index,
                body: Lowered::new(called, if predicate { Fact::Bool } else { Fact::Unknown }),
            })
        })
    }

    /// The body of a function: a block's statements, or an expression returned. An `async`
    /// arrow returning a deferred call awaits it, as JavaScript adopts the promise it returns.
    pub(super) fn function_body(
        &mut self,
        body: Node<'_>,
        is_async: bool,
    ) -> Translation<Vec<Statement>> {
        if body.kind() == STATEMENT_BLOCK {
            let source = self.source;
            let nodes = Parts::read(body, source, |parts| Ok(parts.statements()))?;
            return self.block(&nodes);
        }
        self.spliced(|translator| {
            let value = if translator.scopes.frame() == Frame::Predicate {
                translator.test(body)?.into_expression()
            } else {
                translator.value(body)?.expression
            };
            let returned = match value {
                Expression::Call(call) if is_async && call.is_deferred() => {
                    Expression::Await(Box::new(Expression::Call(call)))
                }
                value => value,
            };
            Ok(vec![Statement::Return(Some(returned))])
        })
    }

    /// The parameter patterns of an arrow function or a `function`, each with its default.
    pub(super) fn parameters<'tree>(
        &mut self,
        parts: &mut Parts<'tree, '_>,
    ) -> Translation<Vec<(Node<'tree>, Option<Node<'tree>>)>> {
        let source = self.source;
        if let Some(single) = parts.field(PARAMETER_FIELD) {
            return Ok(vec![(single, None)]);
        }
        let list = parts.required(PARAMETERS_FIELD)?;
        if list.kind() != FORMAL_PARAMETERS {
            return Err(refused(list, source));
        }
        let declared = Parts::read(list, source, |list| Ok(list.named()))?;
        if declared.len() > MAX_CALLBACK_PARAMETERS && parts.kind() == ARROW_FUNCTION {
            return Err(refused(list, source));
        }
        declared
            .into_iter()
            .map(|parameter| match parameter.kind() {
                REQUIRED_PARAMETER | OPTIONAL_PARAMETER => {
                    Parts::read(parameter, source, |parts| {
                        let pattern = parts.required(PATTERN_FIELD)?;
                        parts.field(TYPE_FIELD);
                        let default = parts.field(VALUE_FIELD);
                        Ok((
                            pattern,
                            default
                                .or((parameter.kind() == OPTIONAL_PARAMETER).then_some(parameter)),
                        ))
                    })
                }
                _ => Err(refused(parameter, source)),
            })
            .collect()
    }

    /// A function's parameters, declared in the function's own scope.
    pub(super) fn declared_parameters(
        &mut self,
        parameters: &[(Node<'_>, Option<Node<'_>>)],
    ) -> Translation<Vec<Parameter>> {
        parameters
            .iter()
            .map(|(pattern, default)| {
                if pattern.kind() != IDENTIFIER {
                    return Err(refused(*pattern, self.source));
                }
                let default = match default {
                    Some(node) if node.kind() == OPTIONAL_PARAMETER => {
                        Some(Expression::Constant(Constant::None))
                    }
                    Some(node) => {
                        let lowered = self.value(*node)?;
                        let literal = match &lowered.expression {
                            Expression::Constant(_) => true,
                            Expression::Negative(inner) => {
                                matches!(**inner, Expression::Constant(Constant::Number(_)))
                            }
                            _ => false,
                        };
                        if !literal {
                            return Err(syntax(DEFAULT, *node));
                        }
                        Some(lowered.expression)
                    }
                    None => None,
                };
                let name = self.declare(*pattern, BindingKind::Parameter, Fact::Unknown)?;
                Ok(Parameter { name, default })
            })
            .collect()
    }

    /// `xs.forEach(x => ...)` as a `for` loop over its body, with no loop around it in reach.
    fn for_each(&mut self, receiver: Expression, callback: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        if callback.kind() != ARROW_FUNCTION {
            return Err(refused(callback, source));
        }
        Parts::read(callback, source, |parts| {
            if parts.token(ASYNC) {
                return Err(syntax(format!("{ASYNC_CALLBACK} `.forEach()`"), callback));
            }
            parts.field(TYPE_PARAMETERS_FIELD);
            parts.field(RETURN_TYPE_FIELD);
            let parameters = self.parameters(parts)?;
            let body = parts.required(BODY_FIELD)?;
            let (item_node, index_node) = match parameters[..] {
                [(item, None)] => (item, None),
                [(item, None), (index, None)] if index.kind() == IDENTIFIER => (item, Some(index)),
                _ => return Err(refused(callback, source)),
            };
            let (item, index, statements) = self.scoped(|translator| {
                let item = translator.target(item_node, BindingKind::Parameter, &receiver)?;
                let index = index_node
                    .map(|index| translator.declare(index, BindingKind::Const, Fact::Count))
                    .transpose()?;
                let hidden = translator.scopes.enter_callback();
                let statements = if body.kind() == STATEMENT_BLOCK {
                    Parts::read(body, source, |parts| Ok(parts.statements()))
                        .and_then(|nodes| translator.block(&nodes))
                } else {
                    translator.spliced(|translator| translator.effect(body))
                };
                translator.scopes.leave_callback(hidden);
                Ok((item, index, statements?))
            })?;
            self.contained(&statements, callback)?;
            let looped = Callback {
                item,
                index,
                body: Lowered::new(Expression::Constant(Constant::None), Fact::Unknown),
            };
            let Clause::For { target, iterable } = looped.clause(receiver, callback)? else {
                return Err(refused(callback, source));
            };
            Ok(Statement::For {
                target,
                iterable,
                body: statements,
            })
        })
    }

    /// A loop variable or callback parameter over `iterable`: a name, or `[key, value]` over
    /// `Object.entries()`, whose every item is such a pair.
    pub(super) fn target(
        &mut self,
        node: Node<'_>,
        kind: BindingKind,
        iterable: &Expression,
    ) -> Translation<Target> {
        let source = self.source;
        match node.kind() {
            IDENTIFIER => Ok(Target::Name(self.declare(node, kind, Fact::Unknown)?)),
            ARRAY_PATTERN => {
                let names = self.pattern_names(node)?;
                if names.len() != ENTRY_NAMES || !is_entries(iterable) {
                    return Err(syntax(ENTRY_PATTERN, node));
                }
                let names = names
                    .into_iter()
                    .map(|name| self.declare(name, kind, Fact::Unknown))
                    .collect::<Translation<Vec<_>>>()?;
                Ok(Target::Tuple(names))
            }
            _ => Err(refused(node, source)),
        }
    }

    /// Refuses a callback whose `body` pushes onto or assigns into a value declared outside it:
    /// Python would run the callback over items JavaScript's snapshot of the receiver skips.
    fn contained(&self, body: &[Statement], callback: Node<'_>) -> Translation<()> {
        let escapes = mutated(body)
            .into_iter()
            .any(|root| root.is_none_or(|name| self.scopes.resolve(name.as_str()).is_some()));
        if escapes {
            return Err(syntax(CALLBACK_MUTATION, callback));
        }
        Ok(())
    }

    /// The names in `[a, b]`, which may hold no hole and nothing but names.
    pub(super) fn pattern_names<'tree>(
        &mut self,
        node: Node<'tree>,
    ) -> Translation<Vec<Node<'tree>>> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let mut names = Vec::new();
            let mut expecting = true;
            for part in parts.sequence() {
                match part.kind() {
                    COMMA if expecting => return Err(syntax(HOLE, part)),
                    COMMA => expecting = true,
                    IDENTIFIER => {
                        names.push(part);
                        expecting = false;
                    }
                    _ if part.is_named() => return Err(refused(part, source)),
                    _ => {}
                }
            }
            Ok(names)
        })
    }

    pub(super) fn declare(
        &mut self,
        node: Node<'_>,
        kind: BindingKind,
        fact: Fact,
    ) -> Translation<Name> {
        let written = &self.source[node.byte_range()];
        let stubs = self.stubs;
        self.scopes
            .declare(written, Binding { kind, fact }, |name| stubs.is_host(name))
            .ok_or_else(|| syntax(format!("the name `{written}`"), node))
    }
}

/// A number literal, when Python reads it as the same number: no BigInt, no legacy octal, no
/// whole number past 2^53 that JavaScript would round, and a whole one written with a fraction or
/// an exponent written as a whole number, since JavaScript has one number type.
fn number(node: Node<'_>, source: &str) -> Translation<Lowered> {
    let text = &source[node.byte_range()];
    let refusal = || syntax(format!("{NUMBER_LITERAL} `{text}`"), node);
    let digits = text.replace(DIGIT_SEPARATOR, "").to_ascii_lowercase();
    let radix = RADIXES
        .iter()
        .find_map(|(prefix, radix)| Some((digits.strip_prefix(prefix)?, *radix)));
    let legacy = radix.is_none()
        && digits.len() > 1
        && digits.starts_with('0')
        && digits.as_bytes()[1].is_ascii_digit();
    if text.ends_with(BIGINT_SUFFIX) || legacy {
        return Err(refusal());
    }
    let whole = match radix {
        Some((digits, radix)) => u64::from_str_radix(digits, radix).map_err(|_| refusal())?,
        None if !digits.contains(FRACTION_MARKS) => digits.parse().map_err(|_| refusal())?,
        None => {
            let value: f64 = digits.parse().map_err(|_| refusal())?;
            if !value.is_finite() || value > MAX_EXACT_WHOLE as f64 {
                return Err(refusal());
            }
            return Ok(if value.fract() == 0.0 {
                Lowered::new(
                    Expression::Constant(Constant::Number((value as u64).to_string())),
                    Fact::Count,
                )
            } else {
                Lowered::new(
                    Expression::Constant(Constant::Number(text.to_owned())),
                    Fact::Number,
                )
            });
        }
    };
    if whole > MAX_EXACT_WHOLE {
        return Err(refusal());
    }
    Ok(Lowered::new(
        Expression::Constant(Constant::Number(whole.to_string())),
        Fact::Count,
    ))
}

/// Whether `node` is a comparison or an equality written bare, which tree-sitter nests to the
/// right where JavaScript groups `a < b < c` to the left.
fn is_comparison(node: Node<'_>) -> bool {
    node.kind() == BINARY_EXPRESSION
        && node
            .child_by_field_name(OPERATOR_FIELD)
            .is_some_and(|operator| {
                let symbol = operator.kind();
                EQUALITIES.contains(&symbol)
                    || INEQUALITIES.contains(&symbol)
                    || ORDERINGS.iter().any(|(written, _)| *written == symbol)
            })
}

/// A string or number literal, maybe negated.
fn is_scalar_literal(node: Node<'_>) -> bool {
    match node.kind() {
        STRING | NUMBER => true,
        UNARY_EXPRESSION => {
            node.child_by_field_name(OPERATOR_FIELD)
                .is_some_and(|operator| operator.kind() == NEGATE)
                && node
                    .child_by_field_name(ARGUMENT_FIELD)
                    .is_some_and(|argument| argument.kind() == NUMBER)
        }
        _ => false,
    }
}

/// What `left operator right`, written at `at`, is known to be when both languages compute it
/// alike: two known numbers, or two strings added. Where JavaScript converts a string, `null` or
/// a list, Python raises or computes otherwise. `/` takes only a literal divisor other than zero,
/// and `%` only counts, as the sign of a negative remainder differs.
pub(super) fn arithmetic(
    left: Fact,
    operator: Operator,
    right: &Lowered,
    at: Node<'_>,
) -> Translation<Fact> {
    let counts = left == Fact::Count && right.fact == Fact::Count;
    let wholes = left.is_whole() && right.fact.is_whole();
    match operator {
        Operator::Add if left == Fact::Str && right.fact == Fact::Str => Ok(Fact::Str),
        Operator::Add if !left.is_numeric() || !right.fact.is_numeric() => Err(syntax(
            format!("`{}` {NOT_NUMBERS_OR_STRINGS}", at.kind()),
            at,
        )),
        _ if !left.is_numeric() || !right.fact.is_numeric() => {
            Err(syntax(format!("`{}` {NOT_NUMBERS}", at.kind()), at))
        }
        Operator::Mod if counts => Ok(Fact::Count),
        Operator::Mod => Err(syntax(MODULO, at)),
        Operator::Div if is_nonzero_number(&right.expression) => Ok(Fact::Number),
        Operator::Div => Err(syntax(DIVISOR, at)),
        Operator::Add | Operator::Mult if counts => Ok(Fact::Count),
        _ if wholes => Ok(Fact::Integer),
        _ => Ok(Fact::Number),
    }
}

/// A number literal other than zero, maybe negated: a divisor Python never raises on.
fn is_nonzero_number(expression: &Expression) -> bool {
    match expression {
        Expression::Constant(Constant::Number(number)) => number
            .replace(DIGIT_SEPARATOR, "")
            .parse::<f64>()
            .is_ok_and(|value| value != 0.0),
        Expression::Negative(inner) => is_nonzero_number(inner),
        _ => false,
    }
}

fn whole(number: &str) -> Option<u64> {
    number.parse().ok()
}

/// `0` or `1`, maybe negated, which Python's `==` finds equal to `False` or `True`.
fn equals_a_boolean(expression: &Expression) -> bool {
    match expression {
        Expression::Constant(Constant::Number(number)) => whole(number).is_some_and(|n| n <= 1),
        Expression::Negative(inner) => {
            matches!(inner.as_ref(), Expression::Constant(Constant::Number(number)) if whole(number) == Some(0))
        }
        _ => false,
    }
}

/// A literal whole number, maybe negated: what `.at()` reads the same in both languages.
fn is_whole(expression: &Expression) -> bool {
    match expression {
        Expression::Constant(Constant::Number(number)) => whole(number).is_some(),
        Expression::Negative(inner) => is_whole(inner),
        _ => false,
    }
}

/// `len(x) - 1`: negative only for an empty `x`, where both languages find nothing.
fn is_last_index(expression: &Expression) -> bool {
    matches!(
        expression,
        Expression::Binary {
            left,
            operator: Operator::Sub,
            right,
        } if matches!(**right, Expression::Constant(Constant::Number(ref one)) if one == ONE)
            && matches!(**left, Expression::Call(Call { callee: Callee::Builtin(Builtin::Len), .. }))
    )
}

/// `o["k"]` as `o.get("k")`, so a missing key tested against `null`, or replaced by `??` or `||`,
/// reads `None` as JavaScript reads `undefined`.
fn lenient(expression: Expression) -> Expression {
    match expression {
        Expression::Subscript { value, index }
            if matches!(*index, Expression::Constant(Constant::Str(_))) =>
        {
            Expression::Call(Call {
                callee: Callee::Method {
                    receiver: value,
                    name: GET,
                },
                arguments: vec![Argument::Positional(*index)],
                line: 0,
            })
        }
        Expression::Guarded {
            checks,
            value,
            fallback,
        } => Expression::Guarded {
            checks,
            value: Box::new(lenient(*value)),
            fallback,
        },
        other => other,
    }
}

/// Whether `iterable` is `list(o.items())`, what `Object.entries(o)` lowers to.
fn is_entries(iterable: &Expression) -> bool {
    let Expression::Call(Call {
        callee: Callee::Builtin(Builtin::List),
        arguments,
        ..
    }) = iterable
    else {
        return false;
    };
    matches!(&arguments[..], [Argument::Positional(Expression::Call(Call {
        callee: Callee::Method { name: ITEMS, .. },
        arguments: read,
        ..
    }))] if read.is_empty())
}

/// A host call's one object as keyword arguments, when every key is a Python name, else as the
/// dict. `None` when it spreads or repeats a key: JavaScript evaluates every value and keeps the
/// last, which neither form says alike.
fn keywords(object: Expression) -> Option<Vec<Argument>> {
    let Expression::Dict(entries) = object else {
        return Some(vec![Argument::Positional(object)]);
    };
    let mut seen = HashSet::new();
    let mut named = true;
    for entry in &entries {
        let Entry::Pair(Expression::Constant(Constant::Str(key)), _) = entry else {
            return None;
        };
        if !seen.insert(key.clone()) {
            return None;
        }
        named &= Name::parse(key).is_some();
    }
    if !named {
        return Some(vec![Argument::Positional(Expression::Dict(entries))]);
    }
    Some(
        entries
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Pair(Expression::Constant(Constant::Str(key)), value) => {
                    Some(Argument::Keyword(Name::parse(&key)?, value))
                }
                Entry::Pair(..) | Entry::Spread(_) => None,
            })
            .collect(),
    )
}

/// What `value` adds to a string `+` chain.
fn pieces(value: Expression) -> Vec<Piece> {
    match value {
        Expression::FString(pieces) => pieces,
        Expression::Constant(Constant::Str(text)) => vec![Piece::Text(text)],
        other => vec![Piece::Value(other)],
    }
}

/// An f-string of `pieces`, or a plain string when none is a value.
fn formatted(pieces: Vec<Piece>) -> Expression {
    let mut merged: Vec<Piece> = Vec::new();
    for piece in pieces {
        match (merged.last_mut(), piece) {
            (_, Piece::Text(text)) if text.is_empty() => {}
            (Some(Piece::Text(last)), Piece::Text(text)) => last.push_str(&text),
            (_, piece) => merged.push(piece),
        }
    }
    match merged.as_slice() {
        [] => Expression::string(""),
        [Piece::Text(_)] => match merged.pop() {
            Some(Piece::Text(text)) => Expression::string(text),
            _ => Expression::string(""),
        },
        _ => Expression::FString(merged),
    }
}

/// Refuses any deferred call in `body` that nothing consumes: one awaited directly, gathered
/// where it is written, a callback's in a gathered `map`, or one of a batch pushed onto a fresh
/// list that is gathered right after.
pub(super) fn consumed(body: &[Statement]) -> Translation<()> {
    let mut index = 0;
    while index < body.len() {
        if let Some(gathered) = batch(body, index)? {
            index = gathered + 1;
            continue;
        }
        let statement = &body[index];
        match discharged(statement) {
            Some(gather) => gathered(gather)?,
            None => statement.expressions().into_iter().try_for_each(free)?,
        }
        statement.blocks().into_iter().try_for_each(consumed)?;
        index += 1;
    }
    Ok(())
}

/// The gather `statement` awaits as all it does: `x = await gather`, `a, b = await gather`,
/// `await gather`, or `return await gather`. Anywhere else, Python would run what surrounds the
/// gather in another order relative to the calls it starts.
fn discharged(statement: &Statement) -> Option<&Gather> {
    let (Statement::Assign {
        target: Target::Name(_) | Target::Tuple(_),
        value: Expression::Await(awaited),
    }
    | Statement::Expression(Expression::Await(awaited))
    | Statement::Return(Some(Expression::Await(awaited)))) = statement
    else {
        return None;
    };
    match &**awaited {
        Expression::Gather(gather) => Some(gather),
        _ => None,
    }
}

fn free(expression: &Expression) -> Translation<()> {
    match expression {
        Expression::Await(inner) => match &**inner {
            Expression::Gather(gather) => Err(Untranslated {
                construct: Construct::Syntax(GATHER_PLACE.to_owned()),
                line: gather.line,
            }),
            Expression::Call(call) => call.children().into_iter().try_for_each(free),
            other => free(other),
        },
        Expression::Call(call) if call.is_deferred() => Err(unawaited(call)),
        Expression::Gather(gather) => Err(Untranslated {
            construct: Construct::Unawaited(GATHER_CALL.to_owned()),
            line: gather.line,
        }),
        _ => expression.children().into_iter().try_for_each(free),
    }
}

/// Refuses a gather of anything but host calls written in it or made by a `map` callback, each
/// with only [`is_plain`] arguments: Python starts every call only once all are made, where
/// JavaScript starts each as it is made, so a call or a raise among them would reorder.
fn gathered(gather: &Gather) -> Translation<()> {
    let refused = || Untranslated {
        construct: Construct::Syntax(GATHERED.to_owned()),
        line: gather.line,
    };
    match &gather.items {
        Gathered::Elements(items) => items
            .iter()
            .try_for_each(|item| gathered_call(item, &[]).ok_or_else(refused)?),
        Gathered::Spread(spread) => match &**spread {
            Expression::Comprehension(comprehension)
                if comprehension.collected == Collected::List =>
            {
                let mut roots = Vec::new();
                for clause in &comprehension.clauses {
                    match clause {
                        Clause::For { target, iterable } => {
                            free(iterable)?;
                            roots.extend(target.names());
                        }
                        Clause::If(test) => free(test.expression())?,
                    }
                }
                gathered_call(&comprehension.element, &roots).ok_or_else(refused)?
            }
            _ => Err(refused()),
        },
    }
}

/// When `expression` is a deferred host call, whether its arguments are all [`is_plain`] over
/// `roots`; `None` for anything else.
fn gathered_call(expression: &Expression, roots: &[&Name]) -> Option<Translation<()>> {
    let Expression::Call(
        call @ Call {
            callee: Callee::Host { deferred: true, .. },
            ..
        },
    ) = expression
    else {
        return None;
    };
    let plain = call.arguments.iter().all(|argument| match argument {
        Argument::Positional(value) | Argument::Keyword(_, value) => is_plain(value, roots),
        Argument::Spread(_) => false,
    });
    Some(if plain {
        Ok(())
    } else {
        Err(Untranslated {
            construct: Construct::Syntax(format!("`{}` {GATHERED_ARGUMENT}", call.written())),
            line: call.line,
        })
    })
}

/// Whether evaluating `argument` can neither call, await nor raise: a literal, a name, a list or
/// dict of those, or a read on one of `roots`, the variables of the loop the call is made in.
fn is_plain(argument: &Expression, roots: &[&Name]) -> bool {
    match argument {
        Expression::Constant(_) | Expression::Name(_) => true,
        Expression::Negative(inner) => matches!(**inner, Expression::Constant(Constant::Number(_))),
        Expression::List(items) => items
            .iter()
            .all(|item| matches!(item, Item::One(value) if is_plain(value, roots))),
        Expression::Dict(entries) => entries.iter().all(|entry| {
            matches!(entry, Entry::Pair(key, value) if is_plain(key, roots) && is_plain(value, roots))
        }),
        Expression::Subscript { .. } => is_read(argument, roots),
        _ => false,
    }
}

fn is_read(expression: &Expression, roots: &[&Name]) -> bool {
    match expression {
        Expression::Name(name) => roots.contains(&name),
        Expression::Subscript { value, index } => {
            is_read(value, roots)
                && matches!(**index, Expression::Constant(_) | Expression::Name(_))
        }
        _ => false,
    }
}

fn unawaited(call: &Call) -> Untranslated {
    Untranslated {
        construct: Construct::Unawaited(call.written()),
        line: call.line,
    }
}

/// When `body[index]` starts a batch, `const list = []` with only pushes of deferred host calls
/// after it and then `await Promise.all(list)`, checks its calls and returns where it gathers.
fn batch(body: &[Statement], index: usize) -> Translation<Option<usize>> {
    let Statement::Assign {
        target: Target::Name(list),
        value: Expression::List(items),
    } = &body[index]
    else {
        return Ok(None);
    };
    if !items.is_empty() {
        return Ok(None);
    }
    let mut end = index + 1;
    while end < body.len() && inside(&body[end], list) {
        end += 1;
    }
    if end == body.len() || !gathers(&body[end], list) {
        return Ok(None);
    }
    let after = &body[index + 1..];
    let mut pushes = 0;
    let mut mentions = 0;
    let mut rebound = false;
    count(&body[index + 1..end], list, &mut pushes);
    walk(after, &mut |expression| {
        mentions += usize::from(matches!(expression, Expression::Name(name) if name == list));
    });
    each_statement(after, &mut |statement| {
        rebound |= statement.targets().contains(&list);
    });
    if mentions != pushes + 1 || rebound {
        return Ok(None);
    }
    pushed(&body[index + 1..end], &mut Vec::new())?;
    Ok(Some(end))
}

/// Refuses a call pushed in a batch's `region` whose arguments are not [`is_plain`] over the
/// variables of the loops it is pushed in, `roots`.
fn pushed<'a>(region: &'a [Statement], roots: &mut Vec<&'a Name>) -> Translation<()> {
    for statement in region {
        match statement {
            Statement::Append { value, .. } => {
                if let Some(checked) = gathered_call(value, roots) {
                    checked?;
                }
            }
            Statement::For { target, body, .. } => {
                let outer = roots.len();
                roots.extend(target.names());
                pushed(body, roots)?;
                roots.truncate(outer);
            }
            _ => statement
                .blocks()
                .into_iter()
                .try_for_each(|block| pushed(block, roots))?,
        }
    }
    Ok(())
}

/// Whether `statement` belongs between a batch's list and its gather: a push of a deferred host
/// call onto it, or a `for` or `if` holding only such pushes, comments and jumps, so nothing
/// raises after a call is pushed but before Python starts it.
fn region(statement: &Statement, list: &Name) -> bool {
    match statement {
        Statement::Append {
            list: Expression::Name(name),
            value: Expression::Call(call),
        } if name == list => matches!(call.callee, Callee::Host { deferred: true, .. }),
        Statement::For {
            target,
            iterable,
            body,
        } => {
            target.expressions().is_empty()
                && untouched(&[Statement::Expression(iterable.clone())], list)
                && body.iter().all(|inner| inside(inner, list))
        }
        Statement::If {
            test,
            body,
            otherwise,
        } => {
            untouched(&[Statement::Expression(test.expression().clone())], list)
                && body
                    .iter()
                    .chain(otherwise)
                    .all(|inner| inside(inner, list))
        }
        _ => false,
    }
}

fn inside(statement: &Statement, list: &Name) -> bool {
    region(statement, list)
        || matches!(
            statement,
            Statement::Comment(_) | Statement::Break | Statement::Continue
        )
}

/// Whether nothing in `body` reads `list`, calls a host or a function of the snippet's own, or
/// gathers.
fn untouched(body: &[Statement], list: &Name) -> bool {
    let mut touched = false;
    walk(body, &mut |expression| {
        touched |= match expression {
            Expression::Name(name) => name == list,
            Expression::Call(call) => {
                matches!(call.callee, Callee::Host { .. } | Callee::Local { .. })
            }
            Expression::Gather(_) | Expression::Await(_) => true,
            _ => false,
        };
    });
    !touched
}

fn gathers(statement: &Statement, list: &Name) -> bool {
    discharged(statement).is_some_and(|gather| {
        matches!(&gather.items, Gathered::Spread(spread)
            if matches!(&**spread, Expression::Name(name) if name == list))
    })
}

fn count(body: &[Statement], list: &Name, pushes: &mut usize) {
    each_statement(body, &mut |statement| {
        *pushes += usize::from(matches!(statement,
            Statement::Append { list: Expression::Name(name), .. } if name == list));
    });
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::tests::{python, refused};
    use super::{
        AWAIT_HERE, AWAIT_IMMEDIATE, BOOLEAN_NUMBER, CALLBACK_MUTATION, CHAINED, CONCATENATED,
        DIVISOR, ENTRY_PATTERN, GATHER_PLACE, GATHERED, HOLE, INCLUDED, INDENT_WIDTH,
        KEYWORDS_SPREAD, LENGTHLESS, LOOSE_EQUALITY, MODULO, NULLISH_CHAINED, OBJECT_TEXT,
        OPTIONAL_CALL, OR_VALUE, REPLACER, SPLIT_SEPARATOR, SPREAD_OBJECT, STRICT_NULL,
        STRICT_UNKNOWN, UNDEFINED_VALUE,
    };

    const PUSH_THEN_GATHER: &str = "const owners = [\"a\", \"b\"];\nconst calls = [];\nfor (const owner of owners) {\n  if (owner !== \"b\") {\n    calls.push(gh.list({owner}));\n  }\n}\nconst results = await Promise.all(calls);\nreturn results.map(r => r.items);";
    const PUSH_THEN_GATHER_PYTHON: &str = "import asyncio\nowners = [\"a\", \"b\"]\ncalls = []\nfor owner in owners:\n    if owner != \"b\":\n        calls.append(gh.list(owner=owner))\nresults = await asyncio.gather(*calls)\n[r[\"items\"] for r in results]";
    const UNAWAITED_LIST: &str = "`gh.list()` without `await`";
    const UNAWAITED_GATHER: &str = "`Promise.all()` without `await`";
    const UNAWAITED_LOCAL: &str = "`f()` without `await`";
    const OPTIONAL_COMPARED: &str = "`r?.x`, which may be `undefined`, compared or computed";
    const GATHERED_GET: &str = "`gh.get()` gathered with an argument other than a literal, a name, or a read of the loop's own item";

    #[test_case("const a = [true, false, null, undefined];", "a = [True, False, None, None]" ; "literals")]
    #[test_case("const a = 1.5;\nconst b = 0x1f;\nconst c = 'it\\'s \"q\"\\n';", "a = 1.5\nb = 31\nc = \"it's \\\"q\\\"\\n\"" ; "numbers_and_strings_requoted")]
    #[test_case("const a = [1e3, 1.0, 0o10, 1_000, 2.5e-1];", "a = [1000, 1, 8, 1000, 2.5e-1]" ; "whole_numbers_written_as_integers")]
    #[test_case("const n = 2;\nconst a = `n=${n} {x} ${n + 1}`;", "n = 2\na = f\"n={n} {{x}} {n + 1}\"" ; "template")]
    #[test_case("const s = `line1\nline2`;", "s = \"line1\\nline2\"" ; "template_across_lines")]
    #[test_case("const s = \"x\";\nconst a = \"s=\" + s + \"!\";", "s = \"x\"\na = f\"s={s}!\"" ; "plus_of_strings")]
    #[test_case("const xs = [1];\nconst a = [xs.length + 1, xs.length - 2 * 3, -xs.length, xs.length / 2, xs.length / -0.5];", "xs = [1]\na = [len(xs) + 1, len(xs) - 2 * 3, -len(xs), len(xs) / 2, len(xs) / -0.5]" ; "arithmetic_of_numbers")]
    #[test_case("const xs = [1];\nconst a = xs.length > 0 && xs.length <= 3;", "xs = [1]\na = len(xs) > 0 and len(xs) <= 3" ; "and_of_tests_as_a_value")]
    #[test_case("const r = await gh.get();\nconsole.log(`${r[\"a\"]} and ${r.b.c}`);", "r = await gh.get()\nprint(f\"{r['a']} and {r['b']['c']}\")" ; "quotes_inside_a_template")]
    #[test_case("const xs = [1];\nconst a = [...xs, 2];", "xs = [1]\na = [*xs, 2]" ; "array_spread")]
    #[test_case("const o = {};\nconst k = \"k\";\nconst a = {o, ...o, [k]: 1, \"b-c\": 2, d: 3};", "o = {}\nk = \"k\"\na = {\"o\": o, **o, k: 1, \"b-c\": 2, \"d\": 3}" ; "object")]
    #[test_case("const a = 1;\nconst b = [a === 1, a !== 0, a === \"x\", 2 !== a];", "a = 1\nb = [a == 1, a != 0, a == \"x\", 2 != a]" ; "equality")]
    #[test_case("const r = await gh.get();\nconst a = [r == null, r.x == undefined, r.x != null];", "r = await gh.get()\na = [r is None, r.get(\"x\") is None, r.get(\"x\") is not None]" ; "nullish_comparison")]
    #[test_case("const r = await gh.get();\nconst a = [r.ok === false, r.ok !== true, r.n === 2, r.s !== \"x\", r.n === -2.5];", "r = await gh.get()\na = [r[\"ok\"] is False, r[\"ok\"] is not True, r[\"n\"] == 2, r[\"s\"] != \"x\", r[\"n\"] == -2.5]" ; "strict_comparison_with_a_literal")]
    #[test_case("const a = 1;\nconst b = a > 0 ? \"p\" : \"n\";", "a = 1\nb = \"p\" if a > 0 else \"n\"" ; "ternary")]
    #[test_case("const r = await gh.get();\nconst a = r.x ?? 5;\nconst b = r?.x ?? 6;", "r = await gh.get()\na = r.get(\"x\") if r.get(\"x\") is not None else 5\nb = r.get(\"x\") if r is not None and r.get(\"x\") is not None else 6" ; "nullish_coalescing")]
    #[test_case("const r = await gh.get();\nconst a = r?.x;\nconst b = r.items?.[0];", "r = await gh.get()\na = r[\"x\"] if r is not None else None\nb = r[\"items\"][0] if r[\"items\"] is not None else None" ; "optional_chain")]
    #[test_case("const r = await gh.get();\nconst a = r.items.length;\nconst b = r.items[0];", "r = await gh.get()\na = len(r[\"items\"])\nb = r[\"items\"][0]" ; "length_and_index")]
    #[test_case("const a = await gh.list({owner: \"o\"});\nconst b = await gh.list({\"owner\": \"o\"});", "a = await gh.list(owner=\"o\")\nb = await gh.list(owner=\"o\")" ; "host_call_keywords")]
    #[test_case("const o = {\"class\": 1};\nconst r = await gh.get(o);", "o = {\"class\": 1}\nr = await gh.get(o)" ; "host_call_object_kept")]
    #[test_case("const a = gh.nope();\nconst b = search(\"q\");\nconst c = await fetch(\"u\");", "a = gh.nope()\nb = search(\"q\")\nc = await fetch(\"u\")" ; "immediate_and_deferred_hosts")]
    #[test_case("const [a, b] = await Promise.all([gh.list({owner: \"a\"}), gh.list({owner: \"b\"})]);", "import asyncio\na, b = await asyncio.gather(gh.list(owner=\"a\"), gh.list(owner=\"b\"))" ; "gather_written_calls")]
    #[test_case("const xs = [\"a\"];\nconst rs = await Promise.all(xs.map(x => gh.list({owner: x})));", "import asyncio\nxs = [\"a\"]\nrs = await asyncio.gather(*[gh.list(owner=x) for x in xs])" ; "gather_mapped_calls")]
    #[test_case("const [a, b] = await Promise.all([gh.get({id: 1}), gh.get({id: 2})]);", "import asyncio\na, b = await asyncio.gather(gh.get(id=1), gh.get(id=2))" ; "gather_destructured")]
    #[test_case("const xs = [{id: 1}];\nconst rs = await Promise.all(xs.map(x => gh.get({id: x.id})));\nreturn await Promise.all([gh.list()]);", "import asyncio\nxs = [{\"id\": 1}]\nrs = await asyncio.gather(*[gh.get(id=x[\"id\"]) for x in xs])\nawait asyncio.gather(gh.list())" ; "gather_reading_its_item_and_returned")]
    #[test_case(PUSH_THEN_GATHER, PUSH_THEN_GATHER_PYTHON ; "gather_pushed_calls")]
    #[test_case("const calls = [];\ncalls.push(gh.list());\nawait Promise.all(calls);", "import asyncio\ncalls = []\ncalls.append(gh.list())\nawait asyncio.gather(*calls)" ; "gather_pushed_calls_as_a_statement")]
    #[test_case("const xs = [\"a\", \"b\"];\nconst a = xs.map(x => x.length * 2);\nconst b = xs.map((x, i) => i * 2);", "xs = [\"a\", \"b\"]\na = [len(x) * 2 for x in xs]\nb = [i * 2 for i, x in enumerate(xs)]" ; "map")]
    #[test_case("const xs = [\"a\", \"b\"];\nconst a = xs.filter(x => x.length > 1);\nconst b = xs.find(x => x === \"b\");", "xs = [\"a\", \"b\"]\na = [x for x in xs if len(x) > 1]\nb = next((x for x in xs if x == \"b\"), None)" ; "filter_and_find")]
    #[test_case("const xs = [\"a\", \"b\"];\nconst a = xs.some(x => x.length > 1);\nconst b = xs.every(x => x.length > 1);", "xs = [\"a\", \"b\"]\na = any(len(x) > 1 for x in xs)\nb = all(len(x) > 1 for x in xs)" ; "some_and_every")]
    #[test_case("const items = await gh.list();\nreturn items.filter(i => i.state === \"open\").map(i => i.title);", "items = await gh.list()\n[i[\"title\"] for i in [i for i in items if i[\"state\"] == \"open\"]]" ; "filter_then_map")]
    #[test_case("const r = await gh.get();\nreturn r.items.filter(i => i.tags.length > 0 && i.name !== \"x\").map(i => ({name: i.name}));", "r = await gh.get()\n[{\"name\": i[\"name\"]} for i in [i for i in r[\"items\"] if len(i[\"tags\"]) > 0 and i[\"name\"] != \"x\"]]" ; "chained_callbacks")]
    #[test_case("const xs = [\"a\"];\nconst seen = xs.map(x => { const ys = []; ys.push(x); return ys; });", "xs = [\"a\"]\ndef _caudra_callback_0(x):\n    ys = []\n    ys.append(x)\n    return ys\nseen = [_caudra_callback_0(x) for x in xs]" ; "callback_pushing_to_its_own_list")]
    #[test_case("const s = \"ab\";\nconst a = [s.split(\"\"), s.split(\",\")];", "s = \"ab\"\na = [list(s), s.split(\",\")]" ; "split")]
    #[test_case("const s = \"\\u00a0a\";\nconst xs = [\"a\"];\nconst a = [s.includes(\"a\"), xs.includes(s), xs.concat([s], xs), s.concat(\"b\")];", "s = \"\\xa0a\"\nxs = [\"a\"]\na = [\"a\" in s, s in xs, xs + [s] + xs, s + \"b\"]" ; "includes_and_concat")]
    #[test_case("const xs = [1];\nconst a = Math.max(xs.length, 2) > Math.min(1, -xs.length);", "xs = [1]\na = max(len(xs), 2) > min(1, -len(xs))" ; "extremum_of_whole_numbers")]
    #[test_case("const o = {a: 1};\nconst a = [JSON.stringify(o), JSON.stringify(o, null, 2)];", "import json\no = {\"a\": 1}\na = [json.dumps(o, separators=(\",\", \":\"), ensure_ascii=False), json.dumps(o, indent=2, ensure_ascii=False)]" ; "stringify")]
    fn expression_translates(js: &str, expected: &str) {
        assert_eq!(python(js), expected);
    }

    #[test_case("const r = await gh.get();\nconst a = r?.x > 0;", OPTIONAL_COMPARED, 2 ; "optional_compared")]
    #[test_case("const r = await gh.get();\nconst i = 1;\nconst a = r.items[i - 1];", "the index `i - 1`", 3 ; "index_that_may_be_negative")]
    #[test_case("const xs = [1];\nconst i = xs.length - 3;\nconst a = i % 2;", MODULO, 3 ; "modulo_of_a_negative")]
    #[test_case("const a = 1;\nconst b = 0 < a < 10;", CHAINED, 2 ; "chained_ordering")]
    #[test_case("const a = 1;\nconst b = a === 1 === true;", CHAINED, 2 ; "chained_equality")]
    #[test_case("const a = 1;\nconst b = a < 2 === true;", CHAINED, 2 ; "ordering_compared")]
    #[test_case("const a = 2 ** 3;", "`**`", 1 ; "power")]
    #[test_case("const a = 1;\nconst b = a == 1;", LOOSE_EQUALITY, 2 ; "loose_equality_with_a_number")]
    #[test_case("const r = await gh.get();\nconst a = r.x === null;", STRICT_NULL, 2 ; "strict_null")]
    #[test_case("const r = await gh.get();\nconst a = r.x !== undefined;", STRICT_NULL, 2 ; "strict_undefined")]
    #[test_case("const r = await gh.get();\nconst a = r.x === r.y;", STRICT_UNKNOWN, 2 ; "strict_between_unknowns")]
    #[test_case("const r = await gh.get();\nconst a = r.x === [1];", STRICT_UNKNOWN, 2 ; "strict_against_an_array")]
    #[test_case("const a = true === 1;", STRICT_UNKNOWN, 1 ; "boolean_against_a_number")]
    #[test_case("const r = await gh.get();\nconst a = r.done === 1;", BOOLEAN_NUMBER, 2 ; "one_against_an_unknown")]
    #[test_case("const r = await gh.get();\nconst a = 0 !== r.done;", BOOLEAN_NUMBER, 2 ; "zero_against_an_unknown")]
    #[test_case("const r = await gh.get();\nconst a = r.n > 0;", "`>` of a value not known to be a number", 2 ; "ordering_of_an_unknown")]
    #[test_case("const r = await gh.get();\nconst a = r.n * 2;", "`*` of a value not known to be a number", 2 ; "product_of_an_unknown")]
    #[test_case("const r = await gh.get();\nconst a = -r.n;", "`-` of a value not known to be a number", 2 ; "negation_of_an_unknown")]
    #[test_case("const r = await gh.get();\nconst a = r.n + 1;", "`+` of values not known to be two numbers or two strings", 2 ; "sum_of_an_unknown")]
    #[test_case("const a = \"n=\" + 1;", "`+` of values not known to be two numbers or two strings", 1 ; "string_plus_number")]
    #[test_case("const xs = [1];\nconst a = xs.length / xs.length;", DIVISOR, 2 ; "division_by_a_value")]
    #[test_case("const xs = [1];\nconst a = xs.length / 0;", DIVISOR, 2 ; "division_by_zero")]
    #[test_case("const a = 9007199254740993;", "the number `9007199254740993`", 1 ; "integer_past_exact")]
    #[test_case("const a = 0x20000000000001;", "the number `0x20000000000001`", 1 ; "hex_past_exact")]
    #[test_case("const a = await gh.list({owner: undefined});", UNDEFINED_VALUE, 1 ; "undefined_keyword")]
    #[test_case("const a = search(undefined);", UNDEFINED_VALUE, 1 ; "undefined_argument")]
    #[test_case("const a = [...{b: 1}];", SPREAD_OBJECT, 1 ; "object_spread_in_an_array")]
    #[test_case("const o = {};\nawait gh.list({...o});", KEYWORDS_SPREAD, 2 ; "spread_in_keywords")]
    #[test_case("const k = \"owner\";\nawait gh.list({[k]: \"a\"});", KEYWORDS_SPREAD, 2 ; "computed_keyword")]
    #[test_case("const s = \"a\";\nconst a = s.trim().includes(s.trim());", "`.includes()` with calls both in what it is called on and in its argument", 2 ; "includes_reordered")]
    #[test_case("const a = Math.PI;", "`Math.PI`", 1 ; "global_constant")]
    #[test_case("const xs = [{}];\nconst a = xs.includes({});", INCLUDED, 2 ; "includes_an_object")]
    #[test_case("const xs = [true];\nconst a = xs.includes(1);", INCLUDED, 2 ; "includes_a_number")]
    #[test_case("const xs = [1];\nconst a = xs.concat(4);", CONCATENATED, 2 ; "concat_of_a_number")]
    #[test_case("const s = \"a\";\nconst xs = [1];\nconst a = s.concat(xs);", CONCATENATED, 3 ; "concat_of_a_string_by_an_array")]
    #[test_case("const xs = [1];\nconst a = Math.max(...xs);", "`...` in this call", 2 ; "max_of_a_spread")]
    #[test_case("const r = await gh.get();\nconst a = Math.min(r.n, 2);", "`Math.min()` of values not known to be whole numbers", 2 ; "min_of_an_unknown")]
    #[test_case("const n = 3;\nconst a = n.length;", LENGTHLESS, 2 ; "length_of_a_number")]
    #[test_case("const o = {length: 5};\nconst a = o.length;", LENGTHLESS, 2 ; "length_of_an_object")]
    #[test_case("const o = {a: 1};\nconst a = [...o];", SPREAD_OBJECT, 2 ; "object_name_spread_in_an_array")]
    #[test_case("const a = 1;\nconst s = `${{a}}`;", OBJECT_TEXT, 2 ; "object_in_a_template")]
    #[test_case("const r = await gh.get();\nconst a = r.a ?? r.b ?? 1;", NULLISH_CHAINED, 2 ; "nullish_after_nullish")]
    #[test_case("const n = 1;\nconsole.log(\"%s items\", n);", "`console.log()` with a `%` format", 2 ; "console_format")]
    #[test_case("const s = \"a\";\nconst t = \",\";\nconst a = s.split(t);", SPLIT_SEPARATOR, 3 ; "split_by_a_name")]
    #[test_case("const a = JSON.stringify({}, null, 0);", INDENT_WIDTH, 1 ; "stringify_indent_zero")]
    #[test_case("const a = JSON.stringify({}, null, \"  \");", INDENT_WIDTH, 1 ; "stringify_indent_string")]
    #[test_case("const xs = [1];\nconst out = [];\nxs.forEach(x => { out.push(x); });", CALLBACK_MUTATION, 3 ; "for_each_pushing_outside")]
    #[test_case("const xs = [1];\nconst o = {};\nconst a = xs.map(x => { o.n = x; return x; });", CALLBACK_MUTATION, 3 ; "map_assigning_outside")]
    #[test_case("const xs = [[1, 2]];\nfor (const [a, b] of xs) { console.log(a); }", ENTRY_PATTERN, 2 ; "pair_pattern_over_a_list")]
    #[test_case("async function f() { const a = await search(\"q\"); return a; }", AWAIT_IMMEDIATE, 1 ; "await_immediate_in_a_function")]
    #[test_case("const xs = [1];\nconst a = await Promise.all(xs.map(async x => { return await gh.list(); }));", GATHERED, 2 ; "gather_async_callback")]
    #[test_case("async function f() { return await gh.list(); }\nconst a = await Promise.all([f()]);", GATHERED, 2 ; "gather_local_function")]
    #[test_case("const a = await Promise.all([gh.get({id: search(\"q\")})]);", GATHERED_GET, 1 ; "gather_argument_calls")]
    #[test_case("const xs = [{tags: []}];\nconst a = await Promise.all(xs.map(x => gh.get({id: x.tags.length})));", GATHERED_GET, 2 ; "gather_argument_computes")]
    #[test_case("const xs = [1];\nconst calls = [];\nfor (const x of xs) { calls.push(gh.get({id: search(\"q\")})); }\nawait Promise.all(calls);", GATHERED_GET, 3 ; "batch_argument_calls")]
    #[test_case("const a = (await Promise.all([gh.list()])).length;", GATHER_PLACE, 1 ; "gather_inside_an_expression")]
    #[test_case("const xs = [1];\nif ((await Promise.all([gh.list()])).length > 0) { console.log(1); }", GATHER_PLACE, 2 ; "gather_in_a_condition")]
    #[test_case("const a = gh.list();", UNAWAITED_LIST, 1 ; "unawaited_host_call")]
    #[test_case("const xs = [\"a\"];\nconst ps = Promise.all(xs.map(x => gh.list({owner: x})));", UNAWAITED_GATHER, 2 ; "unawaited_gather")]
    #[test_case("async function f() { return 1; }\nconst r = f();", UNAWAITED_LOCAL, 2 ; "unawaited_local_async_function")]
    #[test_case("const calls = [];\ncalls.push(gh.list());\nconsole.log(calls.length);\nawait Promise.all(calls);", UNAWAITED_LIST, 2 ; "batch_read_before_its_gather")]
    #[test_case("const calls = [];\ncalls.push(gh.list());\nif (calls.length > 0) { console.log(1); }", UNAWAITED_LIST, 2 ; "batch_never_gathered")]
    #[test_case("const a = await Promise.all([gh.list(), search(\"x\")]);", GATHERED, 1 ; "gather_of_an_immediate_call")]
    #[test_case("const a = await Promise.allSettled([gh.list()]);", "`Promise.allSettled()`", 1 ; "all_settled")]
    #[test_case("const a = await Promise.race([gh.list()]);", "`Promise.race()`", 1 ; "race")]
    #[test_case("const xs = [1];\nxs.forEach(async x => { await gh.list(); });", "an `async` callback to `.forEach()`", 2 ; "async_for_each")]
    #[test_case("const fs = [() => 1];", "a function used as a value", 1 ; "closure_in_an_array")]
    #[test_case("const xs = [1];\nconst a = xs.map(x => await gh.list());", AWAIT_HERE, 2 ; "await_in_an_inline_callback")]
    #[test_case("const r = await gh.get();\nconst a = r.name || \"anon\";", OR_VALUE, 2 ; "or_with_a_fallback_that_is_not_empty")]
    #[test_case("const r = await gh.get();\nconst a = r.items || [];", OR_VALUE, 2 ; "or_with_an_empty_fallback")]
    #[test_case("const a = [1, , 2];", HOLE, 1 ; "array_hole")]
    #[test_case("const a = Math.round(1.5);", "`Math.round()`", 1 ; "math_round")]
    #[test_case("const a = Math.max(1);", "`Math.max()` of one value", 1 ; "math_max_of_one_value")]
    #[test_case("const n = 1.5;\nconst a = n.toFixed(2);", "`.toFixed()`", 2 ; "to_fixed")]
    #[test_case("const a = JSON.stringify({}, (k, v) => v);", REPLACER, 1 ; "stringify_replacer")]
    #[test_case("const s = \"a\";\nconst a = s.replace(/a/g, \"b\");", "a regular expression", 2 ; "regex_replace")]
    #[test_case("const a = typeof 1;", "`typeof`", 1 ; "typeof")]
    #[test_case("const o = {};\ndelete o.a;", "`delete`", 2 ; "delete")]
    #[test_case("const a = new Date();", "`new`", 1 ; "new_date")]
    #[test_case("const a = Boolean(1);", "`Boolean()`", 1 ; "boolean")]
    #[test_case("const xs = [1];\nconst a = xs.reduce((s, x) => s + x, 0);", "`.reduce()`", 2 ; "reduce")]
    #[test_case("const xs = [1];\nxs.sort();", "`.sort()`", 2 ; "sort")]
    #[test_case("const xs = [1];\nxs.splice(0, 1);", "`.splice()`", 2 ; "splice")]
    #[test_case("const xs = [1];\nconst a = xs.indexOf(1);", "`.indexOf()`", 2 ; "index_of")]
    #[test_case("let i = 0;\nconst a = i++;", "`++` or `--` inside an expression", 2 ; "update_in_an_expression")]
    #[test_case("let i = 0;\nconst a = (i = 2);", "an assignment inside an expression", 2 ; "assignment_in_an_expression")]
    #[test_case("const a = (1, 2);", "the comma operator", 1 ; "comma_operator")]
    #[test_case("const o = {};\nconst a = \"k\" in o;", "`in`", 2 ; "in_operator")]
    #[test_case("const o = {};\nconst a = o instanceof Object;", "`instanceof`", 2 ; "instanceof")]
    #[test_case("const a = this;", "`this`", 1 ; "this")]
    #[test_case("const a = 10n;", "the number `10n`", 1 ; "bigint")]
    #[test_case("const f = 1;\n\nf?.();", OPTIONAL_CALL, 3 ; "optional_call")]
    fn expression_refuses(js: &str, construct: &str, line: usize) {
        let refusal = refused(js);
        assert_eq!(
            (refusal.construct.to_string().as_str(), refusal.line),
            (construct, line)
        );
    }
}
