//! Whether the terminal is showing us a light or a dark background, so a
//! light/dark theme pair can follow it without the user re-picking a theme.

use std::io::stdout;
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{ColorScheme, RequestColorScheme};

use crate::repaint::Dirty;
use crate::theme::ThemePair;
use crate::{theme, tty_query};

const APPEARANCE_QUERY: &[u8] = b"\x1b[?996n\x1b]11;?\x07";
/// Matches the truecolor probe: enough for a slow remote terminal, short
/// enough that a terminal which never answers does not stall a frame.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const REPLY_PREFIX: &[u8] = b"\x1b]11;";
const RGB_MARKER: &[u8] = b"rgb";
const CSI: &[u8] = b"\x1b[";
const STRING_TERMINATOR: &[u8] = b"\x1b\\";
const DARK_REPORT: &[u8] = b"?997;1n";
const LIGHT_REPORT: &[u8] = b"?997;2n";
const PASTE_START: &[u8] = b"200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Coefficients and threshold are opencode's, so a terminal that puts it in
/// light mode puts us in light mode on exactly the same colors.
const LUMA_RED: f32 = 0.299;
const LUMA_GREEN: f32 = 0.587;
const LUMA_BLUE: f32 = 0.114;
const LIGHT_THRESHOLD: f32 = 0.5;
/// Long enough to be invisible, short enough that a session left open across
/// a desktop light/dark switch catches up on its own.
const PROBE_INTERVAL: Duration = Duration::from_secs(600);
/// Backoff when a probe is postponed because the user is typing.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Delay applied when something suggests the terminal changed, long enough to
/// collapse a burst of focus and resize events into a single probe.
const WAKE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Floor between probes, so switching windows repeatedly cannot keep parking
/// the input reader.
const MIN_PROBE_GAP: Duration = Duration::from_secs(2);
/// Terminals that never answer are common enough (some multiplexers, serial
/// links) that we stop the blocking probe rather than pay the timeout every
/// interval.
const MAX_SILENT_PROBES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    fn from_background(r: u8, g: u8, b: u8) -> Self {
        let channel = |c: u8| f32::from(c) / f32::from(u8::MAX);
        let luma = LUMA_RED * channel(r) + LUMA_GREEN * channel(g) + LUMA_BLUE * channel(b);
        if luma > LIGHT_THRESHOLD {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

impl From<ColorScheme> for Appearance {
    fn from(scheme: ColorScheme) -> Self {
        match scheme {
            ColorScheme::Dark => Self::Dark,
            ColorScheme::Light => Self::Light,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    Explicit(Appearance),
    Background(Appearance),
}

impl Observation {
    fn appearance(&self) -> Appearance {
        match self {
            Self::Explicit(appearance) | Self::Background(appearance) => *appearance,
        }
    }
}

/// `None` when the terminal does not answer, which is normal under some
/// multiplexers and over bare serial links. Callers keep their current theme
/// rather than guessing.
///
/// Reads the tty directly, so the input reader must not be running.
pub(crate) fn detect() -> Option<Observation> {
    let reply = tty_query::query(APPEARANCE_QUERY, QUERY_TIMEOUT);
    let observation = reply.as_deref().and_then(parse_observation);
    tracing::debug!(
        ?observation,
        response_bytes = reply.as_ref().map(Vec::len),
        "terminal appearance probe"
    );
    observation
}

fn parse_observation(mut buf: &[u8]) -> Option<Observation> {
    let mut explicit = None;
    let mut background = None;
    while let Some(start) = buf.iter().position(|byte| *byte == b'\x1b') {
        buf = &buf[start..];
        if let Some(csi) = buf.strip_prefix(CSI) {
            let Some(end) = csi
                .iter()
                .position(|byte| (b'@'..=b'~').contains(byte) || *byte == b'\x1b')
            else {
                break;
            };
            if csi[end] == b'\x1b' {
                buf = &csi[end..];
                continue;
            }
            let report = &csi[..=end];
            buf = &csi[end + 1..];
            match report {
                DARK_REPORT => explicit = Some(Observation::Explicit(Appearance::Dark)),
                LIGHT_REPORT => explicit = Some(Observation::Explicit(Appearance::Light)),
                PASTE_START => {
                    let Some(end) = tty_query::find(buf, PASTE_END) else {
                        break;
                    };
                    buf = &buf[end + PASTE_END.len()..];
                }
                _ => {}
            }
        } else if matches!(buf.get(1), Some(b']' | b'P' | b'X' | b'^' | b'_')) {
            let osc = buf[1] == b']';
            let Some(end) = buf.iter().enumerate().find_map(|(index, byte)| {
                if osc && *byte == b'\x07' {
                    Some(index + 1)
                } else if buf[index..].starts_with(STRING_TERMINATOR) {
                    Some(index + STRING_TERMINATOR.len())
                } else {
                    None
                }
            }) else {
                break;
            };
            if osc && let Some((r, g, b)) = parse_background(&buf[..end]) {
                background = Some(Observation::Background(Appearance::from_background(
                    r, g, b,
                )));
            }
            buf = &buf[end..];
        } else {
            buf = &buf[1..];
        }
    }
    explicit.or(background)
}

fn parse_background(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let payload = buf.strip_prefix(REPLY_PREFIX)?;
    let payload = payload
        .strip_suffix(b"\x07")
        .or_else(|| payload.strip_suffix(STRING_TERMINATOR))?;

    // Some terminals answer `rgba:` and put alpha last, which we ignore.
    let after_marker = payload.strip_prefix(RGB_MARKER)?;
    let components = after_marker
        .strip_prefix(b"a")
        .unwrap_or(after_marker)
        .strip_prefix(b":")?;
    let mut parts = components.split(|b| *b == b'/');
    let r = scale_component(parts.next()?)?;
    let g = scale_component(parts.next()?)?;
    let b = scale_component(parts.next()?)?;
    Some((r, g, b))
}

/// xterm answers with 16-bit components, others with 8, 4 or 12. Scaling by
/// the width's maximum keeps `ffff`, `ff` and `f` all meaning full intensity.
fn scale_component(hex: &[u8]) -> Option<u8> {
    let text = str::from_utf8(hex).ok()?;
    if text.is_empty() || text.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(text, 16).ok()?;
    let max = (1u32 << (4 * text.len() as u32)) - 1;
    u8::try_from(value * u32::from(u8::MAX) / max).ok()
}

/// Follows the terminal background between a light and a dark theme. Built
/// whenever the theme in use has a light half, either from `THEME_PAIRS` or
/// from `ui.theme_light`.
pub(crate) struct AutoSwitch {
    dark: String,
    light: String,
    applied: Appearance,
    explicit: bool,
    apply_failed: bool,
    next_probe: Option<Instant>,
    last_probe: Instant,
    silent_probes: u8,
}

impl AutoSwitch {
    /// `initial` comes from the startup probe, taken before the input reader
    /// owns the tty. `None` leaves the dark half in place and keeps probing.
    pub(crate) fn new(dark: String, light: String, initial: Option<Observation>) -> Self {
        Self {
            dark,
            light,
            applied: initial
                .as_ref()
                .map_or(Appearance::Dark, Observation::appearance),
            explicit: matches!(initial, Some(Observation::Explicit(_))),
            apply_failed: false,
            next_probe: Some(Instant::now() + PROBE_INTERVAL),
            last_probe: Instant::now(),
            silent_probes: 0,
        }
    }

    /// Adopts a pair mid-session, after `chosen` was picked from `/theme`.
    /// The chosen half stays on screen until the next probe, so picking the
    /// light half of a pair does not flip back before the terminal is asked.
    pub(crate) fn adopt(pair: &ThemePair, chosen: &str) -> Self {
        let mut auto = Self::new(String::new(), String::new(), None);
        auto.retarget(pair, chosen);
        auto
    }

    pub(crate) fn retarget(&mut self, pair: &ThemePair, chosen: &str) {
        self.dark = pair.dark.to_owned();
        self.light = pair.light.to_owned();
        self.applied = if pair.light == chosen {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        self.apply_failed = false;
        self.silent_probes = 0;
        self.next_probe = Some(Instant::now() + PROBE_INTERVAL);
        self.wake();
    }

    /// Ask only for the explicit report, which never pauses input, once the
    /// terminal has sent one or has ignored every blocking probe. A
    /// multiplexer can learn its appearance later without reporting the
    /// change, and answers when asked.
    pub(crate) fn uses_explicit(&self) -> bool {
        self.explicit || self.silent_probes >= MAX_SILENT_PROBES
    }

    pub(crate) fn request_explicit(&mut self) {
        if let Err(error) = stdout().execute(RequestColorScheme) {
            tracing::warn!(%error, "terminal appearance query failed");
        }
        self.last_probe = Instant::now();
        self.next_probe = Some(self.last_probe + PROBE_INTERVAL);
    }

    pub(crate) fn theme_name(&self) -> &str {
        match self.applied {
            Appearance::Dark => &self.dark,
            Appearance::Light => &self.light,
        }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.next_probe.is_some_and(|at| now >= at)
    }

    /// Postpone without spending the silent-probe budget.
    pub(crate) fn defer(&mut self) {
        self.next_probe = Some(Instant::now() + RETRY_INTERVAL);
    }

    /// Bring the next probe forward because a different terminal may now be
    /// showing the session: focus returned, or the viewport was resized.
    /// Reattaching a multiplexer to another terminal produces both.
    pub(crate) fn wake(&mut self) {
        let Some(scheduled) = self.next_probe else {
            return;
        };
        let earliest = (Instant::now() + WAKE_DEBOUNCE).max(self.last_probe + MIN_PROBE_GAP);
        self.next_probe = Some(scheduled.min(earliest));
    }

    /// Installs the current half. In-memory only: the pick the user saved
    /// interactively must survive a session that ran under a light terminal.
    pub(crate) fn apply(&self) -> Result<(), String> {
        self.apply_appearance(self.applied)
    }

    fn apply_appearance(&self, appearance: Appearance) -> Result<(), String> {
        let name = match appearance {
            Appearance::Dark => &self.dark,
            Appearance::Light => &self.light,
        };
        let theme = theme::load_by_name(name)?;
        theme::set_current_name(name);
        theme::set(theme);
        Ok(())
    }

    pub(crate) fn observe(&mut self, observed: Option<Observation>) -> Dirty {
        if matches!(observed, Some(Observation::Explicit(_))) {
            self.explicit = true;
        } else if self.explicit && matches!(observed, Some(Observation::Background(_))) {
            return Dirty::NO;
        }
        if self.apply_failed {
            return Dirty::NO;
        }
        self.last_probe = Instant::now();
        self.next_probe = Some(self.last_probe + PROBE_INTERVAL);
        let Some(observation) = observed else {
            self.silent_probes = self.silent_probes.saturating_add(1);
            if self.silent_probes == MAX_SILENT_PROBES {
                tracing::info!(
                    probes = self.silent_probes,
                    "terminal never answered the appearance probe; explicit queries only"
                );
            }
            return Dirty::NO;
        };

        self.silent_probes = 0;
        let appearance = observation.appearance();
        if appearance == self.applied {
            return Dirty::NO;
        }
        match self.apply_appearance(appearance) {
            Ok(()) => {
                self.applied = appearance;
                tracing::info!(
                    theme = self.theme_name(),
                    ?appearance,
                    "theme auto-switched"
                );
                Dirty::YES
            }
            Err(e) => {
                tracing::warn!(error = %e, "theme auto-switch failed; off");
                self.apply_failed = true;
                self.next_probe = None;
                Dirty::NO
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::ColorScheme;
    use test_case::test_case;

    use super::{
        Appearance, AutoSwitch, MAX_SILENT_PROBES, MIN_PROBE_GAP, Observation, PROBE_INTERVAL,
        RETRY_INTERVAL, WAKE_DEBOUNCE, parse_background, parse_observation,
    };
    use crate::repaint::Dirty;
    use crate::repaint::expect;
    use crate::theme::{self, ThemePair};

    const DARK_THEME: &str = "caudra-dark";
    const LIGHT_THEME: &str = "caudra-light";
    const MANUAL_THEME: &str = "dracula";
    const INVALID_THEME: &str = "\0invalid_theme";
    const OTHER_PAIR: ThemePair = ThemePair {
        dark: "ayu_dark",
        light: "ayu_light",
    };

    fn switch() -> AutoSwitch {
        AutoSwitch::new(
            DARK_THEME.to_owned(),
            LIGHT_THEME.to_owned(),
            Some(Observation::Background(Appearance::Dark)),
        )
    }

    fn installed_switch() -> AutoSwitch {
        let auto = switch();
        auto.apply().expect("bundled pair must load");
        auto
    }

    #[test]
    fn probe_waits_out_the_interval() {
        let auto = switch();
        assert!(!auto.due(Instant::now()));
        assert!(auto.due(Instant::now() + PROBE_INTERVAL));
    }

    #[test]
    fn defer_retries_sooner_than_a_full_interval() {
        let mut auto = switch();
        auto.defer();
        assert!(!auto.due(Instant::now()));
        assert!(auto.due(Instant::now() + RETRY_INTERVAL));
    }

    #[test]
    fn defer_does_not_spend_the_silent_probe_budget() {
        let mut auto = switch();
        for _ in 0..MAX_SILENT_PROBES * 2 {
            auto.defer();
        }
        assert!(auto.due(Instant::now() + PROBE_INTERVAL));
    }

    #[test]
    fn a_terminal_that_never_answers_is_only_asked_explicitly() {
        let mut auto = switch();
        for _ in 0..MAX_SILENT_PROBES {
            assert!(!auto.uses_explicit());
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
            assert!(auto.due(Instant::now() + PROBE_INTERVAL));
        }
        assert!(auto.uses_explicit());
    }

    #[test]
    fn an_answer_restores_the_silent_probe_budget() {
        let mut auto = installed_switch();
        for _ in 1..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
        }
        assert_eq!(
            auto.observe(Some(Observation::Background(Appearance::Dark))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        for _ in 1..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
            assert!(!auto.uses_explicit());
        }
    }

    #[test]
    fn an_unchanged_background_owes_no_frame() {
        let mut auto = installed_switch();
        assert_eq!(
            auto.observe(Some(Observation::Background(Appearance::Dark))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert_eq!(auto.theme_name(), DARK_THEME);
    }

    #[test]
    fn a_flipped_background_switches_halves() {
        let mut auto = installed_switch();
        assert_eq!(
            auto.observe(Some(Observation::Background(Appearance::Light))),
            Dirty::YES,
            "{}",
            expect::OWED
        );
        assert_eq!(auto.theme_name(), LIGHT_THEME);

        assert_eq!(
            auto.observe(Some(Observation::Background(Appearance::Dark))),
            Dirty::YES,
            "{}",
            expect::OWED
        );
        assert_eq!(auto.theme_name(), DARK_THEME);
    }

    #[test]
    fn wake_brings_the_probe_forward() {
        let mut auto = switch();
        assert!(!auto.due(Instant::now() + WAKE_DEBOUNCE));
        auto.wake();
        assert!(!auto.due(Instant::now()));
        assert!(auto.due(Instant::now() + MIN_PROBE_GAP));
    }

    #[test]
    fn wake_keeps_a_floor_between_probes() {
        let mut auto = installed_switch();
        assert_eq!(
            auto.observe(Some(Observation::Background(Appearance::Dark))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        auto.wake();
        assert!(
            !auto.due(Instant::now() + WAKE_DEBOUNCE),
            "a probe just ran, so the debounce alone must not schedule another"
        );
        assert!(auto.due(Instant::now() + MIN_PROBE_GAP));
    }

    #[test]
    fn wake_asks_a_terminal_that_never_answered() {
        let mut auto = switch();
        for _ in 0..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
        }
        auto.wake();
        assert!(
            auto.due(Instant::now() + MIN_PROBE_GAP),
            "a multiplexer may have learned its appearance since the last probe"
        );
    }

    #[test_case(DARK_THEME, Appearance::Dark; "dark_half")]
    #[test_case(LIGHT_THEME, Appearance::Light; "light_half")]
    fn adopt_keeps_the_chosen_half_on_screen(chosen: &str, expected: Appearance) {
        let pair = theme::pair_for(chosen).expect("bundled pair must be listed");
        let auto = AutoSwitch::adopt(pair, chosen);
        assert_eq!(auto.theme_name(), chosen);
        assert_eq!(auto.applied, expected);
    }

    #[test]
    fn adopt_asks_the_terminal_soon_after() {
        let pair = theme::pair_for(LIGHT_THEME).expect("bundled pair must be listed");
        let auto = AutoSwitch::adopt(pair, LIGHT_THEME);
        assert!(!auto.due(Instant::now()));
        assert!(
            auto.due(Instant::now() + MIN_PROBE_GAP),
            "adopting a pair must not wait out the full interval"
        );
    }

    #[test]
    fn an_unpaired_theme_has_nothing_to_follow() {
        assert!(theme::pair_for(MANUAL_THEME).is_none());
    }

    #[test_case(b"\x1b]11;rgb:0000/0000/0000\x07", Some((0x00, 0x00, 0x00)); "16_bit_black")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/ffff\x07", Some((0xff, 0xff, 0xff)); "16_bit_white")]
    #[test_case(b"\x1b]11;rgb:0a0a/0a0a/0a0a\x07", Some((0x0a, 0x0a, 0x0a)); "16_bit_caudra_dark")]
    #[test_case(b"\x1b]11;rgb:ff/ff/ff\x1b\\", Some((0xff, 0xff, 0xff)); "8_bit_st_terminated")]
    #[test_case(b"\x1b]11;rgb:1a/1a/1a\x07", Some((0x1a, 0x1a, 0x1a)); "8_bit_dark")]
    #[test_case(b"\x1b]11;rgb:f/f/f\x07", Some((0xff, 0xff, 0xff)); "4_bit_white")]
    #[test_case(b"\x1b]11;rgba:ffff/ffff/ffff/ffff\x07", Some((0xff, 0xff, 0xff)); "rgba_alpha_ignored")]
    #[test_case(b"\x1b[?65;1;9c", None; "no_osc_reply")]
    #[test_case(b"", None; "empty")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff\x07", None; "missing_component")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/\x07", None; "empty_component")]
    #[test_case(b"\x1b]11;rgb:zzzz/0000/0000\x07", None; "non_hex_component")]
    #[test_case(b"\x1b]11;rgb:fffff/0/0\x07", None; "component_too_wide")]
    #[test_case(b"\x1b]11;\x07", None; "truncated_before_rgb")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/ffff", None; "missing_terminator")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/ffff\x1b", None; "partial_terminator")]
    #[test_case(b"]11;rgb:ffff/ffff/ffff\x07", None; "missing_escape")]
    #[test_case(b"\x1b]11;not_rgb:ffff/ffff/ffff\x07", None; "unanchored_rgb")]
    fn parses_background_reply(buf: &[u8], expected: Option<(u8, u8, u8)>) {
        assert_eq!(parse_background(buf), expected);
    }

    #[test_case(b"\x1b[?997;1n", Some(Observation::Explicit(Appearance::Dark)); "explicit_dark")]
    #[test_case(b"\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "explicit_light")]
    #[test_case(b"\x1b]11;rgb:2828/2a2a/3636\x07\x1b[?65;1;9c", Some(Observation::Background(Appearance::Dark)); "da1_tail_ignored")]
    #[test_case(b"\x1b[<48;2;5M\x1b]11;rgb:ffff/ffff/ffff\x07", Some(Observation::Background(Appearance::Light)); "mouse_report_before_reply")]
    #[test_case(b"\x1b[?997;1n\x1b]11;rgb:ffff/ffff/ffff\x07", Some(Observation::Explicit(Appearance::Dark)); "explicit_before_background")]
    #[test_case(b"\x1b]11;rgb:0000/0000/0000\x07\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "explicit_after_background")]
    #[test_case(b"\x1b[?997;1n\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "last_explicit_light_wins")]
    #[test_case(b"\x1b[?997;2n\x1b[?997;1n", Some(Observation::Explicit(Appearance::Dark)); "last_explicit_dark_wins")]
    #[test_case(b"\x1b[?997;2n\x1b[?997;3n", Some(Observation::Explicit(Appearance::Light)); "invalid_report_keeps_last_valid")]
    #[test_case(b"\x1b[?997;1n\x1b[?997;2", Some(Observation::Explicit(Appearance::Dark)); "truncated_report_keeps_last_valid")]
    #[test_case(b"\x1b[?997;0n", None; "unknown_zero")]
    #[test_case(b"\x1b[?997;3n", None; "unknown_three")]
    #[test_case(b"\x1b[?997;01n", None; "leading_zero")]
    #[test_case(b"\x1b[?997;1;2n", None; "extra_parameter")]
    #[test_case(b"\x1b[?997;1 n", None; "intermediate_byte")]
    #[test_case(b"\x1b[?997;1m", None; "wrong_final")]
    #[test_case(b"\x1b[?997;1", None; "truncated_report")]
    #[test_case(b"\x1b[?997;", None; "truncated_value")]
    #[test_case(b"\x1b[", None; "truncated_csi")]
    #[test_case(b"\x1b", None; "truncated_escape")]
    #[test_case(b"[?997;1n", None; "missing_escape")]
    #[test_case(b"\x1b[997;1n", None; "missing_private_marker")]
    #[test_case(b"\x1b[?1997;1n", None; "wrong_report_number")]
    #[test_case(b"\x1b]0;\x1b[?997;1n\x07", None; "report_in_osc_bel")]
    #[test_case(b"\x1b]0;\x1b[?997;1n\x1b\\", None; "report_in_osc_st")]
    #[test_case(b"\x1b]0;\x1b[?997;1n", None; "report_in_unterminated_osc")]
    #[test_case(b"\x1bP\x1b[?997;1n\x1b\\", None; "report_in_dcs")]
    #[test_case(b"\x1b[200~\x1b[?997;1n\x1b[201~", None; "report_in_paste")]
    #[test_case(b"\x1b[200~\x1b[?997;1n", None; "report_in_unterminated_paste")]
    #[test_case(b"\x1b[200~\x1b]11;rgb:f/f/f\x07\x1b[201~", None; "background_in_paste")]
    #[test_case(b"\x1b[200~\x1b[?997;1n\x1b[201~\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "report_after_paste")]
    #[test_case(b"\x1b]0;\x1b[?997;1n\x07\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "report_after_osc")]
    #[test_case(b"\x1b[?997;\x1b[?997;2n", Some(Observation::Explicit(Appearance::Light)); "report_after_interrupted_csi")]
    #[test_case(b"\x1b[?997;3n\x1b]11;rgb:f/f/f\x07", Some(Observation::Background(Appearance::Light)); "invalid_report_falls_back_to_rgb")]
    fn parses_observation(buf: &[u8], expected: Option<Observation>) {
        assert_eq!(parse_observation(buf), expected);
    }

    #[test_case(ColorScheme::Dark, Appearance::Dark; "dark")]
    #[test_case(ColorScheme::Light, Appearance::Light; "light")]
    fn converts_color_scheme(scheme: ColorScheme, expected: Appearance) {
        assert_eq!(Appearance::from(scheme), expected);
    }

    #[test_case(Appearance::Dark, Appearance::Light; "dark_preference")]
    #[test_case(Appearance::Light, Appearance::Dark; "light_preference")]
    fn explicit_preference_overrides_later_background(
        explicit: Appearance,
        background: Appearance,
    ) {
        let mut auto = switch();
        let _ = auto.observe(Some(Observation::Explicit(explicit)));
        assert!(auto.uses_explicit());
        assert_eq!(auto.applied, explicit);
        assert_eq!(
            auto.observe(Some(Observation::Background(background))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert_eq!(auto.applied, explicit);
        assert_eq!(
            auto.observe(Some(Observation::Explicit(background))),
            Dirty::YES,
            "{}",
            expect::OWED
        );
        assert_eq!(auto.applied, background);
    }

    #[test_case(Appearance::Dark, Appearance::Light; "dark_initial")]
    #[test_case(Appearance::Light, Appearance::Dark; "light_initial")]
    fn initial_explicit_preference_survives_first_background(
        initial: Appearance,
        background: Appearance,
    ) {
        let mut auto = AutoSwitch::new(
            DARK_THEME.to_owned(),
            LIGHT_THEME.to_owned(),
            Some(Observation::Explicit(initial)),
        );
        assert!(auto.uses_explicit());
        assert_eq!(
            auto.observe(Some(Observation::Background(background))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert_eq!(auto.applied, initial);
    }

    #[test_case(Appearance::Light, Dirty::YES; "changed_notification")]
    #[test_case(Appearance::Dark, Dirty::NO; "unchanged_notification")]
    fn notification_recovers_after_silence_exhaustion(appearance: Appearance, expected: Dirty) {
        let mut auto = switch();
        for _ in 0..MAX_SILENT_PROBES {
            let _ = auto.observe(None);
        }
        assert!(!auto.explicit);
        assert_eq!(
            auto.observe(Some(Observation::Explicit(appearance))),
            expected
        );
        assert!(auto.uses_explicit());
        assert_eq!(auto.applied, appearance);
        assert_eq!(auto.silent_probes, 0);
        assert!(auto.due(auto.last_probe + PROBE_INTERVAL));
    }

    #[test_case(Observation::Explicit(Appearance::Light); "explicit_failure")]
    #[test_case(Observation::Background(Appearance::Light); "background_failure")]
    fn failed_application_preserves_applied_and_ignores_notifications(observation: Observation) {
        let mut auto = AutoSwitch::new(DARK_THEME.to_owned(), INVALID_THEME.to_owned(), None);
        assert_eq!(
            auto.observe(Some(observation)),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert_eq!(auto.applied, Appearance::Dark);
        assert_eq!(auto.theme_name(), DARK_THEME);
        assert!(auto.apply_failed);
        assert!(auto.next_probe.is_none());

        auto.light = LIGHT_THEME.to_owned();
        assert_eq!(
            auto.observe(Some(Observation::Explicit(Appearance::Light))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert!(auto.uses_explicit());
        assert_eq!(auto.applied, Appearance::Dark);
        auto.wake();
        assert!(auto.next_probe.is_none());
    }

    #[test_case(OTHER_PAIR.dark, Appearance::Dark, Appearance::Light; "dark_chosen")]
    #[test_case(OTHER_PAIR.light, Appearance::Light, Appearance::Dark; "light_chosen")]
    fn retarget_preserves_explicit_source(
        chosen: &str,
        appearance: Appearance,
        background: Appearance,
    ) {
        let mut auto = AutoSwitch::new(
            DARK_THEME.to_owned(),
            LIGHT_THEME.to_owned(),
            Some(Observation::Explicit(background)),
        );
        auto.retarget(&OTHER_PAIR, chosen);
        assert!(auto.uses_explicit());
        assert_eq!(auto.dark, OTHER_PAIR.dark);
        assert_eq!(auto.light, OTHER_PAIR.light);
        assert_eq!(auto.theme_name(), chosen);
        assert_eq!(auto.applied, appearance);
        assert!(auto.due(Instant::now() + MIN_PROBE_GAP));
        assert_eq!(
            auto.observe(Some(Observation::Background(background))),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        assert_eq!(auto.applied, appearance);
        assert_eq!(
            auto.observe(Some(Observation::Explicit(background))),
            Dirty::YES,
            "{}",
            expect::OWED
        );
        assert_eq!(auto.applied, background);
    }

    #[test_case(0x0a, 0x0a, 0x0a, Appearance::Dark; "caudra_dark_background")]
    #[test_case(0xff, 0xff, 0xff, Appearance::Light; "caudra_light_background")]
    #[test_case(0x28, 0x2a, 0x36, Appearance::Dark; "dracula_background")]
    #[test_case(0xf8, 0xf9, 0xfa, Appearance::Light; "ayu_light_background")]
    #[test_case(0x7f, 0x7f, 0x7f, Appearance::Dark; "one_step_below_mid_gray")]
    #[test_case(0x80, 0x80, 0x80, Appearance::Light; "one_step_above_mid_gray")]
    #[test_case(0x00, 0xff, 0x00, Appearance::Light; "green_dominates_luma")]
    #[test_case(0x00, 0x00, 0xff, Appearance::Dark; "blue_barely_contributes")]
    fn classifies_background(r: u8, g: u8, b: u8, expected: Appearance) {
        assert_eq!(Appearance::from_background(r, g, b), expected);
    }
}
