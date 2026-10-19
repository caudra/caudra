use std::env;
use std::fs;
use std::io::Cursor;

use color_eyre::eyre::{Result, eyre};
use crossterm::style::force_color_output;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use super::{ImageLayout, kitty};

const OUTPUT_ENV: &str = "CAUDRA_IMAGE_FIXTURE";
const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const COLS: u16 = 24;
const ROWS: u16 = 8;
const ID: u8 = 211;
const TILE: u32 = 8;
const MAX_CHANNEL: u32 = 255;

#[test]
#[ignore = "writes an opt-in terminal graphics fixture to CAUDRA_IMAGE_FIXTURE"]
fn emit_kitty_compatibility_fixture() -> Result<()> {
    let path =
        env::var_os(OUTPUT_ENV).ok_or_else(|| eyre!("set {OUTPUT_ENV} to the output path"))?;
    let image = RgbaImage::from_fn(WIDTH, HEIGHT, |x, y| {
        let alpha = if x < WIDTH / 2 || (x / TILE + y / TILE).is_multiple_of(2) {
            MAX_CHANNEL
        } else {
            MAX_CHANNEL / 2
        };
        Rgba([
            (x * MAX_CHANNEL / (WIDTH - 1)) as u8,
            (y * MAX_CHANNEL / (HEIGHT - 1)) as u8,
            if (x + y).is_multiple_of(TILE) { 255 } else { 0 },
            alpha as u8,
        ])
    });
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    let decoded = image::load_from_memory(&png)?;
    assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));

    let layout = ImageLayout {
        cols: COLS,
        rows: ROWS,
    };
    let mut output = b"\x1b[2J\x1b[H".to_vec();
    for escape in kitty::transmit(ID, &png, layout) {
        output.extend_from_slice(escape.as_bytes());
    }
    let area = Rect::new(1, 1, COLS, ROWS);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(kitty::placeholder_lines(ID, layout)).render(area, &mut buffer);
    force_color_output(true);
    let mut backend = CrosstermBackend::new(&mut output);
    backend.draw(buffer.content.iter().enumerate().map(|(index, cell)| {
        (
            area.x + index as u16 % COLS,
            area.y + index as u16 / COLS,
            cell,
        )
    }))?;
    backend.flush()?;
    let foreground = format!("\x1b[38;5;{ID}m");
    assert!(
        output
            .windows(foreground.len())
            .any(|bytes| bytes == foreground.as_bytes())
    );
    fs::write(path, output)?;
    Ok(())
}
