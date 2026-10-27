use crate::components::keybindings::key;
use crate::repaint::Cadence;
use crate::theme::{self, lerp_u8};
use crate::update;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::time::{Duration, Instant};

const LOGO: &str = "caudra";
const TAGLINE: &str = "the complete coding agent";
const HELP_SEGMENTS: &[(&str, bool)] = &[
    (key::HELP.label, true),
    (" help", false),
    (" · ", false),
    ("/help", true),
    (" or ", false),
    ("/docs", true),
    (" in chat", false),
];

const TIPS: &[(&str, &str)] = &[
    (
        key::FILE_PICKER.label,
        "to grab file paths with fuzzy search",
    ),
    ("/tasks", "to see what your subagents are up to"),
    (key::SEARCH.label, "to find things in the conversation"),
    (
        key::ASK_BTW.label,
        "to ask something without interrupting the session",
    ),
    ("/memory", "to view, edit, and delete persistent notes"),
    ("/cd", "to switch to a different directory"),
];

const COLOR_TRANSITION_SECS: f32 = 0.4;

/// Seconds for the initial fade-in animation (ease-out cubic).
const FADE_DURATION: f32 = 1.6;
/// Seconds to wait before the logo starts appearing.
const LOGO_DELAY: f32 = 0.2;
/// Seconds over which the logo fades from dim to full brightness.
const LOGO_RAMP: f32 = 0.8;
/// Rows the centered block of logo, tagline, help and tip occupies.
const TEXT_BLOCK_HEIGHT: u16 = 8;
/// Smallest area worth drawing a splash into.
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 5;

/// The mark is a C lit on a five by five grid of cells, with a cursor cell in
/// its opening. These are the cells of the C as (column, row), grouped in the
/// order a firing signal reaches them: from the head at the top right to the
/// tail at the bottom right, two at once where the C is two cells thick.
const FIRING_PATH: [&[(usize, usize)]; 9] = [
    &[(3, 0)],
    &[(2, 0)],
    &[(1, 0)],
    &[(1, 1), (0, 1)],
    &[(0, 2), (1, 2)],
    &[(0, 3), (1, 3)],
    &[(1, 4)],
    &[(2, 4)],
    &[(3, 4)],
];
const CURSOR_CELL: (usize, usize) = (3, 2);
const GRID: usize = 5;
/// Cells are six times as wide as the gaps between them.
const CELL_TO_GAP: f32 = 6.0;
/// Share of the area the face-on mark may span, across whichever is smaller
/// of the area's width and twice its height, since a row is about twice as
/// tall as a column is wide. The rest leaves room for the sway, which can
/// reach about a fifth further than face-on.
const MARK_SHARE: f32 = 0.75;
/// Peak turn in radians and rate in radians per second about the vertical
/// axis, the horizontal axis, and the line of sight. The periods share no
/// common multiple worth noticing, so the sway never visibly repeats.
const SWAY: [(f32, f32); 3] = [(0.26, 0.47), (0.17, 0.61), (0.05, 0.37)];
/// Seconds over which the sway grows to its full swing, so the first firing
/// plays face-on.
const SWAY_RAMP: f32 = 4.0;
/// Viewer distance in half widths of the mark. Lower means stronger
/// perspective.
const FOCAL: f32 = 3.0;
/// Opacity the mark loses per half width it leans away from the viewer, and
/// gains as it leans closer.
const DEPTH_DIM: f32 = 0.6;
/// How far each part of the mark is mixed from the background toward the
/// foreground, for the grid and the C, or toward the accent, for the pulse and
/// the cursor. Faint, because the text is drawn over the mark.
const UNLIT_ALPHA: f32 = 0.04;
const LIT_ALPHA: f32 = 0.12;
const PULSE_ALPHA: f32 = 0.5;
const CURSOR_ALPHA: f32 = 0.3;
/// Firing timing in seconds, shared with the website's animated mark: after a
/// rest the signal lights the C a step at a time, each step flashing as it
/// lights, and after a pause the cursor fires and blinks before it stays on.
/// The fired mark holds, fades back to rest, and fires again.
const FIRE_REST: f32 = 0.35;
const FIRE_STEP: f32 = 0.085;
const FIRE_FLASH: f32 = 0.16;
const FIRE_PAUSE: f32 = 0.22;
const BLINK: f32 = 0.53;
/// Times the cursor shows or hides before it stays on.
const BLINK_PHASES: u32 = 4;
const FIRE_FADE: f32 = 0.45;
/// Seconds from one firing to the next. The fired mark holds for most of it,
/// so behind the text a firing reads as an event rather than a flicker.
const FIRE_LOOP: f32 = 8.0;
const CURSOR_FIRES_AT: f32 = FIRE_REST + FIRING_PATH.len() as f32 * FIRE_STEP + FIRE_PAUSE;
const FIRING_SETTLES_AT: f32 = CURSOR_FIRES_AT + BLINK_PHASES as f32 * BLINK;
/// Splits a cell into two square pixels: the glyph is the upper one, in the
/// foreground, and the background shows below it.
const UPPER_HALF: char = '▀';

