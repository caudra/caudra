//! Kitty graphics through Unicode placeholders. The picture is sent to the
//! terminal once and placed virtually; the transcript then draws ordinary
//! cells, each a placeholder carrying its row and column as diacritics and
//! the image id as its foreground colour, so scrolling and partial redraws
//! need no escape sequences at all.

use std::num::NonZeroUsize;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lru::LruCache;
use ratatui::style::Color;
use ratatui::text::{Line, Span};

use super::{DrawKey, ImageLayout, pixel_style};

const PLACEHOLDER: char = '\u{10EEEE}';
const ESCAPE_START: &str = "\u{1b}_G";
const ESCAPE_END: &str = "\u{1b}\\";
/// Base64 bytes per transmit chunk, the most the protocol allows.
const CHUNK_BYTES: usize = 4096;
/// Ids travel as a 256-colour index, which the colour downgrade leaves alone.
const MAX_IMAGE_ID: u8 = u8::MAX;
const UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Row and column numbers, in the order the protocol assigns them.
const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// The widest and tallest a placement can be: one diacritic per row or column.
pub(super) const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// The escapes that upload `png` as image `id` and place it virtually across
/// `layout`, one string per escape so a multiplexer can wrap each.
pub(super) fn transmit(id: u8, png: &[u8], layout: ImageLayout) -> Vec<String> {
    let encoded = STANDARD.encode(png);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(CHUNK_BYTES).collect();
    let last = chunks.len().saturating_sub(1);
    let mut escapes: Vec<String> = chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let more = u8::from(index != last);
            let payload = std::str::from_utf8(chunk).unwrap_or_default();
            match index {
                0 => format!("{ESCAPE_START}a=t,f=100,q=2,i={id},m={more};{payload}{ESCAPE_END}"),
                _ => format!("{ESCAPE_START}q=2,m={more};{payload}{ESCAPE_END}"),
            }
        })
        .collect();
    escapes.push(format!(
        "{ESCAPE_START}a=p,U=1,q=2,i={id},c={},r={}{ESCAPE_END}",
        layout.cols, layout.rows
    ));
    escapes
}

/// Frees image `id` and its data, before the id is given to another picture.
pub(super) fn delete(id: u8) -> String {
    format!("{ESCAPE_START}a=d,d=I,q=2,i={id}{ESCAPE_END}")
}

/// The cells that show image `id`: every cell names its own row and column,
/// so a redraw of any one of them stands on its own.
pub(super) fn placeholder_lines(id: u8, layout: ImageLayout) -> Vec<Line<'static>> {
    let style = pixel_style().fg(Color::Indexed(id));
    DIACRITICS
        .iter()
        .take(layout.rows.into())
        .map(|row| {
            let cells: String = DIACRITICS
                .iter()
                .take(layout.cols.into())
                .flat_map(|column| [PLACEHOLDER, *row, *column])
                .collect();
            Line::from(Span::styled(cells, style))
        })
        .collect()
}

/// Hands out ids 1..=255, recycling the least recently drawn picture's id
/// once they run out.
pub(super) struct KittyIds {
    assigned: LruCache<u8, DrawKey>,
}

impl KittyIds {
    pub fn new() -> Self {
        Self {
            assigned: LruCache::new(NonZeroUsize::new(MAX_IMAGE_ID.into()).unwrap()),
        }
    }

    /// An id for `key`, plus the picture that held it before, which the
    /// caller has to delete from the terminal and forget.
    pub fn assign(&mut self, key: DrawKey) -> (u8, Option<DrawKey>) {
        if self.assigned.len() < self.assigned.cap().get() {
            let id = (1..=MAX_IMAGE_ID)
                .find(|id| !self.assigned.contains(id))
                .expect("a non-full cache has a free id");
            self.assigned.put(id, key);
            return (id, None);
        }
        let (id, evicted) = self
            .assigned
            .pop_lru()
            .expect("a full cache has a least recent entry");
        self.assigned.put(id, key);
        (id, Some(evicted))
    }

    /// Marks `id` as just drawn, so it is the last to be recycled.
    pub fn touch(&mut self, id: u8) {
        self.assigned.get(&id);
    }

    pub fn find(&mut self, key: &DrawKey) -> Option<u8> {
        let id = self
            .assigned
            .iter()
            .find_map(|(id, assigned)| (assigned == key).then_some(*id))?;
        self.touch(id);
        Some(id)
    }

    pub fn clear(&mut self) {
        self.assigned.clear();
    }

    pub fn remove(&mut self, id: u8) {
        self.assigned.pop(&id);
    }

