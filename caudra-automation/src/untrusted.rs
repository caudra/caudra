//! Values from outside a script. Text built from them stays untrusted: `+` keeps the taint,
//! predicates and declassifiers return plain values, and `${value}` yields a placeholder that
//! every sink refuses.

use std::any::TypeId;
use std::borrow::Cow;

use regex::RegexBuilder;
use rhai::{
    Array, Dynamic, Engine, EvalAltResult, FLOAT, FUNC_TO_DEBUG, FUNC_TO_STRING, FuncRegistration,
    INT, ImmutableString, Module, NativeCallContext, OP_CONTAINS, OP_EQUALS, OptimizationLevel,
    Position, Variant,
};
use serde::de::{Deserializer, Error as _};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// The key of the one-entry object that marks an untrusted value in tagged JSON.
pub const UNTRUSTED_TAG: &str = "$untrusted";
/// What `type_of` reports for an untrusted value.
pub const UNTRUSTED_TYPE_NAME: &str = "untrusted";
/// What `${value}` and `to_string` give for an untrusted value: "untrusted" with each letter
/// moved into the Private Use Area, so ordinary text never contains it.
pub const PLACEHOLDER: &str =
    "\u{E075}\u{E06E}\u{E074}\u{E072}\u{E075}\u{E073}\u{E074}\u{E065}\u{E064}";
pub(crate) const PLACEHOLDER_ERROR: &str = "text contains the placeholder that `${value}` and \
    `to_string` give for an untrusted value: join untrusted values with `+`, or show one to the \
    model with `attach`";
const PLAIN_APPEND_ERROR: &str = "`+=` cannot add an untrusted value to plain text: write \
    `text = text + value`, which makes the result untrusted";
const ITERATION_ERROR: &str = "untrusted text cannot be iterated: `for` walks the items of an \
    untrusted array or the keys of an untrusted object";
const INVALID_PATTERN: &str = "matches() was given an invalid pattern";
const TEXT_SIZE: &str = "Length of untrusted text";
const NOT_FOUND: INT = -1;
const REGEX_SIZE_LIMIT: usize = 256 * 1024;
const REGEX_DFA_SIZE_LIMIT: usize = 1024 * 1024;

const PLUS: &str = "+";
const PLUS_ASSIGN: &str = "+=";
const NOT_EQUALS: &str = "!=";
const LEN: &str = "len";
const IS_EMPTY: &str = "is_empty";
const TO_LOWER: &str = "to_lower";
const TO_UPPER: &str = "to_upper";
const TRIM: &str = "trim";
const SUB_STRING: &str = "sub_string";
const REPLACE: &str = "replace";
const SPLIT: &str = "split";
const STARTS_WITH: &str = "starts_with";
const ENDS_WITH: &str = "ends_with";
const INDEX_OF: &str = "index_of";
const MATCHES: &str = "matches";
const KEYS: &str = "keys";
const ONE_OF: &str = "one_of";
const PARSE_INT: &str = "parse_int";
const PARSE_FLOAT: &str = "parse_float";
const PARSE_JSON: &str = "parse_json";
/// Rhai answers `value.tag` for every type, so an untrusted object answers it from its JSON.
const TAG: &str = "tag";

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;

/// A value from outside the script: peer text, model output, goal reasons, workflow reports,
/// HTTP bodies and session titles. A script can test it and pass it on, but text built from it
/// never becomes this session's instructions. In tagged JSON it is `{"$untrusted": value}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Untrusted {
    Text(String),
    Json(Value),
}

