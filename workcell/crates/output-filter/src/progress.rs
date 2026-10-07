//! Collapsing of progress frames that arrive as separate lines.
//!
//! Row rendering handles a bar that redraws in place. It cannot help when every
//! frame is already its own line, which is what a logger, a CI log collector, or
//! a container runtime produces after converting a redraw stream. There the
//! frames are ordinary lines and only a judgement about content can reduce them.
//!
//! That judgement is deliberately narrow, because the corpus README is right
//! that deduplicating diagnostics discards the part of the output a caller
//! needs. A run of lines is collapsed only when all three of the following hold:
//!
//! 1. Three or more consecutive lines share a *shape*: identical once digit
//!    runs, bar glyphs, and whitespace are normalized away.
//! 2. Each carries at least two independent progress signals — a percentage, a
//!    ratio, a rate, a timer, an ETA, or a drawn bar.
//! 3. Some number advances monotonically across the run, alongside either a
//!    constant companion, as `240/851` has in `851`, or a percentage.
//!
//! Each condition removes a distinct false positive. Shape alone is the naive
//! deduplication that destroys a numeric table. The two-signal requirement is
//! what actually saves that table, since a row of measurements carries a ratio
//! at most. Monotonicity is what saves repeated diagnostics, which never count
//! toward anything.
//!
//! The final frame is kept rather than the first: it carries the totals, the
//! elapsed time, and how far a bar actually reached before it stopped.

use std::sync::LazyLock;

use regex::Regex;

/// Shortest run worth collapsing. Two frames and a marker is not a reduction.
const MIN_RUN: usize = 3;

/// Independent progress signals a line must carry to be eligible.
const MIN_SIGNALS: usize = 2;

static PERCENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]{1,3}\s?%").expect("percent pattern is valid"));
static RATIO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]+\s?/\s?[0-9]+").expect("ratio pattern is valid"));
static RATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[0-9](\.[0-9]+)?\s?(it|ops|req|[kmgt]?i?b)/s").expect("rate pattern is valid")
});
static TIMER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9]{1,2}:[0-9]{2}(:[0-9]{2})?\s?[<>]").expect("timer pattern is valid")
});
static ETA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\beta:?\s+[0-9]").expect("eta pattern is valid"));

/// Characters a tool draws a bar out of.
///
/// The ASCII members are restricted to those that are not ordinary punctuation.
/// `-` and `.` are excluded on purpose: `---` and `...` are separators and
/// ellipses far more often than they are progress.
fn is_bar_glyph(character: char) -> bool {
    matches!(character,
        '=' | '#' | '\u{2588}'..='\u{258f}'   // block elements
            | '\u{2591}'..='\u{2593}'          // shade blocks
            | '\u{25a0}' | '\u{25ac}' | '\u{25cf}'
            | '\u{2801}'..='\u{28ff}'          // braille spinners
            | '\u{2500}'..='\u{257f}'          // box drawing, used by pip and uv
    )
}

/// A line reduced to the parts that identify it as the same frame as its
/// neighbour, with the varying parts erased.
///
/// Bar glyphs and whitespace normalize to the same marker. A bar fills from
/// blank to solid as it advances, so distinguishing them would give `0%` a
/// different shape from `50%` and split one bar into several runs, none of them
/// long enough to reduce.
fn shape_of(line: &str) -> String {
    let mut shape = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_ascii_digit() {
            shape.push('#');
            while characters.peek().is_some_and(char::is_ascii_digit) {
                characters.next();
            }
        } else if is_fill(character) {
            shape.push(' ');
            while characters.peek().copied().is_some_and(is_fill) {
                characters.next();
            }
        } else {
            shape.push(character);
        }
    }
    shape
}

fn is_fill(character: char) -> bool {
    character.is_whitespace() || is_bar_glyph(character)
}

fn signals(line: &str) -> usize {
    // Every signal but a drawn bar needs a digit, and one signal is never
    // enough, so a line without digits cannot qualify. Checking that first keeps
    // six regexes off the ordinary lines that make up most output.
    if !line.bytes().any(|byte| byte.is_ascii_digit()) {
        return 0;
    }
    let mut count = 0;
    if PERCENT.is_match(line) {
        count += 1;
    }
    if RATIO.is_match(line) {
        count += 1;
    }
    if RATE.is_match(line) {
        count += 1;
    }
    if TIMER.is_match(line) {
        count += 1;
    }
    if ETA.is_match(line) {
        count += 1;
    }
    if has_drawn_bar(line) {
        count += 1;
    }
    count
}

