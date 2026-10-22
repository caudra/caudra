//! The Python a translation writes, and its one printer. Every precedence, quote, escape and indent
//! decision lives here, so arms build trees and never format text.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::ops::RangeInclusive;

use super::truthiness::SafeTest;
use super::{Translation, expressions};

const INDENT: &str = "    ";
const CAUDRA_PREFIX: &str = "_caudra_";
const CALLBACK_PREFIX: &str = "_caudra_callback_";
const VALUE_PREFIX: &str = "_caudra_value_";
const ITEM: &str = "_caudra_item";
const DOUBLE_QUOTE: char = '"';
const SINGLE_QUOTE: char = '\'';
const PASS: &str = "pass";
const COMMENT_MARK: &str = "#";
const ZERO_WIDTH_NO_BREAK_SPACE: char = '\u{feff}';
/// The Latin-1 and Latin Extended-A letters a name may hold besides ASCII ones. Python reads an
/// identifier in NFKC form, so only letters that form leaves alone keep two names apart.
const LATIN_LETTERS: RangeInclusive<char> = '\u{c0}'..='\u{17e}';
/// `×`, `÷`, and the letters NFKC rewrites: `Ĳ`, `ĳ`, `Ŀ`, `ŀ` and `ŉ`.
const NOT_LATIN_LETTERS: [char; 7] = [
    '\u{d7}', '\u{f7}', '\u{132}', '\u{133}', '\u{13f}', '\u{140}', '\u{149}',
];
/// Python's hard keywords: none can name a variable.
const KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// The translated snippet. Imports are not stored: the printer derives them from the body.
pub(super) struct Module {
    pub(super) body: Vec<Statement>,
}

/// A module whose every deferred call was proven consumed. Only [`CheckedModule::consumed`] makes
/// one, and only a checked module prints.
pub(super) struct CheckedModule(Module);

impl CheckedModule {
    pub(super) fn consumed(module: Module) -> Translation<Self> {
        expressions::consumed(&module.body)?;
        Ok(Self(module))
    }
}

#[derive(Clone)]
pub(super) enum Statement {
    Comment(String),
    Assign {
        target: Target,
        value: Expression,
    },
    AugAssign {
        target: Target,
        operator: Operator,
        value: Expression,
    },
    /// `list.append(value)`, kept apart so a batch of calls pushed onto a list is recognisable.
    Append {
        list: Expression,
        value: Expression,
    },
    Expression(Expression),
    /// `otherwise` holding one `If` prints as `elif`.
    If {
        test: SafeTest,
        body: Vec<Statement>,
        otherwise: Vec<Statement>,
    },
    While {
        test: SafeTest,
        body: Vec<Statement>,
    },
    For {
        target: Target,
        iterable: Expression,
        body: Vec<Statement>,
    },
    /// `try:` and `finally:`. A `catch` refuses: a missing key raises in Python where JavaScript
    /// reads `undefined`, so a handler would run where JavaScript's does not.
    Try {
        body: Vec<Statement>,
        finally: Vec<Statement>,
    },
    Def(Def),
    Return(Option<Expression>),
    Raise(Expression),
    Break,
    Continue,
}

#[derive(Clone)]
pub(super) struct Def {
    pub(super) name: Name,
    pub(super) is_async: bool,
    pub(super) parameters: Vec<Parameter>,
    pub(super) body: Vec<Statement>,
}

#[derive(Clone)]
pub(super) struct Parameter {
    pub(super) name: Name,
    /// A literal, maybe negated: Python evaluates a default once, where JavaScript does at every call.
    pub(super) default: Option<Expression>,
}

#[derive(Clone)]
pub(super) enum Target {
    Name(Name),
    Tuple(Vec<Name>),
    Subscript {
        value: Box<Expression>,
        index: Box<Expression>,
    },
}

