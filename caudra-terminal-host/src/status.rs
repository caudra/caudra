use std::borrow::Cow;

/// What a pane shows about the agent in it. `Done` and `Error` are outcomes
/// the user has not acknowledged yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneState {
    Idle,
    Working,
    Blocked(Option<BlockKind>),
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Permission,
    Question,
    Auth,
}

impl PaneState {
    fn priority(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Done => 1,
            Self::Error => 2,
            Self::Working => 3,
            Self::Blocked(_) => 4,
        }
    }

    pub fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

/// The persistent status a pane reports. Only a blocked pane has a label,
/// which names what waits for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneStatus {
    pub state: PaneState,
    pub label: Option<Cow<'static, str>>,
    /// What brings the agent back once the host restores the pane.
    pub resume: Option<Vec<String>>,
}

impl PaneStatus {
    pub const fn new(state: PaneState) -> Self {
        Self {
            state,
            label: None,
            resume: None,
        }
    }
}

/// What a host shows for the pane besides its state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneMetadata<'a> {
    pub title: Cow<'a, str>,
    pub model: Cow<'a, str>,
    pub context_percent: Option<u32>,
}

impl PaneMetadata<'_> {
    pub fn into_owned(self) -> PaneMetadata<'static> {
        PaneMetadata {
            title: Cow::Owned(self.title.into_owned()),
            model: Cow::Owned(self.model.into_owned()),
            context_percent: self.context_percent,
        }
    }
}

/// One status for several sessions in one pane: blocked over working over an
/// error over a completion over idle, the first of a kind winning.
pub fn aggregate(statuses: impl IntoIterator<Item = PaneStatus>) -> PaneStatus {
    statuses
        .into_iter()
        .fold(PaneStatus::new(PaneState::Idle), |selected, candidate| {
            if candidate.state.priority() > selected.state.priority() {
                candidate
            } else {
                selected
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const BLOCKER: &str = "Permission requested: shell";
    const OTHER_BLOCKER: &str = "Question requested";

    fn blocked(label: &'static str) -> PaneStatus {
        PaneStatus {
            label: Some(label.into()),
            ..PaneStatus::new(PaneState::Blocked(Some(BlockKind::Permission)))
        }
    }

    #[test_case(PaneState::Idle, PaneState::Done, PaneState::Done ; "done_over_idle")]
    #[test_case(PaneState::Done, PaneState::Error, PaneState::Error ; "error_over_done")]
    #[test_case(PaneState::Error, PaneState::Working, PaneState::Working ; "work_over_error")]
    #[test_case(PaneState::Working, PaneState::Blocked(None), PaneState::Blocked(None) ; "blocked_over_work")]
    fn aggregate_orders_states(first: PaneState, second: PaneState, expected: PaneState) {
        for order in [[first, second], [second, first]] {
            assert_eq!(
                aggregate(order.map(PaneStatus::new)).state,
                expected,
                "{order:?}"
            );
        }
    }

    #[test]
    fn the_first_blocked_session_names_the_pane() {
        assert_eq!(
            aggregate([
                PaneStatus::new(PaneState::Working),
                blocked(BLOCKER),
                blocked(OTHER_BLOCKER)
            ]),
            blocked(BLOCKER)
        );
    }

    #[test]
    fn no_sessions_is_idle() {
        assert_eq!(aggregate([]), PaneStatus::new(PaneState::Idle));
    }
}