fn has_drawn_bar(line: &str) -> bool {
    let mut run = 0;
    for character in line.chars() {
        if is_bar_glyph(character) {
            run += 1;
            if run >= 3 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// What a number is doing in a line, inferred from the characters beside it.
///
/// Value alone cannot distinguish a counter from a clock, and a clock rises in
/// every repeated status line that carries one. Only the surrounding syntax says
/// whether a number is measured against something.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    /// Followed by a per-cent sign, so bounded by construction.
    Percent,
    /// The left side of `n/m`.
    Ratio,
    /// The right side of `n/m`.
    Denominator,
    Plain,
}

#[derive(Clone, Copy, Debug)]
struct Field {
    value: u64,
    role: Role,
}

/// Every digit run in a line, in order, saturating rather than overflowing.
fn fields(line: &str) -> Vec<Field> {
    let characters: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if !characters[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        let mut value = 0_u64;
        while let Some(digit) = characters.get(index).and_then(|c| c.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(u64::from(digit));
            index += 1;
        }
        let mut after = index;
        while characters.get(after) == Some(&' ') {
            after += 1;
        }
        let mut before = start;
        while before > 0 && characters[before - 1] == ' ' {
            before -= 1;
        }
        let divides = characters.get(after) == Some(&'/') && {
            let mut next = after + 1;
            while characters.get(next) == Some(&' ') {
                next += 1;
            }
            characters.get(next).is_some_and(char::is_ascii_digit)
        };
        let role = if characters.get(after) == Some(&'%') {
            Role::Percent
        } else if divides {
            Role::Ratio
        } else if before > 0 && characters[before - 1] == '/' {
            Role::Denominator
        } else {
            Role::Plain
        };
        out.push(Field { value, role });
    }
    out
}

/// Whether a run's numbers behave like a progress readout rather than data.
///
/// A rising number is not enough on its own: an elapsed clock rises in every
/// repeated line that carries one, and a log sequence number rises forever. What
/// distinguishes progress is a number measured against something — a per-cent
/// sign, or a denominator that stays put while the numerator climbs toward it.
fn advances(run: &[Vec<Field>]) -> bool {
    let Some(width) = run.first().map(Vec::len) else {
        return false;
    };
    if run.iter().any(|values| {
        values.len() != width || values.iter().zip(&run[0]).any(|(a, b)| a.role != b.role)
    }) {
        return false;
    }
    let rising = |position: usize| {
        let column: Vec<u64> = run.iter().map(|values| values[position].value).collect();
        column.windows(2).all(|pair| pair[0] <= pair[1])
            && column.last().is_some_and(|last| column[0] < *last)
    };
    let constant = |position: usize| {
        run.iter()
            .all(|values| values[position].value == run[0][position].value)
    };
    (0..width).any(|position| match run[0][position].role {
        Role::Percent => rising(position),
        Role::Ratio => {
            rising(position)
                && run[0]
                    .get(position + 1)
                    .is_some_and(|next| next.role == Role::Denominator)
                && constant(position + 1)
        }
        Role::Denominator | Role::Plain => false,
    })
}

/// Collapses runs of progress frames, returning the text and the lines removed.
#[must_use]
pub fn collapse_progress_lines(text: &str) -> (String, u64) {
    if text.is_empty() {
        return (String::new(), 0);
    }
    let lines: Vec<&str> = text.lines().collect();
    let eligible: Vec<bool> = lines
        .iter()
        .map(|line| signals(line) >= MIN_SIGNALS)
        .collect();
    let shapes: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if eligible[index] {
                shape_of(line)
            } else {
                String::new()
            }
        })
        .collect();

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut removed = 0_u64;
    let mut index = 0;
    while index < lines.len() {
        if !eligible[index] {
            out.push(lines[index].to_owned());
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < lines.len() && eligible[end] && shapes[end] == shapes[index] {
            end += 1;
        }
        let length = end - index;
        if length < MIN_RUN {
            for line in &lines[index..end] {
                out.push((*line).to_owned());
            }
            index = end;
            continue;
        }
        let values: Vec<Vec<Field>> = lines[index..end].iter().map(|line| fields(line)).collect();
        if !advances(&values) {
            for line in &lines[index..end] {
                out.push((*line).to_owned());
            }
            index = end;
            continue;
        }
        let collapsed = length - 1;
        removed += collapsed as u64;
        out.push(format!("... ({collapsed} progress updates collapsed)"));
        out.push(lines[end - 1].to_owned());
        index = end;
    }

    if removed == 0 {
        return (text.to_owned(), 0);
    }
    let mut rendered = out.join("\n");
    // `lines` drops a trailing newline; restore it so the reduction is the only
    // difference between input and output.
    if text.ends_with('\n') {
        rendered.push('\n');
    }
    (rendered, removed)
}
