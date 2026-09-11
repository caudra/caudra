//! The seam between Caudra's theme and overlay lifecycle and the workbench.
//!
//! `caudra-workbench` cannot depend on this crate without a cycle, so the
//! translation of theme roles into the palette it paints with lives here.

use caudra_workbench::{Workbench, WorkbenchStyles};
use ratatui::style::{Modifier, Style};

use crate::components::Overlay;
use crate::repaint::Cadence;
use crate::theme;

pub(crate) fn styles() -> WorkbenchStyles {
    let t = theme::current();
    WorkbenchStyles {
        background: t.surface_style(),
        text: Style::new().fg(t.foreground),
        dim: t.status_dim,
        border: t.panel_border,
        title: t.panel_title,
        selected: t.item_selected,
        hover: Style::new().add_modifier(Modifier::REVERSED),
        accent: t.accent,
        directory: t.tool_path,
        tab_active: t.active,
        tab_inactive: t.item_desc,
        gutter: t.code_gutter,
        cursor: t.cursor,
        selection: Style::new().add_modifier(Modifier::REVERSED),
        error: t.error,
        git_modified: t.todo_in_progress,
        git_added: t.diff_new,
        git_deleted: t.diff_old,
        git_untracked: t.diff_new,
        git_conflicted: t.error,
        diff_old: t.diff_old,
        diff_new: t.diff_new,
        diff_old_emphasis: t.diff_old_emphasis,
        diff_new_emphasis: t.diff_new_emphasis,
        diff_line_nr: t.diff_line_nr,
        match_highlight: t.item_match,
        current_match: t.item_selected,
        agent_touched: t.accent,
    }
}

impl Overlay for Workbench {
    fn is_open(&self) -> bool {
        Workbench::is_open(self)
    }

    fn close(&mut self) {
        Workbench::close(self);
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(self.is_busy(), Cadence::PENDING)
    }
}
