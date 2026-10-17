//! The `?` a modal carries on its top border: the section of the manual that
//! explains it, opened in the docs modal above it.

use ratatui::layout::Rect;
use strum::{EnumIter, IntoEnumIterator};
use unicode_width::UnicodeWidthStr;

pub(crate) const BADGE: &str = " ? ";
/// The corner and one run of border between the badge and the popup's edge,
/// so it reads as part of the border rather than a gap in it.
const RIGHT_INSET: u16 = 2;
/// Room the title keeps to the left of the badge. Below this a popup is too
/// narrow for both.
const MIN_POPUP_WIDTH: u16 = 24;
/// The columns the badge takes from the right of a top border, so a modal
/// that writes there too can end its text clear of it.
pub(crate) const BADGE_COLUMNS: u16 = BADGE.len() as u16 + RIGHT_INSET;

/// A page or section of the manual a modal links to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub enum DocsTopic {
    Keybindings,
    Usage,
    Logs,
    Context,
    Tools,
    Decisions,
    Skills,
    Storage,
    SystemPrompt,
    Projection,
    Goal,
    Autopilot,
    Search,
    Commands,
    Models,
    PromptProfiles,
    Login,
    Accounts,
    Mcp,
    ShellEnv,
    Permissions,
    Sandboxes,
    Stash,
    Memory,
    Tasks,
    Shells,
    Peers,
    WorkflowRuns,
    WorkflowCatalog,
    Automations,
    Sessions,
    Relocation,
    Worktrees,
    Review,
    Rewind,
    MessageActions,
    Queue,
}

impl DocsTopic {
    /// Where the topic lives, in the form `/docs` takes.
    pub const fn address(self) -> &'static str {
        match self {
            Self::Keybindings => "keybindings",
            Self::Usage => "token-economy#lifetime-spend",
            Self::Logs => "commands#logs",
            Self::Context => "context#inspect-the-active-window",
            Self::Tools => "token-economy#what-the-tools-cost",
            Self::Decisions => "code#decisions-and-reranking",
            Self::Skills => "skills#seeing-what-the-agent-has",
            Self::Storage => "sessions#what-the-state-directory-holds",
            Self::SystemPrompt => "system-prompts",
            Self::Projection => "context#provider-request-projection",
            Self::Goal => "commands#completion-goals",
            Self::Autopilot => "autopilot",
            Self::Search => "sessions#search-past-sessions",
            Self::Commands => "commands#built-in-commands",
            Self::Models => "providers#model-identifiers",
            Self::PromptProfiles => "system-prompts#select-a-profile",
            Self::Login => "accounts#add-an-account",
            Self::Accounts => "accounts#in-the-tui",
            Self::Mcp => "mcp#runtime-toggling",
            Self::ShellEnv => "cli#forwarding-more-variables",
            Self::Permissions => "permissions#stored-rules",
            Self::Sandboxes => "sandboxes#tui-manager",
            Self::Stash => "commands#stash",
            Self::Memory => "memory#the-inspector",
            Self::Tasks => "commands#tasks",
            Self::Shells => "commands#shell-commands",
            Self::Peers => "messaging#find-peers-and-review-messages",
            Self::WorkflowRuns => "workflows#the-inspector",
            Self::WorkflowCatalog => "workflows#run-one",
            Self::Automations => "automations#the-inspector",
            Self::Sessions => "commands#sessions",
            Self::Relocation => "sessions#moving-sessions-to-another-directory",
            Self::Worktrees => "worktrees#the-worktree-picker",
            Self::Review => "review",
            Self::Rewind => "sessions#conversation-revert",
            Self::MessageActions => "sessions#message-actions",
            Self::Queue => "queue",
        }
    }

    /// Every address a modal links to, so the binary that embeds the manual
    /// can check each one resolves.
    pub fn addresses() -> impl Iterator<Item = &'static str> {
        Self::iter().map(Self::address)
    }
}

/// Where the badge sits on `popup`'s top border, or `None` when the popup
/// has no room for it beside its title.
pub(crate) fn badge_area(popup: Rect) -> Option<Rect> {
    if popup.width < MIN_POPUP_WIDTH || popup.height == 0 {
        return None;
    }
    Some(Rect {
        x: popup.right() - BADGE_COLUMNS,
        y: popup.y,
        width: BADGE.width() as u16,
        height: 1,
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(Rect::new(0, 0, MIN_POPUP_WIDTH - 1, 10) ; "too_narrow")]
    #[test_case(Rect::new(0, 0, 40, 0)                    ; "no_rows")]
    fn a_cramped_popup_has_no_badge(popup: Rect) {
        assert_eq!(badge_area(popup), None);
    }

    #[test]
    fn the_badge_ends_short_of_the_corner() {
        let popup = Rect::new(10, 5, 40, 10);
        let badge = badge_area(popup).unwrap();
        assert_eq!(
            (badge.y, badge.right()),
            (popup.y, popup.right() - RIGHT_INSET)
        );
    }
}
