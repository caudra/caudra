use shell_words::split;
use std::io::{self, Write, stdout};
use std::num::NonZeroU16;
use std::path::Path;
use std::time::Instant;

use caudra_terminal_host::alert::visible_text;
use caudra_terminal_host::mux::TerminalMux;
use color_eyre::Result;
use crossterm::Command;
use crossterm::ExecutableCommand;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableColorSchemeReporting, DisableMouseCapture, EnableBracketedPaste,
    EnableColorSchemeReporting, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, RequestColorScheme,
};
#[cfg(not(windows))]
use crossterm::event::{DisableFocusChange, EnableFocusChange};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::buffer::{Cell, CellDiffOption};

/// XTPUSHTITLE saves whatever title the shell left on the window, so the
/// matching XTPOPTITLE on exit or editor handoff hands it back and no plugin
/// title outlives the session. Terminals without a title stack ignore both.
const PUSH_WINDOW_TITLE_SEQUENCE: &str = "\u{1b}[22;2t";
const POP_WINDOW_TITLE_SEQUENCE: &str = "\u{1b}[23;2t";
const OSC8_OPEN: &str = "\u{1b}]8;;";
const OSC8_CLOSE: &str = "\u{1b}]8;;\u{1b}\\";
const STRING_TERMINATOR: &str = "\u{1b}\\";
const KEYBOARD_ENHANCEMENTS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
        .union(KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT);
/// Termux exports this from v0.107, but not on every install or upgrade path,
/// so it confirms Termux and never rules it out.
const TERMUX_VERSION: &str = "TERMUX_VERSION";
/// Every Termux shell gets `PREFIX=/data/data/com.termux/files/usr`, which has
/// been true far longer than `TERMUX_VERSION` has existed. The package name is
/// what makes it Termux rather than any other build environment that likes the
/// name `PREFIX`.
const PREFIX: &str = "PREFIX";
const TERMUX_PACKAGE: &str = "/com.termux/";

pub(crate) struct TerminalGuard;

/// Whether the pointer is a finger. Termux is the only terminal that both runs
/// Caudra and reports touch, and it identifies itself in the environment, so
/// there is nothing to query the terminal about.
pub(crate) fn detect_touch() -> bool {
    is_termux(
        std::env::var_os(TERMUX_VERSION).is_some(),
        std::env::var(PREFIX).ok().as_deref(),
    )
}

fn is_termux(version: bool, prefix: Option<&str>) -> bool {
    version || prefix.is_some_and(|prefix| prefix.contains(TERMUX_PACKAGE))
}

/// OSC 2 sets the window title only; OSC 0 would stomp the icon name too.
fn window_title_sequence(mux: TerminalMux, title: &str) -> String {
    mux.wrap(format!("\u{1b}]2;{}\u{7}", visible_text(title)))
}

pub(crate) fn set_window_title(title: &str) -> std::io::Result<()> {
    write_sequence(&window_title_sequence(TerminalMux::detect(), title))
}

fn write_mux_sequence(sequence: &str) {
    let _ = write_sequence(&TerminalMux::detect().wrap(sequence.into()));
}

fn write_sequence(sequence: &str) -> std::io::Result<()> {
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
}

