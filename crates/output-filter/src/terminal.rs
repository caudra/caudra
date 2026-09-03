//! Single-row terminal rendering of a captured control stream.
//!
//! A command that redraws a progress bar does not emit lines. It emits a control
//! stream: `\r` returns to column zero, later text overwrites what is already
//! there, and an erase sequence clears the rest of the row. Rust's `str::lines`
//! splits only on `\n`, so a bar that redraws four thousand times arrives as one
//! row hundreds of kilobytes wide, and every line-oriented transformation
//! downstream is defeated by it: per-line truncation keeps the opening frame and
//! discards the final one, and a line cap counts the whole bar as one line.
//!
//! Rendering is therefore decoding rather than filtering, and it is applied
//! unconditionally. The rendered row is what a terminal would display, which is
//! also what the writer intended a reader to see.
//!
//! Two scope decisions are deliberate.
//!
//! Sequences that move between rows are consumed rather than modelled. A screen
//! buffer would need retained state proportional to terminal height, and no
//! observed tool needs one: nested `tqdm` bars reduce to one readable line per
//! redraw target, which the line-collapse stage then handles.
//!
//! Sequences with no cursor meaning, SGR colour above all, are zero width. They
//! are anchored to the column they preceded and replayed at flush, so they never
//! consume a cell and never shift the overwrite alignment of a coloured frame.
//! Deciding whether they should survive at all remains `strip_ansi`'s job.

/// Widest row retained before the canvas is flushed and a new one begun.
///
/// Roughly a hundred times a real terminal width. The bound exists so a single
/// row can never make retained state proportional to stream length; output that
/// is genuinely one row is not expected to reach it.
const ROW_CANVAS_CHARS: usize = 16_384;

/// Zero-width sequence bytes retained for one row.
///
/// Colour changes are re-emitted on every redraw of a row, so this is bounded
/// separately from the canvas. Past the bound the row is still rendered; only
/// further decoration is dropped.
const ROW_MARK_BYTES: usize = 4_096;

/// Longest escape sequence scanned before it is treated as a stray byte.
const MAX_ESCAPE_CHARS: usize = 64;

/// Longest operating-system-command payload scanned before the same.
const MAX_OSC_CHARS: usize = 512;

enum Scan {
    /// The sequence ends before this index.
    Found(usize),
    /// The sequence is split across chunks and needs more input.
    Incomplete,
}

/// Incremental renderer for one output stream.
///
/// Chunk boundaries fall wherever a read happened to land, so a row is routinely
/// split across calls and a control sequence routinely straddles one. Retained
/// state is one row wide, never one stream wide: a bar that redraws for an hour
/// occupies the width of its final frame.
#[derive(Debug, Default)]
pub struct RowRenderer {
    canvas: Vec<char>,
    /// Zero-width sequences to replay before the character at a given column.
    marks: Vec<(usize, String)>,
    mark_bytes: usize,
    column: usize,
    /// Redraws seen in the row being built, used to report what a reader is not
    /// being shown.
    row_redraws: u64,
    /// Whether any cursor control has taken effect in this row. Until one has,
    /// the row is ordinary output and is reproduced byte for byte.
    row_dirty: bool,
    /// Bytes carried across a chunk boundary, awaiting the rest of a sequence.
    partial: String,
    redraws: u64,
}

