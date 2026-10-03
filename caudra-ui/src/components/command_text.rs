//! Commands drawn in shell colours wherever a permission is reviewed (the
//! request, its rows, the scopes it offers, and the `/permissions` lists) and
//! on the transcript rows that name one: a shell card's header, a batch's
//! shell rows, and a background job's `Command` row.
//!
//! Every caller hands over text already redacted, escaped, or checked to hold
//! no control character, so colouring decides only how that text is drawn,
//! never what it says. Until the syntax set has loaded the text is drawn
//! plain.
//!
//! A full rebuild of the transcript draws every card at once, so it colours
//! only the commands already remembered. [`deferring`] reports the rest, and
//! the transcript colours those cards as they come near the screen.

use std::cell::RefCell;
use std::collections::HashMap;

use caudra_highlight::Highlighter;
use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::code_view::highlight_spans;
use crate::{highlight, theme};

const SHELL: &str = "bash";
/// The most of one command coloured. Parsing runs on the UI thread at a few
/// microseconds a byte, several more inside a heredoc body, so the lines past
/// this are drawn plain.
const MAX_COLOURED_BYTES: usize = 8 * 1024;
/// How many coloured commands are kept before the memo starts over.
const MAX_REMEMBERED: usize = 256;
const LINE_BREAK: &str = "\n";
const ELLIPSIS: char = '…';
const MARK_OPEN: char = '‹';
const MARK_CLOSE: char = '›';
const BACKTICK: char = '`';
const SLOT_OPEN: char = '<';
const SLOT_CLOSE: char = '>';
/// Where a scope says which folder its commands run in, after the pattern.
const PLACE_SEPARATOR: &str = " in ";
const FOLDER_END: char = '/';

thread_local! {
    static COLOURED: RefCell<Coloured> = RefCell::default();
}

/// Commands already coloured, for the theme they were coloured in. The views
/// showing them are rebuilt every frame, and parsing is the slow part.
#[derive(Default)]
struct Coloured {
    theme: u64,
    commands: HashMap<String, Vec<Vec<Span<'static>>>>,
    /// Inside [`deferring`], whether a command has been drawn plain for now.
    deferred: Option<bool>,
}

/// Runs `build` colouring only the commands already remembered, and reports
/// whether any other was drawn plain, as every command is while the syntax
/// set loads. Parsing every command a full rebuild draws would stall the
/// frame on cards nobody is looking at.
pub(crate) fn deferring<T>(build: impl FnOnce() -> T) -> (T, bool) {
    let outer = COLOURED.with_borrow_mut(|coloured| coloured.deferred.replace(false));
    let built = build();
    let deferred = COLOURED.with_borrow_mut(|coloured| {
        let deferred = coloured.deferred == Some(true);
        coloured.deferred = outer.map(|noted| noted || deferred);
        deferred
    });
    (built, deferred)
}

/// The lines of one command, coloured by one highlighter so a heredoc body
/// is coloured in the language it feeds.
pub(crate) fn command_lines(lines: &[String]) -> Vec<Vec<Span<'static>>> {
    let ready = highlight::is_ready();
    let generation = theme::generation();
    COLOURED.with_borrow_mut(|coloured| {
        if coloured.theme != generation {
            coloured.commands.clear();
            coloured.theme = generation;
        }
        let command = lines.join(LINE_BREAK);
        if let Some(remembered) = coloured.commands.get(&command) {
            return remembered.clone();
        }
        if !ready || coloured.deferred.is_some() {
            coloured.deferred = coloured.deferred.map(|_| true);
            return lines.iter().map(|line| plain(line)).collect();
        }
        if coloured.commands.len() >= MAX_REMEMBERED {
            coloured.commands.clear();
        }
        coloured
            .commands
            .entry(command)
            .or_insert_with(|| colour(lines))
            .clone()
    })
}

fn colour(lines: &[String]) -> Vec<Vec<Span<'static>>> {
    let mut highlighter = Highlighter::for_token(SHELL);
    let mut budget = MAX_COLOURED_BYTES;
    lines
        .iter()
        .map(|line| match budget.checked_sub(line.len()) {
            Some(left) => {
                budget = left;
                highlight_spans(&mut highlighter, line)
            }
            None => {
                budget = 0;
                plain(line)
            }
        })
        .collect()
}