const BG_FALLBACK: (u8, u8, u8) = (15, 15, 25);
const FG_FALLBACK: (u8, u8, u8) = (200, 200, 200);
const ACCENT_FALLBACK: (u8, u8, u8) = (100, 140, 255);
const TIP_FALLBACK: (u8, u8, u8) = (249, 226, 175);

type Rgb = [f32; 3];

pub struct ColorTransition {
    from: (u8, u8, u8),
    to: (u8, u8, u8),
    start: Instant,
}

impl ColorTransition {
    pub fn new(color: Color) -> Self {
        let rgb = extract_rgb(color, ACCENT_FALLBACK);
        Self {
            from: rgb,
            to: rgb,
            start: Instant::now() - Duration::from_secs_f32(COLOR_TRANSITION_SECS),
        }
    }

    pub fn set(&mut self, color: Color) {
        let rgb = extract_rgb(color, ACCENT_FALLBACK);
        if rgb == self.to {
            return;
        }
        let now = Instant::now();
        self.from = self.resolve_rgb(now);
        self.to = rgb;
        self.start = now;
    }

    pub fn is_animating(&self) -> bool {
        Instant::now().duration_since(self.start).as_secs_f32() < COLOR_TRANSITION_SECS
    }

    pub fn resolve(&self) -> Color {
        let (r, g, b) = self.resolve_rgb(Instant::now());
        Color::Rgb(r, g, b)
    }

    fn resolve_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let t = (now.duration_since(self.start).as_secs_f32() / COLOR_TRANSITION_SECS).min(1.0);
        let p = ease_out_cubic(t);
        (
            lerp_u8(self.from.0, self.to.0, p),
            lerp_u8(self.from.1, self.to.1, p),
            lerp_u8(self.from.2, self.to.2, p),
        )
    }
}

pub struct Splash {
    start: Instant,
    animate: bool,
    tip_idx: usize,
}

impl Default for Splash {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Splash {
    pub fn new(animate: bool) -> Self {
        let mut seed = [0u8; 8];
        getrandom::fill(&mut seed).ok();
        Self {
            start: Instant::now(),
            animate,
            tip_idx: u64::from_le_bytes(seed) as usize % TIPS.len(),
        }
    }

    /// Still and past its entry fade, so every frame paints the same colours
    /// however long the test took to reach it.
    #[cfg(test)]
    pub fn settled() -> Self {
        let mut splash = Self::new(false);
        splash.start -= Duration::from_secs_f32(FADE_DURATION);
        splash
    }

    /// The mark keeps swaying for as long as the splash is up. With the
    /// animation off the only motion left is the entry fade, which ends, so
    /// the loop settles on the start screen instead of burning a core on a
    /// still picture.
    pub fn cadence(&self) -> Cadence {
        Cadence::when(
            self.animate || self.start.elapsed().as_secs_f32() < FADE_DURATION,
            Cadence::SMOOTH,
        )
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, accent: Color) {
        self.render_at(area, buf, self.start.elapsed().as_secs_f32(), accent);
    }

    /// Every frame is a function of elapsed time alone, so the same `t` always
    /// paints the same picture and nothing has to be carried between frames.
    fn render_at(&self, area: Rect, buf: &mut Buffer, t: f32, accent: Color) {
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return;
        }

        let fade = if t >= FADE_DURATION {
            1.0
        } else {
            ease_out_cubic(t / FADE_DURATION)
        };

        let top_y = area.y + area.height.saturating_sub(TEXT_BLOCK_HEIGHT) / 2;
        let tag_y = top_y + 1;
        let help_y = tag_y + 2;
        let tip_y = help_y + 2;
        let logo_x = area.x + area.width.saturating_sub(LOGO.len() as u16) / 2;

        self.render_mark(area, buf, t, fade, accent);
        self.render_logo(area, buf, t, fade, (logo_x, top_y), accent);
        render_centered_faded(area, buf, fade, 0.75, tag_y, TAGLINE);
        self.render_help(area, buf, fade, help_y, accent);
        self.render_tip(area, buf, fade, tip_y, accent);
        render_version(area, buf, fade, area.y);
    }

