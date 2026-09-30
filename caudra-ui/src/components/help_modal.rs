use crate::components::Overlay;
use crate::components::keybindings::{
    ALT_SEP, KEYBINDS, KeyLabel, KeybindContext, all_contexts, key,
};
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{ModalScroll, bar_area};
use crate::theme::{self, Theme};

use caudra_config::FeatureFlags;
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Keybindings ";
const KEY_COL_GAP: usize = 2;
const PREFIX_TOP: &str = "  ";
const PREFIX_CHILD: &str = "    ";
const FOOTER_ROWS: u16 = 1;
const DOCS_COMMAND: &str = "/docs";
const DOCS_HINT: &str = " read the documentation";
const FOOTER_TARGETS: [HelpTarget; 2] = [HelpTarget::Command(DOCS_COMMAND), HelpTarget::Close];

const INPUT_PREFIXES: &[(&str, &str)] = &[
    ("!", "Run shell command (visible to agent)"),
    ("!!", "Run shell command (hidden from agent)"),
];

/// What a footer control does when pressed, indexed the way [`FooterLine`]
/// targets are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelpTarget {
    /// A session command line, left to the host to run.
    Command(&'static str),
    Close,
}

/// What the pointer asked of the host. `Ignored` hands the event back, which
/// is how a sweep through the reference and the sideways wheel still reach it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HelpMouse {
    Ignored,
    Consumed,
    Command(&'static str),
}

pub struct HelpModal {
    open: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    /// A binding's description is as long as it is, so a narrow modal cuts the
    /// half of the reference that says what a key does.
    pan_bar: Scrollbar,
    footer: FooterHits,
    popup: Rect,
}

fn key_spans(label: KeyLabel, pad: usize, prefix: &str) -> Vec<Span<'static>> {
    let theme = theme::current();
    match label {
        KeyLabel::Single(s) => {
            let w = UnicodeWidthStr::width(s);
            let trailing = pad.saturating_sub(w);
            vec![Span::styled(
                format!("{prefix}{s}{:trailing$}", ""),
                theme.keybind_key,
            )]
        }
        KeyLabel::Alt(a, b) => multi_key_spans(&[a, b], pad, prefix, &theme),
        KeyLabel::Multi(keys) => multi_key_spans(keys, pad, prefix, &theme),
    }
}

fn multi_key_spans(
    keys: &[&'static str],
    pad: usize,
    prefix: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let sep_w = UnicodeWidthStr::width(ALT_SEP);
    let content_w: usize = keys
        .iter()
        .map(|k| UnicodeWidthStr::width(*k))
        .sum::<usize>()
        + sep_w * keys.len().saturating_sub(1);
    let trailing = pad.saturating_sub(content_w);
    let mut spans = Vec::with_capacity(keys.len() * 2);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(ALT_SEP, theme.keybind_desc));
        }
        let text = if i == 0 && i == keys.len() - 1 {
            format!("{prefix}{k}{:trailing$}", "")
        } else if i == 0 {
            format!("{prefix}{k}")
        } else if i == keys.len() - 1 {
            format!("{k}{:trailing$}", "")
        } else {
            (*k).to_string()
        };
        spans.push(Span::styled(text, theme.keybind_key));
    }
    spans
}

/// Pinned under the reference rather than ending it, so it never pans away
/// with a description or waits at the bottom of a long scroll.
fn footer(theme: &Theme) -> FooterLine {
    let mut footer = FooterLine::default();
    footer.command(DOCS_COMMAND, theme.keybind_key);
    footer.describe(DOCS_HINT, theme.tool_dim);
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(ESC_LABEL, theme.keybind_key);
    footer.describe(CLOSE_HINT, theme.tool_dim);
    footer
}

