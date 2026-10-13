//! A server asking the user for something in the middle of a call.
//!
//! A form request carries a flat JSON schema. [`ElicitForm`] reads it into
//! questions the question form can ask, and turns the labels picked back into
//! the typed content the server asked for, checking every constraint on the
//! way, so nothing the schema forbids is ever sent.

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use thiserror::Error;
use url::Url;

use crate::types::{AnswerKind, AskedQuestion, QuestionOption, TextFormat};

const TYPE_FIELD: &str = "type";
const PROPERTIES_FIELD: &str = "properties";
const REQUIRED_FIELD: &str = "required";
const TITLE_FIELD: &str = "title";
const DESCRIPTION_FIELD: &str = "description";
const DEFAULT_FIELD: &str = "default";
const FORMAT_FIELD: &str = "format";
const MIN_LENGTH_FIELD: &str = "minLength";
const MAX_LENGTH_FIELD: &str = "maxLength";
const MINIMUM_FIELD: &str = "minimum";
const MAXIMUM_FIELD: &str = "maximum";
const MIN_ITEMS_FIELD: &str = "minItems";
const MAX_ITEMS_FIELD: &str = "maxItems";
const ITEMS_FIELD: &str = "items";
const ENUM_FIELD: &str = "enum";
const ENUM_NAMES_FIELD: &str = "enumNames";
const ONE_OF_FIELD: &str = "oneOf";
const ANY_OF_FIELD: &str = "anyOf";
const CONST_FIELD: &str = "const";
const OBJECT_TYPE: &str = "object";
const STRING_TYPE: &str = "string";
const NUMBER_TYPE: &str = "number";
const INTEGER_TYPE: &str = "integer";
const BOOLEAN_TYPE: &str = "boolean";
const ARRAY_TYPE: &str = "array";
const NO_TYPE: &str = "none";

const NOT_A_DEFINITION: &str = "its definition is not an object";
const NO_ITEMS: &str = "a list without an item definition";
const CHOICES_NOT_STRINGS: &str = "its choices are not all strings";

pub const YES_LABEL: &str = "Yes";
pub const NO_LABEL: &str = "No";
const TRUE_WORDS: &[&str] = &["yes", "true"];
const FALSE_WORDS: &[&str] = &["no", "false"];
/// What a credential-looking answer reads as in a summary.
pub const MASKED_ANSWER: &str = "••••";
/// Searched for inside a key or title, case and separators aside.
const CREDENTIAL_TERMS: &[&str] = &[
    "password",
    "passcode",
    "secret",
    "token",
    "apikey",
    "credential",
];
/// Too short to search for inside other words, so matched as whole words.
const CREDENTIAL_WORDS: &[&str] = &["otp", "totp", "2fa"];
const LIST_SEPARATOR: &str = ", ";

pub const REQUIRED: &str = "An answer is required";
pub const ONE_ANSWER: &str = "Takes a single answer";
pub const NOT_OFFERED: &str = "Not one of the offered choices";
pub const NOT_YES_OR_NO: &str = "Answer yes or no";
pub const NOT_A_NUMBER: &str = "Must be a number";
pub const NOT_A_WHOLE_NUMBER: &str = "Must be a whole number";
pub const BELOW_MINIMUM: &str = "Must be at least";
pub const ABOVE_MAXIMUM: &str = "Must be at most";
pub const TOO_SHORT: &str = "Needs at least";
pub const TOO_LONG: &str = "Takes at most";
pub const TOO_FEW: &str = "Pick at least";
pub const TOO_MANY: &str = "Pick at most";
pub const NOT_AN_EMAIL: &str = "Must be an email address";
pub const NOT_A_URI: &str = "Must be a full address, such as https://example.com";
pub const NOT_A_DATE: &str = "Must be a date, such as 2026-10-13";
pub const NOT_A_DATE_TIME: &str = "Must be a date and time, such as 2026-10-13T09:30:00Z";

const TEXT_NOUN: &str = "text";
const NUMBER_NOUN: &str = "a number";
const WHOLE_NUMBER_NOUN: &str = "a whole number";
const BOOLEAN_NOUN: &str = "yes or no";
const EMAIL_NOUN: &str = "an email address";
const URI_NOUN: &str = "a full address (URI)";
const DATE_NOUN: &str = "a date (YYYY-MM-DD)";
const DATE_TIME_NOUN: &str = "a date and time (RFC 3339)";
const CHARACTER_NOUN: &str = "character";
const CHARACTERS_NOUN: &str = "characters";

/// The user's answer to an elicitation, as the protocol spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitAction {
    Accept,
    Decline,
    Cancel,
}

/// A requested schema Caudra will not ask for. The protocol keeps forms flat
/// and primitive, so anything else is a server asking for more than a form
/// may hold.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnsupportedSchema {
    #[error("the requested schema is not an object with flat properties")]
    NotAnObject,
    #[error("field `{0}` is a nested object")]
    Nested(String),
    #[error("field `{field}` has a type a form cannot ask for: {kind}")]
    UnknownType { field: String, kind: String },
    #[error("field `{0}` offers nothing to choose from")]
    NoChoices(String),
    #[error("field `{field}` is malformed: {reason}")]
    Malformed { field: String, reason: &'static str },
}

/// A form request's fields, in the order the server listed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitForm {
    fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    key: String,
    title: Option<String>,
    description: Option<String>,
    required: bool,
    input: Input,
    /// The schema's default as the labels the form starts out with.
    default: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    /// Text, a number, or yes and no. Never a `Choice`: choices are `Select`.
    Answer(AnswerKind),
    Select {
        choices: Vec<Choice>,
        multiple: bool,
        min_items: Option<usize>,
        max_items: Option<usize>,
    },
}

/// One offered choice: the value the server wants back and the label shown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    value: String,
    label: String,
}

