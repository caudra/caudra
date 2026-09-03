//! Row rendering measured against output captured from real tools.
//!
//! Every fixture in `fixtures/progress` was produced by running the generator of
//! the same name in `evals/progress` and recording the exact bytes it wrote to a
//! pipe. Regenerate them with `evals/capture-progress.py`; none is hand-written,
//! because a remembered format is not evidence that a tool emits it.

use workcell_output_filter::{RowRenderer, render_terminal};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/progress/{name}.raw",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

const FIXTURES: [&str; 13] = [
    "bare-cr",
    "bare-cr-erase",
    "cr-only-records",
    "curl-download",
    "go-progressbar",
    "newline-frames",
    "node-ora",
    "numeric-table",
    "repeated-diagnostic",
    "rich-progress",
    "tqdm-nested",
    "tqdm-stderr",
    "tqdm-stdout",
];

/// Renders a stream in fixed-size pieces, as a capture ring receives it.
fn render_in_chunks(text: &str, chunk: usize) -> (String, u64) {
    let mut renderer = RowRenderer::new();
    let mut out = String::new();
    let characters: Vec<char> = text.chars().collect();
    for piece in characters.chunks(chunk) {
        renderer.push(&piece.iter().collect::<String>(), &mut out);
    }
    renderer.finish(&mut out);
    (out, renderer.redraws())
}

// -- Captured tool output -------------------------------------------------

#[test]
fn a_hand_rolled_carriage_return_bar_reduces_to_its_final_frame() {
    // `bare_cr.py` pads its last frame over the widest one it drew, which is
    // what a well-behaved writer does, so nothing is left behind it.
    let (rendered, redraws) = render_terminal(&fixture("bare-cr"));
    assert_eq!(
        rendered,
        "preparing 3 inputs\nextracting complete\nwrote 3 outputs\n"
    );
    assert_eq!(redraws, 21);
}

#[test]
fn erasing_without_padding_leaves_the_residue_a_terminal_would_show() {
    // The last frame of `bare_cr_erase.py` carries no erase, so the tail of the
    // longer frame beneath it survives. This is the case that separates real
    // overwrite semantics from keeping the text after the final carriage
    // return, which would answer "done" and be wrong.
    let (rendered, redraws) = render_terminal(&fixture("bare-cr-erase"));
    assert_eq!(rendered, "doneloading 100% [==========]\n");
    assert_eq!(redraws, 3);
}

#[test]
fn a_tqdm_bar_on_stderr_reduces_to_one_row() {
    let raw = fixture("tqdm-stderr");
    let (rendered, redraws) = render_terminal(&raw);
    assert_eq!(rendered.lines().count(), 1);
    assert!(!rendered.contains('\r'));
    assert!(
        rendered.starts_with("Loading weights: 100%|"),
        "expected the completed frame, got {rendered:?}"
    );
    assert!(rendered.contains("| 400/400 ["), "got {rendered:?}");
    assert!(redraws >= 15, "expected the absorbed frames to be counted");
    // The point of the exercise: what the model reads is a fraction of what the
    // command wrote.
    assert!(
        rendered.len() * 10 < raw.len(),
        "expected at least a tenfold reduction, got {} from {}",
        rendered.len(),
        raw.len()
    );
}

#[test]
fn ordinary_lines_around_a_bar_survive_it() {
    // A bar routed to stdout sits between real output. Reducing the bar must not
    // reduce its neighbours.
    let (rendered, _) = render_terminal(&fixture("tqdm-stdout"));
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 4, "got {lines:?}");
    assert_eq!(lines[0], "resolved 12 shards");
    assert_eq!(lines[1], "dtype=bfloat16");
    assert!(lines[2].starts_with("Fetching shards: 100%|"));
    assert_eq!(lines[3], "done in 1.2s");
}