/// A host function's text argument, with its trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkText {
    Trusted(String),
    Untrusted(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SinkError {
    #[error("expected text or an untrusted value, got {0}")]
    NotText(&'static str),
    #[error("{PLACEHOLDER_ERROR}")]
    Placeholder,
}

/// A value untrusted text joins with or searches for, as Rhai prints it.
trait Fragment: Variant + Clone {
    fn fragment(&self) -> Cow<'_, str>;
}

/// A plain value an untrusted one can equal.
trait Comparand: Variant + Clone {
    fn equals(&self, untrusted: &Untrusted) -> bool;
}

impl SinkText {
    pub fn as_str(&self) -> &str {
        let (Self::Trusted(text) | Self::Untrusted(text)) = self;
        text
    }
}

impl Untrusted {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// The text, or a structure's compact JSON.
    pub(crate) fn to_text(&self) -> Cow<'_, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(text),
            Self::Json(value) => Cow::Owned(value.to_string()),
        }
    }

    /// The value as raw JSON, text as a string.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => Value::String(text.clone()),
            Self::Json(value) => value.clone(),
        }
    }

    fn equals_text(&self, text: &str) -> bool {
        match self {
            Self::Text(own) => own == text,
            Self::Json(value) => value.as_str() == Some(text),
        }
    }

    fn equals(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(left), Self::Json(right)) => left == right,
            (Self::Text(text), other) | (other, Self::Text(text)) => other.equals_text(text),
        }
    }

    /// Whether text has the substring, an array a string item equal to it, or an object the key.
    fn contains(&self, needle: &str) -> bool {
        match self {
            Self::Json(Value::Array(items)) => {
                items.iter().any(|item| item.as_str() == Some(needle))
            }
            Self::Json(Value::Object(entries)) => entries.contains_key(needle),
            other => other.to_text().contains(needle),
        }
    }

    /// The characters of text, the items of an array, or the keys of an object.
    fn count(&self) -> usize {
        match self {
            Self::Json(Value::Array(items)) => items.len(),
            Self::Json(Value::Object(entries)) => entries.len(),
            other => other.to_text().chars().count(),
        }
    }

    fn field(&self, key: &str) -> Dynamic {
        match self {
            Self::Json(Value::Object(entries)) => entries
                .get(key)
                .cloned()
                .map_or(Dynamic::UNIT, untrusted_value),
            _ => Dynamic::UNIT,
        }
    }

    /// An array item, counted from the end when `index` is negative.
    fn item(&self, index: INT) -> Dynamic {
        let Self::Json(Value::Array(items)) = self else {
            return Dynamic::UNIT;
        };
        let position = usize::try_from(index).ok().or_else(|| {
            usize::try_from(index.unsigned_abs())
                .ok()
                .and_then(|back| items.len().checked_sub(back))
        });
        position
            .and_then(|position| items.get(position))
            .cloned()
            .map_or(Dynamic::UNIT, untrusted_value)
    }

    fn keys(&self) -> Array {
        match self {
            Self::Json(Value::Object(entries)) => entries
                .keys()
                .map(|key| Dynamic::from(Self::text(key.as_str())))
                .collect(),
            _ => Array::new(),
        }
    }

    fn into_iteration(self) -> Vec<ScriptResult<Dynamic>> {
        match self {
            Self::Json(Value::Array(items)) => items
                .into_iter()
                .map(|item| Ok(untrusted_value(item)))
                .collect(),
            Self::Json(Value::Object(entries)) => entries
                .into_iter()
                .map(|(key, _)| Ok(Dynamic::from(Self::Text(key))))
                .collect(),
            _ => vec![Err(failure(ITERATION_ERROR))],
        }
    }
}

/// A string is text; anything else is a structure.
impl From<Value> for Untrusted {
    fn from(value: Value) -> Self {
        match value {
            Value::String(text) => Self::Text(text),
            other => Self::Json(other),
        }
    }
}

impl Serialize for Untrusted {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Text(text) => map.serialize_entry(UNTRUSTED_TAG, text)?,
            Self::Json(value) => map.serialize_entry(UNTRUSTED_TAG, value)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Untrusted {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = Map::deserialize(deserializer)?;
        let inner = object
            .remove(UNTRUSTED_TAG)
            .filter(|_| object.is_empty())
            .ok_or_else(|| D::Error::custom(format!("expected {{\"{UNTRUSTED_TAG}\": value}}")))?;
        Ok(inner.into())
    }
}

impl Fragment for ImmutableString {
    fn fragment(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.as_str())
    }
}

impl Fragment for Untrusted {
    fn fragment(&self) -> Cow<'_, str> {
        self.to_text()
    }
}

macro_rules! printed_fragment {
    ($($scalar:ty),+) => {$(
        impl Fragment for $scalar {
            fn fragment(&self) -> Cow<'_, str> {
                Cow::Owned(Dynamic::from(*self).to_string())
            }
        }
    )+};
}

printed_fragment!(char, INT, FLOAT, bool);

impl Comparand for ImmutableString {
    fn equals(&self, untrusted: &Untrusted) -> bool {
        untrusted.equals_text(self)
    }
}

impl Comparand for char {
    fn equals(&self, untrusted: &Untrusted) -> bool {
        untrusted.equals_text(&self.fragment())
    }
}

impl Comparand for () {
    fn equals(&self, _: &Untrusted) -> bool {
        false
    }
}

pub fn contains_placeholder(text: &str) -> bool {
    text.contains(PLACEHOLDER)
}

/// The text a host function receives: a string or char is trusted, untrusted text is not, and
/// an untrusted structure is untrusted compact JSON.
pub fn sink_text(value: &Dynamic) -> Result<SinkText, SinkError> {
    let sink = if let Some(untrusted) = value.read_lock::<Untrusted>() {
        SinkText::Untrusted(untrusted.to_text().into_owned())
    } else if let Some(text) = value.read_lock::<ImmutableString>() {
        SinkText::Trusted(text.to_string())
    } else if let Ok(character) = value.as_char() {
        SinkText::Trusted(character.to_string())
    } else {
        return Err(SinkError::NotText(value.type_name()));
    };
    if contains_placeholder(sink.as_str()) {
        return Err(SinkError::Placeholder);
    }
    Ok(sink)
}