impl RowRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Redraw frames absorbed so far, across every completed row.
    #[must_use]
    pub const fn redraws(&self) -> u64 {
        self.redraws
    }

    /// Feeds a chunk, appending every row it completes to `out`.
    ///
    /// A trailing partial row stays in the renderer until a newline arrives or
    /// [`RowRenderer::finish`] is called.
    pub fn push(&mut self, text: &str, out: &mut String) {
        let source: Vec<char> = if self.partial.is_empty() {
            text.chars().collect()
        } else {
            let mut joined: Vec<char> = std::mem::take(&mut self.partial).chars().collect();
            joined.extend(text.chars());
            joined
        };

        let mut index = 0;
        while index < source.len() {
            match source[index] {
                '\n' => {
                    self.end_row(out);
                    index += 1;
                }
                '\r' => {
                    let Some(next) = source.get(index + 1) else {
                        // A return at a chunk boundary is not yet classifiable:
                        // followed by a newline it is a line ending, otherwise it
                        // begins a redraw. Wait rather than guess.
                        self.partial.push('\r');
                        return;
                    };
                    if *next == '\n' {
                        // CRLF. The newline ends the row; treating the return as
                        // a redraw would mark every row of a Windows-style
                        // stream as overwritten.
                        index += 1;
                        continue;
                    }
                    self.carriage_return();
                    index += 1;
                }
                '\u{8}' => {
                    if self.column > 0 {
                        self.column -= 1;
                        self.row_dirty = true;
                    }
                    index += 1;
                }
                '\u{1b}' => match scan_escape(&source, index) {
                    Scan::Incomplete => {
                        self.partial = source[index..].iter().collect();
                        return;
                    }
                    Scan::Found(end) => {
                        let raw: String = source[index..end].iter().collect();
                        self.apply_escape(raw);
                        index = end;
                    }
                },
                character => {
                    self.write(character, out);
                    index += 1;
                }
            }
        }
    }

    /// Flushes the row under construction, which a stream that ended without a
    /// trailing newline always has.
    ///
    /// No newline is appended: a stream that did not end with one must not gain
    /// one, or `printf 'x'` would no longer render as what it wrote.
    pub fn finish(&mut self, out: &mut String) {
        let orphan = std::mem::take(&mut self.partial);
        if orphan.starts_with('\u{1b}') {
            // An escape truncated by end of stream is no longer a sequence, but
            // its bytes are still what the command wrote.
            self.push_mark(orphan);
        } else if orphan.starts_with('\r') {
            self.carriage_return();
        }
        if self.canvas.is_empty() && !self.row_dirty {
            return;
        }
        self.flush_row(out);
        // Idempotent: a second call must not emit the row again or count its
        // redraws twice.
        self.reset_row();
    }

    fn carriage_return(&mut self) {
        if !self.canvas.is_empty() {
            self.row_redraws = self.row_redraws.saturating_add(1);
            self.row_dirty = true;
        }
        self.column = 0;
        // The writer is redrawing this row and will re-emit its own decoration,
        // so retaining the previous pass's would duplicate it on every frame.
        self.marks.clear();
        self.mark_bytes = 0;
    }

    fn write(&mut self, character: char, out: &mut String) {
        if self.column >= ROW_CANVAS_CHARS {
            // A row this wide is not a row a terminal ever showed. Emit what is
            // held and continue in a fresh canvas so retained state stays
            // bounded without discarding output.
            self.flush_row(out);
            out.push('\n');
            self.reset_row();
        }
        // A cursor moved beyond the end of the row leaves blanks behind it.
        while self.canvas.len() < self.column {
            self.canvas.push(' ');
        }
        if self.column < self.canvas.len() {
            self.canvas[self.column] = character;
        } else {
            self.canvas.push(character);
        }
        self.column += 1;
    }

    fn end_row(&mut self, out: &mut String) {
        self.flush_row(out);
        out.push('\n');
        self.reset_row();
    }

    fn reset_row(&mut self) {
        self.canvas.clear();
        self.marks.clear();
        self.mark_bytes = 0;
        self.column = 0;
        self.row_redraws = 0;
        self.row_dirty = false;
    }

    fn flush_row(&mut self, out: &mut String) {
        self.redraws = self.redraws.saturating_add(self.row_redraws);
        if self.row_dirty {
            // Padding a shorter frame over a longer one is an artifact of
            // redrawing. Rows never redrawn are reproduced exactly, trailing
            // blanks included, because there the blanks are the output.
            while self.canvas.last() == Some(&' ') {
                self.canvas.pop();
            }
        }
        // Backward cursor motion can anchor a later sequence to an earlier
        // column, so order by column rather than by arrival. The sort is stable,
        // which keeps sequences at one column in the order they were written.
        self.marks.sort_by_key(|(column, _)| *column);
        let mut mark = 0;
        for (column, character) in self.canvas.iter().enumerate() {
            while mark < self.marks.len() && self.marks[mark].0 <= column {
                out.push_str(&self.marks[mark].1);
                mark += 1;
            }
            out.push(*character);
        }
        for (_, raw) in &self.marks[mark..] {
            out.push_str(raw);
        }
    }

    fn push_mark(&mut self, raw: String) {
        if self.mark_bytes.saturating_add(raw.len()) > ROW_MARK_BYTES {
            return;
        }
        self.mark_bytes += raw.len();
        self.marks.push((self.column, raw));
    }

    /// Applies one complete escape sequence.
    ///
    /// Anything not modelled is retained as zero-width decoration rather than
    /// dropped, so a rule that expects to see it still can.
    fn apply_escape(&mut self, raw: String) {
        let Some(body) = raw.strip_prefix("\u{1b}[") else {
            self.push_mark(raw);
            return;
        };
        let Some(final_byte) = body.chars().next_back() else {
            self.push_mark(raw);
            return;
        };
        let parameters = &body[..body.len() - final_byte.len_utf8()];
        // Private-parameter sequences such as `\x1b[?25l` control terminal modes
        // rather than the cursor.
        if parameters.starts_with('?') || parameters.contains(':') {
            self.push_mark(raw);
            return;
        }
        let count = parameters.parse::<usize>().unwrap_or(1).max(1);
        match final_byte {
            'K' => match parameters {
                "" | "0" => {
                    if self.canvas.len() > self.column {
                        self.canvas.truncate(self.column);
                        self.row_dirty = true;
                    }
                }
                "1" => {
                    let end = self.column.min(self.canvas.len());
                    if end > 0 {
                        self.canvas[..end].fill(' ');
                        self.row_dirty = true;
                    }
                }
                "2" => {
                    if !self.canvas.is_empty() {
                        self.canvas.clear();
                        self.row_dirty = true;
                    }
                }
                _ => self.push_mark(raw),
            },
            'C' => self.column = self.column.saturating_add(count).min(ROW_CANVAS_CHARS),
            'D' => self.move_to(self.column.saturating_sub(count)),
            'G' => self.move_to(count.saturating_sub(1).min(ROW_CANVAS_CHARS)),
            // Row-relative movement, out of scope by design. Consumed rather
            // than retained: replaying it would reorder a rendering that no
            // longer has the rows it refers to.
            'A' | 'B' | 'J' => {}
            'E' | 'F' | 'H' | 'f' => self.move_to(0),
            _ => self.push_mark(raw),
        }
    }

    fn move_to(&mut self, column: usize) {
        if column < self.column && column < self.canvas.len() {
            self.row_dirty = true;
        }
        self.column = column;
    }
}