#[test]
fn nested_bars_render_one_row_per_redraw_target() {
    // Nested bars move between rows with CSI A, which single-row rendering does
    // not model. Consuming it yields a readable line per redraw target instead
    // of an escape embedded in the text.
    let (rendered, _) = render_terminal(&fixture("tqdm-nested"));
    assert!(!rendered.contains('\u{1b}'), "got {rendered:?}");
    assert!(!rendered.contains('\r'));
    let lines: Vec<&str> = rendered.lines().collect();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("epoch:") || line.starts_with("batch:")),
        "got {lines:?}"
    );
    assert!(
        lines
            .last()
            .is_some_and(|line| line.starts_with("epoch: 100%")),
        "got {lines:?}"
    );
}

#[test]
fn a_curl_transfer_keeps_its_header_and_final_row() {
    let (rendered, redraws) = render_terminal(&fixture("curl-download"));
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 3, "got {lines:?}");
    assert!(lines[0].starts_with("  % Total"));
    assert!(lines[2].starts_with("100 "), "got {:?}", lines[2]);
    assert_eq!(redraws, 1);
}

#[test]
fn line_separated_frames_are_left_for_the_collapse_stage() {
    // No carriage returns, so row rendering has nothing to do and must not
    // invent a reduction.
    let raw = fixture("newline-frames");
    let (rendered, redraws) = render_terminal(&raw);
    assert_eq!(rendered, raw);
    assert_eq!(redraws, 0);
}

#[test]
fn a_go_bar_that_blanks_each_frame_reduces_to_one_row() {
    // `schollz/progressbar` is the hardest observed case and the reason this is
    // measured per library rather than assumed. It redraws into a pipe, blanks
    // each frame with spaces before drawing the next, and emits no newline at
    // all, so the whole run is one row eighteen kilobytes wide. Every
    // line-oriented stage downstream sees exactly one line.
    let raw = fixture("go-progressbar");
    assert_eq!(raw.lines().count(), 1, "the whole run is one row");
    assert!(!raw.contains('\n'), "and it never terminates that row");

    let (rendered, redraws) = render_terminal(&raw);
    assert_eq!(rendered.lines().count(), 1);
    assert!(!rendered.contains('\r'));
    assert!(
        rendered.starts_with("Loading weights 100% |"),
        "expected the completed frame, got {rendered:?}"
    );
    // The blanking frames must not survive as trailing whitespace.
    assert_eq!(rendered.trim_end(), rendered);
    assert!(redraws > 250, "got {redraws}");
    assert!(
        rendered.len() * 100 < raw.len(),
        "expected a hundredfold reduction, got {} from {}",
        rendered.len(),
        raw.len()
    );
}

#[test]
fn a_spinner_that_prints_only_its_first_and_last_frame_is_untouched() {
    // `ora` gates on isatty and never redraws, so it arrives as two ordinary
    // lines. There is nothing to reduce and nothing may be.
    let raw = fixture("node-ora");
    let (rendered, redraws) = render_terminal(&raw);
    assert_eq!(rendered, raw);
    assert_eq!(redraws, 0);
    assert_eq!(raw.lines().count(), 2);
}

#[test]
fn a_library_that_consults_isatty_needs_no_reduction() {
    // `rich` checks whether the stream is a terminal and renders once at the end
    // rather than redrawing, so it arrives as a single line with no control
    // bytes at all. Pinning that keeps "most progress libraries go quiet on a
    // pipe" a measurement rather than a recollection, and proves the reduction
    // does not go looking for work that is not there.
    let raw = fixture("rich-progress");
    assert!(!raw.contains('\r'));
    assert_eq!(raw.lines().count(), 1);
    let (rendered, redraws) = render_terminal(&raw);
    assert_eq!(rendered, raw);
    assert_eq!(redraws, 0);
}

#[test]
fn carriage_return_separated_records_render_as_a_terminal_would() {
    // Classic Mac line endings are the known destructive case: a terminal really
    // does overwrite these into one row. Rendering is faithful, and pinning it
    // here keeps the behaviour visible rather than surprising.
    let (rendered, _) = render_terminal(&fixture("cr-only-records"));
    assert_eq!(rendered, "delta");
}