impl ElicitForm {
    pub fn parse(schema: &Value) -> Result<Self, UnsupportedSchema> {
        let root = schema.as_object().ok_or(UnsupportedSchema::NotAnObject)?;
        if root.get(TYPE_FIELD).and_then(Value::as_str) != Some(OBJECT_TYPE) {
            return Err(UnsupportedSchema::NotAnObject);
        }
        let required: Vec<&str> = root
            .get(REQUIRED_FIELD)
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let fields = match root.get(PROPERTIES_FIELD) {
            None => Vec::new(),
            Some(Value::Object(properties)) => properties
                .iter()
                .map(|(key, schema)| Field::parse(key, schema, required.contains(&key.as_str())))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(UnsupportedSchema::NotAnObject),
        };
        Ok(Self { fields })
    }

    /// One question per field. A choice is labelled with its title, and the
    /// value behind each label stays here for [`Self::content`] to send.
    pub fn questions(&self) -> Vec<AskedQuestion> {
        self.fields.iter().map(Field::question).collect()
    }

    /// The content to accept with: one label list per question, in order,
    /// coerced to the types the schema asked for. An optional field left
    /// empty is omitted; anything the schema forbids is an error naming the
    /// field.
    pub fn content(&self, picked: &[Vec<String>]) -> Result<Map<String, Value>, String> {
        let mut content = Map::new();
        for (index, field) in self.fields.iter().enumerate() {
            let labels = picked.get(index).map_or(&[][..], Vec::as_slice);
            let value = field
                .value(labels)
                .map_err(|error| format!("{}: {error}", field.name()))?;
            if let Some(value) = value {
                content.insert(field.key.clone(), value);
            }
        }
        Ok(content)
    }

    /// The answers as a reader would say them, field by field, with anything
    /// that looks like a credential masked.
    pub fn summary(&self, content: &Map<String, Value>) -> Vec<(String, String)> {
        self.fields
            .iter()
            .filter_map(|field| {
                let value = content.get(&field.key)?;
                let shown = match field.is_credential() {
                    true => MASKED_ANSWER.to_owned(),
                    false => field.display(value),
                };
                Some((field.name().to_owned(), shown))
            })
            .collect()
    }

    /// Whether any field's key or title reads like a password, a token or a
    /// one-time code. Autopilot never answers such a form.
    pub fn has_credential_field(&self) -> bool {
        self.fields.iter().any(Field::is_credential)
    }
}

/// Whether `text` is an answer `kind` takes. An empty answer is the form's
/// business: it may be a skip.
pub fn validate_answer(kind: &AnswerKind, text: &str) -> Result<(), String> {
    coerce(kind, text).map(drop)
}

/// What `kind` takes, in words, for a reader who has to type it. `None` for a
/// `Choice`, whose options say it already.
pub fn describe_answer(kind: &AnswerKind) -> Option<String> {
    match kind {
        AnswerKind::Choice => None,
        AnswerKind::Boolean => Some(BOOLEAN_NOUN.to_owned()),
        AnswerKind::Text {
            format,
            min_length,
            max_length,
        } => {
            let noun = format.map_or(TEXT_NOUN, format_noun);
            let length = match (min_length, max_length) {
                (Some(min), Some(max)) if min == max => Some(characters(*min)),
                (Some(min), Some(max)) => Some(format!("{min} to {}", characters(*max))),
                (Some(min), None) => Some(format!("at least {}", characters(*min))),
                (None, Some(max)) => Some(format!("at most {}", characters(*max))),
                (None, None) => None,
            };
            Some(match length {
                Some(length) => format!("{noun}, {length}"),
                None => noun.to_owned(),
            })
        }
        AnswerKind::Number {
            integer,
            minimum,
            maximum,
        } => {
            let noun = if *integer {
                WHOLE_NUMBER_NOUN
            } else {
                NUMBER_NOUN
            };
            Some(match (minimum, maximum) {
                (Some(min), Some(max)) => format!("{noun} from {min} to {max}"),
                (Some(min), None) => format!("{noun} of at least {min}"),
                (None, Some(max)) => format!("{noun} of at most {max}"),
                (None, None) => noun.to_owned(),
            })
        }
    }
}