/// Finds the end of the escape sequence beginning at `start`.
fn scan_escape(source: &[char], start: usize) -> Scan {
    let Some(introducer) = source.get(start + 1) else {
        return Scan::Incomplete;
    };
    match introducer {
        '[' => {
            let mut index = start + 2;
            while let Some(character) = source.get(index) {
                // A final byte ends a control sequence; parameter and
                // intermediate bytes precede it.
                if ('\u{40}'..='\u{7e}').contains(character) {
                    return Scan::Found(index + 1);
                }
                index += 1;
                if index - start > MAX_ESCAPE_CHARS {
                    // Not a sequence any tool emits. Treat the introducer as a
                    // stray byte so scanning cannot buffer without bound.
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
        ']' => {
            let mut index = start + 2;
            while let Some(character) = source.get(index) {
                if *character == '\u{7}' {
                    return Scan::Found(index + 1);
                }
                if *character == '\u{1b}' && source.get(index + 1) == Some(&'\\') {
                    return Scan::Found(index + 2);
                }
                index += 1;
                if index - start > MAX_OSC_CHARS {
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
        // Every other escape is two characters wide.
        _ => Scan::Found(start + 2),
    }
}

/// Renders a complete captured stream in one call.
///
/// Returns the rendered text and the number of redraw frames absorbed.
#[must_use]
pub fn render_terminal(text: &str) -> (String, u64) {
    let mut renderer = RowRenderer::new();
    let mut out = String::with_capacity(text.len().min(64 * 1024));
    renderer.push(text, &mut out);
    renderer.finish(&mut out);
    (out, renderer.redraws())
}
