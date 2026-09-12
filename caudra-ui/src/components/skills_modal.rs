use caudra_agent::context::ContextSnapshot;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::skill::{
    self, SkillDirCandidate, SkillDirState, SkillInventoryEntry,
};
use caudra_providers::token_label;
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, Overlay, escape_terminal_controls};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Skills ";
const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 82;
const H_PAD: u16 = 2;
const H_PAD_STEP_WIDTH: u16 = 16;
const LOADED_GLYPH: &str = "\u{25cf}";
const AVAILABLE_GLYPH: &str = "\u{25cb}";
const SUPERSEDED_GLYPH: &str = "\u{d7}";
const NO_SKILLS: &str = "No skills found.";
const DIRECTORIES_SECTION: &str = "Directories";
const SKILLS_SECTION: &str = "Skills";
const NOT_MEASURED: &str = "not measured yet";
const ON_DEMAND: &str = "on demand";

/// The catalog lives on disk, not in the request snapshot, so this view reads
/// the registry when it opens and works before the first request. The snapshot
/// only supplies the token numbers, which nothing can know until then.
pub struct SkillsModal {
    open: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
    inventory: Vec<SkillInventoryEntry>,
    dirs: Vec<SkillDirCandidate>,
}

impl SkillsModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            popup: Rect::default(),
            inventory: Vec::new(),
            dirs: Vec::new(),
        }
    }

    pub fn open(&mut self) {
        self.open = true;
        self.scroll.reset();
        let registry = ToolRegistry::global();
        self.inventory = skill::inventory(registry);
        self.dirs = skill::directories(registry);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.scrollbar = Scrollbar::default();
        self.popup = Rect::default();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if !self.scroll.handle_key(key_event) {
            self.close();
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return;
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::ScrollDown => self.scroll(1),
            _ => {}
        }
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        snapshot: Option<&ContextSnapshot>,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let lines = build_lines(&self.inventory, &self.dirs, snapshot, &theme);
        let content_width = content_width(area);
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(content_width.max(1)))
            .unwrap_or(u16::MAX)
            .min(u16::MAX.saturating_sub(CHROME_LINES));
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let horizontal_padding = horizontal_padding(inner.width);
        let padded = Rect {
            x: inner.x.saturating_add(horizontal_padding),
            width: inner
                .width
                .saturating_sub(horizontal_padding.saturating_mul(2)),
            ..inner
        };
        self.scroll.update_dimensions(total, padded.height);
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().fg(theme.foreground))
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        self.scrollbar.draw(frame, inner, total, offset);

        self.popup = popup;
        popup
    }
}

impl Default for SkillsModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for SkillsModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

fn build_lines(
    inventory: &[SkillInventoryEntry],
    dirs: &[SkillDirCandidate],
    snapshot: Option<&ContextSnapshot>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = summary_lines(inventory, snapshot, theme);
    lines.extend(directory_lines(dirs, theme));
    lines.extend(skill_lines(inventory, snapshot, theme));
    lines
}

fn summary_lines(
    inventory: &[SkillInventoryEntry],
    snapshot: Option<&ContextSnapshot>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let tokens = snapshot.map_or_else(
        || NOT_MEASURED.to_owned(),
        |snapshot| {
            let skills = &snapshot.inventory.skills;
            format!(
                "names and descriptions {} \u{b7} loaded bodies {}",
                token_label(skills.definition_tokens),
                token_label(skills.loaded_tokens)
            )
        },
    );
    vec![
        labeled_line("Available", format!("{}", inventory.len()), theme),
        labeled_line("Tokens", tokens, theme),
    ]
}

/// A superseded directory is the answer to "why is my skill missing", so the
/// ones precedence skipped are listed rather than silently dropped.
fn directory_lines(dirs: &[SkillDirCandidate], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default(), section_line(DIRECTORIES_SECTION, theme)];
    for dir in dirs {
        let (glyph, style) = match dir.state {
            SkillDirState::Selected => (LOADED_GLYPH, theme.tool_success),
            SkillDirState::Superseded => (SUPERSEDED_GLYPH, theme.status_dim),
            SkillDirState::Missing => (AVAILABLE_GLYPH, theme.tool_dim),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(
                escape_terminal_controls(&dir.path.to_string_lossy()),
                theme.tool_path,
            ),
            Span::styled(format!("  {}", dir.state.label()), style),
            Span::styled(format!(" \u{b7} {}", dir.scope.label()), theme.status_dim),
        ]));
    }
    lines
}

fn skill_lines(
    inventory: &[SkillInventoryEntry],
    snapshot: Option<&ContextSnapshot>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default(), section_line(SKILLS_SECTION, theme)];
    if inventory.is_empty() {
        lines.push(Line::from(Span::styled(NO_SKILLS, theme.status_dim)));
        return lines;
    }
    for entry in inventory {
        let loaded = loaded_tokens(snapshot, &entry.name);
        let (glyph, style, state) = match loaded {
            Some(tokens) => (
                LOADED_GLYPH,
                theme.tool_success,
                format!("loaded \u{b7} {}", token_label(tokens)),
            ),
            None => (AVAILABLE_GLYPH, theme.tool_dim, ON_DEMAND.to_owned()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(escape_terminal_controls(&entry.name), theme.accent),
            Span::styled(format!("  {state}"), style),
            Span::styled(format!(" \u{b7} {}", entry.scope.label()), theme.status_dim),
        ]));
        if !entry.description.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", escape_terminal_controls(&entry.description)),
                theme.tool_dim,
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("  {}", escape_terminal_controls(&entry.location)),
            theme.status_dim,
        )));
    }
    lines
}

