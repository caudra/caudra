use crate::components::keybindings::key;
use crate::repaint::Cadence;
use crate::theme::{self, lerp_u8};
use crate::update;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::time::Instant;

const LOGO: &str = "caudra";
const TAGLINE: &str = "turns context into effective action";
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
    ("/btw", "to ask something without interrupting the session"),
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

/// Seconds for one candidate-selection cycle.
const CYCLE_SECS: f32 = 4.5;
/// Rows each further candidate route needs before it is worth adding. Routes
/// have to share rows as they converge, and crowding them turns the junctions
/// into noise, so the upper bound stays close to the mark's own path count.
const ROWS_PER_ROUTE: u16 = 6;
const ROUTE_MIN: usize = 3;
const ROUTE_MAX: usize = 5;
/// Cells between the aperture and the first letter of the wordmark.
const APERTURE_GAP: u16 = 2;
/// Columns the convergence curve needs before routes are drawn at all.
const MIN_ROUTE_SPAN: u16 = 4;
/// Opacity of a route this cycle did not take. A hairline: enough to read as
/// a line over the field texture, not enough to compete with it.
const CANDIDATE_ALPHA: f32 = 0.22;
/// Opacity at the head of the pulse running the selected route.
const SELECTED_ALPHA: f32 = 0.85;
/// Columns the pulse stays visible behind its head.
const TRAIL_CELLS: f32 = 12.0;
/// Columns either side of the aperture over which it opens and closes again.
const FLASH_CELLS: f32 = 5.0;
/// How far open the aperture has to be before it reads as a crossing.
const FLASH_OPEN_AT: f32 = 0.5;
/// Columns over which routes fade in at the left edge and out at the right.
const EDGE_FADE_CELLS: f32 = 4.0;
/// Radians per second the candidates orbit the funnel axis.
const SPIN_RATE: f32 = 0.35;
/// Mouth radius as a fraction of the area's height. The mouth is centred on
/// the aperture, which sits above the middle of the area, so a radius that
/// reaches the bottom edge overshoots the top and clips. Routes swooping in
/// from past the top edge look better than a dead band along the bottom.
const FUNNEL_SPREAD: f32 = 0.5;
/// Perspective focal length, in mouth radii. Must exceed the deepest a route
/// can reach or the projection blows up; lower means stronger perspective.
const FOCAL: f32 = 2.5;
/// The mouth breathes out of round on two different periods, so the bundle
/// tumbles rather than spinning flat.
const WOBBLE_Y_RATE: f32 = 0.19;
const WOBBLE_Z_RATE: f32 = 0.13;
const WOBBLE: f32 = 0.22;
/// Opacity the far side of the funnel loses relative to the near side.
const DEPTH_DIM: f32 = 0.55;
/// Rows the exit beam swings through by the right edge, and how fast. Pinned
/// at the aperture and free at the far end, so it sweeps rather than slides,
/// and stays on the wordmark's row for the few cells that matter.
const EXIT_SWING_ROWS: f32 = 4.0;
const EXIT_SWING_RATE: f32 = 0.23;
/// Depth the exit leans toward and away from the viewer as it sweeps.
const EXIT_LEAN: f32 = 0.6;
const EXIT_LEAN_RATE: f32 = 0.17;
/// Cells the whole scene drifts from its resting place, and how fast. The
/// three periods share no common multiple worth noticing, so the aperture
/// traces a figure that never quite comes back to where it was.
const DRIFT_COLS: f32 = 6.0;
const DRIFT_COL_RATE: f32 = 0.09;
const DRIFT_ROWS: f32 = 3.0;
const DRIFT_ROW_RATE: f32 = 0.14;
/// How far the scene swims toward and away from the viewer, as a scale either
/// side of one. This is the third axis: the bundle grows and shrinks bodily
/// rather than only turning within the plane.
const DRIFT_ZOOM: f32 = 0.18;
const DRIFT_ZOOM_RATE: f32 = 0.07;

/// Ascii chars mapped to increasing wave intensity (first must be space).
const FIELD_SYMS: &[&str] = &[" ", ".", ":", "+", "*"];
const FIELD_CHAR_MAX: f32 = (FIELD_SYMS.len() - 1) as f32;
/// Number of overlapping sine wave layers in the background field.
const WAVE_LAYERS: usize = 3;
/// Peak brightness multiplier for the field. Lower = subtler background.
const INTENSITY_SCALE: f32 = 0.22;
/// How quickly the field darkens toward the edges. Higher = tighter spotlight.
const VIGNETTE_SCALE: f32 = 0.25;
/// Base opacity for the dimmest field character (0.0–1.0). Higher = less contrast between chars.
const FIELD_BASE_OPACITY: f32 = 0.5;
/// How far a field character is pulled from the background toward the accent.
const FIELD_TINT: f32 = 0.26;
/// Seconds of wave drift a launch may start at, so no two open alike.
const FIELD_TIME_SPAN: u64 = 10_000;
/// Radians per second the slowest wave layer turns. Successive layers turn
/// faster and in the opposite sense, so the pattern wheels as it drifts. Kept
/// well under the funnel's spin: the field is the medium, not the subject, and
/// turning it hard sweeps big coherent voids across the screen.
const ROTATION_RATE: f32 = 0.08;
/// Upstream tuned its layer frequencies for one fixed set of angles. Turning
/// them visits every other set, where the lowest frequencies can line up into
/// bands wider than the screen, so the whole set is pitched up a little.
const FIELD_FREQ_SCALE: f32 = 1.25;
/// Cells either side of a pulse whose field characters it lifts.
const GLOW_COLS: f32 = 7.0;
const GLOW_ROWS: f32 = 2.5;
/// Field intensity added at the centre of the pulse head and of the aperture.
/// Enough to take a cell to the brightest character and no further: past that
/// the glyph clips and the bloom flattens into a disc instead of a gradient.
const HEAD_GLOW: f32 = 0.55;
const APERTURE_GLOW: f32 = 0.45;