impl HelpModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            footer: FooterHits::default(),
            popup: Rect::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.scroll.reset();
        self.footer.reset();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
        self.footer.reset();
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, target: HelpTarget) -> Rect {
        FOOTER_TARGETS
            .iter()
            .position(|candidate| *candidate == target)
            .map(|index| self.footer.hit(index))
            .unwrap_or_default()
    }

    /// The footer is asked first: on a touch screen the pan bar's margin
    /// reaches up over it, and a press on a control is never the start of a
    /// sweep. The two bars sit on different rows, so at most one of them
    /// answers a press.
    pub(crate) fn handle_mouse(&mut self, event: &MouseEvent) -> HelpMouse {
        if !self.open {
            return HelpMouse::Ignored;
        }
        if let Some(index) = self.footer.handle_mouse(*event) {
            return match FOOTER_TARGETS.get(index).copied() {
                Some(HelpTarget::Command(cmdline)) => HelpMouse::Command(cmdline),
                Some(HelpTarget::Close) => {
                    self.close();
                    HelpMouse::Consumed
                }
                None => HelpMouse::Consumed,
            };
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && self.footer.hovered().is_some()
        {
            return HelpMouse::Consumed;
        }
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return HelpMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return HelpMouse::Consumed;
            }
        }
        match self.pan_bar.handle(event) {
            ScrollbarMouse::Ignored => HelpMouse::Ignored,
            ScrollbarMouse::Consumed => HelpMouse::Consumed,
            ScrollbarMouse::ScrollTo(column) => {
                self.scroll.pan_to(column as u16);
                HelpMouse::Consumed
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// A sideways wheel over the modal, which the app routes here rather than
    /// dropping now that the content can run off the edge.
    pub fn pan(&mut self, delta: i32) {
        self.scroll.pan_by(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        let close = key_event.code == KeyCode::Esc
            || key::HELP.matches(key_event)
            || key::QUIT.matches(key_event);
        if close {
            self.close();
            return true;
        }
        self.scroll.handle_key(key_event);
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, features: FeatureFlags) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("help_modal", area);

        let mut lines: Vec<Line> = Vec::new();
        let theme = theme::current();

        let key_col_width = KEYBINDS
            .iter()
            .filter(|kb| kb.is_visible(features))
            .map(|kb| kb.label.display_width())
            .max()
            .unwrap_or(0)
            + KEY_COL_GAP;

        let mut first = true;
        for ctx in all_contexts() {
            if ctx.parent().is_some() || ctx.feature().is_some_and(|f| !features.enabled(f)) {
                continue;
            }
            if !first {
                lines.push(Line::default());
            }
            first = false;

            lines.push(Line::from(Span::styled(
                format!("  {}", ctx.label()),
                theme.keybind_section,
            )));

            for kb in KEYBINDS
                .iter()
                .filter(|kb| kb.context == ctx && kb.is_visible(features))
            {
                let mut spans = key_spans(kb.label, key_col_width, PREFIX_TOP);
                spans.push(Span::styled(kb.description, theme.keybind_desc));
                lines.push(Line::from(spans));
            }

            for child in all_contexts() {
                if child.parent() != Some(ctx) {
                    continue;
                }
                let child_binds: Vec<_> = KEYBINDS
                    .iter()
                    .filter(|kb| kb.context == child && kb.is_visible(features))
                    .collect();
                if child_binds.is_empty() {
                    continue;
                }
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!("    {}", child.label()),
                    theme.keybind_section,
                )));
                for kb in child_binds {
                    let mut spans = key_spans(kb.label, key_col_width - KEY_COL_GAP, PREFIX_CHILD);
                    spans.push(Span::styled(kb.description, theme.keybind_desc));
                    lines.push(Line::from(spans));
                }
            }

            if ctx == KeybindContext::Editing {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "    Input Prefixes",
                    theme.keybind_section,
                )));
                for &(pfx, desc) in INPUT_PREFIXES {
                    let mut spans = key_spans(
                        KeyLabel::Single(pfx),
                        key_col_width - KEY_COL_GAP,
                        PREFIX_CHILD,
                    );
                    spans.push(Span::styled(desc, theme.keybind_desc));
                    lines.push(Line::from(spans));
                }
            }
        }

        let total = lines.len() as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: 50,
            max_height_percent: 80,
        };
        let (popup, inner) = modal.render(frame, area, total.saturating_add(FOOTER_ROWS));
        let body = Rect {
            height: inner.height.saturating_sub(FOOTER_ROWS),
            ..inner
        };
        let footer_area = Rect {
            y: body.bottom(),
            height: inner.height.min(FOOTER_ROWS),
            ..inner
        };
        let content_w = lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX);
        self.scroll.update_dimensions(total, body.height);
        self.scroll.fit_width(content_w, body.width);
        let scroll = self.scroll.offset();
        let pan = self.scroll.pan();

        frame.render_widget(Paragraph::new(lines).scroll((scroll, pan)), body);

        self.scrollbar.draw(frame, body, total, scroll);
        // Handed the reference, the footer and the bottom border: the bar paints
        // on the last row, which is the only row it can have without taking one
        // from the reference, and a fingertip's hit margin comes out of the rows
        // over it. Nothing is painted while the rows fit, because a track is only
        // built for content that overflows.
        self.pan_bar.draw(frame, bar_area(inner), content_w, pan);

        let footer = footer(&theme);
        self.footer.set(footer.hits(footer_area, 0, FOOTER_ROWS));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer.hovered())),
            footer_area,
        );

        self.popup = popup;
        popup
    }
}

