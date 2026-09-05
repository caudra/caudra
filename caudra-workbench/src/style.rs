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
            diff_old: Style::default().fg(Color::Red),
            diff_new: Style::default().fg(Color::Green),
            diff_old_emphasis: Style::default().fg(Color::Red).bg(Color::DarkGray),
            diff_new_emphasis: Style::default().fg(Color::Green).bg(Color::DarkGray),
            diff_line_nr: dim,
            match_highlight: Style::default().fg(Color::Black).bg(Color::Yellow),
            agent_touched: Style::default().fg(Color::Magenta),
        }
    }
}