// -- Invariants -----------------------------------------------------------

#[test]
fn rendering_is_independent_of_how_the_stream_is_chunked() {
    // Capture arrives in reads that land wherever the kernel filled the buffer,
    // so a row, a carriage return before a newline, and an escape sequence are
    // all routinely split. Rendering must not depend on where.
    for name in FIXTURES {
        let raw = fixture(name);
        let whole = render_terminal(&raw);
        for chunk in [1, 2, 3, 5, 7, 13, 64, 997] {
            let split = render_in_chunks(&raw, chunk);
            assert_eq!(
                split.0, whole.0,
                "{name} rendered differently at chunk size {chunk}"
            );
            assert_eq!(
                split.1, whole.1,
                "{name} counted redraws differently at chunk size {chunk}"
            );
        }
    }
}

#[test]
fn output_without_cursor_controls_is_reproduced_exactly() {
    // Rendering is applied to every command, so anything that is not a redraw
    // has to pass through untouched, trailing blanks and all.
    for text in [
        "",
        "one\n",
        "one\ntwo\n",
        "no trailing newline",
        "trailing blanks   \nkept   \n",
        "tab\tseparated\tvalues\n",
        "unicode: \u{4f60}\u{597d} \u{1f600}\n",
        "blank line follows\n\nand precedes\n",
    ] {
        let (rendered, redraws) = render_terminal(text);
        assert_eq!(rendered, text, "changed {text:?}");
        assert_eq!(redraws, 0, "reported a redraw for {text:?}");
    }
}

#[test]
fn crlf_line_endings_are_not_redraws() {
    // A carriage return before a newline ends a line. Counting it as a redraw
    // would mark every row of a Windows-style stream as overwritten and strip
    // the trailing blanks off each one.
    let (rendered, redraws) = render_terminal("alpha  \r\nbeta\r\n");
    assert_eq!(rendered, "alpha  \nbeta\n");
    assert_eq!(redraws, 0);
}

#[test]
fn a_carriage_return_split_from_its_newline_is_still_a_line_ending() {
    // The classification of `\r` depends on the byte after it, which the next
    // read may hold.
    let mut renderer = RowRenderer::new();
    let mut out = String::new();
    renderer.push("alpha  \r", &mut out);
    renderer.push("\nbeta\n", &mut out);
    renderer.finish(&mut out);
    assert_eq!(out, "alpha  \nbeta\n");
    assert_eq!(renderer.redraws(), 0);
}

#[test]
fn a_stream_without_a_trailing_newline_does_not_gain_one() {
    let (rendered, _) = render_terminal("printed with no newline");
    assert_eq!(rendered, "printed with no newline");
}

#[test]
fn overwriting_is_positional_rather_than_last_writer_wins() {
    let (rendered, redraws) = render_terminal("abcdef\rxy\n");
    assert_eq!(rendered, "xycdef\n");
    assert_eq!(redraws, 1);
}

#[test]
fn backspace_moves_the_cursor_back_one_cell() {
    let (rendered, _) = render_terminal("abc\u{8}\u{8}XY\n");
    assert_eq!(rendered, "aXY\n");
}

#[test]
fn erase_to_end_of_row_removes_what_follows_the_cursor() {
    let (rendered, _) = render_terminal("abcdef\rxy\u{1b}[K\n");
    assert_eq!(rendered, "xy\n");
}

#[test]
fn erase_variants_clear_the_regions_they_name() {
    assert_eq!(render_terminal("abcdef\r\u{1b}[2Kxy\n").0, "xy\n");
    assert_eq!(
        render_terminal("abcdef\r\u{1b}[3C\u{1b}[1Kz\n").0,
        "   zef\n"
    );
}