fn plain(line: &str) -> Vec<Span<'static>> {
    vec![Span::raw(line.to_owned())]
}

/// One line of shell.
pub(crate) fn command_spans(text: &str) -> Vec<Span<'static>> {
    command_lines(&[text.to_owned()]).pop().unwrap_or_default()
}

/// A command pattern, with each `<slot>` of a template in `slot`'s style and
/// any folder the scope names after it left as words in `plain`.
pub(crate) fn pattern_spans(text: &str, plain: Style, slot: Style) -> Vec<Span<'static>> {
    let pattern = match text.rsplit_once(PLACE_SEPARATOR) {
        Some((pattern, place)) if place.ends_with(FOLDER_END) => pattern,
        _ => text,
    };
    let mut spans = Vec::new();
    let mut plain_from = 0;
    let mut search = 0;
    while let Some(open) = pattern[search..].find(SLOT_OPEN).map(|at| search + at) {
        search = open + SLOT_OPEN.len_utf8();
        if let Some(close) = slot_end(&pattern[search..]).map(|at| search + at) {
            spans.extend(command_spans(&pattern[plain_from..open]));
            spans.push(Span::styled(pattern[open..close].to_owned(), slot));
            plain_from = close;
            search = close;
        }
    }
    spans.extend(command_spans(&pattern[plain_from..]));
    spans.push(Span::styled(text[pattern.len()..].to_owned(), plain));
    spans.retain(|span| !span.content.is_empty());
    spans
}

/// Where a template slot's name ends, past its `>`. A name starts at once,
/// which tells a slot from a redirect such as `< in >`.
fn slot_end(rest: &str) -> Option<usize> {
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let name = rest.find([SLOT_OPEN, SLOT_CLOSE])?;
    (name > 0 && rest[name..].starts_with(SLOT_CLOSE)).then(|| name + SLOT_CLOSE.len_utf8())
}

/// A scope in `style`. One that names commands is drawn as a pattern, each
/// part keeping its colour under `style`'s background and modifiers.
pub(crate) fn scope_spans(
    text: &str,
    style: Style,
    slot: Style,
    names_commands: bool,
) -> Vec<Span<'static>> {
    match names_commands {
        true => overlay(pattern_spans(text, style, slot), style),
        false => vec![Span::styled(text.to_owned(), style)],
    }
}

/// A sentence in `style` whose `‹…›` marks hold a scope, each drawn the way
/// [`scope_spans`] draws it.
pub(crate) fn marked_spans(
    sentence: &str,
    style: Style,
    slot: Style,
    names_commands: bool,
) -> Vec<Span<'static>> {
    if !names_commands {
        return vec![Span::styled(sentence.to_owned(), style)];
    }
    let mut spans = Vec::new();
    let mut rest = sentence;
    while let Some(open) = rest.find(MARK_OPEN)
        && let Some(close) = rest[open..].find(MARK_CLOSE).map(|at| open + at)
    {
        let inner = open + MARK_OPEN.len_utf8();
        spans.push(Span::styled(rest[..inner].to_owned(), style));
        spans.extend(scope_spans(&rest[inner..close], style, slot, true));
        rest = &rest[close..];
    }
    spans.push(Span::styled(rest.to_owned(), style));
    spans.retain(|span| !span.content.is_empty());
    spans
}

/// A sentence in `style` with each backtick-quoted command coloured as
/// shell, the backticks kept.
pub(crate) fn code_spans_in(sentence: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = sentence;
    while let Some((before, after)) = rest.split_once(BACKTICK)
        && let Some((code, next)) = after.split_once(BACKTICK)
    {
        spans.push(Span::styled(format!("{before}{BACKTICK}"), style));
        spans.extend(overlay(command_spans(code), style));
        spans.push(Span::styled(BACKTICK.to_string(), style));
        rest = next;
    }
    spans.push(Span::styled(rest.to_owned(), style));
    spans.retain(|span| !span.content.is_empty());
    spans
}

/// `spans` in `style` wherever they have no colour of their own, and under
/// its background and modifiers everywhere.
pub(crate) fn overlay(spans: Vec<Span<'static>>, style: Style) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| {
            let own = span.style.fg;
            let mut patched = span.style.patch(style);
            patched.fg = own.or(patched.fg);
            span.style(patched)
        })
        .collect()
}