const INV_TAU: f32 = 1.0 / std::f32::consts::TAU;
const TAU: f32 = std::f32::consts::TAU;
const PI: f32 = std::f32::consts::PI;
const FRAC_PI_2: f32 = std::f32::consts::FRAC_PI_2;
const BHASKARA_B: f32 = 4.0 / (PI * PI);

const RUN_H: &str = "─";
const RUN_V: &str = "│";
const TURN_DOWN_IN: &str = "╮";
const TURN_DOWN_OUT: &str = "╰";
const TURN_UP_IN: &str = "╯";
const TURN_UP_OUT: &str = "╭";
const APERTURE_SHUT: &str = "·";
const APERTURE_OPEN: &str = "*";

const BG_FALLBACK: (u8, u8, u8) = (15, 15, 25);
const FG_FALLBACK: (u8, u8, u8) = (200, 200, 200);
const ACCENT_FALLBACK: (u8, u8, u8) = (100, 140, 255);
const TIP_FALLBACK: (u8, u8, u8) = (249, 226, 175);

#[inline(always)]
fn fast_sin(x: f32) -> f32 {
    let x = x - (x * INV_TAU).floor() * TAU;
    let (x, sign) = if x > PI { (x - PI, -1.0_f32) } else { (x, 1.0) };
    let raw = BHASKARA_B * x * (PI - x);
    sign * (4.0 * raw) / (5.0 - raw)
}

#[inline(always)]
fn fast_sincos(x: f32) -> (f32, f32) {
    (fast_sin(x), fast_sin(x + FRAC_PI_2))
}

struct Wave {
    fx: f32,
    fy: f32,
    phase: f32,
    weight: f32,
}

/// The interference pattern behind everything else. The field samples it per
/// cell and the routes sample it to decide how far they bend, so both read
/// from one set of numbers and agree about where the medium is.
struct Waves {
    layers: [Wave; WAVE_LAYERS],
    weight_sum: f32,
}

impl Waves {
    fn at(t: f32) -> Self {
        let layers: [Wave; WAVE_LAYERS] = std::array::from_fn(|i| {
            let lf = i as f32;
            let fx = (2.0 + lf * 1.8) * FIELD_FREQ_SCALE;
            let fy = (1.5 + lf * 1.2) * FIELD_FREQ_SCALE;
            let sense = if i % 2 == 0 { 1.0 } else { -1.0 };
            let (sin, cos) = fast_sincos(fy.atan2(fx) + t * ROTATION_RATE * (1.0 + lf) * sense);
            let magnitude = fx.hypot(fy);
            Wave {
                fx: magnitude * cos,
                fy: magnitude * sin,
                phase: t * (0.3 + lf * 0.15) + lf * 2.094,
                weight: 1.0 / (1.5 + lf * 0.5),
            }
        });
        Self {
            weight_sum: layers.iter().map(|layer| layer.weight).sum(),
            layers,
        }
    }
}

/// The whole moving picture: a cone of candidates with its mouth open to the
/// left and its tip at the aperture, the axis it runs out along, and where the
/// aperture itself has drifted to. Nothing here is pinned. The cone spins about
/// its axis, its mouth breathes out of round, the axis sweeps, and the origin
/// they all hang off wanders in three dimensions.
struct Scene {
    spin: f32,
    squash_y: f32,
    squash_z: f32,
    radius: f32,
    axis: Mouth,
    shift_col: f32,
    shift_row: f32,
}

impl Scene {
    fn at(t: f32, height: u16) -> Self {
        let zoom = 1.0 + fast_sin(t * DRIFT_ZOOM_RATE) * DRIFT_ZOOM;
        Self {
            spin: t * SPIN_RATE,
            squash_y: 1.0 + fast_sin(t * WOBBLE_Y_RATE) * WOBBLE,
            squash_z: 1.0 + fast_sin(t * WOBBLE_Z_RATE) * WOBBLE,
            radius: f32::from(height) * FUNNEL_SPREAD * zoom,
            axis: Mouth {
                offset: fast_sin(t * EXIT_SWING_RATE) * EXIT_SWING_ROWS * zoom,
                depth: fast_sin(t * EXIT_LEAN_RATE) * EXIT_LEAN,
            },
            shift_col: fast_sin(t * DRIFT_COL_RATE) * DRIFT_COLS,
            shift_row: fast_sin(t * DRIFT_ROW_RATE) * DRIFT_ROWS,
        }
    }

    /// Where candidate `i` sits on the mouth: rows off the axis, and depth
    /// toward the viewer in mouth radii. A candidate turning through the near
    /// and far sides of the orbit passes edge-on twice, where it flattens onto
    /// the axis and all but disappears, exactly as a real ring would.
    fn mouth(&self, i: usize, count: usize) -> Mouth {
        let (sin, cos) = fast_sincos(TAU * i as f32 / count as f32 + self.spin);
        Mouth {
            offset: sin * self.squash_y * self.radius,
            depth: cos * self.squash_z,
        }
    }
}

#[derive(Clone, Copy)]
struct Mouth {
    offset: f32,
    depth: f32,
}

/// Perspective divide. A route on the near side of the orbit swings wider than
/// the same route on the far side, which is what sells the turn as a turn.
fn perspective(depth: f32) -> f32 {
    FOCAL / (FOCAL - depth)
}