    /// The mark fills the middle of the area, behind the text. Animated, it
    /// sways about all three axes and fires on a loop. Still, it shows the
    /// fired mark face-on.
    fn render_mark(&self, area: Rect, buf: &mut Buffer, t: f32, fade: f32, accent: Color) {
        let theme = theme::current();
        let background = channels(theme.background, BG_FALLBACK);
        let foreground = channels(theme.foreground, FG_FALLBACK);
        let accent = channels(accent, ACCENT_FALLBACK);
        let tint = |target, alpha: f32| mix(background, target, alpha * fade);
        let palette = Palette {
            unlit: tint(foreground, UNLIT_ALPHA),
            lit: tint(foreground, LIT_ALPHA),
            pulse: tint(accent, PULSE_ALPHA),
            cursor: tint(accent, CURSOR_ALPHA),
        };
        let (cells, view) = if self.animate {
            (palette.cells_at(t), View::at(t))
        } else {
            (
                palette.cells_at(FIRING_SETTLES_AT),
                View::turned_by([0.0; 3]),
            )
        };
        paint_mark(area, buf, &cells, &view, background);
    }

    fn render_logo(
        &self,
        area: Rect,
        buf: &mut Buffer,
        t: f32,
        fade: f32,
        at: (u16, u16),
        accent: Color,
    ) {
        let (logo_x, top_y) = at;
        let (ac_r, ac_g, ac_b) = extract_rgb(accent, ACCENT_FALLBACK);
        let (bg_r, bg_g, bg_b) = extract_rgb(theme::current().background, BG_FALLBACK);

        let alpha = 0.85 * ease_out_cubic(((t - LOGO_DELAY) / LOGO_RAMP).clamp(0.0, 1.0)) * fade;
        let style = Style::new()
            .fg(Color::Rgb(
                lerp_u8(bg_r, ac_r, alpha),
                lerp_u8(bg_g, ac_g, alpha),
                lerp_u8(bg_b, ac_b.saturating_add(15), alpha),
            ))
            .add_modifier(Modifier::BOLD);

        for (col, ch) in LOGO.chars().enumerate() {
            let x = logo_x + col as u16;
            if x >= area.x + area.width || top_y >= area.y + area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, top_y)) {
                cell.set_char(ch).set_style(style);
            }
        }
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer, fade: f32, help_y: u16, accent: Color) {
        if help_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let ac = extract_rgb(accent, ACCENT_FALLBACK);
        let fg = extract_rgb(theme.foreground, FG_FALLBACK);
        let bg = extract_rgb(theme.background, BG_FALLBACK);

        let total_width: u16 = HELP_SEGMENTS
            .iter()
            .map(|(s, _)| s.chars().count() as u16)
            .sum();
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: Vec<_> = HELP_SEGMENTS
            .iter()
            .map(|&(text, highlighted)| {
                let (target, alpha) = if highlighted { (ac, 0.75) } else { (fg, 0.5) };
                (text, faded_style(bg, target, alpha * fade))
            })
            .collect();

        render_segments(area, buf, help_y, x_start, &segments);
    }

    fn render_tip(&self, area: Rect, buf: &mut Buffer, fade: f32, tip_y: u16, accent: Color) {
        if tip_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let tip_rgb = extract_rgb(
            theme.todo_in_progress.fg.unwrap_or(Color::Yellow),
            TIP_FALLBACK,
        );
        let ac = extract_rgb(accent, ACCENT_FALLBACK);
        let fg = extract_rgb(theme.foreground, FG_FALLBACK);
        let bg = extract_rgb(theme.background, BG_FALLBACK);

        let (label, desc) = TIPS[self.tip_idx];
        let total_width = (5 + label.len() + 1 + desc.len()) as u16;
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: &[(&str, Style)] = &[
            (
                "tip: ",
                faded_style(bg, tip_rgb, 0.75 * fade).add_modifier(Modifier::BOLD),
            ),
            (label, faded_style(bg, ac, 0.75 * fade)),
            (" ", Style::default()),
            (desc, faded_style(bg, fg, 0.5 * fade)),
        ];

        render_segments(area, buf, tip_y, x_start, segments);
    }
}

/// What each part of the mark looks like where it covers a whole pixel,
/// already faded in.
struct Palette {
    unlit: Rgb,
    lit: Rgb,
    pulse: Rgb,
    cursor: Rgb,
}