/// `spans` cut to `width` columns, ending in `…` when anything was cut.
pub(crate) fn ellipsize_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return spans;
    }
    let mut room = width.saturating_sub(ELLIPSIS.width().unwrap_or_default());
    let mut shown = Vec::new();
    let mut last_style = Style::default();
    for span in spans {
        last_style = span.style;
        let mut text = String::new();
        for character in span.content.chars() {
            let columns = character.width().unwrap_or_default();
            if columns > room {
                room = 0;
                break;
            }
            room -= columns;
            text.push(character);
        }
        let whole = text.len() == span.content.len();
        if !text.is_empty() {
            shown.push(Span::styled(text, span.style));
        }
        if !whole {
            break;
        }
    }
    shown.push(Span::styled(ELLIPSIS.to_string(), last_style));
    shown
}

/// `spans` padded with spaces to `width` columns.
pub(crate) fn pad_spans(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let columns: usize = spans.iter().map(Span::width).sum();
    if columns < width {
        spans.push(Span::raw(" ".repeat(width - columns)));
    }
    spans
}

#[cfg(test)]
pub(crate) mod tests {
    use caudra_highlight::Highlighter;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use test_case::test_case;

    use super::super::code_view::highlight_spans;
    use super::{
        COLOURED, MAX_COLOURED_BYTES, MAX_REMEMBERED, SHELL, code_spans_in, command_lines,
        command_spans, deferring, ellipsize_spans, marked_spans, overlay, pad_spans, pattern_spans,
    };
    use crate::{highlight, theme};

    pub(crate) const COLOUR_THEME: &str = "dracula";
    pub(crate) const SHELL_SYNTAX: &str = SHELL;
    pub(crate) const PYTHON_SYNTAX: &str = "python";
    pub(crate) const NOT_COLOURED: &str = "the syntax set must colour the word";
    const NOT_SHOWN: &str = "not on the screen";
    const OTHER_THEME: &str = "ayu_light";
    const PLAIN: Style = Style::new().fg(Color::Gray);
    const SLOT: Style = Style::new().fg(Color::Magenta);
    const HEREDOC: [&str; 3] = ["python3 - <<'PY'", "import json", "PY"];
    const FIRST_LINE: &str = "echo first";
    const LAST_LINE: &str = "echo last";
    const CHOICE: &str = "Yes, and allow ‹cargo test *› for this conversation";
    const SENTENCE: &str = "Runs `git status` with any arguments";
    const QUOTED: &str = "git status";
    const TEXT_KEPT: &str = "colouring must keep the text as it was";
    const ROW_KEPT: &str = "colouring a command must leave the rest of its row as it was";
    const DEFERRED: &str = "a deferring build draws only remembered commands in colour, and \
        reports any other it drew plain";
    const SCOPE_ENDED: &str = "a command drawn after the deferring build is coloured again";
    pub(crate) const NOT_TOLD_APART: &str = "the theme must colour the two differently";

    /// Colours on, in a theme whose syntax colours tell words apart.
    pub(crate) fn coloured() {
        theme::set(theme::load_by_name(COLOUR_THEME).unwrap());
        highlight::warmup();
    }

    /// The colours of `text`'s characters, once for each place it shows in
    /// `buffer`, top to bottom.
    pub(crate) fn drawn_colours(buffer: &Buffer, text: &str) -> Vec<Vec<Color>> {
        let characters: Vec<String> = text.chars().map(String::from).collect();
        let area = buffer.area;
        (area.top()..area.bottom())
            .flat_map(|y| (area.left()..area.right()).map(move |x| (x, y)))
            .filter_map(|(x, y)| {
                characters
                    .iter()
                    .zip(x..)
                    .map(|(character, x)| {
                        let cell = buffer.cell((x, y))?;
                        (cell.symbol() == character).then_some(cell.fg)
                    })
                    .collect::<Option<Vec<Color>>>()
            })
            .collect()
    }

