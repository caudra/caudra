//! Statements: blocks with the callbacks their expressions lift placed before them, declarations,
//! loops, functions, and the snippet's last `return` as the value it ends on.

use tree_sitter::Node;

use super::expressions::{Called, Fact, Lowered, arithmetic, syntax};
use super::python::{
    Builtin, Call, Callee, Constant, Def, Expression, Gather, Gathered, Module, Name, Operator,
    Statement, Target, each_statement, walk,
};
use super::scope::{BindingKind, ERROR, Frame};
use super::syntax::{
    ALTERNATIVE_FIELD, ARGUMENT_FIELD, ARGUMENTS_FIELD, ARRAY_PATTERN, ARROW_FUNCTION,
    ASSIGNMENT_EXPRESSION, ASYNC, AUGMENTED_ASSIGNMENT_EXPRESSION, BODY_FIELD, BREAK_STATEMENT,
    CALL_EXPRESSION, COMMENT, CONDITION_FIELD, CONSEQUENCE_FIELD, CONST, CONSTRUCTOR_FIELD,
    CONTINUE_STATEMENT, EMPTY_STATEMENT, EXPRESSION_STATEMENT, FINALIZER_FIELD, FOR_IN_STATEMENT,
    FOR_STATEMENT, FUNCTION_DECLARATION, HANDLER_FIELD, IDENTIFIER, IF_STATEMENT, IN,
    INCREMENT_FIELD, INITIALIZER_FIELD, INTERFACE_DECLARATION, KEY_FIELD, KIND_FIELD, LEFT_FIELD,
    LET, LEXICAL_DECLARATION, MEMBER_EXPRESSION, NAME_FIELD, NEW, NEW_EXPRESSION, NUMBER,
    OBJECT_FIELD, OBJECT_PATTERN, OF, OPERATOR_FIELD, OPTIONAL_CHAIN_FIELD, PAIR_PATTERN,
    PROPERTY_FIELD, PROPERTY_IDENTIFIER, Parts, RETURN_STATEMENT, RETURN_TYPE_FIELD, RIGHT_FIELD,
    SEMICOLON, SHORTHAND_PROPERTY_IDENTIFIER_PATTERN, STATEMENT_BLOCK, STRING,
    SUBSCRIPT_EXPRESSION, TEMPLATE_STRING, THROW_STATEMENT, TRY_STATEMENT, TYPE_ALIAS_DECLARATION,
    TYPE_ARGUMENTS_FIELD, TYPE_FIELD, TYPE_PARAMETERS_FIELD, UPDATE_EXPRESSION, VALUE_FIELD, VAR,
    VARIABLE_DECLARATION, WHILE_STATEMENT, decoded, line, refused,
};
use super::{Translation, Translator};

const LINE_COMMENT: &str = "//";
const BLOCK_OPEN: &str = "/*";
const BLOCK_CLOSE: &str = "*/";
const BLOCK_LINE: char = '*';
const LESS: &str = "<";
const LESS_EQUAL: &str = "<=";
const LENGTH: &str = "length";
const INCREMENT: &str = "++";
const DECREMENT: &str = "--";
const ONE: &str = "1";
const ASSIGN: &str = "=";
const ADD_ASSIGN: &str = "+=";
const SUBTRACT_ASSIGN: &str = "-=";
const MULTIPLY_ASSIGN: &str = "*=";
const DIVIDE_ASSIGN: &str = "/=";
const MODULO_ASSIGN: &str = "%=";
const BREAK: &str = "break";
const CONTINUE: &str = "continue";
const AUGMENTED: [(&str, Operator); 5] = [
    (ADD_ASSIGN, Operator::Add),
    (SUBTRACT_ASSIGN, Operator::Sub),
    (MULTIPLY_ASSIGN, Operator::Mult),
    (DIVIDE_ASSIGN, Operator::Div),
    (MODULO_ASSIGN, Operator::Mod),
];

const RETURN_EARLY: &str = "`return` before the end of the snippet";
const RETURN_CALLBACK: &str = "`return` inside a `forEach` callback";
const JUMP: &str = "outside a loop";
const THROWN: &str = "`throw` of something other than `new Error()`";
const CATCH: &str = "`catch`, which a missing property would reach in Python only";
const FOR_IN: &str = "`for…in`";
const ASSIGNED: &str = "an assignment to";
const COUNTED: &str = "a `for` loop other than `for (let i = a; i < b; i++)`";
const BOUND: &str = "a `for` loop whose body mutates a value or may change its bound";
const LOOP_BOUND: &str =
    "a `for` loop bound other than a whole number, a `const` one, or a name's `.length`";
const EMPTY_PATTERN: &str = "an empty pattern";
const ARRAY_PATTERN_VALUE: &str =
    "an array pattern other than of `await Promise.all([…])` with as many calls as names";
const TARGET_CALL: &str = "an assignment to a target that calls something";

