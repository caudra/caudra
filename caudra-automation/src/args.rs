use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::meta::{EMPTY_LIST, MetaError, References, entries, field, invalid, section};

/// The constant a script reads its args from.
pub const ARGS_VARIABLE: &str = "args";
pub const MAX_ARGS: usize = 16;
pub const MAX_ARG_NAME_BYTES: usize = 64;
pub const MAX_ARG_DESCRIPTION_BYTES: usize = 160;
/// The longest string value, measured after trimming.
pub const MAX_STRING_BYTES: usize = 4 * 1024;
pub const MAX_LIST_ITEMS: usize = 64;
/// The most JSON every resolved value takes together.
pub const MAX_ARGS_JSON_BYTES: usize = 16 * 1024;
/// What a smoke run gives a required string without choices or an example.
pub const SMOKE_STRING: &str = "example";
/// How many items a smoke run gives a required list when its bounds allow.
const SMOKE_ITEMS: u64 = 1;
const FIELD_DEFAULT: &str = "default_value";
const FIELD_EXAMPLE: &str = "example";
const FIELD_MIN: &str = "min";
const FIELD_MAX: &str = "max";
const FIELD_CHOICES: &str = "choices";
const FIELD_DESCRIPTION: &str = "description";
pub const TOO_MANY_ARGS: &str = "must declare at most 16 args";
pub const INVALID_ARG_NAME: &str = "arg names are snake_case: a lowercase letter, then lowercase letters, digits or underscores, at most 64 bytes";
pub const BOUND_WITHOUT_MEASURE: &str = "applies only to int, float and list args";
pub const INTEGER_BOUND: &str = "must be an integer";
pub const LIST_BOUND: &str = "must be a whole number from 0 to 64";
pub const MIN_ABOVE_MAX: &str = "must not exceed max";
pub const CHOICES_WITHOUT_STRING: &str = "applies only to string args";
pub const ARG_DESCRIPTION_TOO_LONG: &str = "must be at most 160 bytes";

/// One entry of `meta.args`, in declaration order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArgDecl {
    pub name: String,
    pub spec: ArgSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArgSpec {
    #[serde(rename = "type")]
    pub kind: ArgType,
    /// Without one, the arg is required. Named as the header names it, since `default` is
    /// reserved in Rhai.
    #[serde(rename = "default_value")]
    pub default: Option<Value>,
    /// Bounds on a number, or on a list's length.
    pub min: Option<Number>,
    pub max: Option<Number>,
    /// The strings a `string` arg may take; any when empty.
    pub choices: Vec<String>,
    pub description: Option<String>,
    /// The value `validate` uses for a required arg.
    pub example: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    String,
    Int,
    Float,
    Bool,
    /// A list of strings.
    List,
}

/// Why a value does not fit its spec.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("expected {}", .0.expected())]
    WrongType(ArgType),
    #[error("must not be blank")]
    Blank,
    #[error("must be at most {MAX_STRING_BYTES} bytes")]
    TooLong,
    #[error("must be one of {}", .0.join(", "))]
    NotAChoice(Vec<String>),
    #[error("must be at least {0}")]
    BelowMin(Number),
    #[error("must be at most {0}")]
    AboveMax(Number),
    #[error("must hold at least {0} items")]
    TooFewItems(Number),
    #[error("must hold at most {0} items")]
    TooManyItems(Number),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("args must be a JSON object")]
    NotAnObject,
    #[error("args.{0} is not declared")]
    Undeclared(String),
    #[error("args.{0} is required")]
    Missing(String),
    #[error("args.{name}: {error}")]
    Invalid { name: String, error: ValueError },
    #[error("args take {0} bytes of JSON; the limit is {MAX_ARGS_JSON_BYTES}")]
    TooLarge(usize),
}

