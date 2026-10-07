//! Removal of terminal escape sequences from captured output.
//!
//! Colour, cursor-mode changes, window titles, and hyperlinks are addressed to a
//! terminal. A model is not one, and the bytes cost far more than their length
//! suggests: `\x1b[33m` is several tokens that merge with nothing, decorating
//! text that is often a single character.
//!
//! This is a command-independent reduction for the same reason
//! [`collapse_progress_lines`](crate::collapse_progress_lines) is. A rule is
//! selected by command scope, and a request resolving to more than one scope
//! selects none, so a rule-gated strip never reaches the output of a chain or an
//! ad-hoc script — which is where hardcoded colour overwhelmingly comes from.
//!
//! It is deliberately separate from [`render_terminal`](crate::render_terminal),
//! which is decoding rather than judgement and must keep colour: the renderer
//! anchors these sequences to columns as zero width so a coloured frame
//! overwrites in alignment, and dropping them there would remove the only way to
//! observe a program's decoration at all.
//!
//! A caller that wants the bytes pipes the command through `cat -v`, `od -c`, or
//! `sed -n l`. Those render ESC as a printable `^[` before this ever sees it, so
//! such a request survives the strip intact.

/// Longest escape sequence scanned before it is treated as a stray byte.
pub(crate) const MAX_ESCAPE_CHARS: usize = 64;

/// Longest operating-system-command payload scanned before the same.
pub(crate) const MAX_OSC_CHARS: usize = 512;

/// Characters examined while resolving one sequence.
///
/// [`scan_escape`] gives up once it has advanced `MAX_OSC_CHARS` and looks at
/// most one character beyond that, so a window this wide always resolves. A
/// shorter one would report [`Scan::Incomplete`] for a sequence that is merely
/// long, and the strip would keep it.
const SCAN_WINDOW_CHARS: usize = MAX_OSC_CHARS + 2;

pub(crate) enum Scan {
    /// The sequence ends before this index.
    Found(usize),
    /// The sequence is split across chunks and needs more input.
    Incomplete,
}

