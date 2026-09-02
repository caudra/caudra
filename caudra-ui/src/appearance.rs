//! Whether the terminal is showing us a light or a dark background, so a
//! light/dark theme pair can follow it without the user re-picking a theme.
//!
//! The terminal is asked with OSC 11, whose reply carries the background as
//! `rgb:` components of one to four hex digits each. Terminals disagree on
//! the width and on the terminator, so the parser accepts every shape.

use std::time::{Duration, Instant};

use crate::repaint::Dirty;
use crate::theme::ThemePair;
use crate::{theme, tty_query};

/// Query the background color. The reply is `OSC 11 ; rgb:.../.../... ST`.
const BACKGROUND_QUERY: &[u8] = b"\x1b]11;?\x07";
/// Matches the truecolor probe: enough for a slow remote terminal, short
/// enough that a terminal which never answers does not stall a frame.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const REPLY_PREFIX: &[u8] = b"]11;";
const RGB_MARKER: &[u8] = b"rgb";
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
/// links) that we stop asking rather than pay the timeout every interval.
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

/// `None` when the terminal does not answer, which is normal under some
/// multiplexers and over bare serial links. Callers keep their current theme
/// rather than guessing.
///
/// Reads the tty directly, so the input reader must not be running.
pub(crate) fn detect() -> Option<Appearance> {
    let reply = tty_query::query(BACKGROUND_QUERY, QUERY_TIMEOUT)?;
    let appearance = parse_background(&reply).map(|(r, g, b)| Appearance::from_background(r, g, b));
    tracing::debug!(?appearance, "terminal background probe");
    appearance
}

/// Only the bytes between the `]11;` introducer and the terminator count, so
/// input that arrives mid-probe (mouse reports, a paste) cannot spoof a match.
fn parse_background(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let start = tty_query::find(buf, REPLY_PREFIX)? + REPLY_PREFIX.len();
    let payload = &buf[start..];
    let end = payload
        .iter()
        .position(|b| matches!(b, b'\x07' | b'\x1b'))
        .unwrap_or(payload.len());
    let payload = &payload[..end];

    // Some terminals answer `rgba:` and put alpha last, which we ignore.
    let after_marker = &payload[tty_query::find(payload, RGB_MARKER)? + RGB_MARKER.len()..];
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
    /// `None` once we have stopped probing for good.
    next_probe: Option<Instant>,
    last_probe: Instant,
    silent_probes: u8,
}

impl AutoSwitch {
    /// `initial` comes from the startup probe, taken before the input reader
    /// owns the tty. `None` leaves the dark half in place and keeps probing.
    pub(crate) fn new(dark: String, light: String, initial: Option<Appearance>) -> Self {
        Self {
            dark,
            light,
            applied: initial.unwrap_or(Appearance::Dark),
            next_probe: Some(Instant::now() + PROBE_INTERVAL),
            last_probe: Instant::now(),
            silent_probes: 0,
        }
    }