pub(crate) fn encode_hyperlink_cell(
    cell: &mut Cell,
    expected_symbol: &str,
    width: u16,
    target: &str,
) -> bool {
    let Some(width) = NonZeroU16::new(width) else {
        return false;
    };
    if cell.symbol() != expected_symbol || target.chars().any(char::is_control) {
        return false;
    }
    let encoded = format!("{OSC8_OPEN}{target}{STRING_TERMINATOR}{expected_symbol}{OSC8_CLOSE}");
    cell.set_symbol(&encoded)
        .set_diff_option(CellDiffOption::ForcedWidth(width));
    true
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) const fn local_url_opener_available() -> bool {
    true
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn local_url_opener_available() -> bool {
    [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "WSL_INTEROP",
        "WSL_DISTRO_NAME",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

impl TerminalGuard {
    pub(crate) fn init() -> Result<(Self, ratatui::DefaultTerminal)> {
        let terminal = ratatui::init();
        write_mux_sequence(PUSH_WINDOW_TITLE_SEQUENCE);
        stdout().execute(EnableBracketedPaste)?;
        stdout().execute(EnableMouseCapture)?;
        enable_focus_change();
        push_keyboard_enhancement();
        Ok((Self, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let started = Instant::now();
        pop_terminal_modes();
        ratatui::restore();
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "terminal restored"
        );
    }
}

pub(crate) fn with_normal_terminal<T>(
    terminal: &mut ratatui::DefaultTerminal,
    operation: impl FnOnce() -> T,
) -> T {
    struct ResumeGuard<'a>(&'a mut ratatui::DefaultTerminal);

    impl Drop for ResumeGuard<'_> {
        fn drop(&mut self) {
            resume(self.0);
        }
    }

    teardown();
    let guard = ResumeGuard(terminal);
    let result = operation();
    drop(guard);
    result
}

fn teardown() {
    pop_terminal_modes();
    terminal::disable_raw_mode().ok();
    stdout().execute(LeaveAlternateScreen).ok();
    stdout().flush().ok();
}

fn pop_terminal_modes() {
    set_appearance_reporting(false);
    write_mux_sequence(POP_WINDOW_TITLE_SEQUENCE);
    stdout().execute(crossterm::cursor::Show).ok();
    stdout().execute(PopKeyboardEnhancementFlags).ok();
    disable_focus_change();
    stdout().execute(DisableMouseCapture).ok();
    stdout().execute(DisableBracketedPaste).ok();
}

fn resume(terminal: &mut ratatui::DefaultTerminal) {
    write_mux_sequence(PUSH_WINDOW_TITLE_SEQUENCE);
    stdout().execute(EnterAlternateScreen).ok();
    stdout().execute(EnableBracketedPaste).ok();
    stdout().execute(EnableMouseCapture).ok();
    enable_focus_change();
    terminal::enable_raw_mode().ok();
    push_keyboard_enhancement();
    set_appearance_reporting(true);
    let _ = terminal.clear();
}

pub(crate) fn set_appearance_reporting(enabled: bool) {
    if let Err(error) = write_appearance_reporting(&mut stdout().lock(), enabled) {
        tracing::warn!(%error, enabled, "failed to configure terminal appearance reporting");
    } else {
        tracing::debug!(enabled, "terminal appearance reporting configured");
    }
}

fn write_appearance_reporting(output: &mut impl Write, enabled: bool) -> io::Result<()> {
    if enabled {
        crossterm::execute!(output, EnableColorSchemeReporting, RequestColorScheme)
    } else {
        crossterm::execute!(output, DisableColorSchemeReporting)
    }
}

#[cfg(not(windows))]
fn enable_focus_change() {
    stdout().execute(EnableFocusChange).ok();
}

#[cfg(windows)]
fn enable_focus_change() {}

#[cfg(not(windows))]
fn disable_focus_change() {
    stdout().execute(DisableFocusChange).ok();
}

#[cfg(windows)]
fn disable_focus_change() {}

fn push_keyboard_enhancement() {
    if let Err(e) = stdout().execute(PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENTS)) {
        tracing::warn!(error = %e, "failed to enable keyboard enhancement (Kitty protocol)");
    }
}

pub(crate) fn open_in_editor(
    path: &Path,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<i32, String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| "Set $VISUAL or $EDITOR to open files".to_string())?;

    let args = split(&editor).map_err(|e| format!("Failed to parse $VISUAL or $EDITOR: {e}"))?;

    if args.is_empty() {
        return Err("Empty $VISUAL or $EDITOR".to_string());
    }

    teardown();

    let result = std::process::Command::new(&args[0])
        .args(&args[1..])
        .arg(path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    resume(terminal);

    match result {
        Ok(status) => Ok(status.code().unwrap_or(-1)),
        Err(e) => Err(format!(
            "Failed to open {editor}: {e} - set $VISUAL or $EDITOR"
        )),
    }
}

pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut sequence = String::new();
    CopyToClipboard::to_clipboard_from(text)
        .write_ansi(&mut sequence)
        .map_err(|e| e.to_string())?;
    let sequence = TerminalMux::detect().wrap(sequence);
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_terminal_host::mux::parse_dcs_passthrough;
    use ratatui::backend::{Backend, CrosstermBackend};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use test_case::test_case;

    const WRONG_TERMUX: &str = "the environment was read as the wrong terminal";
    const TERMUX_PREFIX: &str = "/data/data/com.termux/files/usr";
    const EXPECTED_KEYBOARD_ENHANCEMENTS: &str = "\u{1b}[>27u";
    const APPEARANCE_SUBSCRIBE: &[u8] = b"\x1b[?2031h\x1b[?996n";
    const APPEARANCE_UNSUBSCRIBE: &[u8] = b"\x1b[?2031l";

    #[test_case(true, APPEARANCE_SUBSCRIBE; "subscribe_then_query")]
    #[test_case(false, APPEARANCE_UNSUBSCRIBE; "unsubscribe")]
    fn appearance_reporting_sequences(enabled: bool, expected: &[u8]) {
        let mut output = Vec::new();
        write_appearance_reporting(&mut output, enabled).unwrap();
        assert_eq!(output, expected);
    }

    #[test_case(false, Some(TERMUX_PREFIX), true ; "the prefix alone identifies an older termux")]
    #[test_case(true, None, true ; "the version alone identifies one that exports it")]
    #[test_case(false, Some("/usr/local"), false ; "a desktop prefix is not termux")]
    #[test_case(false, Some("/opt/com.termux.example"), false ; "a lookalike path is not termux")]
    #[test_case(false, None, false ; "an environment with neither is not termux")]
    fn termux_is_recognised_without_its_version(
        version: bool,
        prefix: Option<&str>,
        expected: bool,
    ) {
        assert_eq!(is_termux(version, prefix), expected, "{WRONG_TERMUX}");
    }

    #[test]
    fn keyboard_enhancements_report_event_types_without_forcing_all_keys() {
        let mut sequence = String::new();
        PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENTS)
            .write_ansi(&mut sequence)
            .unwrap();
        assert_eq!(sequence, EXPECTED_KEYBOARD_ENHANCEMENTS);
    }

    #[test]
    fn hyperlink_cell_has_a_self_contained_osc8_sequence_and_forced_width() {
        const TARGET: &str = "https://example.com/docs";
        let mut cell = Cell::default();
        cell.set_symbol("界");

        assert!(encode_hyperlink_cell(&mut cell, "界", 2, TARGET));
        assert_eq!(
            cell.symbol(),
            format!("{OSC8_OPEN}{TARGET}{STRING_TERMINATOR}界{OSC8_CLOSE}")
        );
        assert!(matches!(
            cell.diff_option,
            CellDiffOption::ForcedWidth(width) if width.get() == 2
        ));
    }

    #[test]
    fn hyperlink_cell_rejects_stale_symbols_and_control_characters() {
        let mut cell = Cell::default();
        cell.set_symbol("x");

        assert!(!encode_hyperlink_cell(
            &mut cell,
            "y",
            1,
            "https://example.com"
        ));
        assert!(!encode_hyperlink_cell(
            &mut cell,
            "x",
            1,
            "https://example.com/\u{1b}escape"
        ));
        assert_eq!(cell.symbol(), "x");
        assert_eq!(cell.diff_option, CellDiffOption::None);
    }

    #[test]
    fn crossterm_backend_writes_hyperlink_sequences_verbatim() {
        const TARGET: &str = "https://example.com";
        let mut cell = Cell::default();
        cell.set_symbol("x");
        assert!(encode_hyperlink_cell(&mut cell, "x", 1, TARGET));
        let encoded = cell.symbol().as_bytes().to_vec();
        let mut output = Vec::new();
        CrosstermBackend::new(&mut output)
            .draw(std::iter::once((0, 0, &cell)))
            .unwrap();

        assert!(
            output
                .windows(encoded.len())
                .any(|window| window == encoded)
        );
    }

    #[test]
    fn hyperlink_identity_participates_in_buffer_diffing() {
        let mut plain = Buffer::empty(Rect::new(0, 0, 1, 1));
        plain.cell_mut((0, 0)).unwrap().set_symbol("x");
        let mut first = plain.clone();
        assert!(encode_hyperlink_cell(
            first.cell_mut((0, 0)).unwrap(),
            "x",
            1,
            "https://example.com/first"
        ));
        let mut second = plain.clone();
        assert!(encode_hyperlink_cell(
            second.cell_mut((0, 0)).unwrap(),
            "x",
            1,
            "https://example.com/second"
        ));

        assert_eq!(plain.diff_iter(&first).count(), 1);
        assert_eq!(first.diff_iter(&first).count(), 0);
        assert_eq!(first.diff_iter(&second).count(), 1);
        assert_eq!(first.diff_iter(&plain).count(), 1);

        let mut previous_wide = Buffer::empty(Rect::new(0, 0, 2, 1));
        previous_wide.cell_mut((0, 0)).unwrap().set_symbol("界");
        assert!(encode_hyperlink_cell(
            previous_wide.cell_mut((0, 0)).unwrap(),
            "界",
            2,
            "https://example.com"
        ));
        let mut next_narrow = Buffer::empty(Rect::new(0, 0, 2, 1));
        next_narrow.cell_mut((0, 0)).unwrap().set_symbol("x");
        assert!(encode_hyperlink_cell(
            next_narrow.cell_mut((0, 0)).unwrap(),
            "x",
            1,
            "https://example.com"
        ));
        next_narrow
            .cell_mut((1, 0))
            .unwrap()
            .set_diff_option(CellDiffOption::AlwaysUpdate);

        let positions = previous_wide
            .diff_iter(&next_narrow)
            .map(|(x, y, _)| (x, y))
            .collect::<Vec<_>>();
        assert_eq!(positions, [(0, 0), (1, 0)]);
    }

    #[test]
    fn window_title_sequence_encodes_sanitized_title() {
        const TITLE: &str = "◐ building";
        const OSC2_SEQUENCE: &str = "\u{1b}]2;◐ building\u{7}";

        assert_eq!(
            window_title_sequence(TerminalMux::None, TITLE),
            OSC2_SEQUENCE
        );
        // Control bytes are gone, so what remains cannot terminate the
        // payload or start a new sequence.
        assert_eq!(
            window_title_sequence(TerminalMux::None, "\u{1b}\u{7}pwned\u{85}\ntitle"),
            "\u{1b}]2;pwned title\u{7}"
        );
        assert_eq!(
            window_title_sequence(TerminalMux::None, ""),
            "\u{1b}]2;\u{7}"
        );
    }

    #[test]
    fn window_title_roundtrips_mux_passthrough() {
        const TITLE: &str = "caudra";
        const OSC2_SEQUENCE: &str = "\u{1b}]2;caudra\u{7}";

        let tmux = window_title_sequence(TerminalMux::Tmux, TITLE);
        let screen = window_title_sequence(TerminalMux::Screen, TITLE);
        assert_eq!(parse_dcs_passthrough(&tmux, "\u{1b}Ptmux;"), OSC2_SEQUENCE);
        assert_eq!(parse_dcs_passthrough(&screen, "\u{1b}P"), OSC2_SEQUENCE);
    }

    #[test]
    fn tmux_wrap_roundtrips_crossterm_osc52_output() {
        let mut sequence = String::new();
        CopyToClipboard::to_clipboard_from("hello, world!")
            .write_ansi(&mut sequence)
            .expect("crossterm write_ansi");
        let wrapped = TerminalMux::Tmux.wrap(sequence.clone());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), sequence);
    }
}