/// The script value of JSON from outside: integral numbers within `i64` become ints and other
/// numbers floats, bools stay bools, `null` becomes `()`, strings become untrusted text, and
/// arrays and objects stay untrusted structures.
pub fn untrusted_value(value: Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(flag) => Dynamic::from_bool(flag),
        Value::Number(number) => json_number(&number),
        other => Dynamic::from(Untrusted::from(other)),
    }
}

pub(crate) fn json_number(number: &Number) -> Dynamic {
    number.as_i64().map_or_else(
        || Dynamic::from_float(number.as_f64().unwrap_or(FLOAT::NAN)),
        Dynamic::from_int,
    )
}

/// Registers [`Untrusted`] as a script type:
/// - `+` with an untrusted operand gives untrusted text, joining a structure as compact JSON,
///   and numbers and bools as Rhai prints them. `+=` keeps an untrusted left side untrusted,
///   and fails when it would add an untrusted value to plain text.
/// - `==`, `!=`, `in`, the predicates, `len` and the parsers give plain values, and `one_of`
///   gives the script's own string.
/// - The string functions mirror Rhai's and give untrusted text. A structure is indexed by key
///   or position, walked by `for`, and read as its compact JSON by the string functions.
/// - `${value}` and `to_string` give [`PLACEHOLDER`].
///
/// It also turns the optimizer off, which would rewrite `text = text + value` as `text += value`,
/// so scripts must be compiled by this engine.
pub fn register_untrusted(engine: &mut Engine) {
    engine
        .set_optimization_level(OptimizationLevel::None)
        .register_type_with_name::<Untrusted>(UNTRUSTED_TYPE_NAME)
        .register_fn(FUNC_TO_STRING, |_: &mut Untrusted| {
            ImmutableString::from(PLACEHOLDER)
        })
        .register_fn(FUNC_TO_DEBUG, |_: &mut Untrusted| {
            ImmutableString::from(PLACEHOLDER)
        })
        .register_fn(
            PLUS,
            |ctx: NativeCallContext, left: &mut Untrusted, right: Untrusted| {
                joined(&ctx, &left.to_text(), &right.to_text())
            },
        )
        .register_fn(
            PLUS_ASSIGN,
            |_: &mut ImmutableString, _: Untrusted| -> ScriptResult<()> {
                Err(failure(PLAIN_APPEND_ERROR))
            },
        )
        .register_fn(OP_EQUALS, |left: &mut Untrusted, right: Untrusted| {
            left.equals(&right)
        })
        .register_fn(NOT_EQUALS, |left: &mut Untrusted, right: Untrusted| {
            !left.equals(&right)
        })
        .register_fn(LEN, |value: &mut Untrusted| {
            INT::try_from(value.count()).unwrap_or(INT::MAX)
        })
        .register_fn(IS_EMPTY, |value: &mut Untrusted| value.count() == 0)
        .register_fn(TO_LOWER, |value: &mut Untrusted| {
            Untrusted::Text(value.to_text().to_lowercase())
        })
        .register_fn(TO_UPPER, |value: &mut Untrusted| {
            Untrusted::Text(value.to_text().to_uppercase())
        })
        .register_fn(SUB_STRING, |value: &mut Untrusted, start: INT| {
            Untrusted::Text(sub_string(&value.to_text(), start, INT::MAX))
        })
        .register_fn(SUB_STRING, |value: &mut Untrusted, start: INT, len: INT| {
            Untrusted::Text(sub_string(&value.to_text(), start, len))
        })
        .register_fn(
            MATCHES,
            |value: &mut Untrusted, pattern: ImmutableString| matches(&value.to_text(), &pattern),
        )
        .register_fn(KEYS, |value: &mut Untrusted| value.keys())
        .register_fn(ONE_OF, |value: Dynamic, choices: Array| {
            sink_text(&value).map_or(Dynamic::UNIT, |text| one_of(text.as_str(), choices))
        })
        .register_fn(PARSE_INT, |value: Untrusted| {
            value
                .to_text()
                .trim()
                .parse::<INT>()
                .map_or(Dynamic::UNIT, Dynamic::from_int)
        })
        .register_fn(PARSE_FLOAT, |value: Untrusted| {
            value
                .to_text()
                .trim()
                .parse::<FLOAT>()
                .ok()
                .filter(|number| number.is_finite())
                .map_or(Dynamic::UNIT, Dynamic::from_float)
        })
        .register_fn(PARSE_JSON, |value: Untrusted| parse_json(&value.to_text()))
        .register_fn(PARSE_JSON, |text: ImmutableString| parse_json(&text))
        .register_indexer_get(|value: &mut Untrusted, key: ImmutableString| value.field(&key))
        .register_indexer_get(|value: &mut Untrusted, index: INT| value.item(index))
        .register_get(TAG, |value: &mut Untrusted| value.field(TAG))
        .register_global_module(iteration_module().into());
    FuncRegistration::new(TRIM)
        .with_purity(false)
        .register_into_engine(engine, |value: &mut Untrusted| {
            if let Untrusted::Text(text) = value {
                *text = text.trim().to_owned();
            }
        });
    register_join::<ImmutableString>(engine);
    register_join::<char>(engine);
    register_join::<INT>(engine);
    register_join::<FLOAT>(engine);
    register_join::<bool>(engine);
    register_comparison::<ImmutableString>(engine);
    register_comparison::<char>(engine);
    register_comparison::<()>(engine);
    register_needle::<ImmutableString>(engine);
    register_needle::<char>(engine);
    register_needle::<Untrusted>(engine);
}