impl Overlay for HelpModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{buffer_text, key as key_ev};
    use caudra_workbench::scroll::SCROLLBAR_THUMB_HORIZONTAL;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use test_case::test_case;

    /// A small screen: the modal fills its floor and the reference still runs
    /// past it.
    const NARROW_TERMINAL: u16 = 80;
    /// Wide enough that every row fits inside the modal.
    const WIDE_TERMINAL: u16 = 300;
    const TERMINAL_HEIGHT: u16 = 40;
    /// The widest description on the reference's first screenful. A narrow modal
    /// shows it as far as `expanded transcr` and cuts the rest.
    const CUT_DESCRIPTION: &str = "expanded transcript";
    const DESCRIPTION_UNREACHABLE: &str = "panning must bring the description into view";
    const BAR_UNWANTED: &str = "a reference that fits must not wear a pan bar";
    const BAR_MISSING: &str = "a row running off the edge must show what reaches it";
    const CONTROL_MISSING: &str = "the footer must draw the control where a pointer reaches it";
    const HOVER_MISSED: &str = "the whole phrase, and only it, must reverse under the pointer";
    const PRESS_LEAKED: &str = "a press on a control is the modal's, not the start of a sweep";

    /// Key and gloss mark as one phrase, and a click answers with what the
    /// control names: the host runs `/docs`, and `Esc` closes the modal here.
    #[test_case(HelpTarget::Command(DOCS_COMMAND), HelpMouse::Command(DOCS_COMMAND), true ; "docs_is_left_to_the_host")]
    #[test_case(HelpTarget::Close, HelpMouse::Consumed, false ; "close_closes")]
    fn a_footer_control_hovers_as_a_phrase_and_answers_a_click(
        target: HelpTarget,
        answer: HelpMouse,
        open_after: bool,
    ) {
        let mut modal = HelpModal::new();
        modal.toggle();
        draw(&mut modal, NARROW_TERMINAL);
        let hit = modal.footer_hit(target);
        assert!(!hit.is_empty(), "{CONTROL_MISSING}");

        modal.handle_mouse(&mouse(MouseEventKind::Moved, hit));
        let buffer = draw(&mut modal, NARROW_TERMINAL);
        let reversed: Vec<Position> = buffer
            .area
            .positions()
            .filter(|at| buffer[*at].modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(
            reversed.len() == usize::from(hit.width) && reversed.iter().all(|at| hit.contains(*at)),
            "{HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        let press = modal.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), hit));
        assert_eq!(press, HelpMouse::Consumed, "{PRESS_LEAKED}");
        let release = modal.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), hit));
        assert_eq!(release, answer);
        assert_eq!(modal.is_open(), open_after);
    }

    #[test_case(key_ev(KeyCode::Esc)       ; "esc_closes")]
    #[test_case(key::QUIT.to_key_event()    ; "ctrl_c_closes")]
    #[test_case(key::HELP.to_key_event()    ; "f1_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = HelpModal::new();
        modal.toggle();
        assert!(modal.handle_key(k));
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_consumes_all() {
        let mut modal = HelpModal::new();
        modal.toggle();
        assert!(modal.handle_key(key_ev(KeyCode::Char('a'))));
        assert!(modal.is_open());
    }

    /// A description is as long as it is, so on a small screen the half of the
    /// reference that says what a key does is off the edge.
    #[test]
    fn a_description_cut_by_a_narrow_screen_is_reachable_by_panning() {
        let mut modal = HelpModal::new();
        modal.toggle();

        let clipped = render_at(&mut modal, NARROW_TERMINAL);
        assert!(
            !clipped.contains(CUT_DESCRIPTION),
            "the description should start off the edge"
        );

        modal.handle_key(key::PAN_RIGHT.to_key_event());

        assert!(
            render_at(&mut modal, NARROW_TERMINAL).contains(CUT_DESCRIPTION),
            "{DESCRIPTION_UNREACHABLE}"
        );
    }

    /// Enough rows fit that the widest one always runs past a narrow modal, and
    /// never past a wide one.
    #[test]
    fn the_pan_bar_shows_only_while_a_row_runs_past_the_modal() {
        let mut modal = HelpModal::new();
        modal.toggle();

        assert!(
            render_at(&mut modal, NARROW_TERMINAL).contains(SCROLLBAR_THUMB_HORIZONTAL),
            "{BAR_MISSING}"
        );
        assert!(
            !render_at(&mut modal, WIDE_TERMINAL).contains(SCROLLBAR_THUMB_HORIZONTAL),
            "{BAR_UNWANTED}"
        );
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw(modal: &mut HelpModal, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, TERMINAL_HEIGHT)).unwrap();
        terminal
            .draw(|f| {
                modal.view(f, f.area(), FeatureFlags::all());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_at(modal: &mut HelpModal, width: u16) -> String {
        buffer_text(&draw(modal, width))
    }
}
