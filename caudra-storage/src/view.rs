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
    Auto,
    /// Every card that can close is closed.
    Compact,
    /// Every card is open.
    #[default]
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

    use super::{VIEW, ViewMode, persist, read};
    use crate::StateDir;
    use crate::state::{self, SCOPE_GLOBAL};

    const UNSET: &str = "an unwritten mode must leave the caller's default alone";
    const ROUND_TRIP: &str = "a stored mode must read back as written";
    const UNKNOWN: &str = "a missing or unrecognized mode is not a saved choice";
    const UNKNOWN_MODE: &str = "roomy";
    const DEFAULT: &str = "a reader with no recognized saved choice gets expanded mode";
    const CYCLE: &str = "the cycle must remain expanded, auto, compact, then expanded";

    #[test]
    fn an_unchosen_mode_is_expanded() {
        assert_eq!(ViewMode::default(), ViewMode::Expanded, "{DEFAULT}");
    }

    #[test_case(ViewMode::Expanded, ViewMode::Auto ; "expanded_to_auto")]
    #[test_case(ViewMode::Auto, ViewMode::Compact ; "auto_to_compact")]
    #[test_case(ViewMode::Compact, ViewMode::Expanded ; "compact_to_expanded")]
    fn the_cycle_preserves_each_edge(mode: ViewMode, expected: ViewMode) {
        assert_eq!(mode.next(), expected, "{CYCLE}");
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
        assert_eq!(read(&dir).unwrap_or_default(), mode, "{ROUND_TRIP}");
    }

    #[test_case(None ; "missing")]
    #[test_case(Some(UNKNOWN_MODE) ; "unrecognized")]
    fn a_missing_or_unrecognized_mode_falls_back_to_expanded(stored: Option<&str>) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        if let Some(stored) = stored {
            state::set(&dir, SCOPE_GLOBAL, VIEW, &stored).unwrap();
        }
        assert_eq!(read(&dir), None, "{UNKNOWN}");
        assert_eq!(
            read(&dir).unwrap_or_default(),
            ViewMode::Expanded,
            "{DEFAULT}"
        );
    }
}
