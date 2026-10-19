//! Source-space image inspection, independent of the host's rasterizer.

use std::sync::{Arc, Weak};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::keys;

const ZOOM_STEP: f64 = 1.25;
const MIN_SCALE: f64 = 1.0 / 1024.0;
const MAX_SCALE: f64 = 64.0;
const PAN_FRACTION: f64 = 0.1;

fn fit_scale(source: (u32, u32), cell: (f64, f64), area: Rect) -> f64 {
    (f64::from(area.width) * cell.0 / f64::from(source.0))
        .min(f64::from(area.height) * cell.1 / f64::from(source.1))
        .min(1.0)
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ImageSampling {
    #[default]
    Smooth,
    Nearest,
}

impl ImageSampling {
    pub fn label(self) -> &'static str {
        match self {
            Self::Smooth => "smooth",
            Self::Nearest => "nearest",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ImageGeometry {
    pub area: Rect,
    pub crop: (u32, u32, u32, u32),
    /// Scaled integer crop extent and its signed origin, in terminal cells.
    /// The host pads/clips this content to `area`, rather than stretching it.
    pub content: (f64, f64),
    pub offset: (f64, f64),
    pub scale: f64,
    source: (u32, u32),
    cell: (f64, f64),
    bounds: Rect,
}

/// A lifetime belongs to one inspector, so a host can discard work after its
/// tab closes without knowing anything about workbench tabs or modal routing.
#[derive(Debug, Default)]
pub struct ImageViewport {
    centre: Option<(f64, f64)>,
    scale: Option<f64>,
    pub sampling: ImageSampling,
    lifetime: Arc<()>,
    geometry: Option<ImageGeometry>,
    drag: Option<(u16, u16)>,
}

impl ImageViewport {
    pub fn lifetime(&self) -> Weak<()> {
        Arc::downgrade(&self.lifetime)
    }

    pub fn geometry(&self) -> Option<&ImageGeometry> {
        self.geometry.as_ref()
    }

    pub fn clear_geometry(&mut self) {
        self.geometry = None;
        self.drag = None;
        self.lifetime = Arc::new(());
    }

    pub fn place(
        &mut self,
        source: (u32, u32),
        cell: (f64, f64),
        area: Rect,
    ) -> Option<ImageGeometry> {
        if source.0 == 0
            || source.1 == 0
            || area.is_empty()
            || !cell.0.is_finite()
            || !cell.1.is_finite()
            || cell.0 <= 0.0
            || cell.1 <= 0.0
        {
            self.clear_geometry();
            return None;
        }
        let (width, height) = (f64::from(source.0), f64::from(source.1));
        let fit = fit_scale(source, cell, area);
        let scale = self
            .scale
            .unwrap_or(fit)
            .clamp(fit.min(MIN_SCALE), MAX_SCALE);
        let visible_width = (width * scale).min(f64::from(area.width) * cell.0);
        let visible_height = (height * scale).min(f64::from(area.height) * cell.1);
        let cols = (visible_width / cell.0)
            .ceil()
            .clamp(1.0, f64::from(area.width)) as u16;
        let rows = (visible_height / cell.1)
            .ceil()
            .clamp(1.0, f64::from(area.height)) as u16;
        let half_width = (visible_width / scale / 2.0).min(width / 2.0);
        let half_height = (visible_height / scale / 2.0).min(height / 2.0);
        let (cx, cy) = self.centre.unwrap_or((width / 2.0, height / 2.0));
        let cx = cx.clamp(half_width, width - half_width);
        let cy = cy.clamp(half_height, height - half_height);
        self.centre = Some((cx, cy));
        let left = cx - half_width;
        let top = cy - half_height;
        let x = left.floor() as u32;
        let y = top.floor() as u32;
        let crop_width = ((cx + half_width).ceil().min(width) as u32)
            .saturating_sub(x)
            .max(1);
        let crop_height = ((cy + half_height).ceil().min(height) as u32)
            .saturating_sub(y)
            .max(1);
        let padding_x = (f64::from(cols) * cell.0 - visible_width) / 2.0;
        let padding_y = (f64::from(rows) * cell.1 - visible_height) / 2.0;
        let geometry = ImageGeometry {
            area: Rect::new(
                area.x + (area.width - cols) / 2,
                area.y + (area.height - rows) / 2,
                cols,
                rows,
            ),
            crop: (x, y, crop_width, crop_height),
            content: (
                f64::from(crop_width) * scale / cell.0,
                f64::from(crop_height) * scale / cell.1,
            ),
            offset: (
                (padding_x + (f64::from(x) - left) * scale) / cell.0,
                (padding_y + (f64::from(y) - top) * scale) / cell.1,
            ),
            scale,
            source,
            cell,
            bounds: area,
        };
        self.geometry = Some(geometry.clone());
        Some(geometry)
    }

    fn zoom(&mut self, factor: f64, anchor: Option<(u16, u16)>) {
        let Some(g) = &self.geometry else { return };
        let scale = (g.scale * factor).clamp(
            fit_scale(g.source, g.cell, g.bounds).min(MIN_SCALE),
            MAX_SCALE,
        );
        if let Some((x, y)) = anchor {
            let dx = f64::from(x) + 0.5 - (f64::from(g.area.x) + f64::from(g.area.width) / 2.0);
            let dy = f64::from(y) + 0.5 - (f64::from(g.area.y) + f64::from(g.area.height) / 2.0);
            if let Some((cx, cy)) = self.centre {
                self.centre = Some((
                    cx + dx * g.cell.0 * (1.0 / g.scale - 1.0 / scale),
                    cy + dy * g.cell.1 * (1.0 / g.scale - 1.0 / scale),
                ));
            }
        }
        self.scale = Some(scale);
    }

    fn changed(&mut self) {
        self.lifetime = Arc::new(());
        if let Some(g) = self.geometry.clone() {
            self.place(g.source, g.cell, g.bounds);
        }
    }

    pub fn scroll(&mut self, column: u16, row: u16, steps: f64) -> bool {
        if self
            .geometry
            .as_ref()
            .is_none_or(|g| !g.area.contains((column, row).into()))
        {
            return false;
        }
        self.zoom(ZOOM_STEP.powf(steps), Some((column, row)));
        self.changed();
        true
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        if let (Some(g), Some((cx, cy))) = (&self.geometry, self.centre) {
            self.centre = Some((
                (cx + dx).clamp(0.0, f64::from(g.source.0)),
                (cy + dy).clamp(0.0, f64::from(g.source.1)),
            ));
        }
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> bool {
        if event.kind == KeyEventKind::Release {
            return false;
        }
        self.drag = None;
        if keys::IMAGE_ZOOM_IN.matches(event)
            || keys::IMAGE_ZOOM_IN_ALT.matches(event)
            || (event.code == KeyCode::Char('+') && event.modifiers == KeyModifiers::SHIFT)
        {
            self.zoom(ZOOM_STEP, None);
        } else if keys::IMAGE_ZOOM_OUT.matches(event) {
            self.zoom(1.0 / ZOOM_STEP, None);
        } else if keys::IMAGE_FIT.matches(event) {
            self.scale = None;
            self.centre = None;
        } else if keys::IMAGE_ACTUAL.matches(event) {
            self.scale = Some(1.0);
        } else if keys::IMAGE_SAMPLING.matches(event) {
            self.sampling = match self.sampling {
                ImageSampling::Smooth => ImageSampling::Nearest,
                ImageSampling::Nearest => ImageSampling::Smooth,
            };
        } else {
            let Some(g) = &self.geometry else {
                return false;
            };
            let (dx, dy) = if keys::IMAGE_LEFT.matches(event) {
                (-1.0, 0.0)
            } else if keys::IMAGE_RIGHT.matches(event) {
                (1.0, 0.0)
            } else if keys::IMAGE_UP.matches(event) {
                (0.0, -1.0)
            } else if keys::IMAGE_DOWN.matches(event) {
                (0.0, 1.0)
            } else {
                return false;
            };
            self.pan(
                dx * f64::from(g.crop.2) * PAN_FRACTION,
                dy * f64::from(g.crop.3) * PAN_FRACTION,
            );
        }
        self.changed();
        true
    }

    /// Returns whether the picture owns this gesture, including a drag's tail
    /// outside its bounds. The modal alone decides what an unclaimed click does.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let at = (event.column, event.row);
        let inside = self
            .geometry
            .as_ref()
            .is_some_and(|g| g.area.contains(at.into()));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = inside.then_some(at);
                inside
            }
            MouseEventKind::Drag(MouseButton::Left) if self.drag.is_some() => {
                let previous = self.drag.replace(at).unwrap();
                if let Some(g) = &self.geometry {
                    self.pan(
                        (f64::from(previous.0) - f64::from(at.0)) * g.cell.0 / g.scale,
                        (f64::from(previous.1) - f64::from(at.1)) * g.cell.1 / g.scale,
                    );
                }
                self.changed();
                true
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag.take().is_some(),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside => self.scroll(
                at.0,
                at.1,
                if event.kind == MouseEventKind::ScrollUp {
                    1.0
                } else {
                    -1.0
                },
            ),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageSampling, ImageViewport, MAX_SCALE, MIN_SCALE};
    use crate::keys;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use test_case::test_case;

    const SOURCE: (u32, u32) = (1600, 1600);
    const CELL: (f64, f64) = (8.0, 16.0);
    const AREA: Rect = Rect::new(10, 5, 80, 40);

    fn viewport() -> ImageViewport {
        let mut view = ImageViewport::default();
        view.place(SOURCE, CELL, AREA).unwrap();
        view
    }

    #[test_case((1600, 1600), (80, 40); "square")]
    #[test_case((1600, 800), (80, 20); "wide")]
    #[test_case((800, 1600), (40, 40); "tall")]
    #[test_case((1, 1), (1, 1); "single_pixel")]
    #[test_case((8_000_000, 1), (80, 1); "extremely_wide")]
    fn fit_preserves_aspect(source: (u32, u32), size: (u16, u16)) {
        let g = ImageViewport::default().place(source, CELL, AREA).unwrap();
        assert_eq!((g.area.width, g.area.height), size);
        assert_eq!(g.crop, (0, 0, source.0, source.1));
    }

    #[test]
    fn fit_actual_and_sampling_are_local() {
        let mut first = viewport();
        let second = viewport();
        first.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        first.handle_key(keys::IMAGE_SAMPLING.to_key_event());
        let g = first.geometry().unwrap();
        assert_eq!(g.scale, 1.0);
        assert_eq!((g.crop.2, g.crop.3), (640, 640));
        assert_eq!(first.sampling, ImageSampling::Nearest);
        assert_eq!(second.sampling, ImageSampling::Smooth);
        assert_eq!(second.geometry().unwrap().scale, 0.4);
        first.handle_key(keys::IMAGE_FIT.to_key_event());
        assert_eq!(first.geometry().unwrap().crop, (0, 0, SOURCE.0, SOURCE.1));
    }

    #[test]
    fn pointer_zoom_keeps_the_source_under_the_pointer() {
        let mut view = viewport();
        view.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        let at = (30, 15);
        let before = view.geometry().unwrap().clone();
        view.scroll(at.0, at.1, 1.0);
        let after = view.geometry().unwrap();
        let source_at = |g: &super::ImageGeometry| {
            (
                f64::from(g.crop.0)
                    + (f64::from(at.0 - g.area.x) + 0.5 - g.offset.0) * CELL.0 / g.scale,
                f64::from(g.crop.1)
                    + (f64::from(at.1 - g.area.y) + 0.5 - g.offset.1) * CELL.1 / g.scale,
            )
        };
        let (a, b) = (source_at(&before), source_at(after));
        assert!((a.0 - b.0).abs() <= 1.0 && (a.1 - b.1).abs() <= 1.0);
    }

    #[test_case(-100_000.0, -100_000.0, (0, 0); "top_left")]
    #[test_case(100_000.0, 100_000.0, (960, 960); "bottom_right")]
    fn pan_clamps_at_source_edges(dx: f64, dy: f64, expected: (u32, u32)) {
        let mut view = viewport();
        view.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        view.pan(dx, dy);
        view.changed();
        let crop = view.geometry().unwrap().crop;
        assert_eq!((crop.0, crop.1), expected);
    }

    #[test]
    fn resize_preserves_source_centre_and_bounds_zoom() {
        let mut view = viewport();
        view.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        view.handle_key(keys::IMAGE_RIGHT.to_key_event());
        let centre = view.centre;
        view.place(SOURCE, CELL, Rect::new(0, 0, 20, 10));
        assert_eq!(view.centre, centre);
        view.zoom(f64::MAX, None);
        view.changed();
        assert_eq!(view.geometry().unwrap().scale, MAX_SCALE);
        view.zoom(0.0, None);
        view.changed();
        assert_eq!(view.geometry().unwrap().scale, MIN_SCALE);
        assert!(view.place(SOURCE, CELL, Rect::ZERO).is_none());
    }

    #[test]
    fn a_drag_owns_its_release_outside_the_picture() {
        let mut view = viewport();
        let event = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(view.handle_mouse(event(
            MouseEventKind::Down(MouseButton::Left),
            AREA.x,
            AREA.y
        )));
        assert!(view.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), 0, 0)));
        assert!(view.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 0, 0)));
        assert!(!view.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 0, 0)));
    }

    #[test]
    fn changing_or_closing_a_view_expires_previous_work() {
        let mut view = viewport();
        let before = view.lifetime();
        view.handle_key(keys::IMAGE_ZOOM_IN.to_key_event());
        assert!(before.upgrade().is_none());
        let before = view.lifetime();
        view.clear_geometry();
        assert!(before.upgrade().is_none());
    }

    #[test_case((100, 100), (13, 7), (100.0, 100.0), (2.0, 6.0); "partial_cells")]
    #[test_case((9, 9), (2, 1), (9.0, 9.0), (3.5, 3.5); "small_image")]
    fn actual_size_pads_partial_cells_instead_of_stretching(
        source: (u32, u32),
        cells: (u16, u16),
        content: (f64, f64),
        offset: (f64, f64),
    ) {
        let mut view = ImageViewport::default();
        view.handle_key(keys::IMAGE_ACTUAL.to_key_event());
        let g = view.place(source, CELL, AREA).unwrap();
        assert_eq!((g.area.width, g.area.height), cells);
        assert_eq!((g.content.0 * CELL.0, g.content.1 * CELL.1), content);
        assert_eq!((g.offset.0 * CELL.0, g.offset.1 * CELL.1), offset);
    }

    #[test]
    fn high_zoom_accumulates_fractional_source_pixel_drags() {
        let mut view = viewport();
        view.scale = Some(MAX_SCALE);
        view.changed();
        let before = view.centre.unwrap();
        let crop_before = view.geometry().unwrap().crop;
        let event = |kind, column| MouseEvent {
            kind,
            column,
            row: AREA.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        let start = AREA.x + 1;
        view.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), start));
        for column in start + 1..=start + 8 {
            view.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), column));
        }
        assert_eq!(view.centre.unwrap(), (before.0 - 1.0, before.1));
        assert_eq!(view.geometry().unwrap().crop.0, crop_before.0 - 1);
    }

    #[test]
    fn fractional_crop_offsets_preserve_magnified_scale() {
        let mut view = viewport();
        view.scale = Some(MAX_SCALE);
        view.changed();
        view.pan(-0.125, 0.0);
        view.changed();
        let g = view.geometry().unwrap();
        assert_eq!(g.content.0 * CELL.0, f64::from(g.crop.2) * MAX_SCALE);
        assert_eq!(g.offset.0 * CELL.0, -56.0);
    }
}