impl Palette {
    /// Every cell's colour `t` seconds in. A cell of the C flashes as the
    /// signal reaches it, the cursor blinks once it fires, and at the end of
    /// each loop the whole mark fades back to rest.
    fn cells_at(&self, t: f32) -> [[Rgb; GRID]; GRID] {
        let t = t.rem_euclid(FIRE_LOOP);
        let shown = ((FIRE_LOOP - t) / FIRE_FADE).min(1.0);
        let fired = |color| mix(self.unlit, color, shown);
        let mut cells = [[self.unlit; GRID]; GRID];
        for (step, group) in FIRING_PATH.iter().enumerate() {
            let since = t - FIRE_REST - step as f32 * FIRE_STEP;
            if since < 0.0 {
                break;
            }
            let color = fired(mix(self.pulse, self.lit, (since / FIRE_FLASH).min(1.0)));
            for &(col, row) in *group {
                cells[row][col] = color;
            }
        }
        let phase = (t - CURSOR_FIRES_AT) / BLINK;
        if phase >= 0.0 && (phase as u32 >= BLINK_PHASES || (phase as u32).is_multiple_of(2)) {
            cells[CURSOR_CELL.1][CURSOR_CELL.0] = fired(self.cursor);
        }
        cells
    }
}

/// The mark's plane after the sway: its two axes and the way it faces, in
/// screen space with the viewer looking down the third axis.
struct View {
    across: [f32; 3],
    down: [f32; 3],
    facing: [f32; 3],
}

impl View {
    /// The sway runs on the splash's own clock rather than the firing loop's,
    /// so a new firing never jerks the mark back to face-on.
    fn at(t: f32) -> Self {
        let swing = smoothstep(t / SWAY_RAMP);
        Self::turned_by(SWAY.map(|(peak, rate)| peak * swing * (rate * t).sin()))
    }

    fn turned_by(turn: [f32; 3]) -> Self {
        let [across, down, facing] =
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].map(|axis| rotate(axis, turn));
        Self {
            across,
            down,
            facing,
        }
    }

    /// Where the sight line through a point of the screen meets the mark, in
    /// the mark's own coordinates, and how far behind the screen it does.
    fn project(&self, x: f32, y: f32) -> (f32, f32, f32) {
        let reach = self.facing[2] * FOCAL / dot(self.facing, [x, y, FOCAL]);
        let hit = [reach * x, reach * y, (reach - 1.0) * FOCAL];
        (dot(self.across, hit), dot(self.down, hit), hit[2])
    }
}

/// Paints the mark as `view` poses it, centred in `area`, with each terminal
/// cell split into two square pixels. Cells the mark leaves bare keep
/// whatever is already there.
fn paint_mark(
    area: Rect,
    buf: &mut Buffer,
    cells: &[[Rgb; GRID]; GRID],
    view: &View,
    background: Rgb,
) {
    let blank = to_color(background);
    let layout = Layout::of(area);
    let ([mid_x, mid_y], scale) = (layout.centre, layout.half_width);
    let pixel = |x: f32, y: f32| {
        let (across, down, depth) = view.project((x - mid_x) / scale, (y - mid_y) / scale);
        let Some(((col, row), inside)) = layout.cell_at(across, down) else {
            return blank;
        };
        let coverage = (inside * scale + 0.5).clamp(0.0, 1.0);
        to_color(mix(
            background,
            cells[row][col],
            coverage * (1.0 - DEPTH_DIM * depth),
        ))
    };

    for row in 0..area.height {
        for col in 0..area.width {
            let (x, y) = (f32::from(col) + 0.5, f32::from(row) * 2.0 + 0.5);
            let (upper, lower) = (pixel(x, y), pixel(x, y + 1.0));
            if upper == blank && lower == blank {
                continue;
            }
            let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            if upper == lower {
                cell.set_char(' ').set_bg(lower);
            } else {
                cell.set_char(UPPER_HALF).set_fg(upper).set_bg(lower);
            }
        }
    }
}

