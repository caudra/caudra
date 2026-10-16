//! A picture at the largest size the screen allows, opened by clicking one in
//! the transcript or the composer.

use std::path::PathBuf;

use caudra_grab::grab_scope;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::keybindings::key;
use super::modal::Modal;
use super::{Hint, HintBar, Overlay};
use crate::inline_image::{self, ImageRef};
use crate::theme;

const MODAL_WIDTH_PERCENT: u16 = 90;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 90;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
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
}

impl ImageViewer {
    pub fn open(&mut self, image: ImageRef) {
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
        match self.hints.handle_mouse(event) {
            Some(key) => self.handle_key(key),
            None => ImageViewerAction::Consumed,
        }
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

        frame.render_widget(Paragraph::new(meta_line(image)), meta_area);
        match inline_image::draw_fitted(image, picture_area.width, picture_area.height) {
            Some(lines) => {
                let width = lines.iter().map(Line::width).max().unwrap_or_default() as u16;
                let height = lines.len() as u16;
                let [_, column, _] = Layout::horizontal([
                    Constraint::Fill(1),
                    Constraint::Length(width),
                    Constraint::Fill(1),
                ])
                .areas(picture_area);
                let [_, centred, _] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(height),
                    Constraint::Fill(1),
                ])
                .areas(column);
                frame.render_widget(Paragraph::new(lines), centred);
            }
            None => frame.render_widget(
                Paragraph::new(Span::styled(NOT_DRAWN, theme::current().tool_dim)),
                picture_area,
            ),
        }
        let mut hints = Vec::new();
        if image.path().is_some() {
            if can_open_externally() {
                hints.push(Hint::bind(key::IMAGE_OPEN_EXTERNAL, "open"));
            }
            hints.push(Hint::bind(key::IMAGE_OPEN_WORKBENCH, "workbench"));
            hints.push(Hint::bind(key::IMAGE_COPY_PATH, "copy path"));
        }
        hints.push(Hint::bind(key::ESC, "close"));
        self.hints.draw(frame, hint_area, hints);
        popup
    }
}

impl Overlay for ImageViewer {
    fn is_open(&self) -> bool {
        self.image.is_some()
    }

    fn close(&mut self) {
        self.image = None;
        self.hints.reset();
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
    use crate::components::keybindings::Bind;
    use crate::inline_image::tests::{PIXEL_SYMBOL, decode_queued, png_bytes, png_source};
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;
    use test_case::test_case;

    const ALT: &str = "A plot";
    const FILE: &str = "plot.png";
    const SCREEN: (u16, u16) = (60, 30);
    const NO_PICTURE: &str = "the viewer drew no picture once the decode landed";

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
