//! One-off alerts: OSC 9 notifications and the terminal bell.

use std::io::{self, Write, stdout};

use crate::mux::TerminalMux;

const FALLBACK_MESSAGE: &str = "Caudra needs attention";
const BELL_SEQUENCE: &str = "\u{7}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSink {
    Osc9,
    Bell,
}

/// Raises alerts through one sink, wrapped for the multiplexer in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notifier {
    sink: AlertSink,
    mux: TerminalMux,
}

impl Notifier {
    pub fn new(sink: AlertSink) -> Self {
        Self {
            sink,
            mux: TerminalMux::detect(),
        }
    }

    pub fn sink(self) -> AlertSink {
        self.sink
    }

    pub fn reports_focus(self) -> bool {
        self.mux.reports_focus()
    }

    pub fn notify(self, message: &str) -> io::Result<()> {
        write_to_terminal(&alert_sequence(self.sink, self.mux, message))
    }
}

/// The bell stays raw: every multiplexer passes it on and rings for it.
fn alert_sequence(sink: AlertSink, mux: TerminalMux, message: &str) -> String {
    match sink {
        AlertSink::Osc9 => mux.wrap(format!("\u{1b}]9;{}\u{7}", sanitize(message))),
        AlertSink::Bell => BELL_SEQUENCE.to_owned(),
    }
}

fn sanitize(message: &str) -> String {
    let sanitized = visible_text(message);
    if sanitized.is_empty() {
        FALLBACK_MESSAGE.into()
    } else {
        sanitized
    }
}

/// Control bytes would terminate an OSC payload early or inject new
/// sequences; whitespace runs collapse so spinner frames stay tidy.
pub fn visible_text(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn write_to_terminal(sequence: &str) -> io::Result<()> {
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::parse_dcs_passthrough;
    use test_case::test_case;

    const MESSAGE: &str = "Task complete";
    const OSC9_SEQUENCE: &str = "\u{1b}]9;Task complete\u{7}";

    #[test]
    fn osc9_encodes_the_message_and_the_bell_stays_raw() {
        assert_eq!(
            alert_sequence(AlertSink::Osc9, TerminalMux::None, MESSAGE),
            OSC9_SEQUENCE
        );
        assert_eq!(
            alert_sequence(AlertSink::Bell, TerminalMux::Tmux, MESSAGE),
            BELL_SEQUENCE
        );
    }

    #[test_case(TerminalMux::Tmux, "\u{1b}Ptmux;" ; "tmux")]
    #[test_case(TerminalMux::Screen, "\u{1b}P" ; "screen")]
    fn osc9_roundtrips_multiplexer_passthrough(mux: TerminalMux, prefix: &str) {
        assert_eq!(
            parse_dcs_passthrough(&alert_sequence(AlertSink::Osc9, mux, MESSAGE), prefix),
            OSC9_SEQUENCE
        );
    }

    #[test_case("\0Task\t\u{7f}complete\u{85}\nnow\u{2003}", "Task complete now" ; "strips_controls_and_collapses_whitespace")]
    #[test_case("\0\t\u{7f}\u{85}\n\u{2003}", FALLBACK_MESSAGE ; "all_control_input_falls_back")]
    fn messages_are_sanitized(message: &str, expected: &str) {
        assert_eq!(sanitize(message), expected);
    }
}
