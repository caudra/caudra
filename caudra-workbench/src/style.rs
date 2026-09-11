//! Every colour the workbench paints, resolved once by the host.
//!
//! The crate never reads Caudra's theme directly. `caudra-ui` owns the theme
//! and would be a circular dependency, so it builds this struct instead and
//! hands it over whenever the theme generation moves.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Debug, PartialEq)]
pub struct WorkbenchStyles {
    pub background: Style,
    pub text: Style,
    pub dim: Style,
    pub border: Style,
    pub title: Style,
    pub selected: Style,
    /// What the pointer is resting on. Never painted over an already selected
    /// row, so the two are not asked to mean different things at once.
    pub hover: Style,
    pub accent: Style,
    pub directory: Style,
    pub tab_active: Style,
    pub tab_inactive: Style,
    pub gutter: Style,
    pub cursor: Style,
    pub selection: Style,
    pub error: Style,
    pub git_modified: Style,
    pub git_added: Style,
    pub git_deleted: Style,
    pub git_untracked: Style,
    pub git_conflicted: Style,
    pub diff_old: Style,
    pub diff_new: Style,
    pub diff_old_emphasis: Style,
    pub diff_new_emphasis: Style,
    pub diff_line_nr: Style,
    pub match_highlight: Style,
    /// A match on the selected row, which the plain one cannot paint: it carries
    /// no background, so it would punch the selection bar out of the run.
    pub match_highlight_selected: Style,
    /// The match the cursor is on, told apart from the rest of them.
    pub current_match: Style,
    pub agent_touched: Style,
}

impl Default for WorkbenchStyles {
    fn default() -> Self {
        let text = Style::default();
        let dim = Style::default().fg(Color::DarkGray);
        Self {
            background: Style::default(),
            text,
            dim,
            border: dim,
            title: text.add_modifier(Modifier::BOLD),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            hover: Style::default().add_modifier(Modifier::REVERSED),
            accent: Style::default().fg(Color::Cyan),
            directory: Style::default().fg(Color::Blue),
            tab_active: text.add_modifier(Modifier::BOLD),
            tab_inactive: dim,
            gutter: dim,
            cursor: Style::default().add_modifier(Modifier::REVERSED),
            selection: Style::default().bg(Color::DarkGray),
            error: Style::default().fg(Color::Red),
            git_modified: Style::default().fg(Color::Yellow),
            git_added: Style::default().fg(Color::Green),
            git_deleted: Style::default().fg(Color::Red),
            git_untracked: Style::default().fg(Color::Green),
            git_conflicted: Style::default().fg(Color::Red),
            // Backgrounds, not foregrounds: a diff row is a band the syntax
            // colours show through, so a foreground here would erase them.
            diff_old: Style::default().bg(Color::Rgb(0x3d, 0x1c, 0x1c)),
            diff_new: Style::default().bg(Color::Rgb(0x1c, 0x30, 0x20)),
            diff_old_emphasis: Style::default().bg(Color::Rgb(0x5c, 0x2a, 0x2a)),
            diff_new_emphasis: Style::default().bg(Color::Rgb(0x2a, 0x4a, 0x30)),
            diff_line_nr: dim,
            match_highlight: Style::default().fg(Color::Black).bg(Color::Yellow),
            match_highlight_selected: Style::default().fg(Color::Black).bg(Color::Yellow),
            current_match: Style::default().fg(Color::Black).bg(Color::Cyan),
            agent_touched: Style::default().fg(Color::Magenta),
        }
    }
}