impl ArgType {
    const fn expected(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Int => "an integer",
            Self::Float => "a number",
            Self::Bool => "true or false",
            Self::List => "a list of strings",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    #[serde(rename = "type")]
    kind: ArgType,
    default_value: Option<Value>,
    min: Option<Number>,
    max: Option<Number>,
    choices: Option<Vec<String>>,
    description: Option<String>,
    example: Option<Value>,
}

/// Every declared arg with its value: `given` checked against its spec, trimmed, or the
/// default. A blank string and `null` count as missing.
pub fn resolve(decls: &[ArgDecl], given: &Value) -> Result<Map<String, Value>, ArgsError> {
    let no_args = Map::new();
    let given = match given {
        Value::Null => &no_args,
        Value::Object(given) => given,
        _ => return Err(ArgsError::NotAnObject),
    };
    if let Some(name) = given
        .keys()
        .find(|name| !decls.iter().any(|decl| &decl.name == *name))
    {
        return Err(ArgsError::Undeclared(name.clone()));
    }
    let resolved = decls
        .iter()
        .map(|decl| {
            let value = match given.get(&decl.name).filter(|value| !is_missing(value)) {
                Some(value) => {
                    check_value(&decl.spec, value.clone()).map_err(|error| ArgsError::Invalid {
                        name: decl.name.clone(),
                        error,
                    })?
                }
                None => decl
                    .spec
                    .default
                    .clone()
                    .ok_or_else(|| ArgsError::Missing(decl.name.clone()))?,
            };
            Ok((decl.name.clone(), value))
        })
        .collect::<Result<Map<String, Value>, ArgsError>>()?;
    let bytes = serde_json::to_vec(&resolved).map_or(usize::MAX, |json| json.len());
    if bytes > MAX_ARGS_JSON_BYTES {
        return Err(ArgsError::TooLarge(bytes));
    }
    Ok(resolved)
}

/// The args a smoke run uses: each default, else the example, else a value derived from the
/// spec that fits its bounds and choices.
pub fn smoke_args(decls: &[ArgDecl]) -> Map<String, Value> {
    decls
        .iter()
        .map(|decl| {
            let value = decl
                .spec
                .default
                .clone()
                .or_else(|| decl.spec.example.clone())
                .unwrap_or_else(|| derived(&decl.spec));
            (decl.name.clone(), value)
        })
        .collect()
}

/// The args the script reads that `meta.args` does not declare, in name order.
pub fn undeclared(decls: &[ArgDecl], references: &References) -> Vec<String> {
    references
        .args
        .iter()
        .filter(|name| !decls.iter().any(|decl| &decl.name == *name))
        .cloned()
        .collect()
}

/// Parses `meta.args`, a map from arg name to spec, keeping declaration order.
pub(crate) fn parse_decls(
    path: &str,
    specs: Map<String, Value>,
) -> Result<Vec<ArgDecl>, MetaError> {
    if specs.len() > MAX_ARGS {
        return Err(invalid(path, TOO_MANY_ARGS));
    }
    specs
        .into_iter()
        .map(|(name, spec)| parse_decl(&field(path, &name), name, spec))
        .collect()
}

fn parse_decl(path: &str, name: String, spec: Value) -> Result<ArgDecl, MetaError> {
    if !is_arg_name(&name) {
        return Err(invalid(path, INVALID_ARG_NAME));
    }
    let raw: RawSpec = section(path, spec)?;
    for (key, bound) in [(FIELD_MIN, &raw.min), (FIELD_MAX, &raw.max)] {
        if let Some(bound) = bound
            && let Err(reason) = check_bound(raw.kind, bound)
        {
            return Err(invalid(&field(path, key), reason));
        }
    }
    if let (Some(min), Some(max)) = (&raw.min, &raw.max)
        && exceeds(min, max)
    {
        return Err(invalid(&field(path, FIELD_MIN), MIN_ABOVE_MAX));
    }
    if raw
        .description
        .as_ref()
        .is_some_and(|description| description.len() > MAX_ARG_DESCRIPTION_BYTES)
    {
        return Err(invalid(
            &field(path, FIELD_DESCRIPTION),
            ARG_DESCRIPTION_TOO_LONG,
        ));
    }
    let choices = match raw.choices {
        None => Vec::new(),
        Some(_) if raw.kind != ArgType::String => {
            return Err(invalid(&field(path, FIELD_CHOICES), CHOICES_WITHOUT_STRING));
        }
        Some(choices) if choices.is_empty() => {
            return Err(invalid(&field(path, FIELD_CHOICES), EMPTY_LIST));
        }
        Some(choices) => entries(&field(path, FIELD_CHOICES), choices, |path, choice| {
            text(&choice).map_err(|error| invalid(path, error.to_string()))
        })?,
    };
    let spec = ArgSpec {
        kind: raw.kind,
        default: None,
        min: raw.min,
        max: raw.max,
        choices,
        description: raw.description,
        example: None,
    };
    let default = sample(&field(path, FIELD_DEFAULT), &spec, raw.default_value)?;
    let example = sample(&field(path, FIELD_EXAMPLE), &spec, raw.example)?;
    Ok(ArgDecl {
        name,
        spec: ArgSpec {
            default,
            example,
            ..spec
        },
    })
}

/// A default or an example, which must already be a valid value.
fn sample(path: &str, spec: &ArgSpec, value: Option<Value>) -> Result<Option<Value>, MetaError> {
    value
        .map(|value| check_value(spec, value).map_err(|error| invalid(path, error.to_string())))
        .transpose()
}

fn check_bound(kind: ArgType, bound: &Number) -> Result<(), &'static str> {
    match kind {
        ArgType::Int if !bound.is_i64() => Err(INTEGER_BOUND),
        ArgType::List
            if !bound
                .as_u64()
                .is_some_and(|count| count <= MAX_LIST_ITEMS as u64) =>
        {
            Err(LIST_BOUND)
        }
        ArgType::String | ArgType::Bool => Err(BOUND_WITHOUT_MEASURE),
        ArgType::Int | ArgType::Float | ArgType::List => Ok(()),
    }
}