#[derive(Clone)]
pub(super) enum Expression {
    Name(Name),
    Constant(Constant),
    FString(Vec<Piece>),
    List(Vec<Item>),
    /// Only ever of two or more items, so no trailing comma is needed.
    Tuple(Vec<Expression>),
    Dict(Vec<Entry>),
    Subscript {
        value: Box<Expression>,
        index: Box<Expression>,
    },
    Slice {
        value: Box<Expression>,
        lower: Option<Box<Expression>>,
        upper: Option<Box<Expression>>,
    },
    Call(Call),
    Await(Box<Expression>),
    /// `asyncio.gather(...)`, kept apart until the consumer proof has read it.
    Gather(Gather),
    Negative(Box<Expression>),
    Not(Box<SafeTest>),
    Binary {
        left: Box<Expression>,
        operator: Operator,
        right: Box<Expression>,
    },
    Compare {
        left: Box<Expression>,
        comparison: Comparison,
        right: Box<Expression>,
    },
    Logical {
        left: Box<SafeTest>,
        operator: Logic,
        right: Box<SafeTest>,
    },
    IfElse {
        test: Box<SafeTest>,
        body: Box<Expression>,
        orelse: Box<Expression>,
    },
    /// `value if c1 is not None and ... else fallback`, for `?.` and `??`. Every check only reads
    /// names and keys, so reading it twice is safe.
    Guarded {
        checks: Vec<Expression>,
        value: Box<Expression>,
        fallback: Box<Expression>,
    },
    Comprehension(Box<Comprehension>),
}

#[derive(Clone)]
pub(super) struct Call {
    pub(super) callee: Callee,
    pub(super) arguments: Vec<Argument>,
    /// Where the call was written, for a refusal of it.
    pub(super) line: usize,
}

#[derive(Clone)]
pub(super) enum Callee {
    /// A host function, `search`, or a namespace member, `gh.list`. A deferred one is awaitable.
    Host {
        namespace: Option<Name>,
        name: Name,
        deferred: bool,
    },
    /// The snippet's own function, or a lifted callback.
    Local {
        name: Name,
        is_async: bool,
    },
    Builtin(Builtin),
    Imported(Import, &'static str),
    Method {
        receiver: Box<Expression>,
        name: &'static str,
    },
}

#[derive(Clone)]
pub(super) enum Argument {
    Positional(Expression),
    Spread(Expression),
    Keyword(Name, Expression),
}

#[derive(Clone)]
pub(super) struct Gather {
    pub(super) items: Gathered,
    pub(super) line: usize,
}

#[derive(Clone)]
pub(super) enum Gathered {
    Elements(Vec<Expression>),
    Spread(Box<Expression>),
}

#[derive(Clone)]
pub(super) enum Constant {
    None,
    True,
    False,
    /// Already a valid Python number: no BigInt, no legacy octal.
    Number(String),
    Str(String),
}

#[derive(Clone)]
pub(super) enum Piece {
    Text(String),
    Value(Expression),
}

#[derive(Clone)]
pub(super) enum Item {
    One(Expression),
    Spread(Expression),
}

#[derive(Clone)]
pub(super) enum Entry {
    Pair(Expression, Expression),
    Spread(Expression),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Operator {
    Add,
    Sub,
    Mult,
    Div,
    Mod,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Comparison {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    Is,
    IsNot,
    In,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Logic {
    And,
    Or,
}

#[derive(Clone)]
pub(super) struct Comprehension {
    pub(super) collected: Collected,
    pub(super) element: Expression,
    pub(super) clauses: Vec<Clause>,
}

#[derive(Clone)]
pub(super) enum Clause {
    For {
        target: Target,
        iterable: Expression,
    },
    If(SafeTest),
}

/// `[...]`, or a generator, printed bare when it is a call's only argument.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Collected {
    List,
    Generator,
}

/// The builtins the translation calls. A snippet naming one of these refuses, so a call the
/// translation writes always reaches the builtin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Builtin {
    Print,
    Len,
    List,
    Dict,
    Str,
    Float,
    Int,
    Max,
    Min,
    Abs,
    Isinstance,
    Next,
    Any,
    All,
    Enumerate,
    Range,
    Exception,
}

impl Builtin {
    pub(super) const ALL: [Self; 17] = [
        Self::Print,
        Self::Len,
        Self::List,
        Self::Dict,
        Self::Str,
        Self::Float,
        Self::Int,
        Self::Max,
        Self::Min,
        Self::Abs,
        Self::Isinstance,
        Self::Next,
        Self::Any,
        Self::All,
        Self::Enumerate,
        Self::Range,
        Self::Exception,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::Len => "len",
            Self::List => "list",
            Self::Dict => "dict",
            Self::Str => "str",
            Self::Float => "float",
            Self::Int => "int",
            Self::Max => "max",
            Self::Min => "min",
            Self::Abs => "abs",
            Self::Isinstance => "isinstance",
            Self::Next => "next",
            Self::Any => "any",
            Self::All => "all",
            Self::Enumerate => "enumerate",
            Self::Range => "range",
            Self::Exception => "Exception",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Import {
    Asyncio,
    Json,
    Math,
}

impl Import {
    pub(super) const ALL: [Self; 3] = [Self::Asyncio, Self::Json, Self::Math];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Asyncio => "asyncio",
            Self::Json => "json",
            Self::Math => "math",
        }
    }
}

/// A Python identifier that is no keyword and does not start with [`CAUDRA_PREFIX`], unless the
/// translation made it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Name(String);

impl Name {
    pub(super) fn parse(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let valid = chars
            .next()
            .is_some_and(|first| is_latin_letter(first) || first == '_')
            && chars.all(|next| is_latin_letter(next) || next.is_ascii_digit() || next == '_')
            && !KEYWORDS.contains(&text)
            && !text.starts_with(CAUDRA_PREFIX);
        valid.then(|| Self(text.to_owned()))
    }