/// A place in the field a pulse is lighting up, in cells relative to the area.
struct Bump {
    col: f32,
    row: f32,
    strength: f32,
}

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
            start: Instant::now() - std::time::Duration::from_secs_f32(COLOR_TRANSITION_SECS),
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
    seed: u64,
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
        let mut rng = [0u8; 8];
        getrandom::fill(&mut rng).ok();
        let seed = u64::from_le_bytes(rng);
        Self {
            start: Instant::now(),
            seed,
            animate,
            tip_idx: (seed >> u32::BITS) as usize % TIPS.len(),
        }
    }

    /// Routes keep being selected for as long as the splash is up. With the
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

    /// Every frame is a function of elapsed time and [`Self::seed`] alone, so
    /// the same `t` always paints the same picture and nothing has to be
    /// carried between frames.
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

        if self.animate {
            self.render_animation(area, buf, t, fade, (logo_x - APERTURE_GAP, top_y), accent);
        }
        self.render_logo(area, buf, t, fade, (logo_x, top_y), accent);
        render_centered_faded(area, buf, fade, 0.75, tag_y, TAGLINE);
        self.render_help(area, buf, fade, help_y, accent);
        self.render_tip(area, buf, fade, tip_y, accent);
        render_version(area, buf, fade, area.y);
    }

    /// The wave field fills the area, and candidate routes bend with it as they
    /// converge on the aperture sitting just before the wordmark. One route is
    /// selected per cycle: a pulse runs it, lighting the field it passes
    /// through, the aperture opens as the pulse crosses, and the route carries
    /// on behind the wordmark and off the right edge.
    fn render_animation(
        &self,
        area: Rect,
        buf: &mut Buffer,
        t: f32,
        fade: f32,
        aperture: (u16, u16),
        accent: Color,
    ) {
        let waves = Waves::at(t + (self.seed % FIELD_TIME_SPAN) as f32);
        let scene = Scene::at(t, area.height);

        // The aperture is the scene's origin, not a fixture of the layout, so
        // it drifts off the wordmark and back rather than the bundle turning
        // around a nailed-down point.
        let resting = f32::from(aperture.0.saturating_sub(area.x));
        let drifted = (resting + scene.shift_col).max(0.0) as u16;
        let span = drifted.min(area.width.saturating_sub(1));
        if span < MIN_ROUTE_SPAN {
            render_field(area, buf, &waves, fade, accent, &[]);
            return;
        }

        let theme = theme::current();
        let cycle = t / CYCLE_SECS;
        let field = RouteField {
            area,
            paint: RoutePaint {
                bg: extract_rgb(theme.background, BG_FALLBACK),
                fg: extract_rgb(theme.foreground, FG_FALLBACK),
                accent: extract_rgb(accent, ACCENT_FALLBACK),
                fade,
            },
            exit: scene.axis,
            span,
            aperture_row: f32::from(aperture.1.saturating_sub(area.y)) + scene.shift_row,
            head: smoothstep(cycle.fract()) * (f32::from(area.width) + TRAIL_CELLS),
        };

        let count = ((area.height / ROWS_PER_ROUTE) as usize).clamp(ROUTE_MIN, ROUTE_MAX);
        let selected = selected_route(self.seed, cycle as u64, count);
        let chosen = scene.mouth(selected, count);

        let bumps = [field.head_bump(chosen), field.aperture_bump()];
        render_field(area, buf, &waves, fade, accent, &bumps);

        // Painter's algorithm: the far side of the orbit goes down first so the
        // near side crosses over it, rather than whichever index happens to be
        // drawn last winning every overlap.
        let mut order = [0usize; ROUTE_MAX];
        let mut candidates = 0;
        for i in (0..count).filter(|&i| i != selected) {
            order[candidates] = i;
            candidates += 1;
        }
        order[..candidates].sort_by(|&a, &b| {
            scene
                .mouth(a, count)
                .depth
                .total_cmp(&scene.mouth(b, count).depth)
        });
        for &candidate in &order[..candidates] {
            field.draw(buf, scene.mouth(candidate, count), span, false);
        }

        field.draw(buf, chosen, area.width, true);
        field.draw_aperture(buf);
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
        let theme = theme::current();
        let bg = theme.background;
        let (ac_r, ac_g, ac_b) = extract_rgb(accent, ACCENT_FALLBACK);
        let (bg_r, bg_g, bg_b) = extract_rgb(bg, BG_FALLBACK);

        let alpha = 0.85 * ease_out_cubic(((t - LOGO_DELAY) / LOGO_RAMP).clamp(0.0, 1.0)) * fade;
        let style = Style::new()
            .fg(Color::Rgb(
                lerp_u8(bg_r, ac_r, alpha),
                lerp_u8(bg_g, ac_g, alpha),
                lerp_u8(bg_b, ac_b.saturating_add(15), alpha),
            ))
            .bg(bg)
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
        let bg = theme.background;
        let ac = extract_rgb(accent, ACCENT_FALLBACK);
        let fg = extract_rgb(theme.foreground, FG_FALLBACK);
        let bg_rgb = extract_rgb(bg, BG_FALLBACK);

        let total_width: u16 = HELP_SEGMENTS
            .iter()
            .map(|(s, _)| s.chars().count() as u16)
            .sum();
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: Vec<_> = HELP_SEGMENTS
            .iter()
            .map(|&(text, highlighted)| {
                let (target, alpha) = if highlighted { (ac, 0.75) } else { (fg, 0.5) };
                (text, faded_style(bg_rgb, target, alpha * fade, bg))
            })
            .collect();

        render_segments(area, buf, help_y, x_start, &segments);
    }

    fn render_tip(&self, area: Rect, buf: &mut Buffer, fade: f32, tip_y: u16, accent: Color) {
        if tip_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let bg = theme.background;
        let tip_rgb = extract_rgb(
            theme.todo_in_progress.fg.unwrap_or(Color::Yellow),
            TIP_FALLBACK,
        );
        let ac = extract_rgb(accent, ACCENT_FALLBACK);
        let fg = extract_rgb(theme.foreground, FG_FALLBACK);
        let bg_rgb = extract_rgb(bg, BG_FALLBACK);

        let (label, desc) = TIPS[self.tip_idx];
        let total_width = (5 + label.len() + 1 + desc.len()) as u16;
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: &[(&str, Style)] = &[
            (
                "tip: ",
                faded_style(bg_rgb, tip_rgb, 0.75 * fade, bg).add_modifier(Modifier::BOLD),
            ),
            (label, faded_style(bg_rgb, ac, 0.75 * fade, bg)),
            (" ", Style::default()),
            (desc, faded_style(bg_rgb, fg, 0.5 * fade, bg)),
        ];

        render_segments(area, buf, tip_y, x_start, segments);
    }
}