/// `value` as its spec's type, trimmed and within its bounds and choices.
fn check_value(spec: &ArgSpec, value: Value) -> Result<Value, ValueError> {
    let wrong_type = || ValueError::WrongType(spec.kind);
    match spec.kind {
        ArgType::String => {
            let text = text(value.as_str().ok_or_else(wrong_type)?)?;
            if !spec.choices.is_empty() && !spec.choices.contains(&text) {
                return Err(ValueError::NotAChoice(spec.choices.clone()));
            }
            Ok(Value::String(text))
        }
        ArgType::Int => {
            let number = Number::from(value.as_i64().ok_or_else(wrong_type)?);
            within(spec, &number)?;
            Ok(Value::Number(number))
        }
        ArgType::Float => {
            let number = value
                .as_f64()
                .and_then(Number::from_f64)
                .ok_or_else(wrong_type)?;
            within(spec, &number)?;
            Ok(Value::Number(number))
        }
        ArgType::Bool => value.as_bool().map(Value::Bool).ok_or_else(wrong_type),
        ArgType::List => {
            let Value::Array(items) = value else {
                return Err(wrong_type());
            };
            let count = Number::from(items.len());
            if let Some(min) = &spec.min
                && exceeds(min, &count)
            {
                return Err(ValueError::TooFewItems(min.clone()));
            }
            let max = spec
                .max
                .clone()
                .unwrap_or_else(|| Number::from(MAX_LIST_ITEMS));
            if exceeds(&count, &max) {
                return Err(ValueError::TooManyItems(max));
            }
            items
                .iter()
                .map(|item| text(item.as_str().ok_or_else(wrong_type)?).map(Value::String))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
    }
}

fn within(spec: &ArgSpec, number: &Number) -> Result<(), ValueError> {
    if let Some(min) = &spec.min
        && exceeds(min, number)
    {
        return Err(ValueError::BelowMin(min.clone()));
    }
    if let Some(max) = &spec.max
        && exceeds(number, max)
    {
        return Err(ValueError::AboveMax(max.clone()));
    }
    Ok(())
}

/// `left > right`, exact when both are integers.
fn exceeds(left: &Number, right: &Number) -> bool {
    match (left.as_i64(), right.as_i64()) {
        (Some(left), Some(right)) => left > right,
        _ => float(left) > float(right),
    }
}

fn float(number: &Number) -> f64 {
    number.as_f64().unwrap_or(f64::NAN)
}

fn text(value: &str) -> Result<String, ValueError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ValueError::Blank);
    }
    if trimmed.len() > MAX_STRING_BYTES {
        return Err(ValueError::TooLong);
    }
    Ok(trimmed.to_owned())
}

fn is_missing(value: &Value) -> bool {
    value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
}