    pub fn make_room(&mut self, key: &DrawKey) -> Vec<(u8, DrawKey)> {
        let bytes = |key: &DrawKey| u64::from(key.target.0) * u64::from(key.target.1) * 4;
        let mut evicted = Vec::new();
        while self.assigned.iter().map(|(_, key)| bytes(key)).sum::<u64>() + bytes(key)
            > UPLOAD_BYTES
        {
            let Some(entry) = self.assigned.pop_lru() else {
                break;
            };
            evicted.push(entry);
        }
        evicted
    }

    pub fn ids(&self) -> Vec<u8> {
        self.assigned.iter().map(|(id, _)| *id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ImageKey, ImageMode};
    use super::{
        CHUNK_BYTES, DIACRITICS, DrawKey, ESCAPE_END, ESCAPE_START, ImageLayout, KittyIds,
        MAX_IMAGE_ID, PLACEHOLDER, delete, placeholder_lines, transmit,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::widgets::{Paragraph, Widget};
    use test_case::test_case;

    const ID: u8 = 7;
    const LAYOUT: ImageLayout = ImageLayout { cols: 3, rows: 2 };

    fn key(n: u64) -> DrawKey {
        DrawKey {
            image: ImageKey::Bytes(n),
            layout: LAYOUT,
            mode: ImageMode::Kitty,
            crop: (0, 0, 3, 2),
            target: (3, 2),
            content: (3, 2),
            offset: (0, 0),
            sampling: caudra_workbench::ImageSampling::Smooth,
        }
    }

    #[test_case(10, 1 ; "one_chunk")]
    #[test_case(CHUNK_BYTES / 4 * 3, 1 ; "exactly_one_chunk")]
    #[test_case(CHUNK_BYTES, 2 ; "spills_into_a_second")]
    #[test_case(CHUNK_BYTES * 2, 3 ; "three_chunks")]
    fn a_transmit_chunks_its_payload_and_ends_with_a_placement(bytes: usize, chunks: usize) {
        let escapes = transmit(ID, &vec![0xAB; bytes], LAYOUT);
        assert_eq!(escapes.len(), chunks + 1);
        assert!(escapes[0].starts_with(&format!("{ESCAPE_START}a=t,f=100,q=2,i={ID},")));
        for (index, escape) in escapes[..chunks].iter().enumerate() {
            let more = if index + 1 == chunks { "m=0;" } else { "m=1;" };
            assert!(escape.contains(more), "chunk {index}: {escape}");
            assert!(escape.ends_with(ESCAPE_END));
            let payload = escape.split_once(';').unwrap().1;
            assert!(payload.len() - ESCAPE_END.len() <= CHUNK_BYTES);
        }
        assert!(escapes[1..].iter().all(|escape| !escape.contains("a=t")));
        assert_eq!(
            escapes[chunks],
            format!("{ESCAPE_START}a=p,U=1,q=2,i={ID},c=3,r=2{ESCAPE_END}")
        );
    }

    #[test_case(0, 0 ; "origin")]
    #[test_case(1, 2 ; "last_cell")]
    fn each_cell_is_one_wide_and_names_its_row_and_column(row: u16, column: u16) {
        let area = Rect::new(0, 0, LAYOUT.cols, LAYOUT.rows);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(placeholder_lines(ID, LAYOUT)).render(area, &mut buffer);
        let cell = &buffer[(column, row)];
        let expected: String = [
            PLACEHOLDER,
            DIACRITICS[usize::from(row)],
            DIACRITICS[usize::from(column)],
        ]
        .into_iter()
        .collect();
        assert_eq!(cell.symbol(), expected);
        assert_eq!(cell.fg, Color::Indexed(ID));
        assert_eq!(
            placeholder_lines(ID, LAYOUT)[0].width(),
            usize::from(LAYOUT.cols)
        );
    }

    #[test]
    fn ids_are_recycled_least_recent_first() {
        let mut ids = KittyIds::new();
        for n in 0..u64::from(MAX_IMAGE_ID) {
            let (id, evicted) = ids.assign(key(n));
            assert_eq!(u64::from(id), n + 1);
            assert!(evicted.is_none());
        }
        ids.touch(1);
        let (id, evicted) = ids.assign(key(1_000));
        assert_eq!(id, 2);
        assert_eq!(evicted, Some(key(1)));
        assert_eq!(
            delete(id),
            format!("{ESCAPE_START}a=d,d=I,q=2,i=2{ESCAPE_END}")
        );
    }
}