    /// `_caudra_callback_{index}`, the def a callback is lifted to.
    pub(super) fn callback(index: usize) -> Self {
        Self(format!("{CALLBACK_PREFIX}{index}"))
    }

    /// Whether this names a callback the translation lifted, which no snippet name can.
    pub(super) fn is_callback(&self) -> bool {
        self.0.starts_with(CALLBACK_PREFIX)
    }

    /// `_caudra_value_{index}`, holding an object a pattern reads several keys from.
    pub(super) fn temporary(index: usize) -> Self {
        Self(format!("{VALUE_PREFIX}{index}"))
    }

    /// The variable of a comprehension the translation adds, as `join`'s and `flatMap`'s.
    pub(super) fn item() -> Self {
        Self(ITEM.to_owned())
    }

    /// A name the translation writes itself, as the keyword `indent` or the builtin `list`.
    pub(super) fn fixed(name: &'static str) -> Self {
        Self(name.to_owned())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Expression {
    pub(super) fn call(callee: Callee, arguments: Vec<Expression>, line: usize) -> Self {
        Self::Call(Call {
            callee,
            arguments: arguments.into_iter().map(Argument::Positional).collect(),
            line,
        })
    }

    pub(super) fn string(text: impl Into<String>) -> Self {
        Self::Constant(Constant::Str(text.into()))
    }

    /// Whether evaluating it twice is the same as once: a name, and subscripts, `len` and string
    /// keys over one, with no call that could act.
    pub(super) fn is_repeatable(&self) -> bool {
        match self {
            Self::Name(_) | Self::Constant(_) => true,
            Self::Subscript { value, index } => value.is_repeatable() && index.is_repeatable(),
            Self::Call(Call {
                callee: Callee::Builtin(Builtin::Len),
                arguments,
                ..
            }) => arguments.iter().all(
                |argument| matches!(argument, Argument::Positional(value) if value.is_repeatable()),
            ),
            _ => false,
        }
    }

    /// Whether evaluating it may run code: a call, an `await` or a comprehension anywhere in it.
    pub(super) fn has_call(&self) -> bool {
        matches!(
            self,
            Self::Call(_) | Self::Await(_) | Self::Gather(_) | Self::Comprehension(_)
        ) || self.children().into_iter().any(Self::has_call)
    }

    /// Every expression directly inside this one, tests included.
    pub(super) fn children(&self) -> Vec<&Expression> {
        match self {
            Self::Name(_) | Self::Constant(_) => Vec::new(),
            Self::FString(pieces) => pieces
                .iter()
                .filter_map(|piece| match piece {
                    Piece::Value(value) => Some(value),
                    Piece::Text(_) => None,
                })
                .collect(),
            Self::List(items) => items
                .iter()
                .map(|item| match item {
                    Item::One(value) | Item::Spread(value) => value,
                })
                .collect(),
            Self::Tuple(values) => values.iter().collect(),
            Self::Dict(entries) => entries
                .iter()
                .flat_map(|entry| match entry {
                    Entry::Pair(key, value) => vec![key, value],
                    Entry::Spread(value) => vec![value],
                })
                .collect(),
            Self::Subscript { value, index } => vec![value, index],
            Self::Slice {
                value,
                lower,
                upper,
            } => std::iter::once(&**value)
                .chain(lower.as_deref())
                .chain(upper.as_deref())
                .collect(),
            Self::Call(call) => call.children(),
            Self::Await(value) | Self::Negative(value) => vec![value],
            Self::Gather(gather) => match &gather.items {
                Gathered::Elements(values) => values.iter().collect(),
                Gathered::Spread(value) => vec![value],
            },
            Self::Not(test) => vec![test.expression()],
            Self::Binary { left, right, .. } | Self::Compare { left, right, .. } => {
                vec![left, right]
            }
            Self::Logical { left, right, .. } => vec![left.expression(), right.expression()],
            Self::IfElse { test, body, orelse } => vec![test.expression(), body, orelse],
            Self::Guarded {
                checks,
                value,
                fallback,
            } => checks.iter().chain([&**value, &**fallback]).collect(),
            Self::Comprehension(comprehension) => std::iter::once(&comprehension.element)
                .chain(comprehension.clauses.iter().map(Clause::expression))
                .collect(),
        }
    }
}

impl Call {
    pub(super) fn children(&self) -> Vec<&Expression> {
        let receiver = match &self.callee {
            Callee::Method { receiver, .. } => Some(&**receiver),
            _ => None,
        };
        receiver
            .into_iter()
            .chain(self.arguments.iter().map(|argument| match argument {
                Argument::Positional(value)
                | Argument::Spread(value)
                | Argument::Keyword(_, value) => value,
            }))
            .collect()
    }

    /// Whether Python starts its work only once it is awaited: a deferred host call or the
    /// snippet's own async function.
    pub(super) fn is_deferred(&self) -> bool {
        matches!(
            self.callee,
            Callee::Host { deferred: true, .. } | Callee::Local { is_async: true, .. }
        )
    }

    /// The callee as written, `gh.list` or `fetch`, for a refusal.
    pub(super) fn written(&self) -> String {
        match &self.callee {
            Callee::Host {
                namespace: Some(namespace),
                name,
                ..
            } => format!("{}.{}()", namespace.as_str(), name.as_str()),
            Callee::Host { name, .. } | Callee::Local { name, .. } => {
                format!("{}()", name.as_str())
            }
            Callee::Builtin(builtin) => format!("{}()", builtin.name()),
            Callee::Imported(module, name) => format!("{}.{name}()", module.name()),
            Callee::Method { name, .. } => format!(".{name}()"),
        }
    }
}

impl Clause {
    fn expression(&self) -> &Expression {
        match self {
            Self::For { iterable, .. } => iterable,
            Self::If(test) => test.expression(),
        }
    }
}

impl Target {
    pub(super) fn expressions(&self) -> Vec<&Expression> {
        match self {
            Self::Name(_) | Self::Tuple(_) => Vec::new(),
            Self::Subscript { value, index } => vec![value, index],
        }
    }

    pub(super) fn names(&self) -> Vec<&Name> {
        match self {
            Self::Name(name) => vec![name],
            Self::Tuple(names) => names.iter().collect(),
            Self::Subscript { .. } => Vec::new(),
        }
    }
}

impl Statement {
    /// Every expression directly in this statement, outside its nested blocks.
    pub(super) fn expressions(&self) -> Vec<&Expression> {
        match self {
            Self::Comment(_) | Self::Def(_) | Self::Break | Self::Continue | Self::Return(None) => {
                Vec::new()
            }
            Self::Assign { target, value } | Self::AugAssign { target, value, .. } => {
                target.expressions().into_iter().chain([value]).collect()
            }
            Self::Append { list, value } => vec![list, value],
            Self::Expression(value) | Self::Return(Some(value)) | Self::Raise(value) => {
                vec![value]
            }
            Self::If { test, .. } | Self::While { test, .. } => vec![test.expression()],
            Self::For {
                target, iterable, ..
            } => target.expressions().into_iter().chain([iterable]).collect(),
            Self::Try { .. } => Vec::new(),
        }
    }

    /// The blocks nested in this statement.
    pub(super) fn blocks(&self) -> Vec<&[Statement]> {
        match self {
            Self::If {
                body, otherwise, ..
            } => vec![body, otherwise],
            Self::While { body, .. } | Self::For { body, .. } => vec![body],
            Self::Try { body, finally } => vec![body, finally],
            Self::Def(def) => vec![&def.body],
            _ => Vec::new(),
        }
    }

    /// The names it binds, for counting every mention of a batch's list.
    pub(super) fn targets(&self) -> Vec<&Name> {
        match self {
            Self::Assign { target, .. }
            | Self::AugAssign { target, .. }
            | Self::For { target, .. } => target.names(),
            _ => Vec::new(),
        }
    }
}

/// Visits every statement in `body`, those in nested blocks included, each before its blocks.
pub(super) fn each_statement<'a>(body: &'a [Statement], visit: &mut impl FnMut(&'a Statement)) {
    for statement in body {
        visit(statement);
        for block in statement.blocks() {
            each_statement(block, visit);
        }
    }
}

/// Visits every expression in `body`, nested blocks and nested expressions included.
pub(super) fn walk<'a>(body: &'a [Statement], visit: &mut impl FnMut(&'a Expression)) {
    fn expression<'a>(value: &'a Expression, visit: &mut impl FnMut(&'a Expression)) {
        visit(value);
        if let Expression::Comprehension(comprehension) = value {
            for clause in &comprehension.clauses {
                if let Clause::For { target, .. } = clause {
                    for inner in target.expressions() {
                        expression(inner, visit);
                    }
                }
            }
        }
        for child in value.children() {
            expression(child, visit);
        }
    }
    for statement in body {
        for value in statement.expressions() {
            expression(value, visit);
        }
        for block in statement.blocks() {
            walk(block, visit);
        }
    }
}

/// The program as Python source: an `import` line for each module the body uses, in [`Import`]
/// order, then the body with 4-space indents and `pass` in an empty block.
pub(super) fn printed(module: &CheckedModule) -> String {
    let mut imports = BTreeSet::new();
    walk(&module.0.body, &mut |value| match value {
        Expression::Gather(_) => {
            imports.insert(Import::Asyncio);
        }
        Expression::Call(Call {
            callee: Callee::Imported(import, _),
            ..
        }) => {
            imports.insert(*import);
        }
        _ => {}
    });
    let mut printer = Printer::default();
    for import in imports {
        printer.line(0, &format!("import {}", import.name()));
    }
    printer.block(&module.0.body, 0);
    printer.output.truncate(printer.output.trim_end().len());
    printer.output
}

/// Python's binding strengths, loosest first. A child prints in parentheses when it binds looser
/// than its place needs. A comparison's operands need more than a comparison, so two never chain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    IfElse,
    Or,
    And,
    Not,
    Compare,
    Sum,
    Product,
    Unary,
    Await,
    Primary,
    Atom,
}

#[derive(Default)]
struct Printer {
    output: String,
    /// How many f-string fields enclose what is printed now, which picks its quote.
    fields: usize,
}

impl Printer {
    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.output.push_str(INDENT);
        }
        self.output.push_str(text);
        self.output.push('\n');
    }

