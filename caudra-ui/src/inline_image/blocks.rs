//! Pictures as coloured block characters. Each cell covers a small grid of
//! pixels and shows two colours, so every split of the grid into two groups is
//! tried and the one that loses the least colour is drawn: the glyph lights the
//! first group in the foreground colour over the second as background.

use image::imageops::FilterType;
use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::{ImageLayout, pixel_style};

/// What a cell of one colour shows, its foreground and background alike.
pub(crate) const UPPER_HALF: &str = "▀";
const CLEAR: &str = " ";
const FULL: &str = "█";
const LEFT_HALF: &str = "▌";
const RIGHT_HALF: &str = "▐";
/// Below this a pixel is drawn as the background behind the picture.
const OPAQUE_ALPHA: u8 = 128;
const CELL_COLS: u32 = 2;
/// Quadrant glyphs by mask, bit 0 upper left, 1 upper right, 2 lower left,
/// 3 lower right.
const QUADRANTS: [&str; 16] = [
    CLEAR, "▘", "▝", "▀", "▖", LEFT_HALF, "▞", "▛", "▗", "▚", RIGHT_HALF, "▜", "▄", "▙", "▟", FULL,
];
/// Masks of the sextants Unicode left out of its run because older blocks
/// already draw them: the left and the right column.
const SEXTANT_LEFT: u8 = 0b01_0101;
const SEXTANT_RIGHT: u8 = 0b10_1010;
const SEXTANT_FULL: u8 = 0b11_1111;
/// U+1FB00 to U+1FB3B, every sextant that is not an older block.
const SEXTANT_RUN: &str = concat!(
    "\u{1FB00}\u{1FB01}\u{1FB02}\u{1FB03}\u{1FB04}\u{1FB05}\u{1FB06}\u{1FB07}\u{1FB08}\u{1FB09}",
    "\u{1FB0A}\u{1FB0B}\u{1FB0C}\u{1FB0D}\u{1FB0E}\u{1FB0F}\u{1FB10}\u{1FB11}\u{1FB12}\u{1FB13}",
    "\u{1FB14}\u{1FB15}\u{1FB16}\u{1FB17}\u{1FB18}\u{1FB19}\u{1FB1A}\u{1FB1B}\u{1FB1C}\u{1FB1D}",
    "\u{1FB1E}\u{1FB1F}\u{1FB20}\u{1FB21}\u{1FB22}\u{1FB23}\u{1FB24}\u{1FB25}\u{1FB26}\u{1FB27}",
    "\u{1FB28}\u{1FB29}\u{1FB2A}\u{1FB2B}\u{1FB2C}\u{1FB2D}\u{1FB2E}\u{1FB2F}\u{1FB30}\u{1FB31}",
    "\u{1FB32}\u{1FB33}\u{1FB34}\u{1FB35}\u{1FB36}\u{1FB37}\u{1FB38}\u{1FB39}\u{1FB3A}\u{1FB3B}",
);
const SEXTANT_UTF8_LEN: usize = 4;
const MAX_CELL_PIXELS: usize = 6;

type Rgb = [u8; 3];

/// The pixel grid one cell covers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Glyphs {
    /// 2×2, from Block Elements, which every monospace font has.
    Quadrants,
    /// 2×3, from Symbols for Legacy Computing (Unicode 13), which needs a
    /// font or a terminal that draws them.
    Sextants,
}

impl Glyphs {
    const fn rows(self) -> u32 {
        match self {
            Self::Quadrants => 2,
            Self::Sextants => 3,
        }
    }

    const fn pixels(self) -> usize {
        (CELL_COLS * self.rows()) as usize
    }

    fn symbol(self, mask: u8) -> &'static str {
        match self {
            Self::Quadrants => QUADRANTS[usize::from(mask)],
            Self::Sextants => sextant(mask),
        }
    }
}

/// `image` scaled onto the cells of `layout`, one line per row of cells.
pub(super) fn render(
    image: &DynamicImage,
    layout: ImageLayout,
    glyphs: Glyphs,
) -> Vec<Line<'static>> {
    let dimensions = (
        u32::from(layout.cols) * CELL_COLS,
        u32::from(layout.rows) * glyphs.rows(),
    );
    if (image.width(), image.height()) == dimensions {
        lines(&image.to_rgba8(), glyphs)
    } else {
        lines(
            &image
                .resize_exact(dimensions.0, dimensions.1, FilterType::CatmullRom)
                .to_rgba8(),
            glyphs,
        )
    }
}