/// `v` turned about the vertical axis, then the horizontal axis, then the
/// line of sight.
fn rotate([x, y, z]: [f32; 3], [yaw, pitch, roll]: [f32; 3]) -> [f32; 3] {
    let (sin, cos) = yaw.sin_cos();
    let (x, z) = (x * cos + z * sin, z * cos - x * sin);
    let (sin, cos) = pitch.sin_cos();
    let (y, z) = (y * cos - z * sin, y * sin + z * cos);
    let (sin, cos) = roll.sin_cos();
    [x * cos - y * sin, x * sin + y * cos, z]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Where the face-on mark sits in an area, in pixels a column wide and half a
/// row tall, and the size of its cells in half widths of the mark.
struct Layout {
    centre: [f32; 2],
    half_width: f32,
    pitch: f32,
    cell_half: f32,
}

impl Layout {
    /// Cells, gaps and the mark's corner fall on whole pixels, so a mark at
    /// rest has crisp edges and every gap shows, however small the area.
    fn of(area: Rect) -> Self {
        let room = [area.width, area.height.saturating_mul(2)].map(f32::from);
        let span = MARK_SHARE * room[0].min(room[1]);
        let gaps = (GRID - 1) as f32;
        let gap = (span / (GRID as f32 * CELL_TO_GAP + gaps)).round().max(1.0);
        let cell = ((span - gaps * gap) / GRID as f32).floor().max(1.0);
        let half_width = (GRID as f32 * cell + gaps * gap) / 2.0;
        Self {
            centre: room.map(|room| (room / 2.0 - half_width).floor() + half_width),
            half_width,
            pitch: (cell + gap) / half_width,
            cell_half: cell / 2.0 / half_width,
        }
    }

    /// The cell a point of the mark falls in, and how far inside that cell's
    /// square the point lies, negative outside, in half widths of the mark.
    fn cell_at(&self, across: f32, down: f32) -> Option<((usize, usize), f32)> {
        let (col, off_col) = self.cell_axis(across)?;
        let (row, off_row) = self.cell_axis(down)?;
        Some(((col, row), self.cell_half - off_col.max(off_row)))
    }

    /// The cell whose share of the grid a coordinate falls in along one axis,
    /// and how far the coordinate is from that cell's centre line.
    fn cell_axis(&self, at: f32) -> Option<(usize, f32)> {
        let pitches = at / self.pitch + GRID as f32 / 2.0;
        let index = pitches.floor();
        (0.0..GRID as f32)
            .contains(&index)
            .then(|| (index as usize, (pitches - index - 0.5).abs() * self.pitch))
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn render_version(area: Rect, buf: &mut Buffer, fade: f32, y: u16) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let text = format!("v{}", update::CURRENT);
    let style = faded_style(
        extract_rgb(theme.background, BG_FALLBACK),
        extract_rgb(theme.foreground, FG_FALLBACK),
        0.4 * fade,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16 + 1);
    render_segments(area, buf, y, x_start, &[(&text, style)]);
}

fn render_centered_faded(
    area: Rect,
    buf: &mut Buffer,
    fade: f32,
    intensity: f32,
    y: u16,
    text: &str,
) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let style = faded_style(
        extract_rgb(theme.background, BG_FALLBACK),
        extract_rgb(theme.foreground, FG_FALLBACK),
        intensity * fade,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16) / 2;
    render_segments(area, buf, y, x_start, &[(text, style)]);
}

fn extract_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => fallback,
    }
}

fn channels(color: Color, fallback: (u8, u8, u8)) -> Rgb {
    let (r, g, b) = extract_rgb(color, fallback);
    [r, g, b].map(f32::from)
}

fn mix(from: Rgb, to: Rgb, amount: f32) -> Rgb {
    std::array::from_fn(|i| from[i] + (to[i] - from[i]) * amount)
}

/// Rounds, so a mix a hair off a whole colour still lands on it. Float to
/// integer casts saturate, so a mix pushed past either end clamps.
fn to_color(rgb: Rgb) -> Color {
    let [r, g, b] = rgb.map(|channel| channel.round() as u8);
    Color::Rgb(r, g, b)
}

/// Text sets only its foreground, so it sits on the mark rather than cutting
/// a hole in it.
fn faded_style(bg: (u8, u8, u8), fg: (u8, u8, u8), alpha: f32) -> Style {
    Style::new().fg(Color::Rgb(
        lerp_u8(bg.0, fg.0, alpha),
        lerp_u8(bg.1, fg.1, alpha),
        lerp_u8(bg.2, fg.2, alpha),
    ))
}