    fn block(&mut self, body: &[Statement], indent: usize) {
        for statement in body {
            self.statement(statement, indent);
        }
        if body
            .iter()
            .all(|statement| matches!(statement, Statement::Comment(_)))
            && indent > 0
        {
            self.line(indent, PASS);
        }
    }

    fn statement(&mut self, statement: &Statement, indent: usize) {
        match statement {
            Statement::Comment(text) => {
                for line in text.lines() {
                    let line = line.trim_end();
                    let separator = if line.is_empty() { "" } else { " " };
                    self.line(indent, &format!("{COMMENT_MARK}{separator}{line}"));
                }
            }
            Statement::Assign { target, value } => {
                let text = format!(
                    "{} = {}",
                    self.target(target),
                    self.expression(value, Precedence::IfElse)
                );
                self.line(indent, &text);
            }
            Statement::AugAssign {
                target,
                operator,
                value,
            } => {
                let text = format!(
                    "{} {}= {}",
                    self.target(target),
                    operator.symbol(),
                    self.expression(value, Precedence::IfElse)
                );
                self.line(indent, &text);
            }
            Statement::Append { list, value } => {
                let text = format!(
                    "{}.append({})",
                    self.expression(list, Precedence::Primary),
                    self.expression(value, Precedence::IfElse)
                );
                self.line(indent, &text);
            }
            Statement::Expression(value) => {
                let text = self.expression(value, Precedence::IfElse);
                self.line(indent, &text);
            }
            Statement::If {
                test,
                body,
                otherwise,
            } => self.conditional("if", test, body, otherwise, indent),
            Statement::While { test, body } => {
                let text = format!("while {}:", self.test(test));
                self.line(indent, &text);
                self.block(body, indent + 1);
            }
            Statement::For {
                target,
                iterable,
                body,
            } => {
                let text = format!(
                    "for {} in {}:",
                    self.target(target),
                    self.expression(iterable, Precedence::Or)
                );
                self.line(indent, &text);
                self.block(body, indent + 1);
            }
            Statement::Try { body, finally } => {
                self.line(indent, "try:");
                self.block(body, indent + 1);
                self.line(indent, "finally:");
                self.block(finally, indent + 1);
            }
            Statement::Def(def) => {
                let parameters = def
                    .parameters
                    .iter()
                    .map(|parameter| match &parameter.default {
                        Some(default) => format!(
                            "{}={}",
                            parameter.name.as_str(),
                            self.expression(default, Precedence::IfElse)
                        ),
                        None => parameter.name.as_str().to_owned(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let prefix = if def.is_async { "async def" } else { "def" };
                self.line(
                    indent,
                    &format!("{prefix} {}({parameters}):", def.name.as_str()),
                );
                self.block(&def.body, indent + 1);
            }
            Statement::Return(None) => self.line(indent, "return"),
            Statement::Return(Some(value)) => {
                let text = format!("return {}", self.expression(value, Precedence::IfElse));
                self.line(indent, &text);
            }
            Statement::Raise(value) => {
                let text = format!("raise {}", self.expression(value, Precedence::IfElse));
                self.line(indent, &text);
            }
            Statement::Break => self.line(indent, "break"),
            Statement::Continue => self.line(indent, "continue"),
        }
    }

    fn conditional(
        &mut self,
        keyword: &str,
        test: &SafeTest,
        body: &[Statement],
        otherwise: &[Statement],
        indent: usize,
    ) {
        let text = format!("{keyword} {}:", self.test(test));
        self.line(indent, &text);
        self.block(body, indent + 1);
        match otherwise {
            [] => {}
            [
                Statement::If {
                    test,
                    body,
                    otherwise,
                },
            ] => self.conditional("elif", test, body, otherwise, indent),
            _ => {
                self.line(indent, "else:");
                self.block(otherwise, indent + 1);
            }
        }
    }

    fn test(&mut self, test: &SafeTest) -> String {
        self.expression(test.expression(), Precedence::IfElse)
    }

    fn target(&mut self, target: &Target) -> String {
        match target {
            Target::Name(name) => name.as_str().to_owned(),
            Target::Tuple(names) => names
                .iter()
                .map(Name::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            Target::Subscript { value, index } => format!(
                "{}[{}]",
                self.expression(value, Precedence::Primary),
                self.expression(index, Precedence::IfElse)
            ),
        }
    }

    fn expression(&mut self, value: &Expression, needed: Precedence) -> String {
        let (text, precedence) = self.bare(value);
        if precedence < needed {
            format!("({text})")
        } else {
            text
        }
    }

    fn bare(&mut self, value: &Expression) -> (String, Precedence) {
        match value {
            Expression::Name(name) => (name.as_str().to_owned(), Precedence::Atom),
            Expression::Constant(constant) => (self.constant(constant), Precedence::Atom),
            Expression::FString(pieces) => (self.formatted(pieces), Precedence::Atom),
            Expression::List(items) => {
                let items = items
                    .iter()
                    .map(|item| match item {
                        Item::One(value) => self.expression(value, Precedence::IfElse),
                        Item::Spread(value) => {
                            format!("*{}", self.expression(value, Precedence::Sum))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                (format!("[{items}]"), Precedence::Atom)
            }
            Expression::Dict(entries) => {
                let entries = entries
                    .iter()
                    .map(|entry| match entry {
                        Entry::Pair(key, value) => format!(
                            "{}: {}",
                            self.expression(key, Precedence::IfElse),
                            self.expression(value, Precedence::IfElse)
                        ),
                        Entry::Spread(value) => {
                            format!("**{}", self.expression(value, Precedence::Sum))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                (format!("{{{entries}}}"), Precedence::Atom)
            }
            Expression::Tuple(values) => {
                let values = values
                    .iter()
                    .map(|value| self.expression(value, Precedence::IfElse))
                    .collect::<Vec<_>>()
                    .join(", ");
                (format!("({values})"), Precedence::Atom)
            }
            Expression::Subscript { value, index } => (
                format!(
                    "{}[{}]",
                    self.expression(value, Precedence::Primary),
                    self.expression(index, Precedence::IfElse)
                ),
                Precedence::Primary,
            ),
            Expression::Slice {
                value,
                lower,
                upper,
            } => {
                let mut bound = |bound: &Option<Box<Expression>>| {
                    bound
                        .as_deref()
                        .map(|bound| self.expression(bound, Precedence::IfElse))
                        .unwrap_or_default()
                };
                let (lower, upper) = (bound(lower), bound(upper));
                (
                    format!(
                        "{}[{lower}:{upper}]",
                        self.expression(value, Precedence::Primary)
                    ),
                    Precedence::Primary,
                )
            }
            Expression::Call(call) => (self.call(call), Precedence::Primary),
            Expression::Await(value) => (
                format!("await {}", self.expression(value, Precedence::Primary)),
                Precedence::Await,
            ),
            Expression::Gather(gather) => {
                let arguments = match &gather.items {
                    Gathered::Elements(values) => values
                        .iter()
                        .map(|value| self.expression(value, Precedence::IfElse))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Gathered::Spread(value) => {
                        format!("*{}", self.expression(value, Precedence::Sum))
                    }
                };
                (format!("asyncio.gather({arguments})"), Precedence::Primary)
            }
            Expression::Negative(value) => (
                format!("-{}", self.expression(value, Precedence::Unary)),
                Precedence::Unary,
            ),
            Expression::Not(test) => (
                format!(
                    "not {}",
                    self.expression(test.expression(), Precedence::Not)
                ),
                Precedence::Not,
            ),
            Expression::Binary {
                left,
                operator,
                right,
            } => {
                let (own, left_needs, right_needs) = match operator {
                    Operator::Add | Operator::Sub => {
                        (Precedence::Sum, Precedence::Sum, Precedence::Product)
                    }
                    Operator::Mult | Operator::Div | Operator::Mod => {
                        (Precedence::Product, Precedence::Product, Precedence::Unary)
                    }
                };
                (
                    format!(
                        "{} {} {}",
                        self.expression(left, left_needs),
                        operator.symbol(),
                        self.expression(right, right_needs)
                    ),
                    own,
                )
            }
            Expression::Compare {
                left,
                comparison,
                right,
            } => (
                format!(
                    "{} {} {}",
                    self.expression(left, Precedence::Sum),
                    comparison.symbol(),
                    self.expression(right, Precedence::Sum)
                ),
                Precedence::Compare,
            ),
            Expression::Logical {
                left,
                operator,
                right,
            } => {
                let (own, right_needs, word) = match operator {
                    Logic::And => (Precedence::And, Precedence::Not, "and"),
                    Logic::Or => (Precedence::Or, Precedence::And, "or"),
                };
                (
                    format!(
                        "{} {word} {}",
                        self.expression(left.expression(), own),
                        self.expression(right.expression(), right_needs)
                    ),
                    own,
                )
            }
            Expression::IfElse { test, body, orelse } => (
                format!(
                    "{} if {} else {}",
                    self.expression(body, Precedence::Or),
                    self.expression(test.expression(), Precedence::Or),
                    self.expression(orelse, Precedence::IfElse)
                ),
                Precedence::IfElse,
            ),
            Expression::Guarded {
                checks,
                value,
                fallback,
            } => {
                let checks = checks
                    .iter()
                    .map(|check| format!("{} is not None", self.expression(check, Precedence::Sum)))
                    .collect::<Vec<_>>()
                    .join(" and ");
                (
                    format!(
                        "{} if {checks} else {}",
                        self.expression(value, Precedence::Or),
                        self.expression(fallback, Precedence::IfElse)
                    ),
                    Precedence::IfElse,
                )
            }
            Expression::Comprehension(comprehension) => {
                let text = self.comprehension(comprehension);
                match comprehension.collected {
                    Collected::List => (format!("[{text}]"), Precedence::Atom),
                    Collected::Generator => (format!("({text})"), Precedence::Atom),
                }
            }
        }
    }

    fn comprehension(&mut self, comprehension: &Comprehension) -> String {
        let mut text = self.expression(&comprehension.element, Precedence::IfElse);
        for clause in &comprehension.clauses {
            match clause {
                Clause::For { target, iterable } => {
                    let target = self.target(target);
                    let iterable = self.expression(iterable, Precedence::Or);
                    let _ = write!(text, " for {target} in {iterable}");
                }
                Clause::If(test) => {
                    let test = self.expression(test.expression(), Precedence::Or);
                    let _ = write!(text, " if {test}");
                }
            }
        }
        text
    }

    fn call(&mut self, call: &Call) -> String {
        let function = match &call.callee {
            Callee::Host {
                namespace: Some(namespace),
                name,
                ..
            } => format!("{}.{}", namespace.as_str(), name.as_str()),
            Callee::Host { name, .. } | Callee::Local { name, .. } => name.as_str().to_owned(),
            Callee::Builtin(builtin) => builtin.name().to_owned(),
            Callee::Imported(module, name) => format!("{}.{name}", module.name()),
            Callee::Method { receiver, name } => {
                format!("{}.{name}", self.expression(receiver, Precedence::Primary))
            }
        };
        let arguments = match &call.arguments[..] {
            [Argument::Positional(Expression::Comprehension(comprehension))]
                if comprehension.collected == Collected::Generator =>
            {
                self.comprehension(comprehension)
            }
            arguments => arguments
                .iter()
                .map(|argument| match argument {
                    Argument::Positional(value) => self.expression(value, Precedence::IfElse),
                    Argument::Spread(value) => {
                        format!("*{}", self.expression(value, Precedence::Sum))
                    }
                    Argument::Keyword(name, value) => format!(
                        "{}={}",
                        name.as_str(),
                        self.expression(value, Precedence::IfElse)
                    ),
                })
                .collect::<Vec<_>>()
                .join(", "),
        };
        format!("{function}({arguments})")
    }

    fn constant(&self, constant: &Constant) -> String {
        match constant {
            Constant::None => "None".to_owned(),
            Constant::True => "True".to_owned(),
            Constant::False => "False".to_owned(),
            Constant::Number(number) => number.clone(),
            Constant::Str(text) => {
                let quote = self.quote();
                let mut quoted = String::from(quote);
                escape_into(&mut quoted, text, quote, false);
                quoted.push(quote);
                quoted
            }
        }
    }

    fn formatted(&mut self, pieces: &[Piece]) -> String {
        let quote = self.quote();
        let mut text = format!("f{quote}");
        for piece in pieces {
            match piece {
                Piece::Text(part) => escape_into(&mut text, part, quote, true),
                Piece::Value(value) => {
                    self.fields += 1;
                    let field = self.expression(value, Precedence::IfElse);
                    self.fields -= 1;
                    let spacer = if field.starts_with('{') { " " } else { "" };
                    let _ = write!(text, "{{{spacer}{field}{spacer}}}");
                }
            }
        }
        text.push(quote);
        text
    }

    /// Double quotes, and single inside an f-string's fields, so a field never closes its string.
    fn quote(&self) -> char {
        if self.fields.is_multiple_of(2) {
            DOUBLE_QUOTE
        } else {
            SINGLE_QUOTE
        }
    }
}

fn is_latin_letter(character: char) -> bool {
    character.is_ascii_alphabetic()
        || (LATIN_LETTERS.contains(&character) && !NOT_LATIN_LETTERS.contains(&character))
}

fn escape_into(output: &mut String, text: &str, quote: char, formatted: bool) {
    for character in text.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '{' if formatted => output.push_str("{{"),
            '}' if formatted => output.push_str("}}"),
            _ if character == quote => {
                output.push('\\');
                output.push(character);
            }
            _ if character.is_control()
                || (character.is_whitespace() && character != ' ')
                || character == ZERO_WIDTH_NO_BREAK_SPACE =>
            {
                let point = u32::from(character);
                let _ = if point <= 0xff {
                    write!(output, "\\x{point:02x}")
                } else {
                    write!(output, "\\u{point:04x}")
                };
            }
            _ => output.push(character),
        }
    }
}

impl Operator {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mult => "*",
            Self::Div => "/",
            Self::Mod => "%",
        }
    }
}

impl Comparison {
    fn symbol(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtE => "<=",
            Self::Gt => ">",
            Self::GtE => ">=",
            Self::Is => "is",
            Self::IsNot => "is not",
            Self::In => "in",
        }
    }
}
