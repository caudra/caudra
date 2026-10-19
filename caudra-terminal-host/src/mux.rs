use std::env;
use std::ffi::OsString;

use crate::herdr::HerdrEnv;
use crate::orca::OrcaEnv;

const ZELLIJ: &str = "ZELLIJ";
const TMUX: &str = "TMUX";
const SCREEN: &str = "STY";
const HERDR_TERM_PROGRAM: &str = "herdr";
const TMUX_TERM_PROGRAM: &str = "tmux";
const TERM: &str = "TERM";

/// The terminal multiplexer between Caudra and the terminal, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMux {
    Zellij,
    Tmux,
    Screen,
    None,
}

impl TerminalMux {
    pub fn detect() -> Self {
        Self::detect_from(|name| env::var_os(name))
    }

    pub fn detect_from(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let program = get(crate::TERM_PROGRAM);
        // Screen preserves TERM_PROGRAM from its parent, unlike tmux.
        if get(SCREEN).is_some()
            && program.as_deref() != Some(TMUX_TERM_PROGRAM.as_ref())
            && get(TERM).is_some_and(|term| {
                term.to_str().is_some_and(|term| {
                    term == "screen" || term.starts_with("screen-") || term.starts_with("screen.")
                })
            })
        {
            return Self::Screen;
        }
        if (program.as_deref() == Some(crate::orca::TERM_PROGRAM_NAME.as_ref())
            && OrcaEnv::from_env(&get).is_some())
            || (program.as_deref() == Some(HERDR_TERM_PROGRAM.as_ref())
                && HerdrEnv::from_env(&get).is_some())
        {
            return Self::None;
        }
        if get(ZELLIJ).is_some() {
            Self::Zellij
        } else if get(TMUX).is_some() {
            Self::Tmux
        } else if get(SCREEN).is_some() {
            Self::Screen
        } else {
            Self::None
        }
    }

    /// GNU screen passes no focus events on, so focus is never known there.
    pub fn reports_focus(self) -> bool {
        self != Self::Screen
    }

    /// tmux and screen need DCS passthrough with every internal ESC doubled.
    /// Without doubling, the `ESC \` in an ST terminator would close the DCS
    /// wrapper early and truncate the payload.
    pub fn wrap(self, sequence: String) -> String {
        match self {
            Self::Zellij | Self::None => sequence,
            Self::Tmux => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}Ptmux;{escaped}\u{1b}\\")
            }
            Self::Screen => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}P{escaped}\u{1b}\\")
            }
        }
    }
}

