//! The Wire view: the body the provider adapter would send for the request the
//! Projection view shows, laid out by the pretty-printer. Rows are never
//! wrapped, so a long one pans, and a long base64 payload is shown by its size
//! rather than its digits.

use ratatui::text::{Line, Span};
use serde::{Serialize, Serializer};
use serde_json::Value;
use url::Url;

use super::projection::{BASE64_PAD, UNPREPARED, displayable, running_notice};
use super::{Projection, TITLE_END};
use crate::components::document_view::Painted;
use crate::components::modal::SEPARATOR;
use crate::components::{escape_terminal_controls, format_bytes, json_text};
use crate::theme::Theme;

pub(super) const WIRE_TITLE_PREFIX: &str = " Wire - ";
pub(super) const UNBUILT_TITLE: &str = " Wire ";
pub(super) const UNBUILT: &str = "No wire body to show.";
/// A payload up to this many bytes stays in view: a signature or an id reads
/// fine whole, and is sometimes what a reader came to check.
const ELIDE_ABOVE: usize = 256;
const DATA_URL_SCHEME: &str = "data:";
const DATA_URL_BASE64: &str = ";base64,";
/// The digits past the alphanumerics, in the standard alphabet and in the
/// URL-safe one.
const BASE64_SYMBOLS: &[u8] = b"+/-_";
pub(super) const ELIDED_OPEN: &str = "<base64";
const ELIDED_CLOSE: &str = ">";

/// What the provider answered when asked for the body it would send.
pub(super) struct Wire {
    pub(super) title: String,
    /// The body, or why there is none.
    body: Result<Value, String>,
    /// Calls the body leaves open, announced under it.
    running_calls: usize,
}

impl Wire {
    /// Asks the provider for the body it would send for exactly the messages
    /// the Projection view shows. A dry run: nothing is sent, and whatever
    /// only a send could find out is reported instead.
    pub(super) fn build(projection: Option<&Projection>) -> Self {
        let Some(projection) = projection else {
            return Self::unbuilt(UNPREPARED.to_owned());
        };
        let prompt = &projection.prompt;
        let request = prompt.provider.wire_request(
            &prompt.model,
            &projection.messages,
            &prompt.system,
            &prompt.tools,
            &projection.opts.clamped(&prompt.model),
            Some(&projection.cache_key),
        );
        match request {
            Ok(request) => Self {
                title: format!(
                    "{WIRE_TITLE_PREFIX}{} {}{TITLE_END}",
                    request.method,
                    shown_url(&request.url)
                ),
                body: Ok(request.body),
                running_calls: projection.running_calls,
            },
            Err(error) => Self::unbuilt(error.user_message()),
        }
    }

    fn unbuilt(reason: String) -> Self {
        Self {
            title: UNBUILT_TITLE.to_owned(),
            body: Err(reason),
            running_calls: 0,
        }
    }

    /// What `y` hands over with nothing swept: the body whole, with nothing
    /// elided, or nothing where there is no body.
    pub(super) fn source(&self) -> String {
        self.body
            .as_ref()
            .ok()
            .and_then(|body| pretty(body).ok())
            .unwrap_or_default()
    }

    /// The body pretty-printed and coloured, a row per line. The view has no
    /// sections, so it anchors none and `n` and `p` have nowhere to go.
    pub(super) fn paint(&self, theme: &Theme) -> Painted {
        let shown = self
            .body
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|body| pretty(&Elided(body)));
        let mut lines: Vec<Line<'static>> = match shown {
            Ok(json) => json
                .split('\n')
                .map(|line| displayable(json_text::themed_line(line)))
                .collect(),
            Err(reason) => vec![
                Line::from(Span::styled(UNBUILT, theme.status_dim)),
                Line::from(Span::styled(
                    escape_terminal_controls(&reason),
                    theme.tool_dim,
                )),
            ],
        };
        if self.running_calls > 0 {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                running_notice(self.running_calls),
                theme.status_dim,
            )));
        }
        Painted::new(lines, Vec::new(), Vec::new())
    }
}

/// The request's URL with any `user:pass@` a configured base URL carried left
/// out, since the title is on screen for anyone to read. One that does not
/// parse is shown as it came.
fn shown_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return escape_terminal_controls(raw);
    };
    // Both fail only for a URL that cannot carry credentials to begin with.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    escape_terminal_controls(url.as_str())
}

/// A JSON value cannot fail to serialize, but were it to, the view says why
/// rather than showing nothing.
fn pretty(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

/// The body as the view shows it, every long base64 payload standing in for
/// itself by its size. Only the display is shortened; a copy takes the body.
struct Elided<'a>(&'a Value);

impl Serialize for Elided<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::String(text) => {
                serializer.serialize_str(elided(text).as_deref().unwrap_or(text))
            }
            Value::Array(items) => serializer.collect_seq(items.iter().map(Elided)),
            Value::Object(map) => {
                serializer.collect_map(map.iter().map(|(key, value)| (key, Elided(value))))
            }
            scalar => scalar.serialize(serializer),
        }
    }
}