/// The first choice, or the value nearest zero, or the shortest list, within the bounds.
fn derived(spec: &ArgSpec) -> Value {
    let min = spec.min.as_ref();
    let max = spec.max.as_ref();
    match spec.kind {
        ArgType::String => Value::String(
            spec.choices
                .first()
                .map_or(SMOKE_STRING, String::as_str)
                .to_owned(),
        ),
        ArgType::Int => Value::from(
            0_i64
                .max(min.and_then(Number::as_i64).unwrap_or(i64::MIN))
                .min(max.and_then(Number::as_i64).unwrap_or(i64::MAX)),
        ),
        ArgType::Float => Value::from(
            0_f64
                .max(min.map_or(f64::NEG_INFINITY, float))
                .min(max.map_or(f64::INFINITY, float)),
        ),
        ArgType::Bool => Value::Bool(false),
        ArgType::List => {
            let items = SMOKE_ITEMS
                .max(min.and_then(Number::as_u64).unwrap_or(SMOKE_ITEMS))
                .min(
                    max.and_then(Number::as_u64)
                        .unwrap_or(MAX_LIST_ITEMS as u64),
                );
            Value::Array(
                (0..items)
                    .map(|_| Value::String(SMOKE_STRING.to_owned()))
                    .collect(),
            )
        }
    }
}

