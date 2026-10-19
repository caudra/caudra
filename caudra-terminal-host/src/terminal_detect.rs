//! Terminal notification support and graphics transport policy.

use std::env;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::mux::TerminalMux;

/// Raw mode is already on when the tmux query runs, so a wedged tmux server
/// must not be able to hang startup with Ctrl-C disabled.
const TMUX_QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const TMUX: &str = "tmux";
const ALLOW_PASSTHROUGH: &str = "allow-passthrough";

#[derive(Default)]
struct TerminalEnvironment<'a> {
    term_program: Option<&'a str>,
    wezterm: bool,
    iterm: bool,
    kitty: bool,
    term: Option<&'a str>,
}

struct TmuxClient {
    term_type: String,
    term_name: String,
}

/// Whether the terminal, or the one tmux is attached to, shows OSC 9.
pub fn detect_osc9_support() -> bool {
    let term_program = env::var("TERM_PROGRAM").ok();
    let term = env::var("TERM").ok();
    let environment = TerminalEnvironment {
        term_program: term_program.as_deref(),
        wezterm: env::var_os("WEZTERM_VERSION").is_some(),
        iterm: env::var_os("ITERM_SESSION_ID").is_some()
            || env::var_os("ITERM_PROFILE").is_some()
            || env::var_os("ITERM_PROFILE_NAME").is_some(),
        kitty: env::var_os("KITTY_WINDOW_ID").is_some(),
        term: term.as_deref(),
    };
    let tmux = environment
        .term_program
        .filter(|value| normalize_terminal_id(value) == TMUX)
        .and_then(|_| query_tmux_client());
    supports_osc9_in(&environment, tmux.as_ref())
}

fn normalize_terminal_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .collect::<String>()
        .to_ascii_lowercase()
}

fn supports_osc9(value: &str) -> bool {
    matches!(
        normalize_terminal_id(value).as_str(),
        "ghostty"
            | "iterm"
            | "iterm2"
            | "itermapp"
            | "kitty"
            | "warp"
            | "warpterminal"
            | "wezterm"
            | "xtermghostty"
            | "xtermkitty"
    )
}

fn supports_osc9_in(environment: &TerminalEnvironment<'_>, tmux: Option<&TmuxClient>) -> bool {
    if let Some(term_program) = environment
        .term_program
        .filter(|value| !value.trim().is_empty())
    {
        if normalize_terminal_id(term_program) == TMUX {
            return tmux.is_some_and(|client| {
                client
                    .term_type
                    .split_whitespace()
                    .next()
                    .is_some_and(supports_osc9)
                    || supports_osc9(&client.term_name)
            });
        }
        return supports_osc9(term_program);
    }
    environment.wezterm
        || environment.iterm
        || environment.kitty
        || environment.term.is_some_and(supports_osc9)
}

fn query_tmux_client() -> Option<TmuxClient> {
    let output = query_tmux(&[
        "display-message",
        "-p",
        "#{client_termtype}\t#{client_termname}",
    ])?;
    let (term_type, term_name) = output.trim_end().split_once('\t')?;
    Some(TmuxClient {
        term_type: term_type.into(),
        term_name: term_name.into(),
    })
}

/// Whether Kitty graphics written through tmux reach a terminal that draws
/// them: passthrough has to be on, and the client tmux is attached to has to
/// be one of the terminals that implement Unicode placeholders.
pub fn tmux_passes_kitty_graphics() -> bool {
    query_tmux(&["show-options", "-gv", ALLOW_PASSTHROUGH]).is_some_and(|value| {
        passthrough_enabled(&value)
            && query_tmux_client().is_some_and(|client| draws_kitty_placeholders(&client))
    })
}

/// Whether Caudra runs inside tmux, which answers terminal queries itself.
pub fn inside_tmux() -> bool {
    TerminalMux::detect() == TerminalMux::Tmux
}

/// Whether Caudra runs in a Herdr pane, which composes its own grid.
pub fn inside_herdr() -> bool {
    crate::detect().is_some_and(|host| host.name() == crate::herdr::HOST)
}

fn passthrough_enabled(value: &str) -> bool {
    matches!(value.trim(), "on" | "all")
}