struct RoutePaint {
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
    accent: (u8, u8, u8),
    fade: f32,
}

impl RoutePaint {
    /// `lit` walks a cell from candidate to selected: the hue slides from the
    /// theme foreground to the accent and the opacity rises with it.
    fn style(&self, lit: f32, edge: f32) -> Style {
        let target = (
            lerp_u8(self.fg.0, self.accent.0, lit),
            lerp_u8(self.fg.1, self.accent.1, lit),
            lerp_u8(self.fg.2, self.accent.2, lit),
        );
        let alpha = (CANDIDATE_ALPHA + (SELECTED_ALPHA - CANDIDATE_ALPHA) * lit) * self.fade * edge;
        Style::new().fg(Color::Rgb(
            lerp_u8(self.bg.0, target.0, alpha),
            lerp_u8(self.bg.1, target.1, alpha),
            lerp_u8(self.bg.2, target.2, alpha),
        ))
    }
}

/// Route geometry in cells relative to the splash area, so every write is
/// clamped against the area rather than against the whole buffer.
struct RouteField {
    area: Rect,
    paint: RoutePaint,
    exit: Mouth,
    span: u16,
    aperture_row: f32,
    head: f32,
}

impl RouteField {
    /// Which end of the scene a column belongs to, and how far out along it.
    /// Before the aperture a route is coming in off the funnel mouth; after it,
    /// it is running out along the axis. Both reach zero at the aperture, which
    /// is why the two halves meet there without a seam.
    fn governing(&self, col: u16, mouth: Mouth) -> (f32, Mouth) {
        if col >= self.span {
            let run = self.area.width.saturating_sub(self.span).max(1);
            let out = f32::from(col - self.span) / f32::from(run);
            (smoothstep(out), self.exit)
        } else {
            (
                1.0 - smoothstep(f32::from(col) / f32::from(self.span)),
                mouth,
            )
        }
    }

    /// A route narrows onto the axis as it nears the aperture, so its depth
    /// narrows with it and the perspective relaxes. That is what bends a
    /// straight line in space into the curve the eye reads as a route.
    fn row_exact(&self, col: u16, mouth: Mouth) -> f32 {
        let (reach, along) = self.governing(col, mouth);
        let swing = along.offset * reach * perspective(along.depth * reach);
        self.aperture_row + swing
    }

    fn row(&self, col: u16, mouth: Mouth) -> u16 {
        self.row_exact(col, mouth).max(0.0).round() as u16
    }

    /// The cell the aperture has drifted onto, which both halves of every route
    /// are pinned to and which candidates recognise as the trunk.
    fn aperture_cell(&self) -> u16 {
        self.aperture_row.max(0.0).round() as u16
    }

    fn edge(&self, col: u16) -> f32 {
        let entering = (col + 1) as f32;
        let leaving = self.area.width.saturating_sub(col) as f32;
        (entering.min(leaving) / EDGE_FADE_CELLS).min(1.0)
    }

    fn open(&self) -> f32 {
        let reach = (1.0 - (self.head - self.span as f32).abs() / FLASH_CELLS).max(0.0);
        reach * reach
    }

    /// The glow travels with the pulse, so it follows the selected route's
    /// curve in and the trunk out, and dies with the trail at the right edge.
    fn head_bump(&self, mouth: Mouth) -> Bump {
        let col = self.head.clamp(0.0, (self.area.width - 1) as f32) as u16;
        Bump {
            col: self.head,
            row: self.row_exact(col, mouth),
            strength: HEAD_GLOW * trail(self.head, col) * self.edge(col),
        }
    }

    fn aperture_bump(&self) -> Bump {
        Bump {
            col: f32::from(self.span),
            row: self.aperture_row,
            strength: APERTURE_GLOW * self.open(),
        }
    }

    /// Depth reaches the eye as opacity: the far side of the orbit reads as
    /// further away because it is fainter, which is the only depth cue a cell
    /// grid has left once perspective has been spent on position.
    fn style(&self, col: u16, selected: bool, mouth: Mouth) -> Style {
        let lit = if selected { trail(self.head, col) } else { 0.0 };
        let (_, along) = self.governing(col, mouth);
        let near = 1.0 - DEPTH_DIM * (1.0 - along.depth) * 0.5;
        self.paint.style(lit, self.edge(col) * near)
    }