impl Field {
    fn parse(key: &str, schema: &Value, required: bool) -> Result<Self, UnsupportedSchema> {
        let definition = schema
            .as_object()
            .ok_or_else(|| malformed(key, NOT_A_DEFINITION))?;
        let text = |name: &str| {
            definition
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let input = match definition.get(TYPE_FIELD) {
            Some(Value::String(kind)) => Input::parse(key, kind, definition)?,
            other => {
                return Err(UnsupportedSchema::UnknownType {
                    field: key.to_owned(),
                    kind: other.map_or_else(|| NO_TYPE.to_owned(), Value::to_string),
                });
            }
        };
        let default = definition
            .get(DEFAULT_FIELD)
            .map(|default| input.default_labels(default))
            .unwrap_or_default();
        Ok(Self {
            key: key.to_owned(),
            title: text(TITLE_FIELD),
            description: text(DESCRIPTION_FIELD),
            required,
            input,
            default,
        })
    }

    fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.key)
    }

    fn question(&self) -> AskedQuestion {
        let mut question = self
            .description
            .clone()
            .unwrap_or_else(|| self.name().to_owned());
        let (options, multiple, answer) = match &self.input {
            Input::Answer(AnswerKind::Boolean) => (
                [YES_LABEL, NO_LABEL].map(option).to_vec(),
                false,
                AnswerKind::Boolean,
            ),
            Input::Answer(kind) => (Vec::new(), false, kind.clone()),
            Input::Select {
                choices,
                multiple,
                min_items,
                max_items,
            } => {
                if let Some(limits) = pick_limits(*min_items, *max_items) {
                    question.push('\n');
                    question.push_str(&limits);
                }
                let options = choices.iter().map(|choice| option(&choice.label)).collect();
                (options, *multiple, AnswerKind::Choice)
            }
        };
        AskedQuestion {
            question,
            header: self.name().to_owned(),
            options,
            multiple,
            answer,
            optional: !self.required,
            default: self.default.clone(),
        }
    }

    /// `None` for an optional field left empty, which is omitted rather than
    /// sent blank.
    fn value(&self, labels: &[String]) -> Result<Option<Value>, String> {
        let answered: Vec<&str> = labels
            .iter()
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
            .collect();
        if answered.is_empty() {
            return match (&self.input, self.required) {
                (_, false) => Ok(None),
                (
                    Input::Select {
                        multiple: true,
                        min_items: None | Some(0),
                        ..
                    },
                    true,
                ) => Ok(Some(Value::Array(Vec::new()))),
                _ => Err(REQUIRED.into()),
            };
        }
        match &self.input {
            Input::Answer(kind) => {
                let [text] = answered.as_slice() else {
                    return Err(ONE_ANSWER.into());
                };
                coerce(kind, text).map(Some)
            }
            Input::Select {
                choices,
                multiple: false,
                ..
            } => {
                let [label] = answered.as_slice() else {
                    return Err(ONE_ANSWER.into());
                };
                Ok(Some(Value::String(
                    choice_value(choices, label)?.to_owned(),
                )))
            }
            Input::Select {
                choices,
                multiple: true,
                min_items,
                max_items,
            } => {
                let mut values: Vec<&str> = Vec::new();
                for label in answered {
                    let value = choice_value(choices, label)?;
                    if !values.contains(&value) {
                        values.push(value);
                    }
                }
                if let Some(min) = min_items.filter(|min| values.len() < *min) {
                    return Err(format!("{TOO_FEW} {min}"));
                }
                if let Some(max) = max_items.filter(|max| values.len() > *max) {
                    return Err(format!("{TOO_MANY} {max}"));
                }
                Ok(Some(Value::Array(
                    values.into_iter().map(|value| value.into()).collect(),
                )))
            }
        }
    }

    fn display(&self, value: &Value) -> String {
        let label = |value: &str| match &self.input {
            Input::Select { choices, .. } => choices
                .iter()
                .find(|choice| choice.value == value)
                .map_or(value, |choice| &choice.label)
                .to_owned(),
            Input::Answer(_) => value.to_owned(),
        };
        match value {
            Value::String(text) => label(text),
            Value::Bool(flag) => bool_label(*flag).to_owned(),
            Value::Array(values) => values
                .iter()
                .filter_map(Value::as_str)
                .map(label)
                .collect::<Vec<_>>()
                .join(LIST_SEPARATOR),
            other => other.to_string(),
        }
    }

    fn is_credential(&self) -> bool {
        looks_like_credential(&self.key) || self.title.as_deref().is_some_and(looks_like_credential)
    }
}

impl Input {
    fn parse(
        key: &str,
        kind: &str,
        definition: &Map<String, Value>,
    ) -> Result<Self, UnsupportedSchema> {
        Ok(match kind {
            STRING_TYPE => match choices(key, definition, ONE_OF_FIELD)? {
                Some(choices) => Self::Select {
                    choices,
                    multiple: false,
                    min_items: None,
                    max_items: None,
                },
                None => Self::Answer(AnswerKind::Text {
                    format: definition
                        .get(FORMAT_FIELD)
                        .and_then(|format| TextFormat::deserialize(format).ok()),
                    min_length: count(definition, MIN_LENGTH_FIELD),
                    max_length: count(definition, MAX_LENGTH_FIELD),
                }),
            },
            NUMBER_TYPE | INTEGER_TYPE => Self::Answer(AnswerKind::Number {
                integer: kind == INTEGER_TYPE,
                minimum: number(definition, MINIMUM_FIELD),
                maximum: number(definition, MAXIMUM_FIELD),
            }),
            BOOLEAN_TYPE => Self::Answer(AnswerKind::Boolean),
            ARRAY_TYPE => {
                let items = definition
                    .get(ITEMS_FIELD)
                    .and_then(Value::as_object)
                    .ok_or_else(|| malformed(key, NO_ITEMS))?;
                Self::Select {
                    choices: choices(key, items, ANY_OF_FIELD)?
                        .ok_or_else(|| UnsupportedSchema::NoChoices(key.to_owned()))?,
                    multiple: true,
                    min_items: count(definition, MIN_ITEMS_FIELD),
                    max_items: count(definition, MAX_ITEMS_FIELD),
                }
            }
            OBJECT_TYPE => return Err(UnsupportedSchema::Nested(key.to_owned())),
            other => {
                return Err(UnsupportedSchema::UnknownType {
                    field: key.to_owned(),
                    kind: other.to_owned(),
                });
            }
        })
    }

    /// A default the field cannot take is dropped rather than refused: the
    /// form still asks, it only starts out empty.
    fn default_labels(&self, default: &Value) -> Vec<String> {
        match (self, default) {
            (Self::Answer(AnswerKind::Boolean), Value::Bool(flag)) => {
                vec![bool_label(*flag).to_owned()]
            }
            (Self::Answer(AnswerKind::Boolean), _) => Vec::new(),
            (Self::Answer(_), Value::String(text)) => vec![text.clone()],
            (Self::Answer(_), Value::Number(number)) => vec![number.to_string()],
            (Self::Select { choices, .. }, Value::String(value)) => {
                labels_of(choices, [value.as_str()])
            }
            (Self::Select { choices, .. }, Value::Array(values)) => {
                labels_of(choices, values.iter().filter_map(Value::as_str))
            }
            _ => Vec::new(),
        }
    }
}