fn loaded_tokens(snapshot: Option<&ContextSnapshot>, name: &str) -> Option<u32> {
    snapshot?
        .inventory
        .skills
        .skills
        .iter()
        .find(|skill| skill.name == name)
        .map(|skill| skill.loaded_tokens)
        .filter(|tokens| *tokens > 0)
}

fn labeled_line(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), theme.keybind_desc),
        Span::raw(value),
    ])
}

fn section_line(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_owned(), theme.keybind_key))
}

fn content_width(area: Rect) -> u16 {
    let inner = Modal::inner_width(area.width, WIDTH_PERCENT);
    inner.saturating_sub(horizontal_padding(inner).saturating_mul(2))
}

fn horizontal_padding(width: u16) -> u16 {
    if width >= H_PAD_STEP_WIDTH { H_PAD } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_agent::context::{
        ContextInventory, ContextModel, ContextReadiness, ContextReserve, ContextSkill,
        ContextSkillInventory, ContextUsage, ContextWindow,
    };
    use caudra_agent::tools::native::skill::SkillScope;

    use super::{
        ContextSnapshot, NO_SKILLS, NOT_MEASURED, ON_DEMAND, SkillDirCandidate, SkillDirState,
        SkillInventoryEntry, build_lines,
    };
    use crate::theme;

    const LOADED_SKILL: &str = "deploy";
    const UNLOADED_SKILL: &str = "review";
    const CAUDRA_DIR: &str = "/home/u/.config/caudra/skills";
    const CLAUDE_DIR: &str = "/home/u/.claude/skills";
    const PROJECT_DIR: &str = "/repo/.caudra/skills";
    const LOADED_TOKENS: u32 = 420;

    fn entry(name: &str, scope: SkillScope) -> SkillInventoryEntry {
        SkillInventoryEntry {
            name: name.to_owned(),
            description: format!("{name} description"),
            location: format!("{CAUDRA_DIR}/{name}/SKILL.md"),
            scope,
        }
    }

    fn dirs() -> Vec<SkillDirCandidate> {
        vec![
            SkillDirCandidate {
                path: PathBuf::from(CAUDRA_DIR),
                scope: SkillScope::User,
                state: SkillDirState::Selected,
            },
            SkillDirCandidate {
                path: PathBuf::from(CLAUDE_DIR),
                scope: SkillScope::User,
                state: SkillDirState::Superseded,
            },
            SkillDirCandidate {
                path: PathBuf::from(PROJECT_DIR),
                scope: SkillScope::Project,
                state: SkillDirState::Missing,
            },
        ]
    }

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            readiness: ContextReadiness::CapturedCurrentRequest,
            model: ContextModel {
                spec: "test/model".to_owned(),
                provider_display_name: "Test".to_owned(),
            },
            window: ContextWindow {
                tokens: 1_000,
                reserve: ContextReserve::Disabled,
            },
            usage: ContextUsage::default(),
            measured: None,
            inventory: ContextInventory {
                skills: ContextSkillInventory {
                    skills: vec![
                        ContextSkill {
                            name: LOADED_SKILL.to_owned(),
                            description: String::new(),
                            loaded_tokens: LOADED_TOKENS,
                        },
                        ContextSkill {
                            name: UNLOADED_SKILL.to_owned(),
                            description: String::new(),
                            loaded_tokens: 0,
                        },
                    ],
                    definition_tokens: 40,
                    loaded_tokens: LOADED_TOKENS,
                },
                ..ContextInventory::default()
            },
        }
    }

    fn rendered(inventory: &[SkillInventoryEntry], snapshot: Option<&ContextSnapshot>) -> String {
        build_lines(inventory, &dirs(), snapshot, &theme::current())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The superseded directory is the whole point of the section: a skill that
    /// lives there is not missing by accident.
    #[test]
    fn every_candidate_directory_reports_its_state() {
        let out = rendered(&[entry(LOADED_SKILL, SkillScope::User)], None);
        for expected in [
            format!("{CAUDRA_DIR}  selected \u{b7} user"),
            format!("{CLAUDE_DIR}  superseded \u{b7} user"),
            format!("{PROJECT_DIR}  missing \u{b7} project"),
        ] {
            assert!(out.contains(&expected), "missing {expected}:\n{out}");
        }
    }

    #[test]
    fn a_skill_carries_its_scope_and_location() {
        let out = rendered(&[entry(LOADED_SKILL, SkillScope::Project)], None);
        assert!(out.contains("deploy  on demand \u{b7} project"), "{out}");
        assert!(
            out.contains(&format!("{CAUDRA_DIR}/{LOADED_SKILL}/SKILL.md")),
            "{out}"
        );
        assert!(out.contains("deploy description"), "{out}");
    }

    /// Without a request there are no token numbers, but the catalog is on
    /// disk and still renders.
    #[test]
    fn the_report_works_before_the_first_request() {
        let out = rendered(&[entry(LOADED_SKILL, SkillScope::User)], None);
        assert!(out.contains(NOT_MEASURED), "{out}");
        assert!(out.contains(ON_DEMAND), "{out}");
    }

    #[test]
    fn a_snapshot_marks_the_bodies_already_paid_for() {
        let snapshot = snapshot();
        let out = rendered(
            &[
                entry(LOADED_SKILL, SkillScope::User),
                entry(UNLOADED_SKILL, SkillScope::User),
            ],
            Some(&snapshot),
        );
        assert!(out.contains("deploy  loaded \u{b7} "), "{out}");
        assert!(out.contains("review  on demand"), "{out}");
        assert!(!out.contains(NOT_MEASURED), "{out}");
    }

    #[test]
    fn no_skills_says_so_instead_of_rendering_an_empty_section() {
        assert!(rendered(&[], None).contains(NO_SKILLS));
    }
}