    /// `text` shows in `buffer`, its character at `at` in `colour` in every
    /// place it does.
    pub(crate) fn assert_drawn_in(buffer: &Buffer, text: &str, at: usize, colour: Option<Color>) {
        let places = drawn_colours(buffer, text);
        assert!(!places.is_empty(), "{text}: {NOT_SHOWN}");
        for colours in places {
            assert_eq!(colours.get(at).copied(), colour, "{text}");
        }
    }

    /// The colour `language` gives the first character of `word` in `line`.
    pub(crate) fn syntax_colour(language: &str, line: &str, word: &str) -> Option<Color> {
        colour_of(
            &highlight_spans(&mut Highlighter::for_token(language), line),
            word,
        )
    }

    /// The colour `spans` draw the first character of `word` in.
    pub(crate) fn colour_of(spans: &[Span<'_>], word: &str) -> Option<Color> {
        let at = text(spans).find(word)?;
        let mut end = 0;
        spans
            .iter()
            .find(|span| {
                end += span.content.len();
                end > at
            })
            .and_then(|span| span.style.fg)
    }

    /// Each character `line` draws, with the style it is drawn in.
    pub(crate) fn styled_characters(line: &Line<'_>) -> Vec<(char, Style)> {
        line.spans
            .iter()
            .flat_map(|span| {
                span.content
                    .chars()
                    .map(move |character| (character, span.style))
            })
            .collect()
    }

    /// `coloured` reads as `plain` does, with `command` in shell colours over
    /// the style `plain` drew it in and the rest of the row as it was.
    pub(crate) fn assert_command_coloured(plain: &Line<'_>, coloured: &Line<'_>, command: &str) {
        let plain = styled_characters(plain);
        let coloured = styled_characters(coloured);
        let read = |row: &[(char, Style)]| row.iter().map(|(character, _)| character).collect();
        let text: String = read(&plain);
        assert_eq!(read(&coloured), text, "{TEXT_KEPT}");
        let at = text.find(command).expect(NOT_SHOWN);
        let start = text[..at].chars().count();
        let end = start + command.chars().count();
        let shell = styled_characters(&Line::from(overlay(command_spans(command), plain[start].1)));
        assert_ne!(shell, plain[start..end], "{NOT_TOLD_APART}");
        assert_eq!(coloured[start..end], shell, "{command}");
        assert_eq!(coloured[..start], plain[..start], "{ROW_KEPT}");
        assert_eq!(coloured[end..], plain[end..], "{ROW_KEPT}");
    }

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test_case("cargo test -p caudra-agent permissions::structured"; "words")]
    #[test_case("rg -n 'TODO|FIXME' src | head -30 > /dev/null"; "quotes_and_pipes")]
    #[test_case("  env A=\"$HOME\" && echo `date`"; "expansions")]
    fn colours_keep_the_text(command: &str) {
        coloured();
        let spans = command_spans(command);
        assert_eq!(text(&spans), command, "{TEXT_KEPT}");
        assert!(spans.iter().all(|span| span.style.fg.is_some()));
    }

    /// Coloured one line at a time, the body would read as shell.
    #[test]
    fn a_heredoc_body_is_coloured_in_the_language_it_feeds() {
        coloured();
        let body = HEREDOC[1];
        let python = highlight_spans(&mut Highlighter::for_token(PYTHON_SYNTAX), body);
        assert_ne!(command_spans(body), python, "{NOT_TOLD_APART}");
        assert_eq!(command_lines(&owned(&HEREDOC))[1], python);
    }

    #[test]
    fn lines_past_the_budget_stay_plain() {
        coloured();
        let long = "x".repeat(MAX_COLOURED_BYTES);
        let lines = command_lines(&owned(&[FIRST_LINE, long.as_str(), LAST_LINE]));
        assert!(lines[0].iter().all(|span| span.style.fg.is_some()));
        assert_eq!(
            lines[1..],
            [vec![Span::raw(long)], vec![Span::raw(LAST_LINE)]]
        );
    }

    #[test]
    fn the_memo_starts_over_when_full() {
        coloured();
        for index in 0..=MAX_REMEMBERED {
            command_spans(&format!("{FIRST_LINE} {index}"));
        }
        let remembered = COLOURED.with_borrow(|coloured| coloured.commands.len());
        assert!(remembered <= MAX_REMEMBERED, "{remembered}");
    }

