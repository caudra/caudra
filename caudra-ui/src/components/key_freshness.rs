use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

/// How long a key must stay quiet before a terminal that never reports its
/// key-up lets it through again. Auto-repeat starts well inside this, so a
/// held key keeps itself blocked.
pub(crate) const QUIET_GAP: Duration = Duration::from_secs(1);

/// How a held-back key gets through again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rearm {
    /// The terminal reports this key's key-up, so letting go is enough.
    Release,
    /// It never has, so a press after a quiet moment counts as new.
    Wait,
}

/// Holds back a key that was already down when what it would answer last
/// changed, until it is let go or another key is pressed, so a held or
/// repeated key cannot answer something the user has not seen. A key whose
/// key-up the terminal has never reported is let through by a press that
/// comes [`QUIET_GAP`] after the one before it instead.
#[derive(Default)]
pub(crate) struct KeyFreshness {
    last_press: Option<(KeyCode, Instant)>,
    blocked_key: Option<KeyCode>,
    held_back: bool,
    released: HashSet<KeyCode>,
}

impl KeyFreshness {
    pub(crate) fn barrier(&mut self) {
        self.blocked_key = self.blocked_key.or(self.last_press.map(|(code, _)| code));
    }

    /// Holds back the key just pressed because nothing acted on it, which is
    /// worth telling the user about.
    pub(crate) fn refuse(&mut self) {
        self.barrier();
        self.held_back = self.blocked_key.is_some();
    }

    /// Records the event, and says whether it is a press fresh enough to act on.
    pub(crate) fn accept(&mut self, key: KeyEvent, now: Instant) -> bool {
        match key.kind {
            KeyEventKind::Release => {
                self.release(key.code);
                false
            }
            KeyEventKind::Repeat => {
                self.last_press = Some((key.code, now));
                self.barrier();
                self.held_back |= self.blocked_key == Some(key.code);
                false
            }
            KeyEventKind::Press => self.press(key.code, now),
        }
    }

    fn press(&mut self, code: KeyCode, now: Instant) -> bool {
        let previous = self.last_press.replace((code, now));
        if self.blocked_key == Some(code) {
            let quiet = !self.released.contains(&code)
                && previous.is_some_and(|(previous, at)| {
                    previous == code && now.saturating_duration_since(at) >= QUIET_GAP
                });
            if !quiet {
                self.held_back = true;
                return false;
            }
        }
        self.unblock();
        true
    }

    fn release(&mut self, code: KeyCode) {
        self.released.insert(code);
        if self.blocked_key == Some(code) {
            self.unblock();
        }
        if self.last_press.is_some_and(|(last, _)| last == code) {
            self.last_press = None;
        }
    }

    fn unblock(&mut self) {
        self.blocked_key = None;
        self.held_back = false;
    }

    /// Forgets what is held back, but keeps which keys report their key-up.
    pub(crate) fn clear(&mut self) {
        self.last_press = None;
        self.unblock();
    }

    /// The key being held back and how it gets through, once that is worth
    /// saying: always while it is down, and on a terminal without key-ups only
    /// after a press was refused, since the next quiet press passes anyway.
    pub(crate) fn held_back(&self) -> Option<(KeyCode, Rearm)> {
        let code = self.blocked_key?;
        if self.released.contains(&code) {
            Some((code, Rearm::Release))
        } else {
            self.held_back.then_some((code, Rearm::Wait))
        }
    }

    #[cfg(test)]
    pub(crate) fn blocked_key(&self) -> Option<KeyCode> {
        self.blocked_key
    }

    #[cfg(test)]
    pub(crate) fn last_press(&self) -> Option<KeyCode> {
        self.last_press.map(|(code, _)| code)
    }

