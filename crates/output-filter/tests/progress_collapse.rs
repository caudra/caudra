//! Line-collapse behaviour, measured against captured output.
//!
//! The positive fixtures prove the reduction happens. The negative ones matter
//! more: a numeric table and a run of repeated warnings both survive naive
//! deduplication only because of a specific gate, so each has a fixture and each
//! gate has a test that fails if it is removed.

use workcell_output_filter::{collapse_progress_lines, render_terminal};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/progress/{name}.raw",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

// -- Captured tool output -------------------------------------------------

#[test]
fn newline_separated_frames_reduce_to_the_final_one() {
    let raw = fixture("newline-frames");
    let (collapsed, removed) = collapse_progress_lines(&raw);
    assert_eq!(
        collapsed,
        concat!(
            "loading checkpoint shards\n",
            "... (18 progress updates collapsed)\n",
            "Loading weights: 100%|\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}| 851/851 [04:21<00:00,  3.26it/s]\n",
            "checkpoint loaded\n"
        )
    );
    assert_eq!(removed, 18);
    assert!(
        collapsed.len() * 8 < raw.len(),
        "expected a large reduction"
    );
}

#[test]
fn a_numeric_table_is_not_a_progress_run() {
    // Consecutive rows share a shape and carry a monotonic counter against a
    // constant denominator, so shape and monotonicity alone would destroy this.
    // Only the two-signal requirement saves it, since a row of measurements
    // carries a ratio and nothing else.
    let raw = fixture("numeric-table");
    let (collapsed, removed) = collapse_progress_lines(&raw);
    assert_eq!(collapsed, raw);
    assert_eq!(removed, 0);
}

#[test]
fn repeated_diagnostics_survive_whole() {
    // Diagnostics are the part of the output a caller needs. These lines are
    // near-identical and would fall to naive deduplication.
    let raw = fixture("repeated-diagnostic");
    let (collapsed, removed) = collapse_progress_lines(&raw);
    assert_eq!(collapsed, raw);
    assert_eq!(removed, 0);
}

#[test]
fn a_redrawn_bar_needs_no_collapsing_afterwards() {
    // Row rendering already reduced it to one line, so the collapse stage has
    // nothing left to do and must not claim otherwise.
    for name in [
        "tqdm-stderr",
        "tqdm-stdout",
        "bare-cr",
        "curl-download",
        "rich-progress",
        "go-progressbar",
        "node-ora",
    ] {
        let (rendered, _) = render_terminal(&fixture(name));
        let (collapsed, removed) = collapse_progress_lines(&rendered);
        assert_eq!(collapsed, rendered, "{name} was changed");
        assert_eq!(removed, 0, "{name} reported a collapse");
    }
}

#[test]
fn interleaved_nested_bars_are_left_alone() {
    // Rendering `tqdm-nested` yields alternating `epoch:` and `batch:` rows, so
    // no run of one shape is long enough to reduce.
    let (rendered, _) = render_terminal(&fixture("tqdm-nested"));
    let (collapsed, removed) = collapse_progress_lines(&rendered);
    assert_eq!(collapsed, rendered);
    assert_eq!(removed, 0);
}

// -- Gates ----------------------------------------------------------------

fn frames(count: usize) -> String {
    (0..count)
        .map(|step| {
            let percent = step * 100 / count.max(1);
            format!(
                "Fetching:  {percent:>3}%|{:<10}| {step}/{count} [00:0{}<00:09, 4.82it/s]\n",
                "\u{2588}".repeat(percent / 10),
                step % 10
            )
        })
        .collect()
}

#[test]
fn two_frames_are_not_worth_a_marker() {
    let input = frames(2);
    assert_eq!(collapse_progress_lines(&input), (input.clone(), 0));
}

#[test]
fn three_frames_are() {
    let (collapsed, removed) = collapse_progress_lines(&frames(3));
    assert_eq!(removed, 2);
    assert_eq!(collapsed.lines().count(), 2);
    assert!(collapsed.starts_with("... (2 progress updates collapsed)\n"));
}

