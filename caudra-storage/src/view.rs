//! The persisted transcript view mode, so `/view` survives a restart. A
//! global state row.

use tracing::warn;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir};

const VIEW: StateKey = StateKey {
    name: "ui.view",
    class: StateClass::Persistent,
};
const AUTO: &str = "auto";
const COMPACT: &str = "compact";
const EXPANDED: &str = "expanded";

/// How much of each transcript card the reader sees without asking.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The card being written is open and the ones scrolled past are closed,
    /// unless closing one would hide something that changed the workspace.
    #[default]
    Auto,
    /// Every card that can close is closed.
    Compact,
    /// Every card is open.
    Expanded,
}

impl ViewMode {
    /// The order `/view` walks. It starts at the default, so the first press
    /// on a fresh install moves somewhere the reader has not been.
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Compact,
            Self::Compact => Self::Expanded,
            Self::Expanded => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => AUTO,
            Self::Compact => COMPACT,
            Self::Expanded => EXPANDED,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            AUTO => Some(Self::Auto),
            COMPACT => Some(Self::Compact),
            EXPANDED => Some(Self::Expanded),
            _ => None,
        }
    }
}

pub fn persist(dir: &StateDir, mode: ViewMode) {
    if let Err(error) = state::set(dir, SCOPE_GLOBAL, VIEW, &mode.as_str()) {
        warn!(%error, "failed to persist view mode");
    }
}

/// `None` when nothing was stored or the value names a mode this build does
/// not know, which leaves the caller's own default standing.
pub fn read(dir: &StateDir) -> Option<ViewMode> {
    state::get::<String>(dir, SCOPE_GLOBAL, VIEW)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read view mode");
            None
        })
        .and_then(|mode| ViewMode::parse(&mode))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const UNSET: &str = "an unwritten mode must leave the caller's default alone";
    const ROUND_TRIP: &str = "a stored mode must read back as written";
    const UNKNOWN: &str = "a mode this build cannot read is not a mode";
    const DEFAULT: &str = "a reader who never chose gets the mode that follows the writing";
    const CYCLE: &str = "every mode must be reachable by pressing the key three times";

    #[test]
    fn an_unchosen_mode_follows_the_latest_card() {
        assert_eq!(ViewMode::default(), ViewMode::Auto, "{DEFAULT}");
    }

    #[test]
    fn the_cycle_visits_every_mode_and_returns() {
        let mut mode = ViewMode::default();
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(mode);
            mode = mode.next();
        }
        seen.sort_by_key(|mode| mode.as_str());
        assert_eq!(
            seen,
            vec![ViewMode::Auto, ViewMode::Compact, ViewMode::Expanded],
            "{CYCLE}"
        );
        assert_eq!(mode, ViewMode::default(), "{CYCLE}");
    }

    #[test_case(ViewMode::Auto ; "auto")]
    #[test_case(ViewMode::Compact ; "compact")]
    #[test_case(ViewMode::Expanded ; "expanded")]
    fn a_chosen_mode_round_trips(mode: ViewMode) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert_eq!(read(&dir), None, "{UNSET}");
        persist(&dir, mode);
        assert_eq!(read(&dir), Some(mode), "{ROUND_TRIP}");
    }

    #[test]
    fn a_mode_this_build_cannot_read_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        state::set(&dir, SCOPE_GLOBAL, VIEW, &"roomy").unwrap();
        assert_eq!(read(&dir), None, "{UNKNOWN}");
    }
}
