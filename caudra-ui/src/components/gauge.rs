//! Eighth-cell gauges: a filled run whose last cell fills to the nearest eighth, on a light track.

pub(crate) const CELL_EIGHTHS: usize = 8;
/// A cell filled one eighth to seven eighths.
pub(crate) const PARTIAL_CELLS: [char; 7] = [
    '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
];
pub(crate) const FULL_CELL: char = '\u{2588}';
pub(crate) const EMPTY_CELL: char = '\u{2591}';
pub(crate) const GAUGE_OPEN: &str = "\u{2595}";
pub(crate) const GAUGE_CLOSE: &str = "\u{258f}";

/// `eighths` of fill drawn as cells, and how many cells that takes.
pub(crate) fn fill(eighths: usize) -> (String, usize) {
    let (whole, partial) = (eighths / CELL_EIGHTHS, eighths % CELL_EIGHTHS);
    let mut fill = FULL_CELL.to_string().repeat(whole);
    if let Some(index) = partial.checked_sub(1) {
        fill.push(PARTIAL_CELLS[index]);
    }
    (fill, whole + usize::from(partial > 0))
}

pub(crate) fn track(cells: usize) -> String {
    EMPTY_CELL.to_string().repeat(cells)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::fill;

    #[test_case(0, "", 0 ; "empty")]
    #[test_case(8, "\u{2588}", 1 ; "one_cell")]
    #[test_case(11, "\u{2588}\u{258d}", 2 ; "three_eighths_over")]
    fn fill_rounds_to_eighths(eighths: usize, drawn: &str, cells: usize) {
        assert_eq!(fill(eighths), (drawn.to_owned(), cells));
    }
}