impl Translator<'_> {
    pub(super) fn module(&mut self, root: Node<'_>) -> Translation<Module> {
        let nodes = Parts::read(root, self.source, |parts| Ok(parts.statements()))?;
        self.final_return = nodes
            .iter()
            .rev()
            .find(|node| {
                !matches!(
                    node.kind(),
                    COMMENT | EMPTY_STATEMENT | INTERFACE_DECLARATION | TYPE_ALIAS_DECLARATION
                )
            })
            .filter(|node| node.kind() == RETURN_STATEMENT)
            .map(Node::id);
        self.scopes.enter(Frame::Module);
        let body = self.block(&nodes)?;
        Ok(Module { body })
    }

    pub(super) fn block(&mut self, nodes: &[Node<'_>]) -> Translation<Vec<Statement>> {
        let mut body = Vec::new();
        for node in nodes {
            body.extend(self.spliced(|translator| translator.statement(*node))?);
        }
        Ok(body)
    }

    /// What `lower` makes, after the callbacks it lifted: they must be defined before it runs.
    pub(super) fn spliced(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Translation<Vec<Statement>>,
    ) -> Translation<Vec<Statement>> {
        let outer = std::mem::take(&mut self.lifted);
        let lowered = lower(self);
        let mut body = std::mem::replace(&mut self.lifted, outer);
        body.extend(lowered?);
        Ok(body)
    }

    /// A statement or block nested in another, in a block scope of its own.
    fn nested(&mut self, node: Node<'_>) -> Translation<Vec<Statement>> {
        let nodes = if node.kind() == STATEMENT_BLOCK {
            Parts::read(node, self.source, |parts| Ok(parts.statements()))?
        } else {
            vec![node]
        };
        self.scoped(|translator| translator.block(&nodes))
    }

    /// What `lower` makes inside `frame`, which is left again whether it lowers or refuses.
    pub(super) fn framed<T>(
        &mut self,
        frame: Frame,
        lower: impl FnOnce(&mut Self) -> Translation<T>,
    ) -> Translation<T> {
        self.scopes.enter(frame);
        let lowered = lower(self);
        self.scopes.leave();
        lowered
    }

    /// What `lower` makes in a block scope of its own.
    pub(super) fn scoped<T>(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Translation<T>,
    ) -> Translation<T> {
        self.scopes.open();
        let lowered = lower(self);
        self.scopes.close();
        lowered
    }

    /// A loop's body, which `break` and `continue` reach.
    fn looped(&mut self, body: Node<'_>) -> Translation<Vec<Statement>> {
        self.scopes.enter_loop();
        let lowered = self.nested(body);
        self.scopes.leave_loop();
        lowered
    }

    fn statement(&mut self, node: Node<'_>) -> Translation<Vec<Statement>> {
        let source = self.source;
        match node.kind() {
            COMMENT => Ok(vec![Statement::Comment(comment(
                &source[node.byte_range()],
            ))]),
            EMPTY_STATEMENT | INTERFACE_DECLARATION | TYPE_ALIAS_DECLARATION => Ok(Vec::new()),
            EXPRESSION_STATEMENT => {
                let expression = Parts::read(node, source, |parts| parts.only())?;
                self.effect(expression)
            }
            LEXICAL_DECLARATION | VARIABLE_DECLARATION => self.declaration(node),
            STATEMENT_BLOCK => self.nested(node),
            IF_STATEMENT => Ok(vec![self.conditional(node)?]),
            FOR_IN_STATEMENT => Ok(vec![self.for_in(node)?]),
            FOR_STATEMENT => Ok(vec![self.counted(node)?]),
            WHILE_STATEMENT => Ok(vec![self.while_loop(node)?]),
            BREAK_STATEMENT | CONTINUE_STATEMENT => Ok(vec![self.jump(node)?]),
            TRY_STATEMENT => Ok(vec![self.attempt(node)?]),
            THROW_STATEMENT => Ok(vec![self.throw(node)?]),
            RETURN_STATEMENT => Ok(vec![self.returned(node)?]),
            FUNCTION_DECLARATION => Ok(vec![self.function(node)?]),
            _ => Err(refused(node, source)),
        }
    }

    /// An expression in statement position, where an assignment, `++` or `push` may stand.
    pub(super) fn effect(&mut self, node: Node<'_>) -> Translation<Vec<Statement>> {
        match node.kind() {
            ASSIGNMENT_EXPRESSION => Ok(vec![self.assignment(node)?]),
            AUGMENTED_ASSIGNMENT_EXPRESSION => Ok(vec![self.augmented(node)?]),
            UPDATE_EXPRESSION => Ok(vec![self.update(node)?]),
            CALL_EXPRESSION => match self.call(node, true)? {
                Called::Statement(statement) => Ok(vec![statement]),
                Called::Chain(chain) => Ok(vec![Statement::Expression(chain.closed().expression)]),
            },
            _ => Ok(vec![Statement::Expression(self.value(node)?.expression)]),
        }
    }

    fn declaration(&mut self, node: Node<'_>) -> Translation<Vec<Statement>> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let kind = if parts.token(VAR) {
                BindingKind::Let
            } else {
                match parts.required(KIND_FIELD)?.kind() {
                    CONST => BindingKind::Const,
                    LET => BindingKind::Let,
                    _ => return Err(parts.refused()),
                }
            };
            let mut statements = Vec::new();
            for declarator in parts.named() {
                statements.extend(self.declarator(declarator, kind)?);
            }
            Ok(statements)
        })
    }

    fn declarator(&mut self, node: Node<'_>, kind: BindingKind) -> Translation<Vec<Statement>> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let name = parts.required(NAME_FIELD)?;
            parts.field(TYPE_FIELD);
            let value = parts.field(VALUE_FIELD);
            match (name.kind(), value) {
                (IDENTIFIER, Some(arrow))
                    if kind == BindingKind::Const && arrow.kind() == ARROW_FUNCTION =>
                {
                    Ok(vec![self.arrow_definition(name, arrow)?])
                }
                (IDENTIFIER, _) => {
                    let lowered = match value {
                        Some(value) => self.value(value)?,
                        None => Lowered::new(Expression::Constant(Constant::None), Fact::Unknown),
                    };
                    let fact = if kind == BindingKind::Const {
                        lowered.fact
                    } else {
                        kept(node, &source[name.byte_range()], lowered.fact, source)
                    };
                    let declared = self.declare(name, kind, fact)?;
                    Ok(vec![Statement::Assign {
                        target: Target::Name(declared),
                        value: lowered.expression,
                    }])
                }
                (ARRAY_PATTERN, Some(value)) => {
                    let names = self.pattern_names(name)?;
                    if names.is_empty() {
                        return Err(syntax(EMPTY_PATTERN, name));
                    }
                    let lowered = self.value(value)?;
                    let gathered = matches!(&lowered.expression, Expression::Await(awaited)
                        if matches!(&**awaited, Expression::Gather(Gather {
                            items: Gathered::Elements(elements),
                            ..
                        }) if elements.len() == names.len()));
                    if !gathered {
                        return Err(syntax(ARRAY_PATTERN_VALUE, name));
                    }
                    let names = names
                        .into_iter()
                        .map(|name| self.declare(name, kind, Fact::Unknown))
                        .collect::<Translation<Vec<_>>>()?;
                    Ok(vec![Statement::Assign {
                        target: Target::Tuple(names),
                        value: lowered.expression,
                    }])
                }
                (OBJECT_PATTERN, Some(value)) => self.destructured(name, value, kind),
                _ => Err(refused(name, source)),
            }
        })
    }

    /// `const {a, b: c} = o` as one read of `o`, then each key.
    fn destructured(
        &mut self,
        pattern: Node<'_>,
        value: Node<'_>,
        kind: BindingKind,
    ) -> Translation<Vec<Statement>> {
        let source = self.source;
        let entries = Parts::read(pattern, source, |parts| {
            parts
                .named()
                .into_iter()
                .map(|entry| match entry.kind() {
                    SHORTHAND_PROPERTY_IDENTIFIER_PATTERN => {
                        Ok((source[entry.byte_range()].to_owned(), entry))
                    }
                    PAIR_PATTERN => Parts::read(entry, source, |pair| {
                        let key = pair.required(KEY_FIELD)?;
                        let name = pair.required(VALUE_FIELD)?;
                        let key = match key.kind() {
                            PROPERTY_IDENTIFIER => source[key.byte_range()].to_owned(),
                            STRING => decoded(key, source)?,
                            _ => return Err(refused(key, source)),
                        };
                        if name.kind() != IDENTIFIER {
                            return Err(refused(name, source));
                        }
                        Ok((key, name))
                    }),
                    _ => Err(refused(entry, source)),
                })
                .collect::<Translation<Vec<_>>>()
        })?;
        if entries.is_empty() {
            return Err(syntax(EMPTY_PATTERN, pattern));
        }
        let lowered = self.value(value)?;
        let fact = tainted(lowered.fact);
        let mut statements = Vec::new();
        let object = if entries.len() == 1 || lowered.expression.is_repeatable() {
            lowered.expression
        } else {
            let temporary = Name::temporary(self.temporaries);
            self.temporaries += 1;
            statements.push(Statement::Assign {
                target: Target::Name(temporary.clone()),
                value: lowered.expression,
            });
            Expression::Name(temporary)
        };
        for (key, name) in entries {
            let declared = self.declare(name, kind, fact)?;
            statements.push(Statement::Assign {
                target: Target::Name(declared),
                value: Expression::Subscript {
                    value: Box::new(object.clone()),
                    index: Box::new(Expression::string(key)),
                },
            });
        }
        Ok(statements)
    }

    /// `const f = (a) => ...` as `def f(a):`, the one place a function may be a value.
    fn arrow_definition(&mut self, name: Node<'_>, arrow: Node<'_>) -> Translation<Statement> {
        Parts::read(arrow, self.source, |parts| {
            let is_async = parts.token(ASYNC);
            parts.field(TYPE_PARAMETERS_FIELD);
            parts.field(RETURN_TYPE_FIELD);
            let parameters = self.parameters(parts)?;
            let body = parts.required(BODY_FIELD)?;
            let name = self.declare(name, BindingKind::Function { is_async }, Fact::Unknown)?;
            self.definition(name, is_async, &parameters, body)
        })
    }

    fn function(&mut self, node: Node<'_>) -> Translation<Statement> {
        Parts::read(node, self.source, |parts| {
            let is_async = parts.token(ASYNC);
            let name = parts.required(NAME_FIELD)?;
            parts.field(TYPE_PARAMETERS_FIELD);
            parts.field(RETURN_TYPE_FIELD);
            let parameters = self.parameters(parts)?;
            let body = parts.required(BODY_FIELD)?;
            let name = self.declare(name, BindingKind::Function { is_async }, Fact::Unknown)?;
            self.definition(name, is_async, &parameters, body)
        })
    }

    fn definition(
        &mut self,
        name: Name,
        is_async: bool,
        parameters: &[(Node<'_>, Option<Node<'_>>)],
        body: Node<'_>,
    ) -> Translation<Statement> {
        self.framed(Frame::Function { is_async }, |translator| {
            let parameters = translator.declared_parameters(parameters)?;
            let body = translator.function_body(body, is_async)?;
            Ok(Statement::Def(Def {
                name,
                is_async,
                parameters,
                body,
            }))
        })
    }

    fn conditional(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let condition = parts.required(CONDITION_FIELD)?;
            let consequence = parts.required(CONSEQUENCE_FIELD)?;
            let alternative = parts.field(ALTERNATIVE_FIELD);
            let test = self.test(condition)?;
            let body = self.nested(consequence)?;
            let otherwise = match alternative {
                Some(clause) => {
                    let inner = Parts::read(clause, source, |parts| parts.only())?;
                    self.nested(inner)?
                }
                None => Vec::new(),
            };
            Ok(Statement::If {
                test,
                body,
                otherwise,
            })
        })
    }

    fn for_in(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let kind = parts.required(KIND_FIELD)?;
            let left = parts.required(LEFT_FIELD)?;
            let operator = parts.required(OPERATOR_FIELD)?;
            let right = parts.required(RIGHT_FIELD)?;
            let body = parts.required(BODY_FIELD)?;
            let binding = match kind.kind() {
                CONST => BindingKind::Const,
                LET | VAR => BindingKind::Let,
                _ => return Err(refused(kind, source)),
            };
            match operator.kind() {
                OF => {}
                IN => return Err(syntax(FOR_IN, node)),
                _ => return Err(refused(operator, source)),
            }
            let iterable = self.value(right)?;
            self.scoped(|translator| {
                let target = translator.target(left, binding, &iterable.expression)?;
                let body = translator.looped(body)?;
                Ok(Statement::For {
                    target,
                    iterable: iterable.expression,
                    body,
                })
            })
        })
    }

    /// `for (let i = a; i < b; i++)` as `for i in range(a, b)`, `<=` as `range(a, b + 1)`, when
    /// `a` is a whole number literal and the body mutates nothing and reassigns nothing `b`
    /// reads, so Python's once-read bound is JavaScript's every-time one.
    fn counted(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        let unsupported = || syntax(COUNTED, node);
        Parts::read(node, source, |parts| {
            let initializer = parts.required(INITIALIZER_FIELD)?;
            let conditions = parts.fields(CONDITION_FIELD);
            let increment = parts.field(INCREMENT_FIELD).ok_or_else(unsupported)?;
            let body = parts.required(BODY_FIELD)?;
            let [condition, semicolon] = conditions[..] else {
                return Err(unsupported());
            };
            if semicolon.kind() != SEMICOLON || initializer.kind() != LEXICAL_DECLARATION {
                return Err(unsupported());
            }
            let (counter, start) = Parts::read(initializer, source, |declaration| {
                if declaration.required(KIND_FIELD)?.kind() != LET {
                    return Err(unsupported());
                }
                let [declarator] = declaration.named()[..] else {
                    return Err(unsupported());
                };
                Parts::read(declarator, source, |declarator| {
                    let name = declarator.required(NAME_FIELD)?;
                    declarator.field(TYPE_FIELD);
                    let value = declarator.required(VALUE_FIELD)?;
                    Ok((name, value))
                })
            })?;
            let written = &source[counter.byte_range()];
            let is_counter =
                |node: Node<'_>| node.kind() == IDENTIFIER && &source[node.byte_range()] == written;
            if !is_counter(counter) {
                return Err(unsupported());
            }
            let (bound, inclusive) = Parts::read(condition, source, |comparison| {
                let left = comparison.required(LEFT_FIELD)?;
                let operator = comparison.required(OPERATOR_FIELD)?;
                let right = comparison.required(RIGHT_FIELD)?;
                if !is_counter(left) || ![LESS, LESS_EQUAL].contains(&operator.kind()) {
                    return Err(unsupported());
                }
                Ok((right, operator.kind() == LESS_EQUAL))
            })?;
            if !self.is_step(increment, is_counter)? || start.kind() != NUMBER {
                return Err(unsupported());
            }
            let start = self.value(start)?;
            let bound = self.bound(bound)?;
            if start.fact != Fact::Count {
                return Err(unsupported());
            }
            let (name, body) = self.scoped(|translator| {
                let name = translator.declare(counter, BindingKind::Const, Fact::Count)?;
                Ok((name, translator.looped(body)?))
            })?;
            if !mutated(&body).is_empty() || changes(&body, &bound) {
                return Err(syntax(BOUND, node));
            }
            let bound = if inclusive {
                Expression::Binary {
                    left: Box::new(bound),
                    operator: Operator::Add,
                    right: Box::new(Expression::Constant(Constant::Number(ONE.to_owned()))),
                }
            } else {
                bound
            };
            Ok(Statement::For {
                target: Target::Name(name),
                iterable: Expression::call(
                    Callee::Builtin(Builtin::Range),
                    vec![start.expression, bound],
                    line(node),
                ),
                body,
            })
        })
    }

    /// A counted loop's bound: a whole number literal, a `const` whole number, or a name's
    /// `.length`.
    fn bound(&mut self, node: Node<'_>) -> Translation<Expression> {
        let source = self.source;
        let stable = match node.kind() {
            NUMBER => true,
            IDENTIFIER => self
                .scopes
                .resolve(&source[node.byte_range()])
                .is_some_and(|binding| binding.kind == BindingKind::Const),
            MEMBER_EXPRESSION => {
                node.child_by_field_name(OBJECT_FIELD)
                    .is_some_and(|object| object.kind() == IDENTIFIER)
                    && node
                        .child_by_field_name(PROPERTY_FIELD)
                        .is_some_and(|property| &source[property.byte_range()] == LENGTH)
            }
            _ => false,
        };
        let bound = self.value(node)?;
        if !stable || !bound.fact.is_whole() {
            return Err(syntax(LOOP_BOUND, node));
        }
        Ok(bound.expression)
    }

    /// Whether `node` adds one to the counter: `i++`, `++i` or `i += 1`.
    fn is_step(&self, node: Node<'_>, is_counter: impl Fn(Node<'_>) -> bool) -> Translation<bool> {
        let source = self.source;
        match node.kind() {
            UPDATE_EXPRESSION => Parts::read(node, source, |update| {
                let operator = update.required(OPERATOR_FIELD)?;
                let argument = update.required(ARGUMENT_FIELD)?;
                Ok(operator.kind() == INCREMENT && is_counter(argument))
            }),
            AUGMENTED_ASSIGNMENT_EXPRESSION => Parts::read(node, source, |augmented| {
                let left = augmented.required(LEFT_FIELD)?;
                let operator = augmented.required(OPERATOR_FIELD)?;
                let right = augmented.required(RIGHT_FIELD)?;
                Ok(is_counter(left)
                    && operator.kind() == ADD_ASSIGN
                    && right.kind() == NUMBER
                    && &source[right.byte_range()] == ONE)
            }),
            _ => Ok(false),
        }
    }

    fn while_loop(&mut self, node: Node<'_>) -> Translation<Statement> {
        Parts::read(node, self.source, |parts| {
            let condition = parts.required(CONDITION_FIELD)?;
            let body = parts.required(BODY_FIELD)?;
            let test = self.test(condition)?;
            let body = self.looped(body)?;
            Ok(Statement::While { test, body })
        })
    }

    fn jump(&mut self, node: Node<'_>) -> Translation<Statement> {
        let breaks = node.kind() == BREAK_STATEMENT;
        Parts::read(node, self.source, |_| {
            if !self.scopes.in_loop() {
                let keyword = if breaks { BREAK } else { CONTINUE };
                return Err(syntax(format!("`{keyword}` {JUMP}"), node));
            }
            Ok(if breaks {
                Statement::Break
            } else {
                Statement::Continue
            })
        })
    }

    fn attempt(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let body = parts.required(BODY_FIELD)?;
            if let Some(handler) = parts.field(HANDLER_FIELD) {
                return Err(syntax(CATCH, handler));
            }
            let finalizer = parts.required(FINALIZER_FIELD)?;
            let body = self.nested(body)?;
            let finally = Parts::read(finalizer, source, |clause| {
                let body = clause.required(BODY_FIELD)?;
                self.nested(body)
            })?;
            Ok(Statement::Try { body, finally })
        })
    }

    fn throw(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let thrown = parts.only()?;
            if thrown.kind() != NEW_EXPRESSION {
                return Err(syntax(THROWN, thrown));
            }
            Parts::read(thrown, source, |created| {
                created.token(NEW);
                let constructor = created.required(CONSTRUCTOR_FIELD)?;
                created.field(TYPE_ARGUMENTS_FIELD);
                let arguments = created.field(ARGUMENTS_FIELD);
                let is_error = constructor.kind() == IDENTIFIER
                    && &source[constructor.byte_range()] == ERROR
                    && self.scopes.resolve(ERROR).is_none();
                if !is_error {
                    return Err(syntax(THROWN, thrown));
                }
                let arguments = match arguments {
                    Some(arguments) => Parts::read(arguments, source, |list| Ok(list.named()))?,
                    None => Vec::new(),
                };
                let message = match arguments[..] {
                    [] => Vec::new(),
                    [message] => vec![self.value(message)?.expression],
                    _ => return Err(syntax(THROWN, thrown)),
                };
                Ok(Statement::Raise(Expression::call(
                    Callee::Builtin(Builtin::Exception),
                    message,
                    line(thrown),
                )))
            })
        })
    }

    fn returned(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let value = match parts.named()[..] {
                [] => None,
                [value] => Some(value),
                _ => return Err(parts.refused()),
            };
            if self.scopes.in_callback() {
                return Err(syntax(RETURN_CALLBACK, node));
            }
            match self.scopes.frame() {
                Frame::Module => match value {
                    Some(value) if self.final_return == Some(node.id()) => {
                        Ok(Statement::Expression(self.value(value)?.expression))
                    }
                    _ => Err(syntax(RETURN_EARLY, node)),
                },
                Frame::Predicate => Ok(Statement::Return(
                    value
                        .map(|value| self.test(value).map(|test| test.into_expression()))
                        .transpose()?,
                )),
                Frame::Function { .. } | Frame::Comprehension => Ok(Statement::Return(
                    value
                        .map(|value| self.value(value).map(|lowered| lowered.expression))
                        .transpose()?,
                )),
            }
        })
    }

    fn assignment(&mut self, node: Node<'_>) -> Translation<Statement> {
        Parts::read(node, self.source, |parts| {
            let left = parts.required(LEFT_FIELD)?;
            let right = parts.required(RIGHT_FIELD)?;
            let value = self.value(right)?;
            let (target, _) = self.assigned(left, value.fact)?;
            Ok(Statement::Assign {
                target,
                value: value.expression,
            })
        })
    }

    fn augmented(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let left = parts.required(LEFT_FIELD)?;
            let operator = parts.required(OPERATOR_FIELD)?;
            let right = parts.required(RIGHT_FIELD)?;
            let Some((_, kind)) = AUGMENTED
                .iter()
                .find(|(written, _)| *written == operator.kind())
            else {
                return Err(refused(operator, source));
            };
            let value = self.operand(right)?;
            let (target, fact) = self.assigned(left, Fact::Unknown)?;
            arithmetic(fact, *kind, &value, operator)?;
            Ok(Statement::AugAssign {
                target,
                operator: *kind,
                value: value.expression,
            })
        })
    }

    fn update(&mut self, node: Node<'_>) -> Translation<Statement> {
        let source = self.source;
        Parts::read(node, source, |parts| {
            let operator = parts.required(OPERATOR_FIELD)?;
            let argument = parts.required(ARGUMENT_FIELD)?;
            let kind = match operator.kind() {
                INCREMENT => Operator::Add,
                DECREMENT => Operator::Sub,
                _ => return Err(refused(operator, source)),
            };
            let one = Lowered::new(
                Expression::Constant(Constant::Number(ONE.to_owned())),
                Fact::Count,
            );
            let (target, fact) = self.assigned(argument, Fact::Unknown)?;
            arithmetic(fact, kind, &one, operator)?;
            Ok(Statement::AugAssign {
                target,
                operator: kind,
                value: one.expression,
            })
        })
    }

    /// The target of an assignment, and what it held: a `let` or parameter of this function, or
    /// a member or subscript of a value that calls nothing, as Python would call it after the
    /// value where JavaScript calls it before.
    fn assigned(&mut self, node: Node<'_>, fact: Fact) -> Translation<(Target, Fact)> {
        let source = self.source;
        match node.kind() {
            IDENTIFIER => {
                let written = &source[node.byte_range()];
                let name = Name::parse(written).filter(|_| self.scopes.assignable(written));
                let name = name.ok_or_else(|| syntax(format!("{ASSIGNED} `{written}`"), node))?;
                let held = self
                    .scopes
                    .resolve(written)
                    .map_or(Fact::Unknown, |binding| binding.fact);
                if !is_kept(held) {
                    self.scopes.forget(written, tainted(fact));
                }
                Ok((Target::Name(name), held))
            }
            MEMBER_EXPRESSION | SUBSCRIPT_EXPRESSION => {
                let lowered = self.value(node)?;
                let Expression::Subscript { value, index } = lowered.expression else {
                    return Err(refused(node, source));
                };
                if value.has_call() || index.has_call() {
                    return Err(syntax(TARGET_CALL, node));
                }
                Ok((Target::Subscript { value, index }, lowered.fact))
            }
            _ => Err(refused(node, source)),
        }
    }
}