/// Finds the end of the escape sequence beginning at `start`.
///
/// This is the crate's only definition of where a sequence ends. The renderer
/// and the strip share it so they cannot disagree about what a sequence is; a
/// caller may decline to act on what it finds, but must not extend it.
pub(crate) fn scan_escape(source: &[char], start: usize) -> Scan {
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
        // Everything else is the nF form: zero or more intermediate bytes, then
        // a final byte. `\x1b(B` is three characters wide, not two. Consuming
        // only two would leave its final byte behind, and the renderer would
        // write that byte into the row as though the command had printed it.
        _ => {
            let mut index = start + 1;
            while let Some(character) = source.get(index) {
                if !('\u{20}'..='\u{2f}').contains(character) {
                    return Scan::Found(index + 1);
                }
                index += 1;
                if index - start > MAX_ESCAPE_CHARS {
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
    }
}

/// Removes every escape sequence from `text`.
///
/// Returns the text and how many sequences were removed. Nothing else about the
/// text changes: no line is dropped, joined, or reordered, so a reader has
/// nothing to go looking for beyond decoration.
#[must_use]
pub fn strip_escape_sequences(text: &str) -> (String, u64) {
    let Some(first) = text.find('\u{1b}') else {
        return (text.to_owned(), 0);
    };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut removed: u64 = 0;
    let mut rest = &text[first..];

    loop {
        // `rest` begins at an escape character.
        let window: Vec<char> = rest.chars().take(SCAN_WINDOW_CHARS).collect();
        match sequence_bytes(&window) {
            Some(bytes) => {
                rest = &rest[bytes..];
                removed = removed.saturating_add(1);
            }
            None => {
                out.push('\u{1b}');
                rest = &rest[1..];
            }
        }
        match rest.find('\u{1b}') {
            Some(next) => {
                out.push_str(&rest[..next]);
                rest = &rest[next..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }

    (out, removed)
}

/// Byte length of the complete sequence at the front of `window`, or `None` when
/// there is nothing there this strip is willing to remove.
fn sequence_bytes(window: &[char]) -> Option<usize> {
    let Scan::Found(end) = scan_escape(window, 0) else {
        // Truncated by the end of the input. Its bytes are still what the
        // command wrote, so they are kept rather than guessed at.
        return None;
    };
    if end <= 1 {
        // The scanner ran past its bound and reported the introducer as a stray
        // byte. There is no sequence to remove.
        return None;
    }
    // `scan_escape` ends a two-character escape on whatever follows the
    // introducer, including a control character, because the renderer retains
    // such a sequence verbatim and loses nothing by it. Removing `\x1b\n` here
    // would delete a line ending instead. Declining leaves more text than the
    // scanner identified, which is always safe; the scanner remains the only
    // definition of where a sequence ends.
    if !('\u{20}'..='\u{7e}').contains(window.get(1)?) {
        return None;
    }
    Some(
        window[..end]
            .iter()
            .map(|character| character.len_utf8())
            .sum(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stripped(text: &str) -> String {
        strip_escape_sequences(text).0
    }

    #[test]
    fn colour_is_removed() {
        assert_eq!(stripped("\u{1b}[33m!\u{1b}[0m ok"), "! ok");
        assert_eq!(stripped("\u{1b}[38;2;255;0;0mred\u{1b}[m"), "red");
        assert_eq!(strip_escape_sequences("\u{1b}[33m!\u{1b}[0m").1, 2);
    }

    #[test]
    fn private_mode_sequences_are_removed() {
        // The regex this replaced matched `[0-9;]` parameters only, so a cursor
        // hide survived every strip in the crate.
        assert_eq!(stripped("\u{1b}[?25labc\u{1b}[?25h"), "abc");
    }

    #[test]
    fn operating_system_commands_are_removed() {
        assert_eq!(stripped("\u{1b}]0;title\u{7}abc"), "abc");
        assert_eq!(stripped("\u{1b}]0;title\u{1b}\\abc"), "abc");
    }

    #[test]
    fn hyperlinks_are_removed_and_their_text_survives() {
        let link = "\u{1b}]8;;https://example.test\u{1b}\\label\u{1b}]8;;\u{1b}\\";
        assert_eq!(stripped(link), "label");
    }

    #[test]
    fn escapes_outside_csi_and_osc_are_removed() {
        // `\x1b(B` is three characters. Treating it as two leaves a bare `B` in
        // the text, which reads as though the command printed it.
        assert_eq!(stripped("\u{1b}(Babc"), "abc");
        assert_eq!(stripped("\u{1b}#8abc"), "abc");
        assert_eq!(stripped("\u{1b}7abc\u{1b}8"), "abc");
        assert_eq!(stripped("abc\u{1b}("), "abc\u{1b}(");
    }

    #[test]
    fn intermediate_and_parameter_bytes_are_covered() {
        assert_eq!(stripped("\u{1b}[>4;2mabc"), "abc");
        assert_eq!(stripped("\u{1b}[!pabc"), "abc");
    }

    #[test]
    fn printable_caret_form_survives() {
        // This is the documented way to inspect escapes: `cat -v` renders ESC as
        // a printable `^[` before the strip ever sees it. If this ever fails,
        // the escape hatch the tool description promises is gone.
        assert_eq!(stripped("^[[33m!^[[0m ok"), "^[[33m!^[[0m ok");
        assert_eq!(strip_escape_sequences("^[[33m").1, 0);
    }

    #[test]
    fn an_escape_before_a_control_character_is_kept() {
        // Removing this pair would delete the line ending and join two lines.
        assert_eq!(stripped("a\u{1b}\nb"), "a\u{1b}\nb");
        assert_eq!(stripped("a\u{1b}\rb"), "a\u{1b}\rb");
    }

    #[test]
    fn a_truncated_sequence_at_end_of_input_is_kept() {
        assert_eq!(stripped("abc\u{1b}[3"), "abc\u{1b}[3");
        assert_eq!(stripped("abc\u{1b}"), "abc\u{1b}");
        assert_eq!(stripped("abc\u{1b}]0;title"), "abc\u{1b}]0;title");
    }

    #[test]
    fn a_sequence_past_the_scan_bound_keeps_its_bytes() {
        let overlong = format!("\u{1b}[{}m", "1;".repeat(MAX_ESCAPE_CHARS));
        let text = format!("a{overlong}b");
        let (result, removed) = strip_escape_sequences(&text);
        assert_eq!(removed, 0);
        assert_eq!(result, text);
    }

    #[test]
    fn a_long_sequence_within_the_bound_still_resolves() {
        // The scan window must be wide enough that a merely long sequence is not
        // mistaken for a truncated one.
        let long = format!("\u{1b}]0;{}\u{7}", "t".repeat(MAX_OSC_CHARS - 8));
        let (result, removed) = strip_escape_sequences(&format!("a{long}b"));
        assert_eq!(removed, 1);
        assert_eq!(result, "ab");
    }

    #[test]
    fn text_without_escapes_is_returned_unchanged() {
        let text = "line one\nline two\n";
        assert_eq!(strip_escape_sequences(text), (text.to_owned(), 0));
    }

    #[test]
    fn multibyte_text_around_a_sequence_is_preserved() {
        assert_eq!(stripped("\u{1b}[36m•\u{1b}[0m run — now"), "• run — now");
    }
}