fn register_join<T: Fragment>(engine: &mut Engine) {
    engine
        .register_fn(
            PLUS,
            |ctx: NativeCallContext, left: &mut Untrusted, right: T| {
                joined(&ctx, &left.to_text(), &right.fragment())
            },
        )
        .register_fn(PLUS, |ctx: NativeCallContext, left: T, right: Untrusted| {
            joined(&ctx, &left.fragment(), &right.to_text())
        });
}

fn register_comparison<T: Comparand>(engine: &mut Engine) {
    engine
        .register_fn(OP_EQUALS, |left: &mut Untrusted, right: T| {
            right.equals(left)
        })
        .register_fn(NOT_EQUALS, |left: &mut Untrusted, right: T| {
            !right.equals(left)
        })
        .register_fn(OP_EQUALS, |left: T, right: Untrusted| left.equals(&right))
        .register_fn(NOT_EQUALS, |left: T, right: Untrusted| !left.equals(&right));
}

fn register_needle<N: Fragment>(engine: &mut Engine) {
    engine
        .register_fn(OP_CONTAINS, |value: &mut Untrusted, needle: N| {
            value.contains(&needle.fragment())
        })
        .register_fn(STARTS_WITH, |value: &mut Untrusted, needle: N| {
            value.to_text().starts_with(needle.fragment().as_ref())
        })
        .register_fn(ENDS_WITH, |value: &mut Untrusted, needle: N| {
            value.to_text().ends_with(needle.fragment().as_ref())
        })
        .register_fn(INDEX_OF, |value: &mut Untrusted, needle: N| {
            index_of(&value.to_text(), &needle.fragment())
        })
        .register_fn(SPLIT, |value: &mut Untrusted, separator: N| {
            split(&value.to_text(), &separator.fragment())
        });
    register_replace::<N, ImmutableString>(engine);
    register_replace::<N, Untrusted>(engine);
}

fn register_replace<F: Fragment, S: Fragment>(engine: &mut Engine) {
    FuncRegistration::new(REPLACE)
        .with_purity(false)
        .register_into_engine(
            engine,
            |ctx: NativeCallContext,
             value: &mut Untrusted,
             find: F,
             substitute: S|
             -> ScriptResult<()> {
                let replaced = value
                    .to_text()
                    .replace(find.fragment().as_ref(), &substitute.fragment());
                *value = bounded(&ctx, replaced)?;
                Ok(())
            },
        );
}

fn iteration_module() -> Module {
    let mut module = Module::new();
    module.set_iter_result(TypeId::of::<Untrusted>(), |value| {
        Box::new(
            value
                .try_cast::<Untrusted>()
                .map(Untrusted::into_iteration)
                .unwrap_or_default()
                .into_iter(),
        )
    });
    module
}

fn joined(ctx: &NativeCallContext, left: &str, right: &str) -> ScriptResult<Untrusted> {
    bounded(ctx, [left, right].concat())
}

/// Untrusted text within the engine's string limit, which Rhai enforces only on its own strings.
fn bounded(ctx: &NativeCallContext, text: String) -> ScriptResult<Untrusted> {
    let max = ctx.engine().max_string_size();
    if max != 0 && text.len() > max {
        return Err(
            EvalAltResult::ErrorDataTooLarge(TEXT_SIZE.to_owned(), ctx.call_position()).into(),
        );
    }
    Ok(Untrusted::Text(text))
}

/// Rhai's `sub_string`: `len` characters from `start`, which counts from the end when negative.
fn sub_string(text: &str, start: INT, len: INT) -> String {
    let offset = usize::try_from(start).unwrap_or_else(|_| {
        let back = usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX);
        text.chars().count().saturating_sub(back)
    });
    text.chars()
        .skip(offset)
        .take(usize::try_from(len).unwrap_or_default())
        .collect()
}