    fn put(&self, buf: &mut Buffer, col: u16, row: u16, sym: &str, style: Style) {
        if col >= self.area.width || row >= self.area.height {
            return;
        }
        if let Some(cell) = buf.cell_mut((self.area.x + col, self.area.y + row)) {
            cell.set_symbol(sym).set_style(style);
        }
    }

    /// A column whose row differs from its predecessor turns, drops or climbs
    /// the whole run, and turns back, so a steep stretch stays one connected
    /// line however few columns it has to cross.
    ///
    /// A candidate stops the moment it meets the aperture row, and lands on a
    /// plain run rather than a corner. From there it has merged into the trunk,
    /// and a corner would only punch a notch in the line the selected route
    /// draws along it.
    fn draw(&self, buf: &mut Buffer, mouth: Mouth, cols: u16, selected: bool) {
        let mut previous = None;
        for col in 0..cols {
            let row = self.row(col, mouth);
            let style = self.style(col, selected, mouth);
            let merging = !selected && row == self.aperture_cell();
            match previous {
                Some(was) if was != row => {
                    let down = row > was;
                    let (turn_in, turn_out) = if down {
                        (TURN_DOWN_IN, TURN_DOWN_OUT)
                    } else {
                        (TURN_UP_IN, TURN_UP_OUT)
                    };
                    let (first, last) = if down { (was + 1, row) } else { (row + 1, was) };
                    self.put(buf, col, was, turn_in, style);
                    for between in first..last {
                        self.put(buf, col, between, RUN_V, style);
                    }
                    self.put(buf, col, row, if merging { RUN_H } else { turn_out }, style);
                }
                _ => self.put(buf, col, row, RUN_H, style),
            }
            previous = Some(row);
            if merging {
                return;
            }
        }
    }

    fn draw_aperture(&self, buf: &mut Buffer) {
        let open = self.open();
        let style = self.paint.style(open, 1.0);
        let (sym, style) = if open > FLASH_OPEN_AT {
            (APERTURE_OPEN, style.add_modifier(Modifier::BOLD))
        } else {
            (APERTURE_SHUT, style)
        };
        self.put(buf, self.span, self.aperture_cell(), sym, style);
    }
}

/// Upstream's interference field, dimmer and accent-neutral, with the pulses
/// in `bumps` lifting the characters around them.
fn render_field(
    area: Rect,
    buf: &mut Buffer,
    waves: &Waves,
    fade: f32,
    accent: Color,
    bumps: &[Bump],
) {
    let theme = theme::current();
    let (ac_r, ac_g, ac_b) = extract_rgb(accent, ACCENT_FALLBACK);
    let (bg_r, bg_g, bg_b) = extract_rgb(theme.background, BG_FALLBACK);

    let w = area.width as usize;
    let h = area.height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let inv_w = 1.0 / w as f32;
    let inv_h = 1.0 / h as f32;

    let half_weight_sum = waves.weight_sum * 0.5;
    let val_scale = (fade * INTENSITY_SCALE) / half_weight_sum;

    let style_lut: [(&str, Style); 4] = std::array::from_fn(|i| {
        let idx = i + 1;
        let frac = idx as f32 / FIELD_CHAR_MAX;
        let opacity = (FIELD_BASE_OPACITY + frac * (1.0 - FIELD_BASE_OPACITY)) * FIELD_TINT;
        (
            FIELD_SYMS[idx],
            Style::new().fg(Color::Rgb(
                lerp_u8(bg_r, ac_r, opacity),
                lerp_u8(bg_g, ac_g, opacity),
                lerp_u8(bg_b, ac_b, opacity),
            )),
        )
    });

    let vignette_inv = 1.0 / VIGNETTE_SCALE;

    // Single allocation for all per-column data: vx | sin0 | cos0 | sin1 | cos1 | sin2 | cos2
    // Contiguous SoA layout enables LLVM auto-vectorization of the inner wave loops.
    let mut col_data = vec![0.0_f32; w * (1 + WAVE_LAYERS * 2)];
    for col in 0..w {
        let nx = col as f32 * inv_w;
        let d = (nx - 0.5) * 2.0;
        col_data[col] = d * d;
        for i in 0..WAVE_LAYERS {
            let (s, c) = fast_sincos(nx * waves.layers[i].fx);
            col_data[w + i * 2 * w + col] = s * waves.layers[i].weight;
            col_data[w + (i * 2 + 1) * w + col] = c * waves.layers[i].weight;
        }
    }
    let vx = &col_data[..w];
    let col_sin: [&[f32]; WAVE_LAYERS] =
        std::array::from_fn(|i| &col_data[w + i * 2 * w..w + i * 2 * w + w]);
    let col_cos: [&[f32]; WAVE_LAYERS] =
        std::array::from_fn(|i| &col_data[w + (i * 2 + 1) * w..w + (i * 2 + 2) * w]);

    let col_start = vx.partition_point(|&v| v > vignette_inv);
    let col_end = w - vx
        .iter()
        .rev()
        .position(|&v| v <= vignette_inv)
        .unwrap_or(0);
    if col_start >= col_end {
        return;
    }

    let buf_width = buf.area().width as usize;
    let content = &mut buf.content;

    let mut vals = vec![0.0_f32; col_end - col_start];

    for row in 0..h {
        let ny = row as f32 * inv_h;
        let d = (ny - 0.5) * 2.0;
        let vy = d * d;

        let max_vx = vignette_inv - vy;
        if max_vx <= 0.0 {
            continue;
        }

        let row_sincos: [(f32, f32); WAVE_LAYERS] =
            std::array::from_fn(|i| fast_sincos(ny * waves.layers[i].fy + waves.layers[i].phase));

        let rc_start = col_start + vx[col_start..col_end].partition_point(|&v| v > max_vx);
        let rc_end = col_end
            - vx[col_start..col_end]
                .iter()
                .rev()
                .position(|&v| v <= max_vx)
                .unwrap_or(0);

        let out = &mut vals[rc_start - col_start..rc_end - col_start];
        let vx_slice = &vx[rc_start..rc_end];

        // AUTOVECTORIZED - LLVM emits AVX (ymm, 8×f32) for these loops.
        // Do NOT add branches, function calls, or non-contiguous indexing here.
        // Verified via `perf annotate`.
        for i in 0..WAVE_LAYERS {
            let (sr, cr) = row_sincos[i];
            let cs = &col_sin[i][rc_start..rc_end];
            let cc = &col_cos[i][rc_start..rc_end];
            for j in 0..out.len() {
                out[j] += cs[j] * cr + cc[j] * sr;
            }
        }
        for j in 0..out.len() {
            let vignette = 1.0 - (vx_slice[j] + vy) * VIGNETTE_SCALE;
            out[j] = (out[j] + half_weight_sum) * vignette * val_scale;
        }

        // Kept out of the loops above so their bodies stay branch-free. Only
        // the few rows a pulse actually reaches pay for this.
        for bump in bumps {
            let dy = (row as f32 - bump.row) / GLOW_ROWS;
            let near = 1.0 - dy * dy;
            if near <= 0.0 {
                continue;
            }
            for (j, val) in out.iter_mut().enumerate() {
                let dx = ((rc_start + j) as f32 - bump.col) / GLOW_COLS;
                let lift = near - dx * dx;
                if lift > 0.0 {
                    *val += bump.strength * lift;
                }
            }
        }

        let y = area.y + row as u16;
        let row_offset = y as usize * buf_width + area.x as usize;

        for (j, val) in out.iter_mut().enumerate() {
            let idx = (*val * FIELD_CHAR_MAX + 0.5) as usize;
            *val = 0.0;
            if idx == 0 {
                continue;
            }
            let (sym, style) = &style_lut[idx.min(FIELD_SYMS.len() - 1) - 1];

            if let Some(cell) = content.get_mut(row_offset + rc_start + j) {
                cell.set_symbol(sym).set_style(*style);
            }
        }
    }
}