fn draws_kitty_placeholders(client: &TmuxClient) -> bool {
    [
        client
            .term_type
            .split_whitespace()
            .next()
            .unwrap_or_default(),
        client.term_name.as_str(),
    ]
    .into_iter()
    .any(|value| {
        matches!(
            normalize_terminal_id(value).as_str(),
            "kitty" | "xtermkitty" | "ghostty" | "xtermghostty"
        )
    })
}

fn query_tmux(args: &[&str]) -> Option<String> {
    let mut child = Command::new(TMUX)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if !finished_in_time(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    String::from_utf8(child.wait_with_output().ok()?.stdout).ok()
}

fn finished_in_time(child: &mut Child) -> bool {
    matches!(child.wait_timeout(TMUX_QUERY_TIMEOUT), Ok(Some(status)) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn environment(term_program: Option<&str>) -> TerminalEnvironment<'_> {
        TerminalEnvironment {
            term_program,
            ..TerminalEnvironment::default()
        }
    }

    #[test]
    fn known_osc9_terminals_are_recognised() {
        for value in ["Ghostty", "iTerm.app", "kitty", "WarpTerminal", "WezTerm"] {
            assert!(
                supports_osc9_in(&environment(Some(value)), None),
                "terminal: {value}"
            );
        }
    }

    #[test]
    fn orca_is_not_an_osc9_terminal() {
        assert!(!supports_osc9_in(&environment(Some("Orca")), None));
    }

    #[test]
    fn term_program_precedes_terminal_specific_variables() {
        let terminal = TerminalEnvironment {
            term_program: Some("Alacritty"),
            wezterm: true,
            ..TerminalEnvironment::default()
        };
        assert!(!supports_osc9_in(&terminal, None));
    }

    #[test]
    fn specific_variables_and_term_fallbacks_are_recognised() {
        for terminal in [
            TerminalEnvironment {
                wezterm: true,
                ..TerminalEnvironment::default()
            },
            TerminalEnvironment {
                iterm: true,
                ..TerminalEnvironment::default()
            },
            TerminalEnvironment {
                kitty: true,
                ..TerminalEnvironment::default()
            },
            TerminalEnvironment {
                term: Some("xterm-kitty"),
                ..TerminalEnvironment::default()
            },
            TerminalEnvironment {
                term: Some("xterm-ghostty"),
                ..TerminalEnvironment::default()
            },
        ] {
            assert!(supports_osc9_in(&terminal, None));
        }
        let terminal = TerminalEnvironment {
            term: Some("xterm-256color"),
            ..TerminalEnvironment::default()
        };
        assert!(!supports_osc9_in(&terminal, None));
    }

    #[test_case("on\n", true ; "on")]
    #[test_case("all", true ; "all")]
    #[test_case("off\n", false ; "off")]
    #[test_case("", false ; "unset")]
    fn tmux_passthrough_values(value: &str, enabled: bool) {
        assert_eq!(passthrough_enabled(value), enabled);
    }

    #[test_case("ghostty 1.2.3", "xterm-256color", true ; "ghostty_type")]
    #[test_case("xterm-256color", "xterm-kitty", true ; "kitty_name")]
    #[test_case("WezTerm 2024", "xterm-256color", false ; "no_placeholders")]
    fn tmux_clients_that_draw_kitty_placeholders(term_type: &str, term_name: &str, draws: bool) {
        let client = TmuxClient {
            term_type: term_type.into(),
            term_name: term_name.into(),
        };
        assert_eq!(draws_kitty_placeholders(&client), draws);
    }

    #[test]
    fn tmux_is_judged_by_its_client_terminal() {
        let terminal = environment(Some("tmux"));
        let ghostty = TmuxClient {
            term_type: "ghostty 1.2.3".into(),
            term_name: "xterm-256color".into(),
        };
        assert!(supports_osc9_in(&terminal, Some(&ghostty)));
        let ghostty_name = TmuxClient {
            term_type: "xterm-256color".into(),
            term_name: "xterm-ghostty".into(),
        };
        assert!(supports_osc9_in(&terminal, Some(&ghostty_name)));
        assert!(!supports_osc9_in(&terminal, None));
    }
}