#[test]
fn a_single_signal_is_not_enough() {
    // A ratio alone appears in ordinary output, so it cannot license a collapse.
    let input = "shard 1/40\nshard 2/40\nshard 3/40\nshard 4/40\n";
    assert_eq!(collapse_progress_lines(input), (input.to_owned(), 0));
}

#[test]
fn a_counter_that_does_not_advance_is_not_progress() {
    // Two signals and one shape, but nothing rises: this is a status line being
    // reprinted, not a bar advancing.
    let input = "queue 0/64 at 0.0it/s\nqueue 0/64 at 0.0it/s\nqueue 0/64 at 0.0it/s\n";
    assert_eq!(collapse_progress_lines(input), (input.to_owned(), 0));
}

#[test]
fn a_rising_counter_without_a_bound_is_not_progress() {
    // An incrementing index paired with a rate has two signals and rises, but
    // nothing says how far it goes. A log with a sequence number looks like this.
    let input = "event 1 at 12.0req/s\nevent 2 at 13.0req/s\nevent 3 at 14.0req/s\n";
    assert_eq!(collapse_progress_lines(input), (input.to_owned(), 0));
}

#[test]
fn a_decreasing_counter_is_not_progress() {
    let input = "retry 9/9 eta 30 [00:01<00:09]\nretry 8/9 eta 20 [00:02<00:09]\nretry 7/9 eta 10 [00:03<00:09]\n";
    assert_eq!(collapse_progress_lines(input), (input.to_owned(), 0));
}

#[test]
fn a_run_is_bounded_by_the_lines_around_it() {
    let mut input = String::from("before\n");
    input.push_str(&frames(6));
    input.push_str("after\n");
    let (collapsed, removed) = collapse_progress_lines(&input);
    assert_eq!(removed, 5);
    let lines: Vec<&str> = collapsed.lines().collect();
    assert_eq!(lines[0], "before");
    assert_eq!(lines[1], "... (5 progress updates collapsed)");
    assert!(lines[2].starts_with("Fetching:"));
    assert_eq!(lines[3], "after");
}

#[test]
fn two_separate_runs_each_keep_their_own_final_frame() {
    let mut input = frames(4);
    input.push_str("switching phase\n");
    input.push_str(&frames(5));
    let (collapsed, removed) = collapse_progress_lines(&input);
    assert_eq!(removed, 3 + 4);
    let lines: Vec<&str> = collapsed.lines().collect();
    assert_eq!(lines.len(), 5, "got {lines:?}");
    assert_eq!(lines[2], "switching phase");
}

#[test]
fn the_kept_frame_is_the_last_one() {
    // The final frame carries the totals and the elapsed time; the first carries
    // zeroes.
    let (collapsed, _) = collapse_progress_lines(&frames(8));
    let kept = collapsed.lines().next_back().expect("a kept frame");
    assert!(kept.contains("| 7/8 ["), "got {kept:?}");
}

#[test]
fn a_missing_trailing_newline_is_not_added() {
    let input = frames(4);
    let trimmed = input.trim_end_matches('\n');
    let (collapsed, removed) = collapse_progress_lines(trimmed);
    assert_eq!(removed, 3);
    assert!(!collapsed.ends_with('\n'));
}

#[test]
fn a_trailing_newline_is_preserved() {
    let (collapsed, _) = collapse_progress_lines(&frames(4));
    assert!(collapsed.ends_with('\n'));
}

#[test]
fn ordinary_output_passes_through_untouched() {
    for text in [
        "",
        "one line\n",
        "error: expected `;`\n  --> src/main.rs:4:9\n",
        "a\nb\nc\nd\ne\n",
        "same\nsame\nsame\nsame\n",
    ] {
        assert_eq!(
            collapse_progress_lines(text),
            (text.to_owned(), 0),
            "changed {text:?}"
        );
    }
}

#[test]
fn a_failure_line_inside_a_run_breaks_it() {
    // A diagnostic emitted mid-bar has a different shape, so it interrupts the
    // run and survives rather than being absorbed into it.
    let mut input = frames(4);
    input.push_str("error: shard 3 is corrupt\n");
    input.push_str(&frames(4));
    let (collapsed, _) = collapse_progress_lines(&input);
    assert!(collapsed.contains("error: shard 3 is corrupt"));
}
