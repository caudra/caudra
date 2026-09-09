//! The log file is the one artefact a user pastes into a bug report, so the
//! privacy defaults have to hold there as well as at the collector: with
//! nothing opted in, prompt text and tool input must never be written. Owns the
//! process-wide subscriber, so it lives in its own test binary.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudra_otel::emit::{self, ToolResult};
use caudra_storage::log::record::{Entry, Level};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Multi-byte on purpose: a length written in bytes would leak the shape of the
/// prompt even when the text itself does not escape.
const SECRET_PROMPT: &str = "the 秘密 prompt";
const SECRET_INPUT: &str = "cat /etc/shadow";
const TOOL_NAME: &str = "shell";
const TOOL_SOURCE: &str = "builtin";
const PROMPT_LENGTH: &str = "prompt_length";
const TOOL_INPUT: &str = "tool_input";
const PROMPT: &str = "prompt";
const NO_RECORDS: &str = "the file layer wrote nothing";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture").clone()).expect("utf-8 log")
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn prompt_text_and_tool_input_never_reach_the_log_file() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(move || writer.clone())
                .with_filter(tracing_subscriber::EnvFilter::new(Level::Trace.as_str())),
        )
        .init();

    emit::user_prompt(SECRET_PROMPT);
    emit::tool_result(&ToolResult {
        tool_name: TOOL_NAME,
        tool_source: TOOL_SOURCE,
        success: true,
        duration: Duration::from_millis(5),
        error_type: None,
        tool_input: Some(SECRET_INPUT),
    });

    let text = capture.text();
    assert!(!text.contains("秘密"), "prompt text leaked: {text}");
    assert!(!text.contains(SECRET_INPUT), "tool input leaked: {text}");

    // Every line the writer produces has to survive the reader the modal and
    // `caudra logs` share, or the log is unreadable in the places that matter.
    let records: Vec<Entry> = text.lines().map(Entry::parse).collect();
    assert!(!records.is_empty(), "{NO_RECORDS}");
    for entry in &records {
        let Entry::Record(record) = entry else {
            panic!("the writer emitted a line the reader cannot parse: {text}");
        };
        assert!(!record.fields.iter().any(|(key, _)| key == PROMPT));
        assert!(!record.fields.iter().any(|(key, _)| key == TOOL_INPUT));
    }

    let fields: Vec<(String, String)> = records
        .iter()
        .filter_map(|entry| match entry {
            Entry::Record(record) => Some(record.fields.clone()),
            Entry::Raw(_) => None,
        })
        .flatten()
        .collect();
    assert!(
        fields.iter().any(|(key, value)| key == PROMPT_LENGTH
            && value == &SECRET_PROMPT.chars().count().to_string()),
        "the prompt length is what makes the redaction useful: {text}"
    );
}