/// The choices a definition offers: titled entries under `titled`, or a plain
/// `enum` named by the legacy `enumNames` when it has as many. `None` when it
/// offers neither.
fn choices(
    key: &str,
    definition: &Map<String, Value>,
    titled: &str,
) -> Result<Option<Vec<Choice>>, UnsupportedSchema> {
    let choices: Vec<Choice> = if let Some(entries) = definition.get(titled) {
        entries
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .map(|entry| {
                        let value = entry.get(CONST_FIELD)?.as_str()?;
                        let label = entry
                            .get(TITLE_FIELD)
                            .and_then(Value::as_str)
                            .unwrap_or(value);
                        Some(Choice {
                            value: value.to_owned(),
                            label: label.to_owned(),
                        })
                    })
                    .collect()
            })
            .ok_or_else(|| malformed(key, CHOICES_NOT_STRINGS))?
    } else if let Some(values) = definition.get(ENUM_FIELD) {
        let values: Vec<&str> = values
            .as_array()
            .and_then(|values| values.iter().map(Value::as_str).collect())
            .ok_or_else(|| malformed(key, CHOICES_NOT_STRINGS))?;
        let names: Option<Vec<&str>> = definition
            .get(ENUM_NAMES_FIELD)
            .and_then(Value::as_array)
            .and_then(|names| names.iter().map(Value::as_str).collect())
            .filter(|names: &Vec<&str>| names.len() == values.len());
        values
            .iter()
            .enumerate()
            .map(|(index, value)| Choice {
                value: (*value).to_owned(),
                label: names
                    .as_ref()
                    .map_or(*value, |names| names[index])
                    .to_owned(),
            })
            .collect()
    } else {
        return Ok(None);
    };
    if choices.is_empty() {
        return Err(UnsupportedSchema::NoChoices(key.to_owned()));
    }
    Ok(Some(distinct_labels(choices)))
}

/// A label is how a pick finds its value again, so two choices titled alike
/// are told apart by their values.
fn distinct_labels(mut choices: Vec<Choice>) -> Vec<Choice> {
    let repeated: Vec<bool> = choices
        .iter()
        .map(|choice| {
            choices
                .iter()
                .filter(|other| other.label == choice.label)
                .count()
                > 1
        })
        .collect();
    for (choice, repeated) in choices.iter_mut().zip(repeated) {
        if repeated {
            choice.label = format!("{} ({})", choice.label, choice.value);
        }
    }
    choices
}

fn labels_of<'a>(choices: &[Choice], values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(|value| choices.iter().find(|choice| choice.value == value))
        .map(|choice| choice.label.clone())
        .collect()
}

/// A pick is normally a label, but a delegate may answer with the value the
/// server listed, which means the same thing.
fn choice_value<'a>(choices: &'a [Choice], picked: &str) -> Result<&'a str, String> {
    choices
        .iter()
        .find(|choice| choice.label == picked)
        .or_else(|| choices.iter().find(|choice| choice.value == picked))
        .map(|choice| choice.value.as_str())
        .ok_or_else(|| NOT_OFFERED.into())
}

fn coerce(kind: &AnswerKind, text: &str) -> Result<Value, String> {
    let text = text.trim();
    match kind {
        AnswerKind::Choice => Ok(text.into()),
        AnswerKind::Boolean => parse_bool(text).map(Value::Bool),
        AnswerKind::Text {
            format,
            min_length,
            max_length,
        } => {
            let length = text.chars().count();
            if let Some(min) = min_length.filter(|min| length < *min) {
                return Err(format!("{TOO_SHORT} {}", characters(min)));
            }
            if let Some(max) = max_length.filter(|max| length > *max) {
                return Err(format!("{TOO_LONG} {}", characters(max)));
            }
            match format {
                Some(format) => fits_format(*format, text).map(|()| text.into()),
                None => Ok(text.into()),
            }
        }
        AnswerKind::Number {
            integer,
            minimum,
            maximum,
        } => {
            let number = parse_number(text, *integer)?;
            let value = number.as_f64().unwrap_or_default();
            if let Some(min) = minimum.as_ref().filter(|min| min.as_f64() > Some(value)) {
                return Err(format!("{BELOW_MINIMUM} {min}"));
            }
            if let Some(max) = maximum.as_ref().filter(|max| max.as_f64() < Some(value)) {
                return Err(format!("{ABOVE_MAXIMUM} {max}"));
            }
            Ok(Value::Number(number))
        }
    }
}

/// A whole number stays one even where the schema takes any number, so `5`
/// is sent as `5` rather than `5.0`.
fn parse_number(text: &str, integer: bool) -> Result<Number, String> {
    if let Ok(whole) = text.parse::<i64>() {
        return Ok(whole.into());
    }
    if integer {
        return Err(NOT_A_WHOLE_NUMBER.into());
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .ok_or_else(|| NOT_A_NUMBER.into())
}

fn parse_bool(text: &str) -> Result<bool, String> {
    let word = text.to_lowercase();
    if TRUE_WORDS.contains(&word.as_str()) {
        Ok(true)
    } else if FALSE_WORDS.contains(&word.as_str()) {
        Ok(false)
    } else {
        Err(NOT_YES_OR_NO.into())
    }
}

fn fits_format(format: TextFormat, text: &str) -> Result<(), String> {
    let (fits, error) = match format {
        TextFormat::Email => (is_email(text), NOT_AN_EMAIL),
        TextFormat::Uri => (Url::parse(text).is_ok(), NOT_A_URI),
        TextFormat::Date => (text.parse::<Date>().is_ok(), NOT_A_DATE),
        TextFormat::DateTime => (text.parse::<Timestamp>().is_ok(), NOT_A_DATE_TIME),
    };
    fits.then_some(()).ok_or_else(|| error.into())
}

/// One `@` between a local part and a dotted domain, and no spaces: enough to
/// catch a slip without refusing an address a mail server would take.
fn is_email(text: &str) -> bool {
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && !text.chars().any(char::is_whitespace)
        && domain
            .split('.')
            .collect::<Vec<_>>()
            .split_first()
            .is_some_and(|(first, rest)| {
                !first.is_empty() && !rest.is_empty() && rest.iter().all(|label| !label.is_empty())
            })
}

fn format_noun(format: TextFormat) -> &'static str {
    match format {
        TextFormat::Email => EMAIL_NOUN,
        TextFormat::Uri => URI_NOUN,
        TextFormat::Date => DATE_NOUN,
        TextFormat::DateTime => DATE_TIME_NOUN,
    }
}

fn characters(count: usize) -> String {
    let noun = if count == 1 {
        CHARACTER_NOUN
    } else {
        CHARACTERS_NOUN
    };
    format!("{count} {noun}")
}

fn pick_limits(min: Option<usize>, max: Option<usize>) -> Option<String> {
    match (min.filter(|min| *min > 0), max) {
        (Some(min), Some(max)) if min == max => Some(format!("Pick {min}.")),
        (Some(min), Some(max)) => Some(format!("Pick {min} to {max}.")),
        (Some(min), None) => Some(format!("{TOO_FEW} {min}.")),
        (None, Some(max)) => Some(format!("{TOO_MANY} {max}.")),
        (None, None) => None,
    }
}

