use shell_words::split;
use std::io::{Write, stdout};
use std::num::NonZeroU16;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use caudra_config::NotificationMethod;
use color_eyre::Result;
use crossterm::Command;
use crossterm::ExecutableCommand;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
#[cfg(not(windows))]
use crossterm::event::{DisableFocusChange, EnableFocusChange};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::buffer::{Cell, CellDiffOption};

const FALLBACK_NOTIFICATION_MESSAGE: &str = "Caudra needs attention";
const BELL_SEQUENCE: &str = "\u{7}";
/// XTPUSHTITLE saves whatever title the shell left on the window, so the
/// matching XTPOPTITLE on exit or editor handoff hands it back and no plugin
/// title outlives the session. Terminals without a title stack ignore both.
const PUSH_WINDOW_TITLE_SEQUENCE: &str = "\u{1b}[22;2t";
const POP_WINDOW_TITLE_SEQUENCE: &str = "\u{1b}[23;2t";
const OSC8_OPEN: &str = "\u{1b}]8;;";
const OSC8_CLOSE: &str = "\u{1b}]8;;\u{1b}\\";
const STRING_TERMINATOR: &str = "\u{1b}\\";
/// Raw mode is already on when the tmux query runs, so a wedged tmux server
/// must not be able to hang startup with Ctrl-C disabled.
const TMUX_QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const KEYBOARD_ENHANCEMENTS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalMux {
    Zellij,
    Tmux,
    Screen,
    None,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedNotifier {
    Osc9,
    Bell,
}

pub(crate) struct TerminalNotifier {
    notifier: ResolvedNotifier,
    mux: TerminalMux,
}

impl TerminalNotifier {
    pub(crate) fn new(configured: NotificationMethod, herdr: bool) -> Option<Self> {
        let notifier = resolve_notifier(configured, herdr, detect_osc9_support)?;
        Some(Self {
            notifier,
            mux: TerminalMux::detect(),
        })
    }

    pub(crate) fn notifier(&self) -> ResolvedNotifier {
        self.notifier
    }

    pub(crate) fn supports_focus_reporting(&self) -> bool {
        self.mux != TerminalMux::Screen
    }

    pub(crate) fn notify(&self, message: &str) -> std::io::Result<()> {
        write_sequence(&notification_sequence(self.notifier, self.mux, message))
    }
}

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

fn detect_osc9_support() -> bool {
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let term = std::env::var("TERM").ok();
    let env = TerminalEnvironment {
        term_program: term_program.as_deref(),
        wezterm: std::env::var_os("WEZTERM_VERSION").is_some(),
        iterm: std::env::var_os("ITERM_SESSION_ID").is_some()
            || std::env::var_os("ITERM_PROFILE").is_some()
            || std::env::var_os("ITERM_PROFILE_NAME").is_some(),
        kitty: std::env::var_os("KITTY_WINDOW_ID").is_some(),
        term: term.as_deref(),
    };
    let tmux = env
        .term_program
        .filter(|value| normalize_terminal_id(value) == "tmux")
        .and_then(|_| query_tmux_client());
    auto_supports_osc9(&env, tmux.as_ref())
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

/// Herdr raises its own toast when an agent it tracks blocks or finishes, so
/// `auto` stays quiet in a Herdr pane rather than announcing everything twice.
fn resolve_notifier(
    configured: NotificationMethod,
    herdr: bool,
    auto_supports_osc9: impl FnOnce() -> bool,
) -> Option<ResolvedNotifier> {
    match configured {
        NotificationMethod::Off => None,
        NotificationMethod::Osc9 => Some(ResolvedNotifier::Osc9),
        NotificationMethod::Bell => Some(ResolvedNotifier::Bell),
        NotificationMethod::Auto if herdr => None,
        NotificationMethod::Auto => Some(if auto_supports_osc9() {
            ResolvedNotifier::Osc9
        } else {
            ResolvedNotifier::Bell
        }),
    }
}

fn auto_supports_osc9(env: &TerminalEnvironment<'_>, tmux: Option<&TmuxClient>) -> bool {
    if let Some(term_program) = env.term_program.filter(|value| !value.trim().is_empty()) {
        if normalize_terminal_id(term_program) == "tmux" {
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
    env.wezterm || env.iterm || env.kitty || env.term.is_some_and(supports_osc9)
}

fn query_tmux_client() -> Option<TmuxClient> {
    let mut child = ProcessCommand::new("tmux")
        .args([
            "display-message",
            "-p",
            "#{client_termtype}\t#{client_termname}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    match wait_timeout::ChildExt::wait_timeout(&mut child, TMUX_QUERY_TIMEOUT) {
        Ok(Some(status)) if status.success() => {}
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    let output = child.wait_with_output().ok()?;
    let output = String::from_utf8(output.stdout).ok()?;
    let (term_type, term_name) = output.trim_end().split_once('\t')?;
    Some(TmuxClient {
        term_type: term_type.into(),
        term_name: term_name.into(),
    })
}

fn sanitize_notification_message(message: &str) -> String {
    let sanitized = visible_text(message);
    if sanitized.is_empty() {
        FALLBACK_NOTIFICATION_MESSAGE.into()
    } else {
        sanitized
    }
}

/// Control bytes would terminate the OSC payload early or inject new
/// sequences; whitespace runs collapse so spinner frames stay tidy.
fn visible_text(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// OSC 2 sets the window title only; OSC 0 would stomp the icon name too.
fn window_title_sequence(mux: TerminalMux, title: &str) -> String {
    mux.wrap_for_mux(format!("\u{1b}]2;{}\u{7}", visible_text(title)))
}

pub(crate) fn set_window_title(title: &str) -> std::io::Result<()> {
    write_sequence(&window_title_sequence(TerminalMux::detect(), title))
}

fn write_mux_sequence(sequence: &str) {
    let _ = write_sequence(&TerminalMux::detect().wrap_for_mux(sequence.into()));
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

fn notification_sequence(notifier: ResolvedNotifier, mux: TerminalMux, message: &str) -> String {
    match notifier {
        ResolvedNotifier::Osc9 => {
            let message = sanitize_notification_message(message);
            mux.wrap_for_mux(format!("\u{1b}]9;{message}\u{7}"))
        }
        ResolvedNotifier::Bell => BELL_SEQUENCE.to_string(),
    }
}

impl TerminalMux {
    fn detect() -> Self {
        if std::env::var_os("ZELLIJ").is_some() {
            Self::Zellij
        } else if std::env::var_os("TMUX").is_some() {
            Self::Tmux
        } else if std::env::var_os("STY").is_some() {
            Self::Screen
        } else {
            Self::None
        }
    }

    // tmux and screen need DCS-passthrough with every internal ESC doubled.
    // Without doubling, the `ESC \` in an OSC52 ST terminator would close
    // the DCS wrapper early and truncate the payload.
    // Zellij intercepts OSC52 natively, so we just emit the raw sequence.
    fn wrap_for_mux(&self, sequence: String) -> String {
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
    let _ = terminal.clear();
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
    let sequence = TerminalMux::detect().wrap_for_mux(sequence);
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::{Backend, CrosstermBackend};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use test_case::test_case;

    const WRONG_TERMUX: &str = "the environment was read as the wrong terminal";
    const TERMUX_PREFIX: &str = "/data/data/com.termux/files/usr";
    const EXPECTED_KEYBOARD_ENHANCEMENTS: &str = "\u{1b}[>3u";

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

    fn env<'a>(term_program: Option<&'a str>) -> TerminalEnvironment<'a> {
        TerminalEnvironment {
            term_program,
            ..TerminalEnvironment::default()
        }
    }

    // Simulates DCS-passthrough parsing: `ESC ESC` becomes one ESC,
    // `ESC \` ends the DCS. Panics on bad input so tests fail loudly.
    fn parse_dcs_passthrough(wrapped: &str, prefix: &str) -> String {
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

    // Uses the ST terminator that crossterm emits, which puts ESC bytes
    // at the start and in the middle of the payload.
    const OSC52_WITH_ST: &str = "\u{1b}]52;c;SGVsbG8=\u{1b}\\";

    #[test_case(NotificationMethod::Osc9, false, Some(ResolvedNotifier::Osc9) ; "osc9")]
    #[test_case(NotificationMethod::Bell, false, Some(ResolvedNotifier::Bell) ; "bell")]
    #[test_case(NotificationMethod::Off, false, None ; "off")]
    #[test_case(NotificationMethod::Osc9, true, Some(ResolvedNotifier::Osc9) ; "osc9_inside_herdr")]
    #[test_case(NotificationMethod::Bell, true, Some(ResolvedNotifier::Bell) ; "bell_inside_herdr")]
    #[test_case(NotificationMethod::Auto, true, None ; "auto_inside_herdr")]
    fn notification_method_resolves_without_detection(
        configured: NotificationMethod,
        herdr: bool,
        expected: Option<ResolvedNotifier>,
    ) {
        assert_eq!(
            resolve_notifier(configured, herdr, || panic!("auto detection ran")),
            expected
        );
    }

    #[test_case(true, ResolvedNotifier::Osc9 ; "osc9_terminal")]
    #[test_case(false, ResolvedNotifier::Bell ; "other_terminal")]
    fn auto_outside_herdr_detects_the_terminal(osc9: bool, expected: ResolvedNotifier) {
        assert_eq!(
            resolve_notifier(NotificationMethod::Auto, false, || osc9),
            Some(expected)
        );
    }

    #[test]
    fn screen_is_the_only_mux_without_focus_reporting() {
        let notifier = |mux| TerminalNotifier {
            notifier: ResolvedNotifier::Osc9,
            mux,
        };

        assert!(!notifier(TerminalMux::Screen).supports_focus_reporting());
        for mux in [TerminalMux::None, TerminalMux::Tmux, TerminalMux::Zellij] {
            assert!(notifier(mux).supports_focus_reporting());
        }
    }

    #[test]
    fn auto_supports_known_osc9_terminals() {
        for value in ["Ghostty", "iTerm.app", "kitty", "WarpTerminal", "WezTerm"] {
            assert!(
                auto_supports_osc9(&env(Some(value)), None),
                "terminal: {value}"
            );
        }
    }

    #[test]
    fn term_program_precedes_terminal_specific_variables() {
        let terminal = TerminalEnvironment {
            term_program: Some("Alacritty"),
            wezterm: true,
            ..TerminalEnvironment::default()
        };
        assert!(!auto_supports_osc9(&terminal, None));
    }

    #[test]
    fn auto_uses_specific_variables_and_term_fallbacks() {
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
            assert!(auto_supports_osc9(&terminal, None));
        }
        let terminal = TerminalEnvironment {
            term: Some("xterm-256color"),
            ..TerminalEnvironment::default()
        };
        assert!(!auto_supports_osc9(&terminal, None));
    }

    #[test]
    fn auto_inside_tmux_checks_client_terminal() {
        let terminal = env(Some("tmux"));
        let ghostty = TmuxClient {
            term_type: "ghostty 1.2.3".into(),
            term_name: "xterm-256color".into(),
        };
        assert!(auto_supports_osc9(&terminal, Some(&ghostty)));
        let ghostty_name = TmuxClient {
            term_type: "xterm-256color".into(),
            term_name: "xterm-ghostty".into(),
        };
        assert!(auto_supports_osc9(&terminal, Some(&ghostty_name)));
        assert!(!auto_supports_osc9(&terminal, None));
    }

    #[test]
    fn notification_sequences_encode_message_and_keep_bell_raw() {
        const MESSAGE: &str = "Task complete";
        const OSC9_SEQUENCE: &str = "\u{1b}]9;Task complete\u{7}";

        assert_eq!(
            notification_sequence(ResolvedNotifier::Osc9, TerminalMux::None, MESSAGE),
            OSC9_SEQUENCE
        );
        assert_eq!(
            notification_sequence(ResolvedNotifier::Bell, TerminalMux::Tmux, MESSAGE),
            BELL_SEQUENCE
        );
    }

    #[test_case("\0Task\t\u{7f}complete\u{85}\nnow\u{2003}", "Task complete now" ; "strips_controls_and_collapses_whitespace")]
    #[test_case("\0\t\u{7f}\u{85}\n\u{2003}", FALLBACK_NOTIFICATION_MESSAGE ; "all_control_input_falls_back")]
    fn sanitize_notification_message_cases(message: &str, expected: &str) {
        assert_eq!(sanitize_notification_message(message), expected);
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
    fn osc9_roundtrips_mux_passthrough() {
        const MESSAGE: &str = "Task complete";
        const OSC9_SEQUENCE: &str = "\u{1b}]9;Task complete\u{7}";

        let tmux = notification_sequence(ResolvedNotifier::Osc9, TerminalMux::Tmux, MESSAGE);
        let screen = notification_sequence(ResolvedNotifier::Osc9, TerminalMux::Screen, MESSAGE);
        assert_eq!(parse_dcs_passthrough(&tmux, "\u{1b}Ptmux;"), OSC9_SEQUENCE);
        assert_eq!(parse_dcs_passthrough(&screen, "\u{1b}P"), OSC9_SEQUENCE);
    }

    #[test]
    fn none_is_identity() {
        assert_eq!(
            TerminalMux::None.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn zellij_is_identity_because_it_intercepts_osc52() {
        // Zellij handles OSC52 itself; DCS-wrapping would eat the sequence.
        assert_eq!(
            TerminalMux::Zellij.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn tmux_wrap_survives_tmux_passthrough_parser() {
        let wrapped = TerminalMux::Tmux.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(
            parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn screen_wrap_survives_screen_passthrough_parser() {
        let wrapped = TerminalMux::Screen.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}P"), OSC52_WITH_ST);
    }

    // Multiple interior ESC bytes: if any gets left undoubled, the first
    // bare `ESC \` would close the DCS early and truncate everything after.
    #[test]
    fn tmux_preserves_payload_with_multiple_interior_esc_bytes() {
        let payload = "\u{1b}A\u{1b}B\u{1b}C\u{1b}\\";
        let wrapped = TerminalMux::Tmux.wrap_for_mux(payload.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), payload);
    }

    #[test]
    fn tmux_wrap_roundtrips_crossterm_osc52_output() {
        let mut sequence = String::new();
        CopyToClipboard::to_clipboard_from("hello, world!")
            .write_ansi(&mut sequence)
            .expect("crossterm write_ansi");
        let wrapped = TerminalMux::Tmux.wrap_for_mux(sequence.clone());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), sequence);
    }
}