fn render_segments(area: Rect, buf: &mut Buffer, y: u16, x_start: u16, segments: &[(&str, Style)]) {
    let x_end = area.x + area.width;
    let mut x = x_start;
    for &(text, style) in segments {
        for ch in text.chars() {
            if x >= x_end {
                return;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch).set_style(style);
            }
            x += 1;
        }
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::buffer_text;
    use std::time::Duration;
    use test_case::test_case;

    fn transition_at(from: (u8, u8, u8), to: (u8, u8, u8), offset: Duration) -> (u8, u8, u8) {
        let mut ct = ColorTransition::new(Color::Rgb(from.0, from.1, from.2));
        ct.set(Color::Rgb(to.0, to.1, to.2));
        ct.resolve_rgb(ct.start + offset)
    }

    #[test]
    fn interpolation_over_time() {
        let start = transition_at((0, 0, 0), FG_FALLBACK, Duration::ZERO);
        assert_eq!(start, (0, 0, 0));

        let mid = transition_at((0, 0, 0), FG_FALLBACK, Duration::from_millis(200));
        assert!(
            mid.0 > 0 && mid.0 < FG_FALLBACK.0,
            "expected interpolated, got {}",
            mid.0
        );

        let done = transition_at((0, 0, 0), (255, 255, 255), Duration::from_millis(500));
        assert_eq!(done, (255, 255, 255));
    }

    #[test]
    fn chained_set_restarts_toward_new_target() {
        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(200, 100, 50));
        ct.set(Color::Rgb(10, 20, 30));

        let done = ct.resolve_rgb(ct.start + Duration::from_secs(1));
        assert_eq!(done, (10, 20, 30));
    }

    const VERSION_UNSHOWN: &str = "the start screen must name the installed version";
    const UPDATE_DUPLICATED: &str = "the update banner, not the start screen, announces releases";
    const UPDATE_WORD: &str = "update";

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 20,
    };
    const TALL: Rect = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    };
    /// Far from the foreground, so the cursor never looks like the C.
    const ACCENT: Color = Color::Rgb(240, 68, 47);
    /// The second firing has just settled, well past the entry fade and the
    /// sway's ramp, and the mark holds still until the loop ends.
    const SETTLED_SECS: f32 = FIRE_LOOP + FIRING_SETTLES_AT;

    fn painted(splash: &Splash, t: f32) -> Buffer {
        let mut buf = Buffer::empty(AREA);
        splash.render_at(AREA, &mut buf, t, ACCENT);
        buf
    }

    #[test]
    fn the_start_screen_names_the_installed_version_alone() {
        let screen = buffer_text(&painted(&Splash::new(false), SETTLED_SECS));
        assert!(
            screen.contains(&format!("v{}", update::CURRENT)),
            "{VERSION_UNSHOWN}"
        );
        assert!(
            !screen.to_lowercase().contains(UPDATE_WORD),
            "{UPDATE_DUPLICATED}"
        );
    }

    const DOCS_COMMAND: &str = "/docs";
    const DOCS_UNNAMED: &str = "the start screen must name /docs beside /help";
    const OFF_CENTRE: &str = "the help line must sit as far from one edge as the other";

    /// `·` is two bytes and one cell, so a width counted in bytes pushes the
    /// line a column to the left of the tagline above it. The blanks and half
    /// blocks either side belong to the mark behind it.
    #[test]
    fn the_help_line_names_the_docs_and_sits_centred() {
        let buf = painted(&Splash::new(false), SETTLED_SECS);
        let row = (AREA.y..AREA.bottom())
            .map(|y| {
                (AREA.x..AREA.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|row| row.contains(DOCS_COMMAND))
            .expect(DOCS_UNNAMED);
        let behind = |ch: &char| *ch == ' ' || *ch == UPPER_HALF;
        let left = row.chars().take_while(behind).count();
        let right = row.chars().rev().take_while(behind).count();
        assert!(left.abs_diff(right) <= 1, "{OFF_CENTRE}: {row:?}");
    }

    /// `splash_animation = false` is what a user on a slow machine reaches
    /// for, so the start screen really has to stop painting once the entry
    /// fade is over.
    #[test_case(false, false => Cadence::SMOOTH ; "entry_fade_is_running")]
    #[test_case(false, true  => Cadence::IDLE   ; "still_splash_settles_after_the_fade")]
    #[test_case(true,  true  => Cadence::SMOOTH ; "the_mark_keeps_swaying_for_as_long_as_it_is_up")]
    fn splash_cadence(animate: bool, faded: bool) -> Cadence {
        let mut splash = Splash::new(animate);
        if faded {
            splash.start -= Duration::from_secs_f32(FADE_DURATION);
        }
        splash.cadence()
    }

    #[test]
    fn is_animating_lifecycle() {
        let ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        assert!(!ct.is_animating(), "settled on construction");

        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(255, 0, 0));
        assert!(ct.is_animating(), "animating after set");
    }

    #[test]
    fn non_rgb_color_uses_fallback() {
        let ct = ColorTransition::new(Color::Blue);
        assert_eq!(
            ct.resolve_rgb(ct.start + Duration::from_secs(1)),
            ACCENT_FALLBACK
        );
    }

    const ROLE_MIXED: &str = "every cell of one role must wear one colour face-on";
    const ROLES_BLURRED: &str = "the C, its cursor and the rest of the grid must look different";
    const OUTSIDE: &str = "the splash must not write outside the area it was given";
    const SAME_FRAME: &str = "one elapsed time must always paint one picture";
    const CLIPPED: &str = "the sway must never carry the mark over the edge of the area";
    const JERKED: &str = "a new firing must not jerk the swaying mark back to face-on";
    /// Seconds apart the motion probe samples, well short of any sway period
    /// and inside the hold, so only the sway can tell the frames apart.
    const MOTION_PROBE_SECS: f32 = 1.5;
    /// Seconds either side of a restart the continuity probe compares.
    const RESTART_PROBE_SECS: f32 = 0.01;
    /// Each colour's channels hold its index in `SHADES`, so a colour a hair
    /// off after a loop still names its shade.
    const PALETTE: Palette = Palette {
        unlit: [0.0; 3],
        lit: [1.0; 3],
        pulse: [2.0; 3],
        cursor: [3.0; 3],
    };
    const SHADES: [&str; 4] = ["unlit", "lit", "pulse", "cursor"];
    const BLENDED: &str = "a mark at rest must paint whole cells, with no blended edge";
    /// Every cell of the mark at full strength over a black background, so
    /// any pixel the mark only partly covers shows as a grey.
    const SOLID: [[Rgb; GRID]; GRID] = [[[u8::MAX as f32; 3]; GRID]; GRID];
    const BARE: Rgb = [0.0; 3];
    /// An odd width centres the mark between two columns.
    const ODD: Rect = Rect {
        x: 0,
        y: 0,
        width: 81,
        height: 23,
    };
    const HEAD: (usize, usize) = (3, 0);
    const TAIL: (usize, usize) = (3, 4);

    /// The terminal cell under the middle of a cell of the face-on mark.
    fn under((col, row): (usize, usize)) -> (u16, u16) {
        let layout = Layout::of(AREA);
        let from_centre =
            |index: usize| (index as f32 - (GRID / 2) as f32) * layout.pitch * layout.half_width;
        let x = layout.centre[0] + from_centre(col);
        let y = layout.centre[1] + from_centre(row);
        (AREA.x + x as u16, AREA.y + (y / 2.0) as u16)
    }

    fn one_color(buf: &Buffer, cells: &[(usize, usize)]) -> Color {
        let color = buf[under(cells[0])].bg;
        assert!(
            cells.iter().all(|&cell| buf[under(cell)].bg == color),
            "{ROLE_MIXED}: {cells:?}"
        );
        color
    }

    /// Text only sets its foreground, so even the cells under it show the mark.
    #[test]
    fn a_still_splash_shows_the_whole_mark() {
        let buf = painted(&Splash::new(false), SETTLED_SECS);
        let lit = FIRING_PATH.concat();
        let unlit: Vec<_> = (0..GRID)
            .flat_map(|row| (0..GRID).map(move |col| (col, row)))
            .filter(|cell| *cell != CURSOR_CELL && !lit.contains(cell))
            .collect();
        let [c, grid, cursor] = [
            one_color(&buf, &lit),
            one_color(&buf, &unlit),
            one_color(&buf, &[CURSOR_CELL]),
        ];
        assert!(
            c != grid && grid != cursor && cursor != c,
            "{ROLES_BLURRED}: {c:?}, {grid:?}, {cursor:?}"
        );
    }

    /// `splash_animation = false` keeps the mark and drops only its motion.
    #[test_case(true  => true  ; "an_animated_mark_keeps_swaying")]
    #[test_case(false => false ; "a_still_mark_holds_its_pose")]
    fn the_animation_setting_governs_the_motion(animate: bool) -> bool {
        let splash = Splash::new(animate);
        painted(&splash, SETTLED_SECS) != painted(&splash, SETTLED_SECS + MOTION_PROBE_SECS)
    }

    fn blink_phase(phase: u32) -> f32 {
        CURSOR_FIRES_AT + (phase as f32 + 0.5) * BLINK
    }

    /// The signal runs the C from its head to its tail, and only then does
    /// the cursor fire, blink, and stay on. The mark holds through the pause,
    /// fades back to rest, and fires again.
    #[test_case(0.0, HEAD => "unlit" ; "nothing_lights_during_the_rest")]
    #[test_case(FIRE_REST, HEAD => "pulse" ; "the_head_flashes_first")]
    #[test_case(FIRE_REST, TAIL => "unlit" ; "the_tail_waits_for_the_signal")]
    #[test_case(CURSOR_FIRES_AT, TAIL => "lit" ; "the_tail_is_lit_before_the_cursor_fires")]
    #[test_case(blink_phase(0), CURSOR_CELL => "cursor" ; "the_cursor_fires")]
    #[test_case(blink_phase(1), CURSOR_CELL => "unlit" ; "the_cursor_blinks")]
    #[test_case(blink_phase(BLINK_PHASES), CURSOR_CELL => "cursor" ; "the_cursor_stays_on")]
    #[test_case(FIRE_LOOP - FIRE_FADE, TAIL => "lit" ; "the_mark_holds_through_the_pause")]
    #[test_case(FIRE_LOOP - RESTART_PROBE_SECS, CURSOR_CELL => "unlit" ; "the_mark_fades_back_to_rest")]
    #[test_case(FIRE_LOOP + FIRE_REST, HEAD => "pulse" ; "the_signal_fires_again")]
    fn the_signal_fires_the_c_before_the_cursor(
        t: f32,
        (col, row): (usize, usize),
    ) -> &'static str {
        SHADES[PALETTE.cells_at(t)[row][col][0].round() as usize]
    }

    /// The sway is widest with every axis at a peak at once. However the
    /// peaks combine, the margin around the face-on mark has to hold it, or a
    /// turn would cut it off at the edge of the area.
    #[test_case(AREA ; "wide")]
    #[test_case(TALL ; "tall")]
    fn the_sway_keeps_the_mark_inside_the_area(area: Rect) {
        for signs in 0..1 << SWAY.len() {
            let turn: [f32; 3] = std::array::from_fn(|axis| {
                let (peak, _) = SWAY[axis];
                if signs >> axis & 1 == 1 { peak } else { -peak }
            });
            let mut buf = Buffer::empty(area);
            paint_mark(area, &mut buf, &SOLID, &View::turned_by(turn), BARE);
            let rows = (area.x..area.right()).flat_map(|x| [(x, area.y), (x, area.bottom() - 1)]);
            let cols = (area.y..area.bottom()).flat_map(|y| [(area.x, y), (area.right() - 1, y)]);
            for at in rows.chain(cols) {
                assert_eq!(
                    buf[at].bg,
                    Color::Reset,
                    "{CLIPPED}: cell {at:?}, turned {turn:?}"
                );
            }
        }
    }

    /// A blended edge is a colour no theme chose, so it can land on any
    /// colour at all, the previous theme's text colour included.
    #[test_case(AREA ; "wide")]
    #[test_case(TALL ; "tall")]
    #[test_case(ODD ; "odd_width")]
    fn a_mark_at_rest_has_crisp_edges(area: Rect) {
        let mut buf = Buffer::empty(area);
        paint_mark(area, &mut buf, &SOLID, &View::turned_by([0.0; 3]), BARE);
        let whole = [Color::Reset, to_color(BARE), to_color(SOLID[0][0])];
        for cell in buf.content() {
            assert!(
                whole.contains(&cell.fg) && whole.contains(&cell.bg),
                "{BLENDED}: {cell:?}"
            );
        }
    }

    /// If the sway restarted with the firing, the mark would snap back to
    /// face-on, and the turn across a restart would be as large as the sway.
    #[test]
    fn the_sway_carries_on_through_a_restart() {
        let apart = |a: &View, b: &View| {
            (0..3)
                .map(|axis| (a.facing[axis] - b.facing[axis]).abs())
                .sum::<f32>()
        };
        let before = View::at(FIRE_LOOP - RESTART_PROBE_SECS);
        let after = View::at(FIRE_LOOP + RESTART_PROBE_SECS);
        let face_on = View::turned_by([0.0; 3]);
        assert!(apart(&before, &after) < apart(&after, &face_on), "{JERKED}");
    }

    /// The field it replaced indexed `Buffer::content` directly, which is
    /// bounds-checked against the whole buffer and so could spill into the
    /// row below when the area was inset.
    #[test]
    fn nothing_is_painted_outside_the_area() {
        let whole = Rect::new(0, 0, 100, 30);
        let area = Rect::new(10, 4, AREA.width, AREA.height);
        let mut buf = Buffer::empty(whole);
        Splash::new(true).render_at(area, &mut buf, SETTLED_SECS, ACCENT);

        let blank = Buffer::empty(whole);
        for y in whole.y..whole.bottom() {
            for x in whole.x..whole.right() {
                if area.contains((x, y).into()) {
                    continue;
                }
                assert_eq!(buf[(x, y)], blank[(x, y)], "{OUTSIDE}: cell {x},{y}");
            }
        }
    }

    #[test]
    fn a_frame_is_a_function_of_elapsed_time_alone() {
        let splash = Splash::new(true);
        assert_eq!(
            painted(&splash, SETTLED_SECS),
            painted(&splash, SETTLED_SECS),
            "{SAME_FRAME}"
        );
    }
}
