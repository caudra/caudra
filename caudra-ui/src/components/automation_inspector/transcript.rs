//! The transcript row an automation's delivery draws: one header naming the
//! automation, then the text it gave the model. A click anywhere on the row
//! opens the firing that queued it.

use caudra_providers::AutomationEventOrigin;
use ratatui::text::{Line, Span};

use crate::components::code_view::WrappedRows;
use crate::components::escape_terminal_controls;
use crate::components::task_card::markdown_body;
use crate::markdown::LinkMap;
use crate::theme;

pub(crate) const AUTOMATION_LABEL: &str = "Automation ";
pub(crate) const OPEN_FIRING: &str = " \u{b7} open firing";

pub(crate) fn row(
    origin: &AutomationEventOrigin,
    text: &str,
    width: u16,
) -> (Vec<Line<'static>>, LinkMap) {
    let t = theme::current();
    let heading = Line::from(vec![
        Span::styled(AUTOMATION_LABEL, t.tool_dim),
        Span::styled(escape_terminal_controls(&origin.automation), t.tool_prefix),
        Span::styled(OPEN_FIRING, t.accent),
    ]);
    let mut lines = WrappedRows::new(vec![heading], 0, width).lines();
    let mut links = LinkMap::none_for(&lines);
    if !text.trim().is_empty() {
        lines.push(Line::default());
        links.rows.push(Vec::new());
        let (body, body_links) = markdown_body(text.trim(), width);
        lines.extend(body);
        links.rows.extend(body_links.rows);
    }
    (lines, links)
}