fn bool_label(flag: bool) -> &'static str {
    if flag { YES_LABEL } else { NO_LABEL }
}

fn option(label: &str) -> QuestionOption {
    QuestionOption {
        label: label.to_owned(),
        description: String::new(),
    }
}

fn count(definition: &Map<String, Value>, name: &str) -> Option<usize> {
    definition
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
}

fn number(definition: &Map<String, Value>, name: &str) -> Option<Number> {
    match definition.get(name)? {
        Value::Number(number) => Some(number.clone()),
        _ => None,
    }
}

fn malformed(key: &str, reason: &'static str) -> UnsupportedSchema {
    UnsupportedSchema::Malformed {
        field: key.to_owned(),
        reason,
    }
}

/// Long terms are searched for anywhere in the run of letters and digits, so
/// `api_key`, `apiKey` and `API key` all read as `apikey`. Short ones would
/// turn up inside unrelated words, so they have to stand alone.
fn looks_like_credential(name: &str) -> bool {
    let joined: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    CREDENTIAL_TERMS.iter().any(|term| joined.contains(term))
        || words(name)
            .iter()
            .any(|word| CREDENTIAL_WORDS.contains(&word.as_str()))
}

/// Lowercase words, split at separators, at camel-case humps (`OTPCode` is
/// `otp` and `code`), and where digits follow letters (`my2FA` is `my` and
/// `2fa`).
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (index, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            words.push(std::mem::take(&mut word));
            continue;
        }
        let previous = index.checked_sub(1).map(|index| chars[index]);
        let next = chars.get(index + 1).copied();
        let hump = c.is_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_lowercase()
                    || (previous.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        let digits = c.is_numeric() && previous.is_some_and(char::is_alphabetic);
        if hump || digits {
            words.push(std::mem::take(&mut word));
        }
        word.extend(c.to_lowercase());
    }
    words.push(word);
    words.retain(|word| !word.is_empty());
    words
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const NAME: &str = "name";
    const NAME_TITLE: &str = "Display name";
    const NAME_DESCRIPTION: &str = "How others see you";
    const AGE: &str = "age";
    const COLOR: &str = "color";
    const COLORS: &str = "colors";
    const RED: &str = "#FF0000";
    const GREEN: &str = "#00FF00";
    const RED_LABEL: &str = "Red";
    const GREEN_LABEL: &str = "Green";
    const AGREE: &str = "agree";
    const TYPED: &str = "octocat";
    const MALFORMED_NOTE: &str = "a schema a form cannot hold must be refused, not guessed at";
    const STRICT: &str = "an answer the schema forbids must never be sent";

    fn form(properties: Value, required: &[&str]) -> ElicitForm {
        ElicitForm::parse(&json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }))
        .unwrap()
    }

    fn one(property: Value, required: bool) -> ElicitForm {
        let required: &[&str] = if required { &[NAME] } else { &[] };
        form(json!({ NAME: property }), required)
    }

    fn picks(labels: &[&str]) -> Vec<Vec<String>> {
        vec![labels.iter().map(|label| (*label).to_owned()).collect()]
    }

    fn answered(form: &ElicitForm, labels: &[&str]) -> Result<Value, String> {
        form.content(&picks(labels))
            .map(|content| content.get(NAME).cloned().unwrap_or(Value::Null))
    }

    fn titled_colors() -> Value {
        json!([{ "const": RED, "title": RED_LABEL }, { "const": GREEN, "title": GREEN_LABEL }])
    }

    #[test]
    fn a_text_field_reads_its_title_description_limits_and_default() {
        let form = one(
            json!({
                "type": "string",
                "title": NAME_TITLE,
                "description": NAME_DESCRIPTION,
                "minLength": 3,
                "maxLength": 20,
                "format": "email",
                "default": TYPED,
            }),
            true,
        );

        let [question] = form.questions().try_into().unwrap();

        assert_eq!(question.header, NAME_TITLE);
        assert_eq!(question.question, NAME_DESCRIPTION);
        assert!(question.options.is_empty());
        assert!(!question.optional);
        assert_eq!(question.default, [TYPED]);
        assert_eq!(
            question.answer,
            AnswerKind::Text {
                format: Some(TextFormat::Email),
                min_length: Some(3),
                max_length: Some(20),
            }
        );
    }

    #[test_case("email", Some(TextFormat::Email) ; "email")]
    #[test_case("uri", Some(TextFormat::Uri) ; "uri")]
    #[test_case("date", Some(TextFormat::Date) ; "date")]
    #[test_case("date-time", Some(TextFormat::DateTime) ; "date_time")]
    #[test_case("uuid", None ; "an_unknown_format_is_plain_text")]
    fn a_text_format_is_read(format: &str, expected: Option<TextFormat>) {
        let form = one(json!({ "type": "string", "format": format }), true);

        let AnswerKind::Text { format, .. } = &form.questions()[0].answer else {
            panic!("a string is text");
        };
        assert_eq!(*format, expected);
    }

    #[test]
    fn a_field_without_a_title_is_named_by_its_key() {
        let form = one(json!({ "type": "string" }), false);

        let question = &form.questions()[0];

        assert_eq!(question.header, NAME);
        assert_eq!(question.question, NAME);
        assert!(question.optional);
    }

    #[test_case("number", false ; "number")]
    #[test_case("integer", true ; "integer")]
    fn a_number_field_reads_its_range_and_default(kind: &str, integer: bool) {
        let form = one(
            json!({ "type": kind, "minimum": 1, "maximum": 10, "default": 5 }),
            true,
        );

        let question = &form.questions()[0];

        assert_eq!(
            question.answer,
            AnswerKind::Number {
                integer,
                minimum: Some(1.into()),
                maximum: Some(10.into()),
            }
        );
        assert_eq!(question.default, ["5"]);
    }

    #[test_case(json!(true), &[YES_LABEL] ; "a_true_default_picks_yes")]
    #[test_case(json!(false), &[NO_LABEL] ; "a_false_default_picks_no")]
    #[test_case(json!("yes"), &[] ; "a_default_of_the_wrong_type_is_dropped")]
    fn a_boolean_offers_yes_and_no(default: Value, expected: &[&str]) {
        let form = one(json!({ "type": "boolean", "default": default }), true);

        let question = &form.questions()[0];

        assert_eq!(question.answer, AnswerKind::Boolean);
        let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, [YES_LABEL, NO_LABEL]);
        assert_eq!(question.default, expected);
    }

    #[test_case(json!({ "type": "string", "enum": [RED, GREEN], "default": GREEN }), &[RED, GREEN], &[GREEN] ; "plain_enum")]
    #[test_case(json!({ "type": "string", "oneOf": titled_colors(), "default": GREEN }), &[RED_LABEL, GREEN_LABEL], &[GREEN_LABEL] ; "titled_one_of")]
    #[test_case(json!({ "type": "string", "enum": [RED, GREEN], "enumNames": [RED_LABEL, GREEN_LABEL] }), &[RED_LABEL, GREEN_LABEL], &[] ; "legacy_enum_names")]
    #[test_case(json!({ "type": "string", "enum": [RED, GREEN], "enumNames": [RED_LABEL] }), &[RED, GREEN], &[] ; "enum_names_of_the_wrong_length_are_ignored")]
    fn a_single_select_offers_its_choices(property: Value, labels: &[&str], default: &[&str]) {
        let form = one(property, true);

        let question = &form.questions()[0];

        assert_eq!(question.answer, AnswerKind::Choice);
        assert!(!question.multiple);
        let offered: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(offered, labels);
        assert_eq!(question.default, default);
    }

    #[test_case(json!({ "enum": [RED, GREEN] }), &[RED, GREEN] ; "plain_items")]
    #[test_case(json!({ "anyOf": titled_colors() }), &[RED_LABEL, GREEN_LABEL] ; "titled_items")]
    fn a_multi_select_offers_its_choices_and_says_its_limits(items: Value, labels: &[&str]) {
        let form = one(
            json!({
                "type": "array",
                "description": NAME_DESCRIPTION,
                "items": items,
                "minItems": 1,
                "maxItems": 2,
                "default": [RED],
            }),
            true,
        );

        let question = &form.questions()[0];

        assert!(question.multiple);
        let offered: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(offered, labels);
        assert_eq!(question.default, [labels[0]]);
        assert_eq!(
            question.question,
            format!("{NAME_DESCRIPTION}\nPick 1 to 2.")
        );
    }

    #[test]
    fn choices_titled_alike_are_told_apart() {
        let form = one(
            json!({ "type": "string", "oneOf": [
                { "const": RED, "title": RED_LABEL },
                { "const": GREEN, "title": RED_LABEL },
            ]}),
            true,
        );

        let question = &form.questions()[0];

        assert_eq!(question.options[0].label, format!("{RED_LABEL} ({RED})"));
        assert_eq!(
            answered(&form, &[&question.options[1].label]),
            Ok(json!(GREEN))
        );
    }

    #[test]
    fn fields_keep_the_order_the_server_listed() {
        let form = form(
            json!({
                NAME: { "type": "string" },
                AGE: { "type": "integer" },
                AGREE: { "type": "boolean" },
            }),
            &[],
        );

        let headers: Vec<String> = form.questions().into_iter().map(|q| q.header).collect();

        assert_eq!(headers, [NAME, AGE, AGREE]);
    }

    #[test]
    fn a_form_with_no_properties_asks_nothing() {
        let form = ElicitForm::parse(&json!({ "type": "object" })).unwrap();

        assert!(form.questions().is_empty());
        assert_eq!(form.content(&[]), Ok(Map::new()));
    }

    #[test_case(json!([]) ; "an_array_root")]
    #[test_case(json!({ "type": "string" }) ; "a_string_root")]
    #[test_case(json!({ "properties": {} }) ; "a_root_without_a_type")]
    #[test_case(json!({ "type": "object", "properties": [] }) ; "properties_that_are_not_an_object")]
    fn a_root_that_is_not_a_flat_object_is_refused(schema: Value) {
        assert_eq!(
            ElicitForm::parse(&schema),
            Err(UnsupportedSchema::NotAnObject),
            "{MALFORMED_NOTE}"
        );
    }

    #[test_case(json!({ "type": "object", "properties": {} }), UnsupportedSchema::Nested(NAME.into()) ; "a_nested_object")]
    #[test_case(json!({ "type": "null" }), UnsupportedSchema::UnknownType { field: NAME.into(), kind: "null".into() } ; "an_unknown_type")]
    #[test_case(json!({ "enum": [RED] }), UnsupportedSchema::UnknownType { field: NAME.into(), kind: NO_TYPE.into() } ; "no_type")]
    #[test_case(json!({ "type": ["string", "null"] }), UnsupportedSchema::UnknownType { field: NAME.into(), kind: r#"["string","null"]"#.into() } ; "a_type_list")]
    #[test_case(json!({ "type": "string", "enum": [] }), UnsupportedSchema::NoChoices(NAME.into()) ; "an_empty_enum")]
    #[test_case(json!({ "type": "array", "items": { "type": "string" } }), UnsupportedSchema::NoChoices(NAME.into()) ; "a_list_of_free_text")]
    #[test_case(json!({ "type": "array" }), malformed(NAME, NO_ITEMS) ; "a_list_without_items")]
    #[test_case(json!({ "type": "string", "enum": [1, 2] }), malformed(NAME, CHOICES_NOT_STRINGS) ; "numeric_choices")]
    #[test_case(json!({ "type": "string", "oneOf": [{ "title": RED_LABEL }] }), malformed(NAME, CHOICES_NOT_STRINGS) ; "a_titled_choice_without_a_value")]
    #[test_case(json!("string"), malformed(NAME, NOT_A_DEFINITION) ; "a_definition_that_is_not_an_object")]
    fn a_field_a_form_cannot_hold_is_refused(property: Value, expected: UnsupportedSchema) {
        assert_eq!(
            ElicitForm::parse(&json!({ "type": "object", "properties": { NAME: property } })),
            Err(expected),
            "{MALFORMED_NOTE}"
        );
    }

    #[test_case(json!({ "type": "string" }), &[TYPED], Ok(json!(TYPED)) ; "text")]
    #[test_case(json!({ "type": "string" }), &["  padded  "], Ok(json!("padded")) ; "text_is_trimmed")]
    #[test_case(json!({ "type": "integer" }), &["42"], Ok(json!(42)) ; "integer")]
    #[test_case(json!({ "type": "number" }), &["2.5"], Ok(json!(2.5)) ; "fraction")]
    #[test_case(json!({ "type": "number" }), &["5"], Ok(json!(5)) ; "a_whole_number_stays_whole")]
    #[test_case(json!({ "type": "boolean" }), &[YES_LABEL], Ok(json!(true)) ; "yes")]
    #[test_case(json!({ "type": "boolean" }), &[NO_LABEL], Ok(json!(false)) ; "no")]
    #[test_case(json!({ "type": "boolean" }), &["TRUE"], Ok(json!(true)) ; "a_delegates_true")]
    #[test_case(json!({ "type": "string", "oneOf": titled_colors() }), &[GREEN_LABEL], Ok(json!(GREEN)) ; "a_title_sends_its_value")]
    #[test_case(json!({ "type": "string", "oneOf": titled_colors() }), &[GREEN], Ok(json!(GREEN)) ; "a_value_sends_itself")]
    #[test_case(json!({ "type": "string", "enum": [RED, GREEN], "enumNames": [RED_LABEL, GREEN_LABEL] }), &[RED_LABEL], Ok(json!(RED)) ; "a_legacy_name_sends_its_value")]
    #[test_case(json!({ "type": "array", "items": { "anyOf": titled_colors() } }), &[GREEN_LABEL, RED_LABEL, GREEN_LABEL], Ok(json!([GREEN, RED])) ; "several_picks_once_each")]
    fn an_answer_is_sent_as_the_type_the_schema_asked_for(
        property: Value,
        labels: &[&str],
        expected: Result<Value, String>,
    ) {
        assert_eq!(answered(&one(property, true), labels), expected);
    }

    #[test_case(json!({ "type": "string", "minLength": 3 }), &["ab"], TOO_SHORT ; "too_short")]
    #[test_case(json!({ "type": "string", "maxLength": 3 }), &["abcd"], TOO_LONG ; "too_long")]
    #[test_case(json!({ "type": "string", "format": "email" }), &["not an email"], NOT_AN_EMAIL ; "not_an_email")]
    #[test_case(json!({ "type": "string", "format": "email" }), &["a@localhost"], NOT_AN_EMAIL ; "an_email_without_a_dotted_domain")]
    #[test_case(json!({ "type": "string", "format": "uri" }), &["example.com"], NOT_A_URI ; "a_uri_without_a_scheme")]
    #[test_case(json!({ "type": "string", "format": "date" }), &["2026-02-30"], NOT_A_DATE ; "a_date_that_does_not_exist")]
    #[test_case(json!({ "type": "string", "format": "date-time" }), &["2026-10-13T09:30:00"], NOT_A_DATE_TIME ; "a_date_time_without_an_offset")]
    #[test_case(json!({ "type": "number" }), &["many"], NOT_A_NUMBER ; "not_a_number")]
    #[test_case(json!({ "type": "number" }), &["inf"], NOT_A_NUMBER ; "infinity")]
    #[test_case(json!({ "type": "integer" }), &["2.5"], NOT_A_WHOLE_NUMBER ; "a_fraction_for_an_integer")]
    #[test_case(json!({ "type": "integer", "minimum": 18 }), &["17"], BELOW_MINIMUM ; "below_the_minimum")]
    #[test_case(json!({ "type": "number", "maximum": 1.5 }), &["1.6"], ABOVE_MAXIMUM ; "above_the_maximum")]
    #[test_case(json!({ "type": "boolean" }), &["maybe"], NOT_YES_OR_NO ; "neither_yes_nor_no")]
    #[test_case(json!({ "type": "string", "enum": [RED] }), &[GREEN], NOT_OFFERED ; "a_choice_not_offered")]
    #[test_case(json!({ "type": "string", "enum": [RED, GREEN] }), &[RED, GREEN], ONE_ANSWER ; "two_picks_for_one_choice")]
    #[test_case(json!({ "type": "string" }), &[TYPED, TYPED], ONE_ANSWER ; "two_answers_for_one_text")]
    #[test_case(json!({ "type": "array", "items": { "enum": [RED, GREEN] }, "minItems": 2 }), &[RED], TOO_FEW ; "too_few_picks")]
    #[test_case(json!({ "type": "array", "items": { "enum": [RED, GREEN] }, "maxItems": 1 }), &[RED, GREEN], TOO_MANY ; "too_many_picks")]
    #[test_case(json!({ "type": "string" }), &[], REQUIRED ; "a_required_field_left_empty")]
    #[test_case(json!({ "type": "string" }), &["  "], REQUIRED ; "a_required_field_left_blank")]
    fn an_answer_the_schema_forbids_is_refused_naming_its_field(
        property: Value,
        labels: &[&str],
        error: &str,
    ) {
        let refused = answered(&one(property, true), labels).unwrap_err();

        assert!(
            refused.starts_with(&format!("{NAME}: {error}")),
            "{STRICT}: {refused}"
        );
    }

    #[test_case("2026-10-13", TextFormat::Date ; "date")]
    #[test_case("2026-10-13T09:30:00Z", TextFormat::DateTime ; "utc_date_time")]
    #[test_case("2026-10-13T09:30:00+02:00", TextFormat::DateTime ; "offset_date_time")]
    #[test_case("octo@example.com", TextFormat::Email ; "email")]
    #[test_case("https://example.com/path", TextFormat::Uri ; "uri")]
    fn a_well_formed_answer_fits_its_format(text: &str, format: TextFormat) {
        let kind = AnswerKind::Text {
            format: Some(format),
            min_length: None,
            max_length: None,
        };

        assert_eq!(validate_answer(&kind, text), Ok(()));
    }

    #[test]
    fn an_optional_field_left_empty_is_omitted() {
        let form = form(
            json!({ NAME: { "type": "string" }, AGE: { "type": "integer" } }),
            &[NAME],
        );

        let content = form.content(&[vec![TYPED.into()], Vec::new()]).unwrap();

        assert_eq!(Value::Object(content), json!({ NAME: TYPED }));
    }

    #[test]
    fn a_missing_answer_list_counts_as_empty() {
        let form = form(json!({ NAME: { "type": "string" } }), &[NAME]);

        assert!(form.content(&[]).is_err(), "{STRICT}");
    }

    #[test_case(None, Ok(json!([])) ; "without_a_minimum_an_empty_list_is_an_answer")]
    #[test_case(Some(1), Err(REQUIRED) ; "with_a_minimum_it_is_not")]
    fn a_required_multi_select_left_empty(min_items: Option<usize>, expected: Result<Value, &str>) {
        let form = one(
            json!({ "type": "array", "items": { "enum": [RED] }, "minItems": min_items }),
            true,
        );

        let sent = answered(&form, &[]);

        match expected {
            Ok(value) => assert_eq!(sent, Ok(value)),
            Err(error) => assert!(sent.unwrap_err().ends_with(error), "{STRICT}"),
        }
    }

    #[test]
    fn a_summary_says_the_answers_as_the_reader_saw_them() {
        let form = form(
            json!({
                NAME: { "type": "string", "title": NAME_TITLE },
                AGREE: { "type": "boolean" },
                COLOR: { "type": "string", "oneOf": titled_colors() },
                COLORS: { "type": "array", "items": { "anyOf": titled_colors() } },
                AGE: { "type": "integer" },
            }),
            &[],
        );
        let content = form
            .content(&[
                vec![TYPED.into()],
                vec![YES_LABEL.into()],
                vec![RED_LABEL.into()],
                vec![RED_LABEL.into(), GREEN_LABEL.into()],
                Vec::new(),
            ])
            .unwrap();

        let summary = form.summary(&content);

        let expected: Vec<(String, String)> = [
            (NAME_TITLE, TYPED.to_owned()),
            (AGREE, YES_LABEL.to_owned()),
            (COLOR, RED_LABEL.to_owned()),
            (COLORS, format!("{RED_LABEL}{LIST_SEPARATOR}{GREEN_LABEL}")),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        assert_eq!(summary, expected);
    }

    #[test_case("password", None ; "password")]
    #[test_case("newPassword", None ; "camel_case_password")]
    #[test_case("passcode", None ; "passcode")]
    #[test_case("client_secret", None ; "secret")]
    #[test_case("accessToken", None ; "token")]
    #[test_case("api_key", None ; "snake_case_api_key")]
    #[test_case("apiKey", None ; "camel_case_api_key")]
    #[test_case("value", Some("API Key") ; "a_title_saying_api_key")]
    #[test_case("OTPCode", None ; "otp_hump")]
    #[test_case("otp", None ; "otp")]
    #[test_case("totp", None ; "totp")]
    #[test_case("code", Some("Your 2FA code") ; "a_title_saying_2fa")]
    #[test_case("my2FA", None ; "digits_after_letters")]
    #[test_case("aws_credentials", None ; "credentials")]
    fn a_credential_looking_field_is_masked(key: &str, title: Option<&str>) {
        let form = form(json!({ key: { "type": "string", "title": title } }), &[]);
        let content = form.content(&[vec![TYPED.into()]]).unwrap();

        assert!(form.has_credential_field());
        assert_eq!(form.summary(&content)[0].1, MASKED_ANSWER);
    }

    #[test_case("footprint" ; "otp_inside_a_word")]
    #[test_case("username" ; "username")]
    #[test_case("hotpot" ; "otp_spanning_a_word")]
    fn an_ordinary_field_is_not_mistaken_for_a_credential(key: &str) {
        let form = form(json!({ key: { "type": "string" } }), &[]);

        assert!(!form.has_credential_field());
    }

    #[test_case(AnswerKind::Choice, None ; "choice")]
    #[test_case(AnswerKind::Boolean, Some("yes or no") ; "boolean")]
    #[test_case(AnswerKind::Text { format: None, min_length: None, max_length: None }, Some("text") ; "plain_text")]
    #[test_case(AnswerKind::Text { format: Some(TextFormat::Email), min_length: None, max_length: Some(50) }, Some("an email address, at most 50 characters") ; "email_with_a_cap")]
    #[test_case(AnswerKind::Text { format: None, min_length: Some(3), max_length: Some(20) }, Some("text, 3 to 20 characters") ; "length_range")]
    #[test_case(AnswerKind::Text { format: None, min_length: Some(1), max_length: Some(1) }, Some("text, 1 character") ; "exact_length")]
    #[test_case(AnswerKind::Number { integer: true, minimum: Some(1.into()), maximum: Some(10.into()) }, Some("a whole number from 1 to 10") ; "integer_range")]
    #[test_case(AnswerKind::Number { integer: false, minimum: Some(0.into()), maximum: None }, Some("a number of at least 0") ; "number_floor")]
    fn an_answer_kind_is_described_with_its_limits(kind: AnswerKind, expected: Option<&str>) {
        assert_eq!(describe_answer(&kind).as_deref(), expected);
    }

    #[test_case(None, Some(2), Some("Pick at most 2.") ; "a_cap")]
    #[test_case(Some(2), None, Some("Pick at least 2.") ; "a_floor")]
    #[test_case(Some(2), Some(2), Some("Pick 2.") ; "an_exact_count")]
    #[test_case(Some(0), None, None ; "a_zero_floor_says_nothing")]
    fn pick_limits_read_as_a_sentence(
        min: Option<usize>,
        max: Option<usize>,
        expected: Option<&str>,
    ) {
        assert_eq!(pick_limits(min, max).as_deref(), expected);
    }
}
