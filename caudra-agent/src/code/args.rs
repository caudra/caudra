//! Reading a host function's arguments the way Python binds them.

use serde_json::{Map, Value};

use super::runner::{ScriptCall, ScriptError, ScriptErrorKind};

/// A helper's arguments by parameter name, positional ones first.
pub fn bind(
    helper: &str,
    params: &[&str],
    call: &ScriptCall,
) -> Result<Map<String, Value>, ScriptError> {
    let type_error = |message: String| ScriptError::new(ScriptErrorKind::Type, message);
    if call.args.len() > params.len() {
        return Err(type_error(format!(
            "{helper}() takes at most {} positional arguments ({} given)",
            params.len(),
            call.args.len()
        )));
    }
    let mut bound: Map<String, Value> = params
        .iter()
        .zip(&call.args)
        .map(|(param, value)| ((*param).to_owned(), value.clone()))
        .collect();
    for (name, value) in &call.kwargs {
        if !params.contains(&name.as_str()) {
            return Err(type_error(format!(
                "{helper}() got an unexpected keyword argument '{name}'"
            )));
        }
        if bound.insert(name.clone(), value.clone()).is_some() {
            return Err(type_error(format!(
                "{helper}() got multiple values for argument '{name}'"
            )));
        }
    }
    Ok(bound)
}

pub fn text(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<String>, ScriptError> {
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() argument '{param}' must be a str"),
        )),
    }
}

pub fn required_text(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<String, ScriptError> {
    text(arguments, helper, param)?.ok_or_else(|| {
        ScriptError::new(
            ScriptErrorKind::Type,
            format!("{helper}() missing required argument: '{param}'"),
        )
    })
}

pub fn count(
    arguments: &Map<String, Value>,
    helper: &str,
    param: &str,
) -> Result<Option<usize>, ScriptError> {
    match arguments.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .map(Some)
            .ok_or_else(|| {
                ScriptError::new(
                    ScriptErrorKind::Value,
                    format!("{helper}() argument '{param}' must be a non-negative int"),
                )
            }),
    }
}
