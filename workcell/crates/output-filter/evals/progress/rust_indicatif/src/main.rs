//! An indicatif bar, the dominant Rust progress crate.
//!
//! Measured rather than assumed. indicatif resolves its draw target through
//! `console::Term`, which reports a pipe as not a terminal, so it draws nothing
//! at all. Keeping the generator makes that a fact the harness re-checks rather
//! than a claim in a comment.

use indicatif::{ProgressBar, ProgressStyle};
use std::{thread, time::Duration};

fn main() {
    let bar = ProgressBar::new(200);
    bar.set_style(
        ProgressStyle::with_template("Loading weights {bar:20} {pos}/{len} ({eta})")
            .expect("template is valid"),
    );
    for _ in 0..200 {
        bar.inc(1);
        thread::sleep(Duration::from_millis(3));
    }
    bar.finish();
}