/// Rhai's `index_of`: the character position of the first match, or -1, which empty text always
/// gives.
fn index_of(text: &str, needle: &str) -> INT {
    if text.is_empty() {
        return NOT_FOUND;
    }
    text.find(needle).map_or(NOT_FOUND, |byte| {
        INT::try_from(text[..byte].chars().count()).unwrap_or(NOT_FOUND)
    })
}

fn split(text: &str, separator: &str) -> Array {
    text.split(separator)
        .map(|piece| Dynamic::from(Untrusted::text(piece)))
        .collect()
}

fn matches(text: &str, pattern: &str) -> ScriptResult<bool> {
    let regex = RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|error| failure(format!("{INVALID_PATTERN}: {error}")))?;
    Ok(regex.is_match(text))
}

/// The script's own string that equals `text`, or `()`.
fn one_of(text: &str, choices: Array) -> Dynamic {
    choices
        .into_iter()
        .find(|choice| {
            choice
                .read_lock::<ImmutableString>()
                .is_some_and(|choice| choice.as_str() == text)
        })
        .unwrap_or(Dynamic::UNIT)
}

fn parse_json(text: &str) -> Dynamic {
    serde_json::from_str(text).map_or(Dynamic::UNIT, untrusted_value)
}

fn failure(message: impl Into<String>) -> Box<EvalAltResult> {
    EvalAltResult::ErrorRuntime(message.into().into(), Position::NONE).into()
}

#[cfg(test)]
mod tests {
    use caudra_script::{SandboxLimits, restricted_engine};
    use rhai::{Map as ScriptMap, Scope};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const HINT: &str = "untrusted tests";
    const LIMITS: SandboxLimits = SandboxLimits {
        max_operations: 100_000,
        max_call_levels: 16,
        max_expr_depth: 64,
        max_string_size: 1024,
        max_array_size: 64,
        max_map_size: 64,
    };
    const GREETING: &str = "Hello, World";
    const CAUGHT: &str = "caught";
    const MUST_RUN: &str = "the script must run";
    const MUST_FAIL: &str = "the script must fail";
    const NOT_UNTRUSTED_TEXT: &str = "expected untrusted text";
    const NOT_UNTRUSTED_STRUCTURE: &str = "expected an untrusted structure";
    const NOT_PLAIN: &str = "expected a plain value";
    const NOT_UNIT: &str = "expected ()";
    const MESSAGE_NOT_TEXT: &str = "the error message must be text";
    const MUST_SERIALIZE: &str = "an untrusted value serializes";