/// Simulates DCS-passthrough parsing: `ESC ESC` becomes one ESC, `ESC \`
/// ends the DCS. Panics on bad input so tests fail loudly.
#[cfg(any(test, feature = "test-support"))]
pub fn parse_dcs_passthrough(wrapped: &str, prefix: &str) -> String {
    let body = wrapped
        .strip_prefix(prefix)
        .unwrap_or_else(|| panic!("missing DCS prefix {prefix:?} in {wrapped:?}"));
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    loop {
        match bytes.get(i) {
            None => panic!("DCS body missing ST terminator: {body:?}"),
            Some(&0x1B) => match bytes.get(i + 1) {
                Some(&0x1B) => {
                    out.push(0x1B);
                    i += 2;
                }
                Some(&b'\\') => {
                    assert_eq!(
                        i + 2,
                        bytes.len(),
                        "unexpected trailing bytes after DCS ST: {:?}",
                        &bytes[i + 2..]
                    );
                    return String::from_utf8(out).expect("utf-8 body");
                }
                Some(b) => panic!("unexpected byte 0x{b:02x} after ESC inside DCS"),
                None => panic!("lone trailing ESC in DCS body"),
            },
            Some(&b) => {
                out.push(b);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TMUX_PREFIX: &str = "\u{1b}Ptmux;";
    const SCREEN_PREFIX: &str = "\u{1b}P";
    /// An ST terminator puts ESC bytes at the start and in the middle.
    const OSC52_WITH_ST: &str = "\u{1b}]52;c;SGVsbG8=\u{1b}\\";

    fn detect(set: &[&str]) -> TerminalMux {
        TerminalMux::detect_from(|name| set.contains(&name).then(|| OsString::from("1")))
    }

    #[test_case(&[ZELLIJ, TMUX], TerminalMux::Zellij ; "zellij_first")]
    #[test_case(&[TMUX, SCREEN], TerminalMux::Tmux ; "tmux_before_screen")]
    #[test_case(&[SCREEN], TerminalMux::Screen ; "screen")]
    #[test_case(&[], TerminalMux::None ; "none")]
    fn the_innermost_known_multiplexer_is_detected(set: &[&str], expected: TerminalMux) {
        assert_eq!(detect(set), expected);
    }

    #[test_case("herdr", true, false, TerminalMux::None ; "herdr_inside_tmux")]
    #[test_case("Orca", true, true, TerminalMux::None ; "orca_inherits_herdr_and_tmux")]
    #[test_case("tmux", true, true, TerminalMux::Tmux ; "tmux_inside_hosts")]
    #[test_case("Orca", true, false, TerminalMux::Tmux ; "unconfirmed_orca_keeps_transport")]
    #[test_case("herdr", false, false, TerminalMux::Tmux ; "partial_herdr_keeps_transport")]
    fn confirmed_inner_hosts_do_not_wrap_for_inherited_muxes(
        program: &str,
        herdr: bool,
        orca: bool,
        expected: TerminalMux,
    ) {
        let mux = TerminalMux::detect_from(|name| {
            match name {
                crate::TERM_PROGRAM => Some(program),
                TMUX => Some("inherited"),
                "HERDR_ENV" if herdr => Some("1"),
                "HERDR_PANE_ID" if herdr => Some("w1:p1"),
                "HERDR_SOCKET_PATH" if herdr => Some("/tmp/herdr.sock"),
                "ORCA_PANE_KEY" if orca => Some("tab:leaf"),
                _ => None,
            }
            .map(OsString::from)
        });
        assert_eq!(mux, expected);
    }

    #[test_case("herdr", "screen", TerminalMux::Screen ; "screen_inside_herdr")]
    #[test_case("Orca", "screen-256color", TerminalMux::Screen ; "screen_inside_orca")]
    #[test_case("Orca", "screen.xterm-256color", TerminalMux::Screen ; "screen_extended_terminal_name")]
    #[test_case("herdr", "xterm-256color", TerminalMux::None ; "herdr_inside_screen")]
    #[test_case("Orca", "xterm-256color", TerminalMux::None ; "orca_inside_screen")]
    #[test_case("tmux", "screen-256color", TerminalMux::Tmux ; "tmux_inside_screen")]
    fn screen_nesting_uses_its_terminal_identity(program: &str, term: &str, expected: TerminalMux) {
        let mux = TerminalMux::detect_from(|name| {
            match name {
                crate::TERM_PROGRAM => Some(program),
                TERM => Some(term),
                SCREEN | TMUX => Some("inherited"),
                "HERDR_ENV" => Some("1"),
                "HERDR_PANE_ID" => Some("w1:p1"),
                "HERDR_SOCKET_PATH" => Some("/tmp/herdr.sock"),
                "ORCA_PANE_KEY" => Some("tab:leaf"),
                _ => None,
            }
            .map(OsString::from)
        });
        assert_eq!(mux, expected);
    }

    #[test_case(TerminalMux::None ; "none")]
    #[test_case(TerminalMux::Zellij ; "zellij_intercepts_osc_itself")]
    fn unwrapped_multiplexers_pass_sequences_unchanged(mux: TerminalMux) {
        assert_eq!(mux.wrap(OSC52_WITH_ST.into()), OSC52_WITH_ST);
    }

    #[test_case(TerminalMux::Tmux, TMUX_PREFIX ; "tmux")]
    #[test_case(TerminalMux::Screen, SCREEN_PREFIX ; "screen")]
    fn passthrough_roundtrips(mux: TerminalMux, prefix: &str) {
        assert_eq!(
            parse_dcs_passthrough(&mux.wrap(OSC52_WITH_ST.into()), prefix),
            OSC52_WITH_ST
        );
    }

    /// If any interior ESC were left undoubled, the first bare `ESC \` would
    /// close the DCS early and truncate everything after.
    #[test]
    fn tmux_preserves_multiple_interior_esc_bytes() {
        let payload = "\u{1b}A\u{1b}B\u{1b}C\u{1b}\\";
        assert_eq!(
            parse_dcs_passthrough(&TerminalMux::Tmux.wrap(payload.into()), TMUX_PREFIX),
            payload
        );
    }

    #[test]
    fn screen_is_the_only_mux_without_focus_reporting() {
        assert!(!TerminalMux::Screen.reports_focus());
        for mux in [TerminalMux::None, TerminalMux::Tmux, TerminalMux::Zellij] {
            assert!(mux.reports_focus());
        }
    }
}