    /// Moves the last press back in time, so a test can wait out the gap
    /// without sleeping.
    #[cfg(test)]
    pub(crate) fn age(&mut self, by: Duration) {
        if let Some((_, at)) = &mut self.last_press {
            *at = at
                .checked_sub(by)
                .expect("the clock reaches back far enough");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use test_case::test_case;

    use super::{KeyFreshness, QUIET_GAP, Rearm};

    const AUTO_REPEAT: Duration = Duration::from_millis(30);
    const REPEATS: u32 = 100;

    fn event(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    fn press(code: KeyCode) -> KeyEvent {
        event(code, KeyEventKind::Press)
    }

    fn release(code: KeyCode) -> KeyEvent {
        event(code, KeyEventKind::Release)
    }

    fn blocked_after_press(code: KeyCode, at: Instant) -> KeyFreshness {
        let mut freshness = KeyFreshness::default();
        assert!(freshness.accept(press(code), at));
        freshness.barrier();
        freshness
    }

    #[test_case(KeyCode::Char('y'))]
    #[test_case(KeyCode::Char('n'))]
    #[test_case(KeyCode::Enter)]
    fn barrier_rejects_repeat_and_unreleased_press(code: KeyCode) {
        let now = Instant::now();
        let mut freshness = blocked_after_press(code, now);
        assert!(!freshness.accept(press(code), now));
        assert!(!freshness.accept(event(code, KeyEventKind::Repeat), now));
        assert!(!freshness.accept(release(code), now));
        assert!(freshness.accept(press(code), now));
        freshness.barrier();
        assert!(freshness.accept(press(KeyCode::Tab), now));
        assert!(freshness.accept(press(code), now));
    }

    #[test_case(QUIET_GAP, true; "after_the_gap")]
    #[test_case(QUIET_GAP - AUTO_REPEAT, false; "inside_the_gap")]
    fn without_key_ups_a_quiet_press_is_fresh(gap: Duration, fresh: bool) {
        let start = Instant::now();
        let mut freshness = blocked_after_press(KeyCode::Enter, start);
        assert_eq!(freshness.accept(press(KeyCode::Enter), start + gap), fresh);
        assert_eq!(freshness.blocked_key().is_none(), fresh);
    }

    #[test]
    fn without_key_ups_a_held_key_stays_blocked() {
        let start = Instant::now();
        let mut freshness = blocked_after_press(KeyCode::Enter, start);
        for step in 1..=REPEATS {
            assert!(!freshness.accept(press(KeyCode::Enter), start + AUTO_REPEAT * step));
        }
        assert_eq!(freshness.held_back(), Some((KeyCode::Enter, Rearm::Wait)));
        let quiet = start + AUTO_REPEAT * REPEATS + QUIET_GAP;
        assert!(freshness.accept(press(KeyCode::Enter), quiet));
    }

    #[test]
    fn a_repeat_restarts_the_gap() {
        let start = Instant::now();
        let mut freshness = blocked_after_press(KeyCode::Enter, start);
        let repeat = start + QUIET_GAP;
        freshness.accept(event(KeyCode::Enter, KeyEventKind::Repeat), repeat);
        assert!(!freshness.accept(press(KeyCode::Enter), repeat + AUTO_REPEAT));
    }

    #[test]
    fn a_seen_key_up_keeps_the_strict_rule() {
        let start = Instant::now();
        let mut freshness = KeyFreshness::default();
        freshness.accept(press(KeyCode::Enter), start);
        freshness.accept(release(KeyCode::Enter), start);
        freshness.accept(press(KeyCode::Enter), start);
        freshness.barrier();
        assert_eq!(
            freshness.held_back(),
            Some((KeyCode::Enter, Rearm::Release))
        );
        assert!(!freshness.accept(press(KeyCode::Enter), start + QUIET_GAP * 10));
    }

    #[test]
    fn key_ups_are_learned_per_key() {
        let start = Instant::now();
        let mut freshness = KeyFreshness::default();
        freshness.accept(press(KeyCode::Esc), start);
        freshness.accept(release(KeyCode::Esc), start);
        freshness.accept(press(KeyCode::Enter), start);
        freshness.barrier();
        assert!(freshness.accept(press(KeyCode::Enter), start + QUIET_GAP));
    }

    #[test]
    fn without_key_ups_the_hint_waits_for_a_refused_press() {
        let start = Instant::now();
        let mut freshness = blocked_after_press(KeyCode::Enter, start);
        assert_eq!(freshness.held_back(), None);
        assert!(!freshness.accept(press(KeyCode::Enter), start + AUTO_REPEAT));
        assert_eq!(freshness.held_back(), Some((KeyCode::Enter, Rearm::Wait)));
    }

    #[test]
    fn clearing_keeps_what_the_terminal_reports() {
        let start = Instant::now();
        let mut freshness = KeyFreshness::default();
        freshness.accept(press(KeyCode::Enter), start);
        freshness.accept(release(KeyCode::Enter), start);
        freshness.accept(press(KeyCode::Enter), start);
        freshness.clear();
        freshness.accept(press(KeyCode::Enter), start);
        freshness.barrier();
        assert!(!freshness.accept(press(KeyCode::Enter), start + QUIET_GAP));
    }
}