fn trail(head: f32, col: u16) -> f32 {
    let behind = head - col as f32;
    if behind < 0.0 {
        return 0.0;
    }
    let remaining = (1.0 - behind / TRAIL_CELLS).max(0.0);
    remaining * remaining
}

/// A stride in `1..count` is never a multiple of `count`, so the route a cycle
/// selects can never be the one its neighbours selected.
fn selected_route(seed: u64, cycle: u64, count: usize) -> usize {
    let routes = count as u64;
    let stride = 1 + seed % (routes - 1);
    ((seed % routes + (cycle % routes) * stride) % routes) as usize
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
    let bg = theme.background;
    let text = format!("v{}", update::CURRENT);
    let style = faded_style(
        extract_rgb(bg, BG_FALLBACK),
        extract_rgb(theme.foreground, FG_FALLBACK),
        0.4 * fade,
        bg,
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
    let bg = theme.background;
    let style = faded_style(
        extract_rgb(bg, BG_FALLBACK),
        extract_rgb(theme.foreground, FG_FALLBACK),
        intensity * fade,
        bg,
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

fn faded_style(bg: (u8, u8, u8), fg: (u8, u8, u8), alpha: f32, bg_color: Color) -> Style {
    Style::new()
        .fg(Color::Rgb(
            lerp_u8(bg.0, fg.0, alpha),
            lerp_u8(bg.1, fg.1, alpha),
            lerp_u8(bg.2, fg.2, alpha),
        ))
        .bg(bg_color)
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
    use ratatui::buffer::Cell;
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
    /// Vermilion, the identity's colour for a selected route.
    const ACCENT: Color = Color::Rgb(240, 68, 47);
    /// Far enough in that the entry fade is over and the cycle count is stable.
    const SETTLED_CYCLE: f32 = 2.0;

    fn at_phase(phase: f32) -> f32 {
        CYCLE_SECS * (SETTLED_CYCLE + phase)
    }

    fn painted(splash: &Splash, t: f32) -> Buffer {
        let mut buf = Buffer::empty(AREA);
        splash.render_at(AREA, &mut buf, t, ACCENT);
        buf
    }

    fn rendered(splash: &Splash) -> String {
        buffer_text(&painted(splash, at_phase(0.0)))
    }

    #[test]
    fn the_start_screen_names_the_installed_version_alone() {
        let screen = rendered(&Splash::new(false));
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
    /// line a column to the left of the tagline above it.
    #[test]
    fn the_help_line_names_the_docs_and_sits_centred() {
        let buf = painted(&seeded(false), at_phase(0.0));
        let row = (AREA.y..AREA.bottom())
            .map(|y| {
                (AREA.x..AREA.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|row| row.contains(DOCS_COMMAND))
            .expect(DOCS_UNNAMED);
        let left = row.len() - row.trim_start().len();
        let right = row.len() - row.trim_end().len();
        assert!(left.abs_diff(right) <= 1, "{OFF_CENTRE}: {row:?}");
    }

    /// `splash_animation = false` is what a user on a slow machine reaches
    /// for, so the start screen really has to stop painting once the entry
    /// fade is over.
    #[test_case(false, false => Cadence::SMOOTH ; "entry_fade_is_running")]
    #[test_case(false, true  => Cadence::IDLE   ; "still_splash_settles_after_the_fade")]
    #[test_case(true,  true  => Cadence::SMOOTH ; "routes_keep_being_selected_for_as_long_as_it_is_up")]
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

    const ROUTE_GLYPHS: &[&str] = &[
        RUN_H,
        RUN_V,
        TURN_DOWN_IN,
        TURN_DOWN_OUT,
        TURN_UP_IN,
        TURN_UP_OUT,
    ];
    const REPEATED: &str = "a cycle must never select the route its neighbour took";
    const NOT_TAKEN: &str = "the pulse must reach the right margin by late cycle";
    const OUTSIDE: &str = "the splash must not write outside the area it was given";
    const SAME_FRAME: &str = "one elapsed time must always paint one picture";
    const STRETCHED: &str = "rotating a layer must turn it, not resize it";
    const UNTURNED: &str = "a layer must face somewhere else once it has turned";
    const UNPIVOTED: &str = "the funnel must keep turning over time";
    const THROUGH_LENS: &str = "no route may reach the focal plane";
    const MISSED: &str = "a route must land on the aperture however it is turned";
    const WANDERED: &str = "the exit must still be crossing the wordmark at the wordmark";
    /// Rows the exit may have climbed by the end of the wordmark.
    const WORDMARK_SLACK_ROWS: u16 = 1;
    const UNSWEPT: &str = "the exit must leave the screen somewhere other than dead level";
    /// Orbit positions the turn probes sample, spread over many revolutions.
    const ORBIT_SAMPLES: usize = 64;
    const SPILLED: &str = "a pulse must not light the field beyond its own window";
    const DIMMED: &str = "a pulse must only ever lift a field character";
    const UNLIT: &str = "a pulse must light something";
    const SEEDS: u64 = 64;
    const CYCLES: u64 = 16;
    /// Columns in from the right edge, clear of the edge fade.
    const MARGIN_PROBE: u16 = 6;
    const EARLY_PHASE: f32 = 0.2;
    const LATE_PHASE: f32 = 0.75;
    /// Seconds apart the rotation and pivot probes sample.
    const TURN_SECS: f32 = 6.0;
    const TOLERANCE: f32 = 1e-3;
    /// `fast_sin` is a Bhaskara approximation, so `cos² + sin²` is only nearly
    /// one and a turned layer's magnitude wobbles by a fraction of a percent.
    /// Anything that actually resized a layer would be far coarser than this.
    const MAGNITUDE_DRIFT: f32 = 0.01;
    /// Rows the fan has to have swung through to count as having moved.
    const MIN_PIVOT_ROWS: f32 = 0.5;

    fn seeded(animate: bool) -> Splash {
        let mut splash = Splash::new(animate);
        splash.seed = 0;
        splash
    }

    /// `splash_animation = false` has to mean no background at all. Text rows
    /// are skipped because tips carry `+` and `:` and the version carries `.`.
    #[test_case(true  => (true, true)   ; "an_animated_splash_draws_field_and_routes")]
    #[test_case(false => (false, false) ; "a_still_splash_draws_neither")]
    fn the_animation_setting_governs_both_layers(animate: bool) -> (bool, bool) {
        let buf = painted(&seeded(animate), at_phase(LATE_PHASE));
        let (mut field, mut routes) = (false, false);
        for y in (AREA.y + 1)..AREA.height.saturating_sub(TEXT_BLOCK_HEIGHT) / 2 {
            for x in 0..AREA.width {
                let sym = buf[(x, y)].symbol();
                field |= FIELD_SYMS[1..].contains(&sym);
                routes |= ROUTE_GLYPHS.contains(&sym);
            }
        }
        (field, routes)
    }

    /// Turning a layer may change where it points and nothing else. A layer
    /// whose magnitude drifted would change wavelength as it wheeled.
    #[test]
    fn rotation_turns_the_layers_without_resizing_them() {
        let (still, turned) = (Waves::at(0.0), Waves::at(TURN_SECS));
        for (before, after) in still.layers.iter().zip(turned.layers.iter()) {
            let (was, now) = (before.fx.hypot(before.fy), after.fx.hypot(after.fy));
            let drift = (was - now).abs() / was;
            assert!(drift < MAGNITUDE_DRIFT, "{STRETCHED}: {was} then {now}");
            assert!((before.fx - after.fx).abs() > TOLERANCE, "{UNTURNED}");
        }
    }

    /// Every candidate has to keep orbiting, and the projection has to stay
    /// finite: a depth that ever reached the focal plane would divide by zero
    /// and throw a route to infinity.
    #[test]
    fn the_funnel_keeps_turning_without_passing_through_the_lens() {
        let mut swung = false;
        for step in 0..ORBIT_SAMPLES {
            let t = TURN_SECS * step as f32;
            let scene = Scene::at(t, AREA.height);
            let resting = Scene::at(0.0, AREA.height);
            for i in 0..ROUTE_MAX {
                let (now, then) = (scene.mouth(i, ROUTE_MAX), resting.mouth(i, ROUTE_MAX));
                assert!(now.depth < FOCAL, "{THROUGH_LENS}: depth {}", now.depth);
                swung |= (now.offset - then.offset).abs() > MIN_PIVOT_ROWS;
            }
        }
        assert!(swung, "{UNPIVOTED}");
    }

    fn probe_field(scene: &Scene) -> RouteField {
        RouteField {
            area: AREA,
            paint: RoutePaint {
                bg: BG_FALLBACK,
                fg: FG_FALLBACK,
                accent: ACCENT_FALLBACK,
                fade: 1.0,
            },
            exit: scene.axis,
            span: AREA.width / 2,
            aperture_row: f32::from(AREA.height.saturating_sub(TEXT_BLOCK_HEIGHT) / 2)
                + scene.shift_row,
            head: 0.0,
        }
    }

    /// However the bundle is turned and however the exit beam is swung, the
    /// aperture is the one cell both halves are pinned to. A seam there would
    /// break the line exactly where the eye is looking.
    #[test]
    fn both_halves_meet_on_the_aperture() {
        for step in 0..ORBIT_SAMPLES {
            let scene = Scene::at(TURN_SECS * step as f32, AREA.height);
            let field = probe_field(&scene);
            for i in 0..ROUTE_MAX {
                assert_eq!(
                    field.row(field.span, scene.mouth(i, ROUTE_MAX)),
                    field.aperture_cell(),
                    "{MISSED}: route {i} at step {step}"
                );
            }
        }
    }

    /// The exit is pinned at the aperture and free at the far end. Crossing the
    /// wordmark it has barely begun to climb, so it still reads as running
    /// through the word rather than away from it; by the far edge it has to be
    /// somewhere else entirely or the sweep is not a sweep.
    #[test]
    fn the_exit_beam_sweeps_yet_still_crosses_the_wordmark() {
        let mut swept = false;
        for step in 0..ORBIT_SAMPLES {
            let scene = Scene::at(TURN_SECS * step as f32, AREA.height);
            let field = probe_field(&scene);
            let mouth = scene.mouth(0, ROUTE_MAX);

            let past_wordmark = field.span + APERTURE_GAP + LOGO.len() as u16;
            let strayed = field
                .row(past_wordmark, mouth)
                .abs_diff(field.aperture_cell());
            assert!(strayed <= WORDMARK_SLACK_ROWS, "{WANDERED}: step {step}");
            swept |= field.row(AREA.width - 1, mouth) != field.aperture_cell();
        }
        assert!(swept, "{UNSWEPT}");
    }

    fn glyph_rank(cell: &Cell) -> usize {
        FIELD_SYMS
            .iter()
            .position(|sym| *sym == cell.symbol())
            .unwrap_or_default()
    }

    /// The field on its own never reaches the brightest character, so anything
    /// that does is a pulse, and a pulse may only brighten cells inside its own
    /// falloff.
    #[test]
    fn a_pulse_lights_the_field_around_it() {
        let waves = Waves::at(TURN_SECS);
        let bump = Bump {
            col: f32::from(AREA.width) * 0.5,
            row: f32::from(AREA.height) * 0.5,
            strength: HEAD_GLOW,
        };

        let mut dark = Buffer::empty(AREA);
        let mut lit = Buffer::empty(AREA);
        render_field(AREA, &mut dark, &waves, 1.0, ACCENT, &[]);
        render_field(
            AREA,
            &mut lit,
            &waves,
            1.0,
            ACCENT,
            std::slice::from_ref(&bump),
        );

        let mut changed = 0;
        for y in 0..AREA.height {
            for x in 0..AREA.width {
                let (before, after) = (&dark[(x, y)], &lit[(x, y)]);
                if before.symbol() == after.symbol() {
                    continue;
                }
                changed += 1;
                let dx = (f32::from(x) - bump.col) / GLOW_COLS;
                let dy = (f32::from(y) - bump.row) / GLOW_ROWS;
                assert!(dx * dx + dy * dy < 1.0, "{SPILLED}: cell {x},{y}");
                assert!(
                    glyph_rank(after) > glyph_rank(before),
                    "{DIMMED}: cell {x},{y}"
                );
            }
        }
        assert!(changed > 0, "{UNLIT}");
    }

    #[test]
    fn consecutive_cycles_never_select_the_same_route() {
        for seed in 0..SEEDS {
            for count in ROUTE_MIN..=ROUTE_MAX {
                for cycle in 0..CYCLES {
                    assert_ne!(
                        selected_route(seed, cycle, count),
                        selected_route(seed, cycle + 1, count),
                        "{REPEATED}: seed {seed}, {count} routes, cycle {cycle}"
                    );
                }
            }
        }
    }

    /// The brightest cell in a column near the right edge. The exit beam
    /// sweeps, so which row carries it is not fixed; nothing else below the
    /// version line gets near a lit route's opacity, and the version line is
    /// right-aligned into this very column at a brightness that never moves.
    fn margin_brightness(phase: f32) -> u32 {
        let buf = painted(&seeded(true), at_phase(phase));
        ((AREA.y + 1)..AREA.height)
            .map(|y| match buf[(AREA.width - MARGIN_PROBE, y)].fg {
                Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
                _ => 0,
            })
            .max()
            .unwrap_or_default()
    }

    /// The selected route only means anything if the pulse actually leaves
    /// through the right edge, which is the "continues as action" half of the
    /// mark. Early in the cycle that stretch is still an unlit candidate.
    #[test]
    fn the_selected_route_runs_out_through_the_right_margin() {
        let early = margin_brightness(EARLY_PHASE);
        let late = margin_brightness(LATE_PHASE);
        assert!(late > early, "{NOT_TAKEN}: early {early}, late {late}");
    }

    /// The field it replaced indexed `Buffer::content` directly, which is
    /// bounds-checked against the whole buffer and so could spill into the
    /// row below when the area was inset.
    #[test]
    fn nothing_is_painted_outside_the_area() {
        let whole = Rect::new(0, 0, 100, 30);
        let area = Rect::new(10, 4, AREA.width, AREA.height);
        let mut buf = Buffer::empty(whole);
        seeded(true).render_at(area, &mut buf, at_phase(LATE_PHASE), ACCENT);

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
        let splash = seeded(true);
        let t = at_phase(LATE_PHASE);
        assert_eq!(painted(&splash, t), painted(&splash, t), "{SAME_FRAME}");
    }
}