#[test]
fn horizontal_cursor_motion_positions_the_next_write() {
    assert_eq!(render_terminal("abcdef\u{1b}[3DX\n").0, "abcXef\n");
    assert_eq!(render_terminal("abcdef\u{1b}[2GX\n").0, "aXcdef\n");
    assert_eq!(render_terminal("ab\u{1b}[4CX\n").0, "ab    X\n");
}

#[test]
fn colour_sequences_are_zero_width_and_survive() {
    // SGR does not occupy a cell. Letting it consume one would shift the
    // overwrite alignment of every coloured frame, and dropping it would take a
    // decision that belongs to `strip_ansi`.
    let (rendered, _) = render_terminal("\u{1b}[32mabcdef\u{1b}[0m\rxy\u{1b}[K\n");
    assert_eq!(rendered, "xy\n");
    let (kept, _) = render_terminal("\u{1b}[32mgreen\u{1b}[0m\n");
    assert_eq!(kept, "\u{1b}[32mgreen\u{1b}[0m\n");
}

#[test]
fn a_coloured_bar_overwrites_on_visible_columns_only() {
    // The frames differ in escape length but not in visible width, so a reducer
    // that let escapes occupy cells would misalign the second frame over the
    // first and leave a fragment behind.
    let (rendered, _) =
        render_terminal("\u{1b}[1;32m[###  ]\u{1b}[0m\r\u{1b}[32m[#####]\u{1b}[0m\n");
    assert!(rendered.ends_with("[#####]\u{1b}[0m\n"), "got {rendered:?}");
    assert!(!rendered.contains("]\u{1b}[0m["), "got {rendered:?}");
}

#[test]
fn private_mode_sequences_are_left_for_strip_ansi() {
    // `\x1b[?25l` hides the cursor; it is not motion and must not be silently
    // consumed by a renderer that claims only to model the cursor.
    let (rendered, _) = render_terminal("\u{1b}[?25labc\u{1b}[?25h\n");
    assert_eq!(rendered, "\u{1b}[?25labc\u{1b}[?25h\n");
}

#[test]
fn an_unterminated_escape_at_end_of_stream_keeps_its_bytes() {
    let (rendered, _) = render_terminal("abc\u{1b}[3");
    assert_eq!(rendered, "abc\u{1b}[3");
}

#[test]
fn a_row_wider_than_the_canvas_is_broken_rather_than_buffered() {
    // The bound exists so retained state cannot grow with stream length. It must
    // bound memory without discarding output.
    let wide = "x".repeat(40_000);
    let (rendered, _) = render_terminal(&wide);
    assert_eq!(rendered.replace('\n', ""), wide);
    assert!(rendered.contains('\n'), "expected the row to be broken");
}

#[test]
fn a_long_row_without_controls_is_not_reported_as_redrawn() {
    let (_, redraws) = render_terminal(&"y".repeat(40_000));
    assert_eq!(redraws, 0);
}

#[test]
fn retained_state_stays_one_row_wide_across_a_long_run() {
    // The property that makes capture-time rendering affordable: a bar that
    // redraws for an hour must not accumulate.
    let mut renderer = RowRenderer::new();
    let mut out = String::new();
    for step in 0..50_000 {
        renderer.push(
            &format!("\rstep {step} of 50000 [{:>3}%]", step / 500),
            &mut out,
        );
    }
    assert!(
        out.is_empty(),
        "no row completed, so nothing should be emitted"
    );
    renderer.finish(&mut out);
    assert_eq!(out, "step 49999 of 50000 [ 99%]");
    assert_eq!(renderer.redraws(), 49_999);
}

#[test]
fn finishing_twice_neither_repeats_a_row_nor_recounts_it() {
    let mut renderer = RowRenderer::new();
    let mut out = String::new();
    renderer.push("step 1/2\rstep 2/2", &mut out);
    renderer.finish(&mut out);
    let after_first = (out.clone(), renderer.redraws());
    renderer.finish(&mut out);
    assert_eq!((out, renderer.redraws()), after_first);
    assert_eq!(after_first.0, "step 2/2");
    assert_eq!(after_first.1, 1);
}