fn lines(image: &RgbaImage, glyphs: Glyphs) -> Vec<Line<'static>> {
    let rows = glyphs.rows();
    (0..image.height().div_ceil(rows))
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for col in 0..image.width().div_ceil(CELL_COLS) {
                let mut pixels = [None; MAX_CELL_PIXELS];
                for (index, pixel) in pixels[..glyphs.pixels()].iter_mut().enumerate() {
                    let index = index as u32;
                    let x = col * CELL_COLS + index % CELL_COLS;
                    let y = row * rows + index / CELL_COLS;
                    *pixel = image.get_pixel_checked(x, y).and_then(opaque);
                }
                let (symbol, style) = cell(&pixels[..glyphs.pixels()], glyphs);
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(symbol),
                    _ => spans.push(Span::styled(symbol, style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn opaque(pixel: &Rgba<u8>) -> Option<Rgb> {
    let [r, g, b, a] = pixel.0;
    (a >= OPAQUE_ALPHA).then_some([r, g, b])
}

fn cell(pixels: &[Option<Rgb>], glyphs: Glyphs) -> (&'static str, Style) {
    let style = pixel_style();
    let opaque_mask = mask_of(pixels.iter().map(Option::is_some));
    if opaque_mask == 0 {
        return (CLEAR, style);
    }
    let mut colours = [[0; 3]; MAX_CELL_PIXELS];
    let mut count = 0;
    for colour in pixels.iter().flatten() {
        colours[count] = *colour;
        count += 1;
    }
    if count < pixels.len() {
        let (lit, _) = means(&colours[..count], u8::MAX);
        return (glyphs.symbol(opaque_mask), style.fg(rgb(lit)));
    }
    match best_split(&colours[..count]) {
        (0, colour, _) => (UPPER_HALF, style.fg(rgb(colour)).bg(rgb(colour))),
        (mask, fg, bg) => (glyphs.symbol(mask), style.fg(rgb(fg)).bg(rgb(bg))),
    }
}

/// The split of `pixels` into two groups whose means stray least from them,
/// as the mask of the first group and the two means. Each split is tried once:
/// the last pixel always falls in the second group, so mask 0 is one colour,
/// returned as the first mean.
fn best_split(pixels: &[Rgb]) -> (u8, Rgb, Rgb) {
    let splits = 1u8 << (pixels.len() - 1);
    let mut best = (u64::MAX, 0, [0; 3], [0; 3]);
    for mask in 0..splits {
        let (lit, unlit) = means(pixels, mask);
        let error = pixels
            .iter()
            .enumerate()
            .map(|(index, pixel)| match lit_at(mask, index) {
                true => distance(*pixel, lit),
                false => distance(*pixel, unlit),
            })
            .sum::<u64>();
        if error < best.0 {
            best = (error, mask, lit, unlit);
        }
    }
    match best {
        (_, 0, _, unlit) => (0, unlit, unlit),
        (_, mask, lit, unlit) => (mask, lit, unlit),
    }
}

/// The mean colours of the pixels `mask` lights and of the rest.
fn means(pixels: &[Rgb], mask: u8) -> (Rgb, Rgb) {
    let mut sums = [[0u32; 3]; 2];
    let mut counts = [0u32; 2];
    for (index, pixel) in pixels.iter().enumerate() {
        let group = usize::from(!lit_at(mask, index));
        counts[group] += 1;
        for (sum, channel) in sums[group].iter_mut().zip(pixel) {
            *sum += u32::from(*channel);
        }
    }
    let mean = |group: usize| {
        let count = counts[group].max(1);
        sums[group].map(|sum| ((sum + count / 2) / count) as u8)
    };
    (mean(0), mean(1))
}

fn lit_at(mask: u8, index: usize) -> bool {
    mask & (1 << index) != 0
}

fn distance(pixel: Rgb, centre: Rgb) -> u64 {
    pixel
        .iter()
        .zip(centre)
        .map(|(channel, centre)| u64::from(channel.abs_diff(centre)).pow(2))
        .sum()
}

fn mask_of(bits: impl Iterator<Item = bool>) -> u8 {
    bits.enumerate()
        .fold(0, |mask, (index, set)| mask | (u8::from(set) << index))
}

fn rgb([r, g, b]: Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// The sextant glyph lighting `mask`, bit 0 upper left through bit 5 lower
/// right. Unicode numbers them in mask order from U+1FB00, skipping the four
/// masks older blocks already draw.
fn sextant(mask: u8) -> &'static str {
    match mask {
        0 => CLEAR,
        SEXTANT_LEFT => LEFT_HALF,
        SEXTANT_RIGHT => RIGHT_HALF,
        SEXTANT_FULL => FULL,
        _ => {
            let skipped = u8::from(mask > SEXTANT_LEFT) + u8::from(mask > SEXTANT_RIGHT);
            let start = usize::from(mask - 1 - skipped) * SEXTANT_UTF8_LEN;
            &SEXTANT_RUN[start..start + SEXTANT_UTF8_LEN]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CLEAR, FULL, Glyphs, LEFT_HALF, RIGHT_HALF, UPPER_HALF, lines, render, sextant};
    use crate::inline_image::ImageLayout;
    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::widgets::{Paragraph, Widget};
    use test_case::test_case;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const CLEAR_PIXEL: [u8; 4] = [9, 9, 9, 0];

    fn rgb(pixel: [u8; 4]) -> Color {
        Color::Rgb(pixel[0], pixel[1], pixel[2])
    }

    /// A picture of `rows` pixel rows, written top to bottom.
    fn picture(rows: &[&[[u8; 4]]]) -> RgbaImage {
        RgbaImage::from_fn(rows[0].len() as u32, rows.len() as u32, |x, y| {
            Rgba(rows[y as usize][x as usize])
        })
    }

    fn first_cell(image: &RgbaImage, glyphs: Glyphs) -> (String, Color, Color) {
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines(image, glyphs)).render(area, &mut buffer);
        let cell = &buffer[(0, 0)];
        (cell.symbol().to_owned(), cell.fg, cell.bg)
    }

    #[test_case(&[&[RED, BLUE], &[BLUE, BLUE]], "▘", rgb(RED), rgb(BLUE) ; "one_corner")]
    #[test_case(&[&[RED, BLUE], &[BLUE, RED]], "▞", rgb(BLUE), rgb(RED) ; "diagonal")]
    #[test_case(&[&[RED, RED], &[BLUE, BLUE]], UPPER_HALF, rgb(RED), rgb(BLUE) ; "top_over_bottom")]
    #[test_case(&[&[RED, RED], &[RED, RED]], UPPER_HALF, rgb(RED), rgb(RED) ; "one_colour")]
    #[test_case(&[&[CLEAR_PIXEL, RED], &[RED, RED]], "▟", rgb(RED), Color::Reset ; "clear_corner")]
    #[test_case(&[&[CLEAR_PIXEL, CLEAR_PIXEL], &[CLEAR_PIXEL, CLEAR_PIXEL]], CLEAR, Color::Reset, Color::Reset ; "all_clear")]
    fn a_quadrant_cell_splits_its_pixels_into_two_colours(
        rows: &[&[[u8; 4]]],
        symbol: &str,
        fg: Color,
        bg: Color,
    ) {
        assert_eq!(
            first_cell(&picture(rows), Glyphs::Quadrants),
            (symbol.to_owned(), fg, bg)
        );
    }

    #[test]
    fn a_sextant_cell_draws_three_rows_of_pixels() {
        let image = picture(&[&[RED, BLUE], &[RED, BLUE], &[RED, BLUE]]);
        assert_eq!(
            first_cell(&image, Glyphs::Sextants),
            (LEFT_HALF.to_owned(), rgb(RED), rgb(BLUE))
        );
    }

    #[test_case(0, CLEAR ; "none")]
    #[test_case(1, "\u{1FB00}" ; "first")]
    #[test_case(20, "\u{1FB13}" ; "before_left_column")]
    #[test_case(21, LEFT_HALF ; "left_column")]
    #[test_case(22, "\u{1FB14}" ; "after_left_column")]
    #[test_case(42, RIGHT_HALF ; "right_column")]
    #[test_case(43, "\u{1FB28}" ; "after_right_column")]
    #[test_case(62, "\u{1FB3B}" ; "last")]
    #[test_case(63, FULL ; "full")]
    fn sextants_follow_unicode_order(mask: u8, expected: &str) {
        assert_eq!(sextant(mask), expected);
    }

    #[test_case(Glyphs::Quadrants ; "quadrants")]
    #[test_case(Glyphs::Sextants ; "sextants")]
    fn a_picture_fills_exactly_its_layout(glyphs: Glyphs) {
        const LAYOUT: ImageLayout = ImageLayout { cols: 5, rows: 3 };
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(40, 30, Rgba(RED)));
        let lines = render(&image, LAYOUT, glyphs);
        assert_eq!(lines.len(), usize::from(LAYOUT.rows));
        for line in lines {
            assert_eq!(line.width(), usize::from(LAYOUT.cols));
        }
    }

    #[test]
    fn neighbouring_cells_of_one_colour_share_a_span() {
        let image = RgbaImage::from_pixel(6, 2, Rgba(RED));
        let lines = lines(&image, Glyphs::Quadrants);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content, UPPER_HALF.repeat(3));
    }
}