    /// Adopts a pair mid-session, after `chosen` was picked from `/theme`.
    /// The chosen half stays on screen until the next probe, so picking the
    /// light half of a pair does not flip back before the terminal is asked.
    pub(crate) fn adopt(pair: &ThemePair, chosen: &str) -> Self {
        let applied = if pair.light == chosen {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        let mut auto = Self::new(pair.dark.to_owned(), pair.light.to_owned(), Some(applied));
        auto.wake();
        auto
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
    ///
    /// A terminal we have already given up on stays given up on, so window
    /// switching cannot restart probing that went unanswered.
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
        let name = self.theme_name();
        let theme = theme::load_by_name(name)?;
        theme::set_current_name(name);
        theme::set(theme);
        Ok(())
    }

    pub(crate) fn observe(&mut self, observed: Option<Appearance>) -> Dirty {
        self.last_probe = Instant::now();
        let Some(appearance) = observed else {
            self.silent_probes += 1;
            self.next_probe =
                (self.silent_probes < MAX_SILENT_PROBES).then(|| Instant::now() + PROBE_INTERVAL);
            if self.next_probe.is_none() {
                tracing::info!(
                    probes = self.silent_probes,
                    "terminal never reported its background; theme auto-switch off"
                );
            }
            return Dirty::NO;
        };

        self.silent_probes = 0;
        self.next_probe = Some(Instant::now() + PROBE_INTERVAL);
        if appearance == self.applied {
            return Dirty::NO;
        }
        self.applied = appearance;
        match self.apply() {
            Ok(()) => {
                tracing::info!(
                    theme = self.theme_name(),
                    ?appearance,
                    "theme auto-switched"
                );
                Dirty::YES
            }
            Err(e) => {
                tracing::warn!(error = %e, "theme auto-switch failed; off");
                self.next_probe = None;
                Dirty::NO
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repaint::expect;
    use test_case::test_case;

    const DARK_THEME: &str = "opencode";
    const LIGHT_THEME: &str = "opencode_light";
    const MANUAL_THEME: &str = "dracula";

    fn switch() -> AutoSwitch {
        AutoSwitch::new(
            DARK_THEME.to_owned(),
            LIGHT_THEME.to_owned(),
            Some(Appearance::Dark),
        )
    }

    /// Installing the pair is what makes `current_theme_name` deterministic,
    /// so the manual-pick check has a known baseline to compare against.
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
    fn a_terminal_that_never_answers_is_given_up_on() {
        let mut auto = switch();
        for _ in 1..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
            assert!(auto.due(Instant::now() + PROBE_INTERVAL));
        }
        assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
        assert!(!auto.due(Instant::now() + PROBE_INTERVAL * 100));
    }

    #[test]
    fn an_answer_restores_the_silent_probe_budget() {
        let mut auto = installed_switch();
        for _ in 1..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
        }
        assert_eq!(
            auto.observe(Some(Appearance::Dark)),
            Dirty::NO,
            "{}",
            expect::QUIET
        );
        for _ in 1..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
            assert!(auto.due(Instant::now() + PROBE_INTERVAL));
        }
    }

    #[test]
    fn an_unchanged_background_owes_no_frame() {
        let mut auto = installed_switch();
        assert_eq!(
            auto.observe(Some(Appearance::Dark)),
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
            auto.observe(Some(Appearance::Light)),
            Dirty::YES,
            "{}",
            expect::OWED
        );
        assert_eq!(auto.theme_name(), LIGHT_THEME);
        assert_eq!(theme::current_theme_name(), LIGHT_THEME);

        assert_eq!(
            auto.observe(Some(Appearance::Dark)),
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
            auto.observe(Some(Appearance::Dark)),
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
    fn wake_does_not_revive_a_terminal_we_gave_up_on() {
        let mut auto = switch();
        for _ in 0..MAX_SILENT_PROBES {
            assert_eq!(auto.observe(None), Dirty::NO, "{}", expect::QUIET);
        }
        auto.wake();
        assert!(!auto.due(Instant::now() + PROBE_INTERVAL * 100));
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
    #[test_case(b"\x1b]11;rgb:0a0a/0a0a/0a0a\x07", Some((0x0a, 0x0a, 0x0a)); "16_bit_opencode_dark")]
    #[test_case(b"\x1b]11;rgb:ff/ff/ff\x1b\\", Some((0xff, 0xff, 0xff)); "8_bit_st_terminated")]
    #[test_case(b"\x1b]11;rgb:1a/1a/1a\x07", Some((0x1a, 0x1a, 0x1a)); "8_bit_dark")]
    #[test_case(b"\x1b]11;rgb:f/f/f\x07", Some((0xff, 0xff, 0xff)); "4_bit_white")]
    #[test_case(b"\x1b]11;rgba:ffff/ffff/ffff/ffff\x07", Some((0xff, 0xff, 0xff)); "rgba_alpha_ignored")]
    #[test_case(b"\x1b]11;rgb:2828/2a2a/3636\x07\x1b[?65;1;9c", Some((0x28, 0x2a, 0x36)); "da1_tail_ignored")]
    #[test_case(b"\x1b[<48;2;5M\x1b]11;rgb:0000/0000/0000\x07", Some((0x00, 0x00, 0x00)); "mouse_report_before_reply")]
    #[test_case(b"\x1b[?65;1;9c", None; "no_osc_reply")]
    #[test_case(b"", None; "empty")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff\x07", None; "missing_component")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/\x07", None; "empty_component")]
    #[test_case(b"\x1b]11;rgb:zzzz/0000/0000\x07", None; "non_hex_component")]
    #[test_case(b"\x1b]11;rgb:fffff/0/0\x07", None; "component_too_wide")]
    #[test_case(b"\x1b]11;\x07", None; "truncated_before_rgb")]
    fn parses_background_reply(buf: &[u8], expected: Option<(u8, u8, u8)>) {
        assert_eq!(parse_background(buf), expected);
    }

    #[test_case(0x0a, 0x0a, 0x0a, Appearance::Dark; "opencode_dark_background")]
    #[test_case(0xff, 0xff, 0xff, Appearance::Light; "opencode_light_background")]
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
