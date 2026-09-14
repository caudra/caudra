//! Telling one click from two.
//!
//! A terminal reports every press the same way, so a double click is only ever
//! two presses that landed on the same cell close enough together. The clock is
//! passed in rather than read here, which is what lets the window be tested
//! without sleeping through it.

use std::time::{Duration, Instant};

const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// The longest run this reports. Nothing in the workbench acts on a fourth
/// click, so a run that reaches it starts over rather than counting up forever.
const LONGEST_RUN: u8 = 3;

#[derive(Debug, Default)]
pub struct Clicks {
    last: Option<(Instant, (u16, u16))>,
    count: u8,
}

impl Clicks {
    /// How many presses in a row have landed on this cell, counting this one.
    /// A press elsewhere, or one that arrives too late, starts a fresh run.
    pub fn press(&mut self, at: (u16, u16), now: Instant) -> u8 {
        let continues = self.last.is_some_and(|(when, where_)| {
            where_ == at && now.duration_since(when) < MULTI_CLICK_WINDOW
        });
        self.count = match continues {
            true if self.count < LONGEST_RUN => self.count + 1,
            _ => 1,
        };
        self.last = Some((now, at));
        self.count
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use test_case::test_case;

    use super::{Clicks, MULTI_CLICK_WINDOW};

    const WRONG_RUN: &str = "the click run was not counted as expected";

    const CELL: (u16, u16) = (4, 9);
    const ELSEWHERE: (u16, u16) = (4, 10);

    #[test_case(Duration::ZERO, CELL, 2 ; "a press on the same cell continues the run")]
    #[test_case(MULTI_CLICK_WINDOW, CELL, 1 ; "a press past the window starts over")]
    #[test_case(Duration::ZERO, ELSEWHERE, 1 ; "a press on another cell starts over")]
    fn a_second_press_continues_only_when_it_is_near_in_time_and_place(
        gap: Duration,
        second: (u16, u16),
        expected: u8,
    ) {
        let start = Instant::now();
        let mut clicks = Clicks::default();

        assert_eq!(clicks.press(CELL, start), 1, "{WRONG_RUN}");
        assert_eq!(clicks.press(second, start + gap), expected, "{WRONG_RUN}");
    }

    #[test]
    fn a_run_saturates_at_three_and_then_starts_over() {
        let start = Instant::now();
        let step = MULTI_CLICK_WINDOW / 2;
        let mut clicks = Clicks::default();

        let run: Vec<u8> = (0..5u32)
            .map(|press| clicks.press(CELL, start + step * press))
            .collect();

        assert_eq!(run, vec![1, 2, 3, 1, 2], "{WRONG_RUN}");
    }

    #[test]
    fn each_press_restarts_the_window_from_itself() {
        let start = Instant::now();
        let step = MULTI_CLICK_WINDOW * 3 / 4;
        let mut clicks = Clicks::default();

        clicks.press(CELL, start);

        assert_eq!(clicks.press(CELL, start + step), 2, "{WRONG_RUN}");
        assert_eq!(clicks.press(CELL, start + step * 2), 3, "{WRONG_RUN}");
    }
}