/// `[a-z][a-z0-9_]*` within [`MAX_ARG_NAME_BYTES`].
fn is_arg_name(name: &str) -> bool {
    name.len() <= MAX_ARG_NAME_BYTES
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::meta::{DUPLICATE_ENTRY, references};

    const ROOT: &str = "meta.args";
    const VALID_SPECS: &str = "the specs are valid";
    const INVALID_SPECS: &str = "the specs are invalid";
    const VALID_ARGS: &str = "the args resolve";
    const INVALID_ARGS: &str = "the args do not resolve";
    const VALID_SOURCE: &str = "the source compiles";
    const BACKLOG: &str = "TODO.md";
    const UNDECLARED_READS: &str = r#"let meta = #{ name: "probe" };
message(args.file + args.typo);
notify(args["other_typo"]);
"#;

    fn parsed(specs: Value) -> Result<Vec<ArgDecl>, MetaError> {
        parse_decls(ROOT, specs.as_object().cloned().unwrap_or_default())
    }

    fn decls(specs: Value) -> Vec<ArgDecl> {
        parsed(specs).expect(VALID_SPECS)
    }

    fn spec_rejection(specs: Value) -> MetaError {
        parsed(specs).expect_err(INVALID_SPECS)
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn invalid_arg(name: &str, error: ValueError) -> ArgsError {
        ArgsError::Invalid {
            name: name.to_owned(),
            error,
        }
    }

    /// `goals` is the one required arg.
    fn backlog() -> Vec<ArgDecl> {
        decls(json!({
            "file": { "type": "string", "default_value": BACKLOG },
            "goals": { "type": "list", "min": 1, "max": 3 },
            "count": { "type": "int", "default_value": 24, "min": 1, "max": 100 },
            "ratio": { "type": "float", "default_value": 0.5, "min": 0, "max": 1 },
            "mode": { "type": "string", "choices": ["fast", "thorough"], "default_value": "fast" },
            "verbose": { "type": "bool", "default_value": false },
        }))
    }

    #[test_case(json!({ "Goals": { "type": "list" } }), "meta.args.Goals", INVALID_ARG_NAME; "an_uppercase_name")]
    #[test_case(json!({ "2nd": { "type": "int" } }), "meta.args.2nd", INVALID_ARG_NAME; "a_leading_digit")]
    #[test_case(json!({ "goal-list": { "type": "list" } }), "meta.args.goal-list", INVALID_ARG_NAME; "a_hyphen")]
    #[test_case(json!({ "file": { "type": "string", "min": 1 } }), "meta.args.file.min", BOUND_WITHOUT_MEASURE; "min_on_a_string")]
    #[test_case(json!({ "flag": { "type": "bool", "max": 1 } }), "meta.args.flag.max", BOUND_WITHOUT_MEASURE; "max_on_a_bool")]
    #[test_case(json!({ "count": { "type": "int", "min": 1.5 } }), "meta.args.count.min", INTEGER_BOUND; "a_fractional_int_bound")]
    #[test_case(json!({ "goals": { "type": "list", "max": 65 } }), "meta.args.goals.max", LIST_BOUND; "a_list_bound_past_the_cap")]
    #[test_case(json!({ "goals": { "type": "list", "min": -1 } }), "meta.args.goals.min", LIST_BOUND; "a_negative_list_bound")]
    #[test_case(json!({ "count": { "type": "int", "min": 5, "max": 1 } }), "meta.args.count.min", MIN_ABOVE_MAX; "crossed_int_bounds")]
    #[test_case(json!({ "ratio": { "type": "float", "min": 0.5, "max": 0.25 } }), "meta.args.ratio.min", MIN_ABOVE_MAX; "crossed_float_bounds")]
    #[test_case(json!({ "count": { "type": "int", "choices": ["1"] } }), "meta.args.count.choices", CHOICES_WITHOUT_STRING; "choices_on_an_int")]
    #[test_case(json!({ "mode": { "type": "string", "choices": [] } }), "meta.args.mode.choices", EMPTY_LIST; "no_choices")]
    #[test_case(json!({ "mode": { "type": "string", "choices": ["fast", "fast"] } }), "meta.args.mode.choices[1]", DUPLICATE_ENTRY; "a_repeated_choice")]
    #[test_case(json!({ "file": { "type": "string", "description": "d".repeat(MAX_ARG_DESCRIPTION_BYTES + 1) } }), "meta.args.file.description", ARG_DESCRIPTION_TOO_LONG; "a_description_past_the_limit")]
    fn specs_are_validated(specs: Value, path: &str, reason: &str) {
        assert_eq!(spec_rejection(specs), invalid(path, reason));
    }

    #[test_case(json!({ "mode": { "type": "string", "choices": [" "] } }), "meta.args.mode.choices[0]", ValueError::Blank; "a_blank_choice")]
    #[test_case(json!({ "count": { "type": "int", "default_value": "24" } }), "meta.args.count.default_value", ValueError::WrongType(ArgType::Int); "a_default_of_another_type")]
    #[test_case(json!({ "count": { "type": "int", "default_value": 0, "min": 1 } }), "meta.args.count.default_value", ValueError::BelowMin(Number::from(1)); "a_default_below_min")]
    #[test_case(json!({ "file": { "type": "string", "default_value": "  " } }), "meta.args.file.default_value", ValueError::Blank; "a_blank_default")]
    #[test_case(json!({ "mode": { "type": "string", "choices": ["fast"], "default_value": "slow" } }), "meta.args.mode.default_value", ValueError::NotAChoice(strings(&["fast"])); "a_default_outside_the_choices")]
    #[test_case(json!({ "goals": { "type": "list", "min": 2, "example": ["one"] } }), "meta.args.goals.example", ValueError::TooFewItems(Number::from(2)); "an_example_below_min")]
    fn samples_must_be_valid_values(specs: Value, path: &str, error: ValueError) {
        assert_eq!(spec_rejection(specs), invalid(path, error.to_string()));
    }

    #[test_case(json!({ "goals": {} }), "meta.args.goals: missing field `type`"; "no_type")]
    #[test_case(json!({ "when": { "type": "date" } }), "meta.args.when: unknown variant `date`"; "an_unknown_type")]
    #[test_case(json!({ "count": { "type": "int", "default": 1 } }), "meta.args.count: unknown field `default`"; "the_keyword_the_plan_renamed")]
    fn spec_shapes_are_checked(specs: Value, prefix: &str) {
        let rejection = spec_rejection(specs).to_string();
        assert!(rejection.starts_with(prefix), "{rejection}");
    }

    #[test]
    fn at_most_sixteen_args_are_declared() {
        let specs = (0..=MAX_ARGS)
            .map(|index| (format!("arg_{index}"), json!({ "type": "bool" })))
            .collect();
        assert_eq!(parse_decls(ROOT, specs), Err(invalid(ROOT, TOO_MANY_ARGS)));
    }

    #[test_case(json!({ "a".repeat(MAX_ARG_NAME_BYTES): { "type": "bool" } }); "the_longest_name")]
    #[test_case(json!({ "file": { "type": "string", "description": "d".repeat(MAX_ARG_DESCRIPTION_BYTES) } }); "the_longest_description")]
    #[test_case(json!({ "goals": { "type": "list", "min": 0, "max": MAX_LIST_ITEMS } }); "the_widest_list_bounds")]
    #[test_case(json!({ "count": { "type": "int", "min": 3, "max": 3, "default_value": 3 } }); "equal_bounds")]
    fn spec_limits_are_inclusive(specs: Value) {
        decls(specs);
    }

    #[test]
    fn declaration_order_is_kept() {
        let names: Vec<String> = decls(json!({
            "zeta": { "type": "bool" },
            "alpha": { "type": "bool" },
        }))
        .into_iter()
        .map(|decl| decl.name)
        .collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn samples_are_normalised_like_given_values() {
        let decls = decls(json!({
            "file": { "type": "string", "default_value": "  NOTES.md " },
            "ratio": { "type": "float", "example": 1 },
        }));
        assert_eq!(decls[0].spec.default, Some(json!("NOTES.md")));
        assert_eq!(decls[1].spec.example, Some(json!(1.0)));
    }

    #[test]
    fn defaults_fill_what_is_not_given_in_declaration_order() {
        let resolved = resolve(&backlog(), &json!({ "goals": ["ship it"] })).expect(VALID_ARGS);
        assert_eq!(
            resolved.keys().collect::<Vec<_>>(),
            ["file", "goals", "count", "ratio", "mode", "verbose"]
        );
        assert_eq!(
            Value::Object(resolved),
            json!({
                "file": BACKLOG,
                "goals": ["ship it"],
                "count": 24,
                "ratio": 0.5,
                "mode": "fast",
                "verbose": false,
            })
        );
    }

    #[test_case(json!({ "goals": ["a"], "file": "  BACKLOG.md " }), "file", json!("BACKLOG.md"); "strings_are_trimmed")]
    #[test_case(json!({ "goals": ["a"], "file": "   " }), "file", json!(BACKLOG); "a_blank_string_takes_the_default")]
    #[test_case(json!({ "goals": ["a"], "file": null }), "file", json!(BACKLOG); "null_takes_the_default")]
    #[test_case(json!({ "goals": ["a"], "ratio": 1 }), "ratio", json!(1.0); "an_integer_is_a_float")]
    #[test_case(json!({ "goals": [" a "] }), "goals", json!(["a"]); "list_items_are_trimmed")]
    fn given_values_are_normalised(given: Value, name: &str, expected: Value) {
        assert_eq!(
            resolve(&backlog(), &given).expect(VALID_ARGS)[name],
            expected
        );
    }

    #[test_case(json!([]), ArgsError::NotAnObject; "an_array")]
    #[test_case(Value::Null, ArgsError::Missing("goals".to_owned()); "no_args_at_all")]
    #[test_case(json!({}), ArgsError::Missing("goals".to_owned()); "a_required_arg")]
    #[test_case(json!({ "goals": ["a"], "typo": 1 }), ArgsError::Undeclared("typo".to_owned()); "an_undeclared_name")]
    #[test_case(json!({ "goals": "a" }), invalid_arg("goals", ValueError::WrongType(ArgType::List)); "a_string_for_a_list")]
    #[test_case(json!({ "goals": [1] }), invalid_arg("goals", ValueError::WrongType(ArgType::List)); "a_number_in_a_list")]
    #[test_case(json!({ "goals": ["a", " "] }), invalid_arg("goals", ValueError::Blank); "a_blank_item")]
    #[test_case(json!({ "goals": [] }), invalid_arg("goals", ValueError::TooFewItems(Number::from(1))); "too_few_items")]
    #[test_case(json!({ "goals": ["a", "b", "c", "d"] }), invalid_arg("goals", ValueError::TooManyItems(Number::from(3))); "too_many_items")]
    #[test_case(json!({ "goals": ["a"], "count": 0 }), invalid_arg("count", ValueError::BelowMin(Number::from(1))); "below_min")]
    #[test_case(json!({ "goals": ["a"], "count": 101 }), invalid_arg("count", ValueError::AboveMax(Number::from(100))); "above_max")]
    #[test_case(json!({ "goals": ["a"], "count": 2.5 }), invalid_arg("count", ValueError::WrongType(ArgType::Int)); "a_fraction_for_an_int")]
    #[test_case(json!({ "goals": ["a"], "ratio": 1.5 }), invalid_arg("ratio", ValueError::AboveMax(Number::from(1))); "a_float_above_max")]
    #[test_case(json!({ "goals": ["a"], "mode": "quick" }), invalid_arg("mode", ValueError::NotAChoice(strings(&["fast", "thorough"]))); "outside_the_choices")]
    #[test_case(json!({ "goals": ["a"], "verbose": "yes" }), invalid_arg("verbose", ValueError::WrongType(ArgType::Bool)); "a_string_for_a_bool")]
    #[test_case(json!({ "goals": ["a"], "file": "x".repeat(MAX_STRING_BYTES + 1) }), invalid_arg("file", ValueError::TooLong); "a_string_past_the_limit")]
    fn given_values_are_checked(given: Value, error: ArgsError) {
        assert_eq!(resolve(&backlog(), &given), Err(error));
    }

    #[test]
    fn a_long_list_needs_no_max_to_be_refused() {
        let decls = decls(json!({ "goals": { "type": "list" } }));
        let given = json!({ "goals": vec![BACKLOG; MAX_LIST_ITEMS + 1] });
        assert_eq!(
            resolve(&decls, &given),
            Err(invalid_arg(
                "goals",
                ValueError::TooManyItems(Number::from(MAX_LIST_ITEMS))
            ))
        );
    }

    #[test]
    fn errors_name_the_arg() {
        let error =
            resolve(&backlog(), &json!({ "goals": ["a"], "count": 0 })).expect_err(INVALID_ARGS);
        assert_eq!(
            error.to_string(),
            format!("args.count: {}", ValueError::BelowMin(Number::from(1)))
        );
    }

    #[test]
    fn all_values_together_are_bounded() {
        let parts = MAX_ARGS_JSON_BYTES / MAX_STRING_BYTES + 1;
        let specs = (0..parts)
            .map(|index| (format!("part_{index}"), json!({ "type": "string" })))
            .collect();
        let decls = parse_decls(ROOT, specs).expect(VALID_SPECS);
        let given = decls
            .iter()
            .map(|decl| (decl.name.clone(), json!("x".repeat(MAX_STRING_BYTES))))
            .collect();
        assert!(matches!(
            resolve(&decls, &Value::Object(given)),
            Err(ArgsError::TooLarge(bytes)) if bytes > MAX_ARGS_JSON_BYTES
        ));
    }

    #[test]
    fn smoke_args_prefer_the_default_then_the_example() {
        let decls = decls(json!({
            "file": { "type": "string", "default_value": BACKLOG, "example": "NOTES.md" },
            "topic": { "type": "string", "example": "ci.failures" },
        }));
        assert_eq!(
            Value::Object(smoke_args(&decls)),
            json!({ "file": BACKLOG, "topic": "ci.failures" })
        );
    }

    #[test_case(json!({ "type": "string" }), json!(SMOKE_STRING); "a_string")]
    #[test_case(json!({ "type": "string", "choices": ["fast", "thorough"] }), json!("fast"); "the_first_choice")]
    #[test_case(json!({ "type": "int" }), json!(0); "an_int")]
    #[test_case(json!({ "type": "int", "min": 5 }), json!(5); "an_int_above_zero")]
    #[test_case(json!({ "type": "int", "max": -5 }), json!(-5); "an_int_below_zero")]
    #[test_case(json!({ "type": "float" }), json!(0.0); "a_float")]
    #[test_case(json!({ "type": "float", "min": 0.5 }), json!(0.5); "a_float_above_zero")]
    #[test_case(json!({ "type": "bool" }), json!(false); "a_bool")]
    #[test_case(json!({ "type": "list" }), json!([SMOKE_STRING]); "a_list")]
    #[test_case(json!({ "type": "list", "min": 2 }), json!([SMOKE_STRING, SMOKE_STRING]); "a_list_with_a_minimum")]
    #[test_case(json!({ "type": "list", "max": 0 }), json!([]); "a_list_that_must_be_empty")]
    fn smoke_args_derive_a_value_that_resolves(spec: Value, expected: Value) {
        let decls = decls(json!({ "arg": spec }));
        let smoke = smoke_args(&decls);
        assert_eq!(smoke["arg"], expected);
        assert_eq!(resolve(&decls, &Value::Object(smoke.clone())), Ok(smoke));
    }

    #[test]
    fn undeclared_lists_reads_without_a_declaration() {
        let decls = decls(json!({ "file": { "type": "string", "default_value": BACKLOG } }));
        let references = references(UNDECLARED_READS).expect(VALID_SOURCE);
        assert_eq!(undeclared(&decls, &references), ["other_typo", "typo"]);
    }
}