    fn scope() -> Scope<'static> {
        let mut scope = Scope::new();
        scope.push("text", Untrusted::text(GREETING));
        scope.push("padded", Untrusted::text("  hi  "));
        scope.push(
            "json",
            Untrusted::from(json!({
                "state": "failure",
                "count": 3,
                "ratio": 0.5,
                "huge": u64::MAX,
                "ok": true,
                "none": null,
                "digits": " 12 ",
                "tag": "v1",
                "labels": ["bug", "ci"],
                "workflow_runs": [{"id": 42, "name": "ci"}, {"id": 43, "name": "nightly"}],
                "body": r#"{"id": 7, "name": "deploy"}"#,
            })),
        );
        scope
    }

    fn run(source: &str) -> ScriptResult<Dynamic> {
        let mut engine = restricted_engine(&LIMITS, HINT);
        register_untrusted(&mut engine);
        engine.eval_with_scope(&mut scope(), source)
    }

    fn eval(source: &str) -> Dynamic {
        run(source).expect(MUST_RUN)
    }

    fn innermost(error: EvalAltResult) -> EvalAltResult {
        match error {
            EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => innermost(*inner),
            other => other,
        }
    }

    fn failure(source: &str) -> EvalAltResult {
        innermost(*run(source).expect_err(MUST_FAIL))
    }

    fn runtime_message(source: &str) -> String {
        match failure(source) {
            EvalAltResult::ErrorRuntime(message, _) => {
                message.into_string().expect(MESSAGE_NOT_TEXT)
            }
            other => panic!("{MUST_FAIL} with a runtime error: {other:?}"),
        }
    }

    #[test_case(r#"text + "!""# => "Hello, World!"; "untrusted_plus_string")]
    #[test_case(r#""> " + text"# => "> Hello, World"; "string_plus_untrusted")]
    #[test_case("text + text" => "Hello, WorldHello, World"; "untrusted_plus_untrusted")]
    #[test_case("text + 1" => "Hello, World1"; "untrusted_plus_int")]
    #[test_case("2 + text" => "2Hello, World"; "int_plus_untrusted")]
    #[test_case("text + 1.5" => "Hello, World1.5"; "untrusted_plus_float")]
    #[test_case("false + text" => "falseHello, World"; "bool_plus_untrusted")]
    #[test_case("text + '!'" => "Hello, World!"; "untrusted_plus_char")]
    #[test_case("'>' + text" => ">Hello, World"; "char_plus_untrusted")]
    #[test_case(r#""a" + 1 + text"# => "a1Hello, World"; "plain_chain_then_untrusted")]
    #[test_case(r#"text + "a" + 1"# => "Hello, Worlda1"; "untrusted_chain")]
    #[test_case("json.state + json.count" => "failure3"; "indexed_values_join")]
    #[test_case(r#"json.labels + """# => r#"["bug","ci"]"#; "structure_joins_as_compact_json")]
    #[test_case(r#"let s = text; s += "!"; s"# => "Hello, World!"; "op_assign_keeps_untrusted")]
    #[test_case("let n = 1; n += text; n" => "1Hello, World"; "op_assign_on_a_number_taints")]
    #[test_case(r#"let s = "seen: "; s = s + text; s"# => "seen: Hello, World"; "plain_text_reassigned_with_untrusted")]
    #[test_case("text.to_lower()" => "hello, world"; "to_lower")]
    #[test_case("text.to_upper()" => "HELLO, WORLD"; "to_upper")]
    #[test_case("text.sub_string(7)" => "World"; "sub_string_from")]
    #[test_case("text.sub_string(-5)" => "World"; "sub_string_from_the_end")]
    #[test_case("text.sub_string(0, 5)" => "Hello"; "sub_string_with_len")]
    #[test_case("text.sub_string(-5, 3)" => "Wor"; "sub_string_from_the_end_with_len")]
    #[test_case("let s = padded; s.trim(); s" => "hi"; "trim_in_place")]
    #[test_case(r#"let s = text; s.replace("World", "there"); s"# => "Hello, there"; "replace_in_place")]
    #[test_case(r#"let s = text; s.replace("World", json.state); s"# => "Hello, failure"; "replace_with_untrusted")]
    #[test_case(r#"let s = text; s.replace(text.sub_string(7), "there"); s"# => "Hello, there"; "replace_untrusted")]
    #[test_case(r#"let s = text; s.replace('o', "0"); s"# => "Hell0, W0rld"; "replace_char")]
    #[test_case(r#"text.split(", ")[1]"# => "World"; "split")]
    #[test_case("json.state" => "failure"; "property")]
    #[test_case(r#"json["state"]"# => "failure"; "string_index")]
    #[test_case("json.workflow_runs[0].name" => "ci"; "property_path")]
    #[test_case("json.workflow_runs[-1].name" => "nightly"; "index_from_the_end")]
    #[test_case("json.tag" => "v1"; "tag_property")]
    #[test_case("json.keys()[0]" => "state"; "keys")]
    #[test_case("parse_json(json.body).name" => "deploy"; "parsed_property")]
    #[test_case(r#"let out = ""; for label in json.labels { out = out + label; } out"# => "bugci"; "for_yields_untrusted_items")]
    #[test_case(r#"let out = ""; for key in json.workflow_runs[0] { out = out + key; } out"# => "idname"; "for_yields_untrusted_keys")]
    fn taint_spreads(source: &str) -> String {
        match eval(source).try_cast::<Untrusted>() {
            Some(Untrusted::Text(text)) => text,
            other => panic!("{NOT_UNTRUSTED_TEXT}: {other:?}"),
        }
    }

    #[test_case("json.workflow_runs[0]" => json!({"id": 42, "name": "ci"}); "indexed_object")]
    #[test_case("json.labels" => json!(["bug", "ci"]); "indexed_array")]
    #[test_case("parse_json(json.body)" => json!({"id": 7, "name": "deploy"}); "parsed_untrusted")]
    #[test_case(r#"parse_json("[1, 2]")"# => json!([1, 2]); "parsed_string")]
    fn structures_stay_untrusted(source: &str) -> Value {
        match eval(source).try_cast::<Untrusted>() {
            Some(Untrusted::Json(value)) => value,
            other => panic!("{NOT_UNTRUSTED_STRUCTURE}: {other:?}"),
        }
    }

    #[test_case(r#"text == "Hello, World""# => true; "untrusted_equals_string")]
    #[test_case(r#""Hello, World" == text"# => true; "string_equals_untrusted")]
    #[test_case(r#"text != "Hello""# => true; "untrusted_differs_from_string")]
    #[test_case(r#""Hello" != text"# => true; "string_differs_from_untrusted")]
    #[test_case("text == text" => true; "untrusted_equals_untrusted")]
    #[test_case("text != text" => false; "untrusted_differs_from_untrusted")]
    #[test_case("json.labels == json.labels" => true; "structures_compare_as_json")]
    #[test_case("json.labels == text" => false; "structure_never_equals_text")]
    #[test_case("json.state.sub_string(0, 1) == 'f'" => true; "untrusted_equals_char")]
    #[test_case("'f' == json.state.sub_string(0, 1)" => true; "char_equals_untrusted")]
    #[test_case("text == ()" => false; "untrusted_never_equals_unit")]
    #[test_case("() == text" => false; "unit_never_equals_untrusted")]
    #[test_case("text != ()" => true; "untrusted_differs_from_unit")]
    #[test_case(r#"json.state in ["success", "failure"]"# => true; "untrusted_in_list")]
    #[test_case(r#"json.state in ["success"]"# => false; "untrusted_not_in_list")]
    #[test_case(r#""World" in text"# => true; "string_in_text")]
    #[test_case("'W' in text" => true; "char_in_text")]
    #[test_case(r#""bug" in json.labels"# => true; "string_in_array")]
    #[test_case("json.labels[0] in json.labels" => true; "untrusted_in_array")]
    #[test_case(r#""state" in json"# => true; "key_in_object")]
    #[test_case(r#""failure" in json"# => false; "object_matches_keys_not_values")]
    #[test_case(r#"text.contains("World")"# => true; "contains_string")]
    #[test_case("text.contains(json.state)" => false; "contains_untrusted")]
    #[test_case(r#"text.starts_with("Hello")"# => true; "starts_with_string")]
    #[test_case("text.starts_with(text)" => true; "starts_with_untrusted")]
    #[test_case("text.ends_with('d')" => true; "ends_with_char")]
    #[test_case(r#"text.matches("^Hello, \\w+$")"# => true; "matches")]
    #[test_case(r#"text.matches("(?i)WORLD")"# => true; "matches_ignoring_case")]
    #[test_case(r#"text.matches("^World")"# => false; "matches_anchored")]
    #[test_case("text.is_empty()" => false; "is_empty")]
    #[test_case("text.sub_string(99).is_empty()" => true; "empty_after_sub_string")]
    #[test_case("json.labels.is_empty()" => false; "structure_is_empty")]
    #[test_case("json.ok" => true; "indexed_bool")]
    fn predicates_are_plain(source: &str) -> bool {
        eval(source).as_bool().expect(NOT_PLAIN)
    }

    #[test_case("text.len()" => 12; "len")]
    #[test_case("json.labels.len()" => 2; "array_len")]
    #[test_case("json.workflow_runs[0].len()" => 2; "object_len")]
    #[test_case(r#"text.index_of("World")"# => 7; "index_of_string")]
    #[test_case("text.index_of('W')" => 7; "index_of_char")]
    #[test_case("text.index_of(json.state)" => NOT_FOUND; "index_of_missing")]
    #[test_case(r#"text.split(", ").len()"# => 2; "split_len")]
    #[test_case("json.count" => 3; "indexed_int")]
    #[test_case("json.workflow_runs[0].id" => 42; "property_path")]
    #[test_case("parse_int(json.digits)" => 12; "parse_int")]
    #[test_case("let total = 0; for run in json.workflow_runs { total += run.id; } total" => 85; "for_over_array")]
    #[test_case(r#"let found = 0; for key in json { if key == "tag" { found += 1; } } found"# => 1; "for_over_object")]
    fn counts_are_plain(source: &str) -> INT {
        eval(source).as_int().expect(NOT_PLAIN)
    }

    #[test_case("json.ratio" => 0.5; "indexed_float")]
    #[test_case("json.huge" => u64::MAX as FLOAT; "beyond_int_range")]
    #[test_case("parse_float(json.digits)" => 12.0; "parse_float")]
    fn floats_are_plain(source: &str) -> FLOAT {
        eval(source).as_float().expect(NOT_PLAIN)
    }

    #[test_case("json.none"; "null")]
    #[test_case("json.missing"; "missing_key")]
    #[test_case("json.workflow_runs[2]"; "index_past_the_end")]
    #[test_case("json.workflow_runs[-3]"; "index_before_the_start")]
    #[test_case(r#"json.labels["x"]"#; "key_into_array")]
    #[test_case("text.anything"; "property_of_text")]
    #[test_case("text[0]"; "index_into_text")]
    #[test_case(r#"one_of(json.state, ["success"])"#; "one_of_without_a_match")]
    #[test_case(r#"one_of(json.missing, ["failure"])"#; "one_of_unit")]
    #[test_case(r#"one_of(json.count, ["3"])"#; "one_of_number")]
    #[test_case(r#"one_of(`${text}`, [`${text}`])"#; "one_of_placeholder")]
    #[test_case("parse_int(text)"; "parse_int_of_words")]
    #[test_case("parse_float(text)"; "parse_float_of_words")]
    #[test_case(r#"parse_float(text.sub_string(99) + "inf")"#; "parse_float_of_infinity")]
    #[test_case("parse_json(text)"; "parse_json_of_words")]
    #[test_case(r#"parse_json("null")"#; "parse_json_of_null")]
    fn absent_values_are_unit(source: &str) {
        assert!(eval(source).is_unit(), "{NOT_UNIT}");
    }

    #[test_case(r#"one_of(json.state, ["success", "failure"])"# => "failure"; "one_of_untrusted")]
    #[test_case(r#"one_of("failure", ["failure"])"# => "failure"; "one_of_string")]
    #[test_case(r#""CI: " + one_of(json.state, ["failure"])"# => "CI: failure"; "declassified_text_joins_plainly")]
    #[test_case(r#"`${text}`"# => PLACEHOLDER; "interpolation")]
    #[test_case(r#"`got ${json}`"# => format!("got {PLACEHOLDER}"); "interpolated_structure")]
    #[test_case("text.to_string()" => PLACEHOLDER; "to_string")]
    #[test_case("text.to_debug()" => PLACEHOLDER; "to_debug")]
    #[test_case("type_of(json)" => UNTRUSTED_TYPE_NAME; "type_of")]
    #[test_case(r#"let outcome = "uncaught"; try { text.matches("("); } catch { outcome = "caught"; } outcome"# => CAUGHT; "invalid_pattern_is_catchable")]
    fn plain_strings(source: &str) -> String {
        eval(source).into_string().expect(NOT_PLAIN)
    }

    #[test]
    fn plain_append_of_untrusted_fails() {
        assert_eq!(
            runtime_message(r#"let s = "seen: "; s += text;"#),
            PLAIN_APPEND_ERROR
        );
    }

    #[test]
    fn iterating_text_fails() {
        assert_eq!(
            runtime_message("let n = 0; for c in text { n += 1; } n"),
            ITERATION_ERROR
        );
    }

    #[test]
    fn invalid_pattern_fails() {
        assert!(runtime_message(r#"text.matches("(")"#).starts_with(INVALID_PATTERN));
    }

    #[test]
    fn joined_text_respects_the_string_limit() {
        assert!(matches!(
            failure("let s = text; loop { s = s + s; }"),
            EvalAltResult::ErrorDataTooLarge(..)
        ));
    }

    #[test_case(Dynamic::from(GREETING) => Ok(SinkText::Trusted(GREETING.to_owned())); "string_is_trusted")]
    #[test_case(Dynamic::from('c') => Ok(SinkText::Trusted("c".to_owned())); "char_is_trusted")]
    #[test_case(Dynamic::from(Untrusted::text(GREETING)) => Ok(SinkText::Untrusted(GREETING.to_owned())); "untrusted_text")]
    #[test_case(Dynamic::from(Untrusted::Json(json!({"a": [1]}))) => Ok(SinkText::Untrusted(r#"{"a":[1]}"#.to_owned())); "structure_as_compact_json")]
    #[test_case(Dynamic::from_int(1) => Err(SinkError::NotText(Dynamic::from_int(1).type_name())); "number_is_not_text")]
    #[test_case(Dynamic::from(format!("x{PLACEHOLDER}")) => Err(SinkError::Placeholder); "placeholder_in_string")]
    #[test_case(Dynamic::from(Untrusted::text(PLACEHOLDER)) => Err(SinkError::Placeholder); "placeholder_in_untrusted")]
    fn sink_text_outcomes(value: Dynamic) -> Result<SinkText, SinkError> {
        sink_text(&value)
    }

    #[test_case(r#""note: " + text"# => Ok(SinkText::Untrusted(format!("note: {GREETING}"))); "joined_untrusted")]
    #[test_case(r#"`note: ${text}`"# => Err(SinkError::Placeholder); "interpolated")]
    #[test_case(r#""CI " + one_of(json.state, ["failure"])"# => Ok(SinkText::Trusted("CI failure".to_owned())); "declassified")]
    #[test_case("#{ text: text }" => Err(SinkError::NotText(Dynamic::from_map(ScriptMap::new()).type_name())); "map")]
    fn script_values_at_a_sink(source: &str) -> Result<SinkText, SinkError> {
        sink_text(&eval(source))
    }

    #[test_case(Untrusted::text(GREETING); "text")]
    #[test_case(Untrusted::Json(json!({"a": [1, null]})); "structure")]
    fn serde_wraps_the_value(value: Untrusted) {
        let tagged = serde_json::to_value(&value).expect(MUST_SERIALIZE);
        assert_eq!(tagged, json!({ UNTRUSTED_TAG: value.to_json() }));
        assert_eq!(
            serde_json::from_value::<Untrusted>(tagged).ok(),
            Some(value)
        );
    }

    #[test_case(json!({}); "no_tag")]
    #[test_case(json!({ UNTRUSTED_TAG: GREETING, "other": 1 }); "extra_key")]
    #[test_case(json!(GREETING); "bare_string")]
    fn serde_rejects_malformed_wrappers(tagged: Value) {
        assert!(serde_json::from_value::<Untrusted>(tagged).is_err());
    }
}
