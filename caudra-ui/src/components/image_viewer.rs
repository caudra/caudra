//! A picture at the largest size the screen allows, opened by clicking one in
//! the transcript or the composer.

use std::path::PathBuf;

use caudra_grab::grab_scope;
use caudra_workbench::ImageViewport;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::keybindings::key;
use super::modal::Modal;
use super::{Hint, HintBar, Overlay};
use crate::inline_image::{self, ImageRef};
use crate::repaint::Cadence;
use crate::theme;

const MODAL_WIDTH_PERCENT: u16 = 90;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 90;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 2;
const UNTITLED: &str = "Image";
pub(crate) const NOT_DRAWN: &str = "This picture cannot be drawn here";

pub(crate) enum ImageViewerAction {
    Consumed,
    Close,
    /// Hand the file to the system's viewer.
    OpenExternal(PathBuf),
    OpenInWorkbench(PathBuf),
    CopyPath(String),
    Passthrough,
}

#[derive(Default)]
pub(crate) struct ImageViewer {
    image: Option<ImageRef>,
    hints: HintBar,
    inspection_hints: HintBar,
    viewport: ImageViewport,
}

impl ImageViewer {
    pub fn open(&mut self, image: ImageRef) {
        self.close();
        self.image = Some(image);
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> ImageViewerAction {
        let Some(image) = &self.image else {
            return ImageViewerAction::Passthrough;
        };
        let path = image.path().map(PathBuf::from);
        if key::ESC.matches(event) {
            return ImageViewerAction::Close;
        }
        if self.viewport.handle_key(event) {
            return ImageViewerAction::Consumed;
        }
        match path {
            Some(path) if key::IMAGE_OPEN_EXTERNAL.matches(event) && can_open_externally() => {
                ImageViewerAction::OpenExternal(path)
            }
            Some(path) if key::IMAGE_OPEN_WORKBENCH.matches(event) => {
                ImageViewerAction::OpenInWorkbench(path)
            }
            Some(path) if key::IMAGE_COPY_PATH.matches(event) => {
                ImageViewerAction::CopyPath(path.display().to_string())
            }
            _ => ImageViewerAction::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ImageViewerAction {
        if self.image.is_none() {
            return ImageViewerAction::Passthrough;
        }
        let hint = self.hints.handle_mouse(event);
        let inspection = self.inspection_hints.handle_mouse(event);
        if let Some(key) = hint.or(inspection) {
            return self.handle_key(key);
        }
        if (self.hints.hovered().is_some() || self.inspection_hints.hovered().is_some())
            && event.kind == MouseEventKind::Down(MouseButton::Left)
        {
            return ImageViewerAction::Consumed;
        }
        if self.viewport.handle_mouse(event) {
            return ImageViewerAction::Consumed;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            return ImageViewerAction::Close;
        }
        ImageViewerAction::Consumed
    }

    pub fn scroll(&mut self, column: u16, row: u16, delta: i32) {
        self.viewport.scroll(column, row, f64::from(delta));
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Some(image) = &self.image else {
            return Rect::default();
        };
        grab_scope!("image_viewer", area);
        let title = format!(" {} ", title(image));
        let modal = Modal {
            title: &title,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let desired = area
            .height
            .saturating_mul(MODAL_MAX_HEIGHT_PERCENT)
            .div_ceil(100)
            .saturating_sub(2);
        let (popup, inner) = modal.render(frame, area, desired);
        let [meta_area, picture_area, hint_area] = Layout::vertical([
            Constraint::Length(META_ROWS),
            Constraint::Min(1),
            Constraint::Length(HINT_ROWS),
        ])
        .areas(inner);

        let mut meta = meta_line(image);
        match inline_image::draw_viewport(image, &mut self.viewport, picture_area) {
            Some(picture) => {
                meta.spans.insert(
                    1,
                    Span::styled(format!("{}  ", picture.status), theme::current().item_desc),
                );
                frame.render_widget(Paragraph::new(picture.lines), picture.geometry.area);
            }
            None => frame.render_widget(
                Paragraph::new(Span::styled(NOT_DRAWN, theme::current().tool_dim)),
                picture_area,
            ),
        }
        frame.render_widget(Paragraph::new(meta), meta_area);
        let [inspection_area, actions_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(hint_area);
        self.inspection_hints.draw(
            frame,
            inspection_area,
            vec![
                Hint::bind(key::IMAGE_ZOOM_IN, "zoom"),
                Hint::bind(key::IMAGE_ZOOM_OUT, "out"),
                Hint::bind(key::IMAGE_FIT, "fit"),
                Hint::bind(key::IMAGE_ACTUAL, "actual"),
                Hint::bind(key::IMAGE_SAMPLING, "sampling"),
            ],
        );
        let mut hints = Vec::new();
        if image.path().is_some() {
            if can_open_externally() {
                hints.push(Hint::bind(key::IMAGE_OPEN_EXTERNAL, "open"));
            }
            hints.push(Hint::bind(key::IMAGE_OPEN_WORKBENCH, "workbench"));
            hints.push(Hint::bind(key::IMAGE_COPY_PATH, "copy path"));
        }
        hints.push(Hint::bind(key::ESC, "close"));
        self.hints.draw(frame, actions_area, hints);
        popup
    }
}

impl Overlay for ImageViewer {
    fn is_open(&self) -> bool {
        self.image.is_some()
    }

    fn close(&mut self) {
        self.image = None;
        self.viewport = ImageViewport::default();
        self.hints.reset();
        self.inspection_hints.reset();
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(
            self.is_open() && inline_image::is_pending(),
            Cadence::PENDING,
        )
    }
}

fn can_open_externally() -> bool {
    crate::terminal::local_url_opener_available()
}

fn title(image: &ImageRef) -> &str {
    match image.alt() {
        "" => image
            .path()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or(UNTITLED),
        alt => alt,
    }
}

fn meta_line(image: &ImageRef) -> Line<'static> {
    let (width, height) = image.pixels();
    let mut spans = vec![Span::styled(
        format!(" {width}×{height} "),
        theme::current().active,
    )];
    if let Some(path) = image.path() {
        spans.push(Span::styled(
            path.display().to_string(),
            theme::current().item_desc,
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::{ImageRef, ImageViewer, ImageViewerAction, KeyEvent, key};
    use crate::components::Overlay;
    use crate::components::keybindings::Bind;
    use crate::inline_image::tests::{PIXEL_SYMBOL, decode_queued, png_bytes, png_source};
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use tempfile::TempDir;
    use test_case::test_case;

    const ALT: &str = "A plot";
    const FILE: &str = "plot.png";
    const SCREEN: (u16, u16) = (60, 30);
    const NO_PICTURE: &str = "the viewer drew no picture once the decode landed";

    fn mouse(viewer: &mut ImageViewer, kind: MouseEventKind, at: (u16, u16)) -> ImageViewerAction {
        viewer.handle_mouse(MouseEvent {
            kind,
            column: at.0,
            row: at.1,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn paint(viewer: &mut ImageViewer) -> Rect {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN.0, SCREEN.1)).unwrap();
        let mut popup = Rect::ZERO;
        terminal
            .draw(|frame| {
                popup = viewer.view(frame, frame.area());
            })
            .unwrap();
        popup
    }

    #[test_case(false; "loading")]
    #[test_case(true; "decoded")]
    fn fresh_clicks_use_actual_image_bounds(decoded: bool) {
        let (_dir, mut viewer) = file_viewer();
        let popup = paint(&mut viewer);
        if decoded {
            decode_queued();
            paint(&mut viewer);
        }
        let image = viewer.viewport.geometry().unwrap().area;
        assert!(matches!(
            mouse(
                &mut viewer,
                MouseEventKind::Down(MouseButton::Left),
                (image.x, image.y)
            ),
            ImageViewerAction::Consumed
        ));
        for at in [
            (0, 0),
            (popup.x, popup.y),
            (image.x - 1, image.y),
            (image.right(), image.y),
        ] {
            assert!(matches!(
                mouse(&mut viewer, MouseEventKind::Down(MouseButton::Left), at),
                ImageViewerAction::Close
            ));
        }
    }

    #[test]
    fn dragging_outside_never_dismisses_and_hints_win() {
        let (_dir, mut viewer) = file_viewer();
        paint(&mut viewer);
        let image = viewer.viewport.geometry().unwrap().area;
        mouse(
            &mut viewer,
            MouseEventKind::Down(MouseButton::Left),
            (image.x, image.y),
        );
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollUp,
            MouseEventKind::Down(MouseButton::Right),
        ] {
            assert!(matches!(
                mouse(&mut viewer, kind, (0, 0)),
                ImageViewerAction::Consumed
            ));
        }
        let hint = viewer.inspection_hints.hits.hit(0);
        assert!(!hint.is_empty());
        let before = viewer.viewport.geometry().unwrap().scale;
        assert!(matches!(
            mouse(
                &mut viewer,
                MouseEventKind::Down(MouseButton::Left),
                (hint.x, hint.y)
            ),
            ImageViewerAction::Consumed
        ));
        assert!(matches!(
            mouse(
                &mut viewer,
                MouseEventKind::Up(MouseButton::Left),
                (hint.x, hint.y)
            ),
            ImageViewerAction::Consumed
        ));
        assert!(viewer.viewport.geometry().unwrap().scale > before);
    }

    #[test]
    fn close_reopen_resets_hits_zoom_and_sampling() {
        let (_dir, mut viewer) = file_viewer();
        paint(&mut viewer);
        press(&mut viewer, KeyCode::Char('+'));
        press(&mut viewer, KeyCode::Char('s'));
        viewer.close();
        viewer.open(ImageRef::from_source(&png_source(8, 8), ALT).unwrap());
        assert!(viewer.viewport.geometry().is_none());
        assert_eq!(
            viewer.viewport.sampling,
            caudra_workbench::ImageSampling::Smooth
        );
        assert!(matches!(
            mouse(&mut viewer, MouseEventKind::Down(MouseButton::Left), (0, 0)),
            ImageViewerAction::Close
        ));
    }

    #[test]
    fn failed_image_has_no_hit_rectangle() {
        let (dir, mut viewer) = file_viewer();
        paint(&mut viewer);
        std::fs::write(dir.path().join(FILE), b"not an image").unwrap();
        decode_queued();
        paint(&mut viewer);
        assert!(viewer.viewport.geometry().is_none());
        assert!(matches!(
            mouse(
                &mut viewer,
                MouseEventKind::Down(MouseButton::Left),
                (SCREEN.0 / 2, SCREEN.1 / 2)
            ),
            ImageViewerAction::Close
        ));
    }

    fn press(viewer: &mut ImageViewer, code: KeyCode) -> ImageViewerAction {
        viewer.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn file_viewer() -> (TempDir, ImageViewer) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, png_bytes(40, 40)).unwrap();
        let mut viewer = ImageViewer::default();
        viewer.open(ImageRef::from_file(&path, ALT).unwrap());
        (dir, viewer)
    }

    #[test]
    fn escape_closes() {
        let (_dir, mut viewer) = file_viewer();
        assert!(matches!(
            press(&mut viewer, KeyCode::Esc),
            ImageViewerAction::Close
        ));
    }

    #[test_case(key::IMAGE_COPY_PATH ; "copy_path")]
    #[test_case(key::IMAGE_OPEN_WORKBENCH ; "workbench")]
    fn a_file_offers_its_path(bind: Bind) {
        let (dir, mut viewer) = file_viewer();
        let expected = dir.path().join(FILE);
        match press(&mut viewer, bind.code) {
            ImageViewerAction::CopyPath(text) => assert_eq!(text, expected.display().to_string()),
            ImageViewerAction::OpenInWorkbench(path) => assert_eq!(path, expected),
            _ => panic!("the key did nothing"),
        }
    }

    #[test]
    fn pasted_bytes_have_no_path_to_offer() {
        let mut viewer = ImageViewer::default();
        viewer.open(ImageRef::from_source(&png_source(8, 8), ALT).unwrap());
        assert!(matches!(
            press(&mut viewer, key::IMAGE_COPY_PATH.code),
            ImageViewerAction::Consumed
        ));
    }

    #[test]
    fn the_picture_is_drawn_inside_the_modal() {
        let (_dir, mut viewer) = file_viewer();
        let mut terminal = Terminal::new(TestBackend::new(SCREEN.0, SCREEN.1)).unwrap();
        terminal
            .draw(|frame| {
                viewer.view(frame, frame.area());
            })
            .unwrap();
        decode_queued();
        terminal
            .draw(|frame| {
                viewer.view(frame, frame.area());
            })
            .unwrap();
        let pixels = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == PIXEL_SYMBOL)
            .count();
        assert!(pixels > 0, "{NO_PICTURE}");
    }
}
