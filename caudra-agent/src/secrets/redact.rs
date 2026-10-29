//! Hides known secret values in text the host shows or stores, such as an automation's HTTP
//! response or a shell command's output. It only catches the literal value: a command that
//! encodes or splits a secret still prints it.

use std::borrow::Cow;
use std::mem;

use serde_json::Value;

const VARIABLE_OPEN: &str = "${";
const VARIABLE_CLOSE: &str = "}";

/// The secrets to hide, longest first so a secret that holds another is hidden whole, each with
/// the text shown in its place.
#[derive(Default)]
pub struct Redactor {
    hidden: Vec<(String, String)>,
}

impl Redactor {
    pub fn hide(&mut self, secret: &str, shown: &str) {
        if secret.is_empty() || secret == shown {
            return;
        }
        let at = self
            .hidden
            .partition_point(|(hidden, _)| hidden.len() >= secret.len());
        self.hidden
            .insert(at, (secret.to_owned(), shown.to_owned()));
    }

    /// Shows `secret` as `${variable}`.
    pub fn hide_variable(&mut self, variable: &str, secret: &str) {
        self.hide(secret, &shown_variable(variable));
    }

    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }

    /// `text` with every secret replaced, scanned once per secret, and borrowed when it holds none.
    pub fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.hidden
            .iter()
            .fold(Cow::Borrowed(text), |text, (secret, shown)| {
                replaced(text, secret, shown)
            })
    }

    /// `text` itself, unless it holds a secret.
    pub fn owned(&self, text: String) -> String {
        let replaced = match self.text(&text) {
            Cow::Owned(replaced) => Some(replaced),
            Cow::Borrowed(_) => None,
        };
        replaced.unwrap_or(text)
    }

    /// Replaces the secrets in the strings and keys of `value`.
    pub fn value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.owned(mem::take(text)),
            Value::Array(items) => items.iter_mut().for_each(|item| self.value(item)),
            Value::Object(entries) => {
                entries.values_mut().for_each(|item| self.value(item));
                if entries.keys().any(|key| self.hides(key)) {
                    *entries = mem::take(entries)
                        .into_iter()
                        .map(|(key, item)| (self.owned(key), item))
                        .collect();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn hides(&self, text: &str) -> bool {
        self.hidden
            .iter()
            .any(|(secret, _)| text.contains(secret.as_str()))
    }
}

pub fn shown_variable(variable: &str) -> String {
    format!("{VARIABLE_OPEN}{variable}{VARIABLE_CLOSE}")
}

/// `text` with every `secret` in it replaced by `shown`, in one pass.
fn replaced<'a>(text: Cow<'a, str>, secret: &str, shown: &str) -> Cow<'a, str> {
    let Some(first) = text.find(secret) else {
        return text;
    };
    let mut replaced = String::with_capacity(text.len());
    let mut copied = 0;
    for (found, _) in text[first..].match_indices(secret) {
        let at = first + found;
        replaced.push_str(&text[copied..at]);
        replaced.push_str(shown);
        copied = at + secret.len();
    }
    replaced.push_str(&text[copied..]);
    Cow::Owned(replaced)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SECRET_URL: &str = "https://hooks.example.com/services/hook-4711";
    const HOOK_ORIGIN: &str = "https://hooks.example.com";
    const TOKEN: &str = "hook-4711";
    const TOKEN_VARIABLE: &str = "HOOK_TOKEN";
    const PLAIN: &str = "nothing to hide";

    #[test]
    fn a_secret_that_holds_another_is_hidden_whole() {
        let mut redactor = Redactor::default();
        redactor.hide_variable(TOKEN_VARIABLE, TOKEN);
        redactor.hide(SECRET_URL, HOOK_ORIGIN);
        let text = format!("{SECRET_URL} sent {TOKEN}, then {TOKEN}");

        let shown = redactor.text(&text);

        let token = shown_variable(TOKEN_VARIABLE);
        assert_eq!(shown, format!("{HOOK_ORIGIN} sent {token}, then {token}"));
    }

    #[test]
    fn text_without_a_secret_is_borrowed() {
        let mut redactor = Redactor::default();
        redactor.hide_variable(TOKEN_VARIABLE, TOKEN);

        assert!(matches!(redactor.text(PLAIN), Cow::Borrowed(PLAIN)));
    }

    #[test]
    fn json_strings_and_keys_are_redacted() {
        let mut redactor = Redactor::default();
        redactor.hide_variable(TOKEN_VARIABLE, TOKEN);
        let mut value = json!({ "echo": TOKEN, TOKEN: [TOKEN] });

        redactor.value(&mut value);

        let token = shown_variable(TOKEN_VARIABLE);
        assert_eq!(value, json!({ "echo": token, token.clone(): [token] }));
    }
}