/// What a `let` keeps of a value it is not known to keep: only whether it may be `undefined`.
fn tainted(fact: Fact) -> Fact {
    if fact == Fact::Optional {
        Fact::Optional
    } else {
        Fact::Unknown
    }
}

/// Whether a `let` known as `fact` stays so through every assignment, which [`kept`] proved.
fn is_kept(fact: Fact) -> bool {
    matches!(
        fact,
        Fact::Optional | Fact::Integer | Fact::Count | Fact::Str
    )
}

/// What a `let` named `written`, declared by `declarator` with a value known as `fact`, stays
/// known as through every assignment after it in its block: a whole number only set or stepped by
/// whole number literals or `.length`, or a string only set or extended by string literals and
/// templates.
fn kept(declarator: Node<'_>, written: &str, fact: Fact, source: &str) -> Fact {
    let Some(block) = declarator
        .parent()
        .and_then(|declaration| declaration.parent())
    else {
        return Fact::Unknown;
    };
    let mut fact = if fact.is_whole() || fact == Fact::Str {
        fact
    } else {
        return tainted(fact);
    };
    let mut pending = vec![block];
    while let Some(node) = pending.pop() {
        if node.end_byte() <= declarator.end_byte() {
            continue;
        }
        let target = match node.kind() {
            ASSIGNMENT_EXPRESSION | AUGMENTED_ASSIGNMENT_EXPRESSION => {
                node.child_by_field_name(LEFT_FIELD)
            }
            UPDATE_EXPRESSION => node.child_by_field_name(ARGUMENT_FIELD),
            _ => None,
        };
        if target.is_some_and(|target| &source[target.byte_range()] == written) {
            fact = stepped(node, fact, source);
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    fact
}

/// What a value known as `fact` is known as after `assignment` changes it.
fn stepped(assignment: Node<'_>, fact: Fact, source: &str) -> Fact {
    let operator = assignment
        .child_by_field_name(OPERATOR_FIELD)
        .map_or(ASSIGN, |operator| operator.kind());
    let by = assignment
        .child_by_field_name(RIGHT_FIELD)
        .map(|right| match right.kind() {
            NUMBER
                if source[right.byte_range()]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()) =>
            {
                Fact::Count
            }
            MEMBER_EXPRESSION if is_length(right, source) => Fact::Count,
            STRING | TEMPLATE_STRING => Fact::Str,
            _ => Fact::Unknown,
        });
    match (fact, operator, by) {
        (Fact::Count | Fact::Integer, INCREMENT, None) => fact,
        (Fact::Count | Fact::Integer, DECREMENT, None) => Fact::Integer,
        (Fact::Count | Fact::Integer, ASSIGN | ADD_ASSIGN | MULTIPLY_ASSIGN, Some(Fact::Count)) => {
            fact
        }
        (Fact::Count | Fact::Integer, SUBTRACT_ASSIGN, Some(Fact::Count)) => Fact::Integer,
        (Fact::Str, ASSIGN | ADD_ASSIGN, Some(Fact::Str)) => Fact::Str,
        _ => Fact::Unknown,
    }
}

/// Whether `member` reads `.length` without `?.`: translated as `len()`, it is a count wherever it
/// translates at all.
fn is_length(member: Node<'_>, source: &str) -> bool {
    member.child_by_field_name(OPTIONAL_CHAIN_FIELD).is_none()
        && member
            .child_by_field_name(PROPERTY_FIELD)
            .is_some_and(|property| &source[property.byte_range()] == LENGTH)
}

/// A comment's text, without its `//` or `/* */` and each block line's leading `*`.
fn comment(written: &str) -> String {
    if let Some(text) = written.strip_prefix(LINE_COMMENT) {
        return text.trim().to_owned();
    }
    let inner = written
        .strip_prefix(BLOCK_OPEN)
        .and_then(|text| text.strip_suffix(BLOCK_CLOSE))
        .unwrap_or(written);
    inner
        .lines()
        .map(|line| line.trim().trim_start_matches(BLOCK_LINE).trim())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// What `body` pushes onto or assigns into, each as the name it starts from: `None` for a value
/// that starts from no name.
pub(super) fn mutated(body: &[Statement]) -> Vec<Option<&Name>> {
    let mut roots = Vec::new();
    each_statement(body, &mut |statement| match statement {
        Statement::Append { list, .. } => roots.push(root(list)),
        Statement::Assign {
            target: Target::Subscript { value, .. },
            ..
        }
        | Statement::AugAssign {
            target: Target::Subscript { value, .. },
            ..
        } => roots.push(root(value)),
        _ => {}
    });
    roots
}

/// Whether `body`, which mutates nothing, may change what `bound` reads: it reassigns a name in
/// it, or calls a function of the snippet's own. A lifted callback cannot, as it mutates nothing
/// declared outside it.
fn changes(body: &[Statement], bound: &Expression) -> bool {
    let mut names = Vec::new();
    collect_names(bound, &mut names);
    if names.is_empty() {
        return false;
    }
    let mut changed = false;
    walk(body, &mut |expression| {
        changed |= matches!(
            expression,
            Expression::Call(Call {
                callee: Callee::Local { name, .. },
                ..
            }) if !name.is_callback()
        );
    });
    each_statement(body, &mut |statement| {
        changed |= statement
            .targets()
            .into_iter()
            .any(|target| names.contains(&target));
    });
    changed
}

fn collect_names<'a>(expression: &'a Expression, names: &mut Vec<&'a Name>) {
    if let Expression::Name(name) = expression {
        names.push(name);
    }
    for child in expression.children() {
        collect_names(child, names);
    }
}

fn root(expression: &Expression) -> Option<&Name> {
    match expression {
        Expression::Name(name) => Some(name),
        Expression::Subscript { value, .. } => root(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::tests::{python, refused};

    use super::{
        ARRAY_PATTERN_VALUE, BOUND, CATCH, COUNTED, EMPTY_PATTERN, FOR_IN, LOOP_BOUND,
        RETURN_EARLY, TARGET_CALL, THROWN,
    };

    #[test_case("let a = 1;\na = 2;\na", "a = 1\na = 2\na" ; "let_reassigned")]
    #[test_case("let n = 0;\nn += 2;\nn--;\nn = 5;\nif (n > 1) { console.log(n * 2); }", "n = 0\nn += 2\nn -= 1\nn = 5\nif n > 1:\n    print(n * 2)" ; "let_number_kept")]
    #[test_case("let s = \"a\";\ns += \"b\";\nconst t = s + \"c\";", "s = \"a\"\ns += \"b\"\nt = f\"{s}c\"" ; "let_string_kept")]
    #[test_case("const o = {a: 1, b: 2};\nconst {a, b: c} = o;\nc", "o = {\"a\": 1, \"b\": 2}\na = o[\"a\"]\nc = o[\"b\"]\nc" ; "object_pattern")]
    #[test_case("const {a, b} = await gh.list();\na", "_caudra_value_0 = await gh.list()\na = _caudra_value_0[\"a\"]\nb = _caudra_value_0[\"b\"]\na" ; "object_pattern_read_once")]
    #[test_case("let n = 0;\nn++;\nn += 2;\nn", "n = 0\nn += 1\nn += 2\nn" ; "updates")]
    #[test_case("const o = {};\no.a = 1;\no[\"b\"] = 2;", "o = {}\no[\"a\"] = 1\no[\"b\"] = 2" ; "member_assignment")]
    #[test_case("const n = 1;\nif (n > 1) { console.log(1); } else if (n < 0) { console.log(2); } else { console.log(3); }", "n = 1\nif n > 1:\n    print(1)\nelif n < 0:\n    print(2)\nelse:\n    print(3)" ; "if_else_chain")]
    #[test_case("let n = 3;\nwhile (n > 0) { n--; if (n === 1) break; else continue; }", "n = 3\nwhile n > 0:\n    n -= 1\n    if n == 1:\n        break\n    else:\n        continue" ; "while_with_jumps")]
    #[test_case("const xs = [1];\nfor (const x of xs) { console.log(x); }", "xs = [1]\nfor x in xs:\n    print(x)" ; "for_of")]
    #[test_case("const o = {a: 1};\nfor (const [k, v] of Object.entries(o)) console.log(k, v);", "o = {\"a\": 1}\nfor k, v in list(o.items()):\n    print(k, v)" ; "for_of_entries")]
    #[test_case("const xs = [1];\nfor (let i = 0; i < xs.length; i++) console.log(xs[i]);", "xs = [1]\nfor i in range(0, len(xs)):\n    print(xs[i])" ; "counted_loop")]
    #[test_case("const n = 3;\nfor (let i = 1; i <= n; i++) console.log(i);", "n = 3\nfor i in range(1, n + 1):\n    print(i)" ; "inclusive_counted_loop")]
    #[test_case("const a = 1;\ntry { console.log(a); } finally { console.log(2); }", "a = 1\ntry:\n    print(a)\nfinally:\n    print(2)" ; "try_finally")]
    #[test_case("const a = 1;\nif (a > 2) throw new Error(`big ${a}`);", "a = 1\nif a > 2:\n    raise Exception(f\"big {a}\")" ; "throw_error")]
    #[test_case("async function f(a: number, b = 1): Promise<number[]> { return [a, b]; }\nreturn await f(1);", "async def f(a, b=1):\n    return [a, b]\nawait f(1)" ; "async_function")]
    #[test_case("const f = (a, b?: string) => a;\nf(1)", "def f(a, b=None):\n    return a\nf(1)" ; "arrow_definition")]
    #[test_case("interface A { a: number }\ntype B = A;\nconst a = 1 as number;\nconst b = a satisfies number;\nconst c = a!;\nb + c", "a = 1\nb = a\nc = a\nb + c" ; "typescript_erased")]
    #[test_case("// first\nconst a = 1; // trailing\n/* block\n * more */\na", "# first\na = 1\n# trailing\n# block\n# more\na" ; "comments")]
    #[test_case("const xs = [1];\nxs.forEach((x, i) => { if (i > 0) console.log(x); });", "xs = [1]\nfor i, x in enumerate(xs):\n    if i > 0:\n        print(x)" ; "for_each")]
    #[test_case("const xs = [\"a\"];\nconst ys = xs.map(x => { const y = x.length + 1; return y; });", "xs = [\"a\"]\ndef _caudra_callback_0(x):\n    y = len(x) + 1\n    return y\nys = [_caudra_callback_0(x) for x in xs]" ; "lifted_callback")]
    #[test_case("const xs = [\"a\"];\nfor (const x of xs) { const ys = xs.filter(y => { return y.length > x.length; }); }", "xs = [\"a\"]\nfor x in xs:\n    def _caudra_callback_0(y):\n        return len(y) > len(x)\n    ys = [y for y in xs if _caudra_callback_0(y)]" ; "lifted_callback_stays_in_its_loop")]
    #[test_case("const words = [\"ab\"];\nlet total = 0;\nfor (const w of words) { total += w.length; }\ntotal", "words = [\"ab\"]\ntotal = 0\nfor w in words:\n    total += len(w)\ntotal" ; "let_number_stepped_by_a_length")]
    #[test_case("const rs = [{a: 1}];\nlet total: number = 0;\nfor (const r of rs) { total += Object.keys(r).length; }\nconst big = total > 1;", "rs = [{\"a\": 1}]\ntotal = 0\nfor r in rs:\n    total += len(list(r))\nbig = total > 1" ; "let_number_stepped_by_a_key_count")]
    fn statement_translates(js: &str, expected: &str) {
        assert_eq!(python(js), expected);
    }

    #[test_case("const a = 1;\nif (a > 0) return a;\nreturn 2;", RETURN_EARLY, 2 ; "early_return")]
    #[test_case("const xs = [1];\nfor (let i = 0; i < xs.length; i++) xs.push(i);", BOUND, 2 ; "bound_changed")]
    #[test_case("const xs = [1];\nconst o = {};\nfor (let i = 0; i < xs.length; i++) o.n = i;", BOUND, 3 ; "counted_body_assigns_a_member")]
    #[test_case("const xs = [1];\nfor (let i = 0; i < xs.length; i++) { const ys = []; ys.push(i); }", BOUND, 2 ; "counted_body_pushes")]
    #[test_case("const xs = [1];\nfor (let i = 0; i > xs.length; i++) {}", COUNTED, 2 ; "counted_downward_test")]
    #[test_case("const xs = [1];\nfor (let i = xs.length; i < 3; i++) {}", COUNTED, 2 ; "counted_start_not_a_literal")]
    #[test_case("let n = 3;\nfor (let i = 0; i < n; i++) {}", LOOP_BOUND, 2 ; "counted_bound_let")]
    #[test_case("const r = { n: 3 };\nfor (let i = 0; i < r.n; i++) {}", LOOP_BOUND, 2 ; "counted_bound_member")]
    #[test_case("const o = {a: 1};\nfor (const k in o) console.log(o[k]);", FOR_IN, 2 ; "for_in")]
    #[test_case("const a = 1;\ntry { console.log(a); } catch (e) { console.log(e.message); }", CATCH, 2 ; "try_catch")]
    #[test_case("const [a, b] = [1, 2];", ARRAY_PATTERN_VALUE, 1 ; "array_pattern_of_a_list")]
    #[test_case("const [a] = await Promise.all([gh.list(), gh.list()]);", ARRAY_PATTERN_VALUE, 1 ; "array_pattern_of_fewer_names")]
    #[test_case("const [] = await Promise.all([]);", EMPTY_PATTERN, 1 ; "empty_array_pattern")]
    #[test_case("const xs = [[1]];\nfor (const [a] of xs) { console.log(a); }", "an array pattern other than `[key, value]` over `Object.entries()`", 2 ; "array_pattern_in_a_loop")]
    #[test_case("const xs = [1];\nfor (const x of xs) { x = 2; }", "an assignment to `x`", 2 ; "loop_constant_reassigned")]
    #[test_case("const o = {};\no[String(1)] = 1;", TARGET_CALL, 2 ; "target_with_a_call")]
    #[test_case("let n = 0;\nn = n * 3;", "`*` of a value not known to be a number", 2 ; "let_number_set_by_a_value")]
    #[test_case("let n = 0;\nn = parseFloat(\"1\");\nn += 1;", "`+=` of values not known to be two numbers or two strings", 3 ; "let_number_set_to_a_parsed_number")]
    #[test_case("const xs = [\"a\"];\nlet n = 0;\nn += xs[0]?.length;", "`xs[0]?.length`, which may be `undefined`, compared or computed", 3 ; "let_number_stepped_by_an_optional_length")]
    #[test_case("const a = 1;\na = 2;", "an assignment to `a`", 2 ; "const_reassigned")]
    #[test_case("let a = 1;\nfunction f() { a = 2; }", "an assignment to `a`", 2 ; "outer_assignment")]
    #[test_case("const a = [1];\nif (a) { console.log(1); }", "the condition `a`", 2 ; "bare_condition")]
    #[test_case("const a = 1;\nif (a > 0) { const a = 2; }", "the name `a`", 2 ; "shadowing")]
    #[test_case("const list = [1];", "the name `list`", 1 ; "builtin_name")]
    #[test_case("const gh = 1;", "the name `gh`", 1 ; "host_name")]
    #[test_case("const xs = [1];\nxs.forEach(x => { return; });", "`return` inside a `forEach` callback", 2 ; "return_in_for_each")]
    #[test_case("const xs = [1];\nxs.forEach(x => { break; });", "`break` outside a loop", 2 ; "break_in_for_each")]
    #[test_case("for (const x of [1]) { [1].forEach(y => { break; }); }", "`break` outside a loop", 1 ; "break_in_for_each_inside_a_loop")]
    #[test_case("const xs = [1]; for (const x of xs) { xs.forEach(y => { continue; }); }", "`continue` outside a loop", 1 ; "continue_in_for_each_inside_a_loop")]
    #[test_case("const a = 1;\nthrow a;", THROWN, 2 ; "throw_value")]
    #[test_case("const a = 1;\nfunction f(b = []) {}", "a parameter default that is not a literal", 2 ; "mutable_default")]
    #[test_case("function f() { await gh.list(); }", "`await` outside an async function", 1 ; "await_in_sync_function")]
    #[test_case("const xs = [1];\nfor (const x of xs) { const y = x; }\ny", "the undeclared name `y`", 3 ; "block_scoped_name")]
    fn statement_refuses(js: &str, construct: &str, line: usize) {
        let refusal = refused(js);
        assert_eq!(
            (refusal.construct.to_string().as_str(), refusal.line),
            (construct, line)
        );
    }
}