/// How a long base64 string reads in the view: its size in place of its
/// digits, behind the header when it is a data URL's payload. `None` for any
/// other string, which is shown as it is.
fn elided(text: &str) -> Option<String> {
    let header = data_url_header(text).unwrap_or_default();
    let payload = &text[header.len()..];
    (payload.len() > ELIDE_ABOVE && is_base64(payload)).then(|| {
        let size = format_bytes(payload.len() as u64);
        format!("{header}{ELIDED_OPEN}{SEPARATOR}{size}{ELIDED_CLOSE}")
    })
}

/// The `data:<type>;base64,` a data URL carrying base64 opens on.
fn data_url_header(text: &str) -> Option<&str> {
    let marker = text.strip_prefix(DATA_URL_SCHEME)?.find(DATA_URL_BASE64)?;
    Some(&text[..DATA_URL_SCHEME.len() + marker + DATA_URL_BASE64.len()])
}

/// Digits of the standard or the URL-safe alphabet, padded or not.
fn is_base64(text: &str) -> bool {
    text.trim_end_matches(BASE64_PAD)
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || BASE64_SYMBOLS.contains(&byte))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::theme;

    const DIGIT: &str = "Q";
    const URL_SAFE_DIGITS: &str = "-_";
    const PADDING: &str = "==";
    const PROSE: &str = "not base64 ";
    const DATA_URL: &str = "data:image/png;base64,";
    const TEXT_KEY: &str = "text";
    const CONTROL_TEXT: &str = "red\u{9b}31m";
    const URL: &str = "https://api.example.test/v1/messages";
    const CREDENTIALED_URL: &str = "https://user:hunter2@api.example.test/v1/messages";
    const PASSWORD_ONLY_URL: &str = "https://:hunter2@api.example.test/v1/messages";
    /// Relative, so it cannot parse, and carrying a control character.
    const UNPARSABLE_URL: &str = "api.example.test/v1\u{1b}";
    const ESCAPED_URL: &str = "api.example.test/v1\\u{1b}";
    const NOT_ESCAPED: &str = "a control character must never reach the terminal";
    const COPY_ALTERED: &str = "a copy must hand over the body as it was built";
    const NOTICE_WRONG: &str = "no body must be a notice saying why, with nothing to copy";

    fn built(body: Value) -> Wire {
        Wire {
            title: String::new(),
            body: Ok(body),
            running_calls: 0,
        }
    }

    fn rows(wire: &Wire) -> Vec<String> {
        let painted = wire.paint(&theme::current());
        painted.lines().iter().map(ToString::to_string).collect()
    }

    /// What [`elided`] shows for a payload of `len` digits behind `header`.
    fn sized(header: &str, len: usize) -> String {
        let size = format_bytes(len as u64);
        format!("{header}{ELIDED_OPEN}{SEPARATOR}{size}{ELIDED_CLOSE}")
    }

    #[test_case(DIGIT.repeat(ELIDE_ABOVE) => None ; "a_payload_at_the_threshold_stays")]
    #[test_case(
        DIGIT.repeat(ELIDE_ABOVE + 1) => Some(sized("", ELIDE_ABOVE + 1)) ;
        "a_payload_past_it_is_sized"
    )]
    #[test_case(
        format!("{}{PADDING}", DIGIT.repeat(ELIDE_ABOVE)) => Some(sized("", ELIDE_ABOVE + PADDING.len())) ;
        "padding_counts"
    )]
    #[test_case(
        URL_SAFE_DIGITS.repeat(ELIDE_ABOVE) => Some(sized("", URL_SAFE_DIGITS.len() * ELIDE_ABOVE)) ;
        "the_url_safe_alphabet"
    )]
    #[test_case(
        format!("{DATA_URL}{}", DIGIT.repeat(ELIDE_ABOVE + 1)) => Some(sized(DATA_URL, ELIDE_ABOVE + 1)) ;
        "a_data_url_keeps_its_header"
    )]
    #[test_case(PROSE.repeat(ELIDE_ABOVE) => None ; "prose_stays")]
    fn a_long_base64_payload_is_shown_by_its_size(text: String) -> Option<String> {
        elided(&text)
    }

    #[test]
    fn controls_are_escaped_on_screen_and_kept_in_the_copy() {
        let wire = built(json!({ TEXT_KEY: CONTROL_TEXT }));

        assert!(
            !rows(&wire).iter().any(|row| row.contains(char::is_control)),
            "{NOT_ESCAPED}"
        );
        assert!(wire.source().contains(CONTROL_TEXT), "{COPY_ALTERED}");
    }

    #[test_case(CREDENTIALED_URL  => URL ; "credentials_are_left_out")]
    #[test_case(PASSWORD_ONLY_URL => URL ; "a_bare_password_is_left_out")]
    #[test_case(URL               => URL ; "a_plain_url_is_kept")]
    #[test_case(UNPARSABLE_URL    => ESCAPED_URL ; "an_unparsable_url_is_shown_escaped")]
    fn a_url_is_shown_without_its_credentials(raw: &str) -> String {
        shown_url(raw)
    }

    #[test]
    fn an_unprepared_request_has_no_body_to_show() {
        let wire = Wire::build(None);

        assert_eq!(wire.title, UNBUILT_TITLE, "{NOTICE_WRONG}");
        assert_eq!(rows(&wire), [UNBUILT, UNPREPARED], "{NOTICE_WRONG}");
        assert!(wire.source().is_empty(), "{NOTICE_WRONG}");
    }
}