    #[test]
    fn a_new_theme_recolours_a_remembered_command() {
        coloured();
        let before = command_spans(QUOTED);
        theme::set(theme::load_by_name(OTHER_THEME).unwrap());
        assert_ne!(command_spans(QUOTED), before);
    }

    #[test_case(false; "not_yet_coloured")]
    #[test_case(true; "remembered")]
    fn a_deferring_build_colours_only_remembered_commands(remembered: bool) {
        coloured();
        let plain = vec![Span::raw(QUOTED)];
        let shell = remembered.then(|| command_spans(QUOTED));
        let (spans, deferred) = deferring(|| command_spans(QUOTED));
        assert_eq!(
            (spans, deferred),
            (shell.unwrap_or_else(|| plain.clone()), !remembered),
            "{DEFERRED}"
        );
        assert_ne!(command_spans(QUOTED), plain, "{SCOPE_ENDED}");
    }

    #[test_case("cargo test -p <crate> *", &["<crate>"]; "one_slot")]
    #[test_case("cp <source> <target>", &["<source>", "<target>"]; "two_slots")]
    #[test_case("sort < in > out", &[]; "redirects")]
    #[test_case("echo <> x", &[]; "empty_name")]
    fn template_slots_take_the_slot_style(pattern: &str, slots: &[&str]) {
        coloured();
        let spans = pattern_spans(pattern, PLAIN, SLOT);
        assert_eq!(text(&spans), pattern, "{TEXT_KEPT}");
        let styled: Vec<&str> = spans
            .iter()
            .filter(|span| span.style == SLOT)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(styled, slots);
    }

    #[test]
    fn the_folder_a_scope_names_stays_in_words() {
        let spans = pattern_spans("cargo test * in ~/project/", PLAIN, SLOT);
        assert_eq!(spans.last(), Some(&Span::styled(" in ~/project/", PLAIN)));
    }

    #[test_case(true; "commands")]
    #[test_case(false; "words")]
    fn marks_colour_only_a_scope_that_names_commands(names_commands: bool) {
        coloured();
        let spans = marked_spans(CHOICE, PLAIN, SLOT, names_commands);
        assert_eq!(text(&spans), CHOICE, "{TEXT_KEPT}");
        let coloured: String = spans
            .iter()
            .filter(|span| span.style.fg != PLAIN.fg)
            .map(|span| span.content.as_ref())
            .collect();
        let expected = if names_commands { "cargo test *" } else { "" };
        assert_eq!(coloured, expected);
    }

    #[test_case(SENTENCE, QUOTED; "one_command")]
    #[test_case("Reads ~/notes/a`b", ""; "unpaired_backtick")]
    fn backticks_mark_the_commands_in_a_sentence(sentence: &str, quoted: &str) {
        coloured();
        let spans = code_spans_in(sentence, PLAIN);
        assert_eq!(text(&spans), sentence, "{TEXT_KEPT}");
        let coloured: String = spans
            .iter()
            .filter(|span| span.style.fg != PLAIN.fg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(coloured, quoted);
    }

    #[test]
    fn overlay_keeps_colours_and_adds_modifiers() {
        let style = Style::new()
            .fg(Color::Cyan)
            .add_modifier(Modifier::REVERSED);
        let spans = overlay(
            vec![
                Span::styled("git", Style::new().fg(Color::Red)),
                Span::raw(" status"),
            ],
            style,
        );
        assert_eq!(
            spans,
            vec![
                Span::styled(
                    "git",
                    Style::new().fg(Color::Red).add_modifier(Modifier::REVERSED)
                ),
                Span::styled(" status", style),
            ]
        );
    }

    #[test_case(QUOTED, 8, "git sta…"; "cut")]
    #[test_case(QUOTED, 10, QUOTED; "fits")]
    #[test_case(QUOTED, 12, QUOTED; "padded")]
    #[test_case("ls 日本", 5, "ls …"; "wide_characters")]
    fn ellipsized_spans_keep_their_width(command: &str, width: usize, shown: &str) {
        coloured();
        let cell = pad_spans(ellipsize_spans(command_spans(command), width), width);
        assert_eq!(text(&cell).trim_end(), shown);
        assert_eq!(cell.iter().map(Span::width).sum::<usize>(), width);
    }
}
