//! Decides whether a `sed` invocation can only write to stdout.
//!
//! Rejecting flags cannot make sed safe: `w` writes and `e` executes from inside
//! the script, and neither one is a flag. So the script is what gets recognized,
//! and only one shape is, a list of line addresses to print. Everything else is
//! refused and prompted, which is the bargain the rest of the read-only
//! allowlist makes too: the list never has to be exhaustive to be safe.

/// Short options that change how sed reads or matches. None of them takes a
/// value, and leaving out every option that does is what makes the first bare
/// word the script, with no way for a flag to eat it first.
const HARMLESS_SHORT_FLAGS: &str = "nszEru";
/// The long spellings of the same, chosen under the same rule.
const HARMLESS_LONG_FLAGS: &[&str] = &[
    "--debug",
    "--null-data",
    "--posix",
    "--quiet",
    "--regexp-extended",
    "--sandbox",
    "--separate",
    "--silent",
    "--unbuffered",
];
const PRINT: char = 'p';
const DELETE: char = 'd';
const SUBSTITUTE: char = 's';
const TRANSLITERATE: char = 'y';
const LAST_LINE: &str = "$";
const IN_PLACE_FLAG: char = 'i';
const EXPRESSION_FLAG: char = 'e';
const IN_PLACE_LONG: &str = "in-place";
const EXPRESSION_LONG: &str = "expression";
const END_OF_OPTIONS: &str = "--";
const STANDARD_INPUT: &str = "-";
/// Flags a substitution may carry without writing, reading or running anything
/// but the line it rewrites, which leaves out `w` and `e`.
const SUBSTITUTE_FLAGS: &str = "gpIiMm0123456789";
/// A backup suffix holding either of these names a path the suffix builds
/// rather than one it ends.
const BACKUP_PATTERN_CHARACTERS: [char; 2] = ['*', '/'];
const BLANKS: [char; 2] = [' ', '\t'];
const COMMAND_SEPARATORS: [char; 2] = [';', '\n'];
const REGEX_DELIMITER: char = '/';
const ESCAPE: char = '\\';
const BRACKET_OPEN: char = '[';
const BRACKET_CLOSE: char = ']';
const BRACKET_NEGATION: char = '^';

/// Reports whether a `sed` call only prints the lines it was given.
///
/// `arguments` are the words after the executable. Establishing that each one
/// means its own text is the caller's job, because a word that stands for
/// something outside the command describes a script nobody reviewed.
pub fn sed_only_prints(arguments: &[&str]) -> bool {
    let mut script = None;
    for argument in arguments {
        if argument.starts_with("--") {
            if !HARMLESS_LONG_FLAGS.contains(argument) {
                return false;
            }
        } else if let Some(cluster) = argument.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            if !cluster
                .chars()
                .all(|flag| HARMLESS_SHORT_FLAGS.contains(flag))
            {
                return false;
            }
        } else if script.is_none() {
            script = Some(*argument);
        }
    }
    // Operands after the script are files, judged by the confinement check that
    // judges every other reader's operands.
    script.is_some_and(prints_every_address)
}

/// The files a `sed` call writes, backups included, or `None` when it could
/// write anything else.
///
/// Only scripts that substitute, transliterate, delete or print at a line, the
/// last line or a regex, or a range of those, are recognized, and so only the
/// operands can change: a call without `-i` writes nothing but its standard
/// output. An option that is not understood, a script file among them, is
/// refused, as is a backup suffix that builds a path instead of ending one.
pub fn sed_written_files(arguments: &[&str]) -> Option<Vec<String>> {
    let mut in_place = None;
    let mut scripts = Vec::new();
    let mut operands = Vec::new();
    let mut options_ended = false;
    let mut words = arguments.iter().copied();
    while let Some(word) = words.next() {
        if options_ended || word == STANDARD_INPUT || !word.starts_with('-') {
            operands.push(word);
        } else if word == END_OF_OPTIONS {
            options_ended = true;
        } else if let Some(long) = word.strip_prefix(END_OF_OPTIONS) {
            match long.split_once('=') {
                Some((IN_PLACE_LONG, suffix)) => in_place = Some(suffix),
                Some((EXPRESSION_LONG, script)) => scripts.push(script),
                None if long == IN_PLACE_LONG => in_place = Some(""),
                None if long == EXPRESSION_LONG => scripts.push(words.next()?),
                None if HARMLESS_LONG_FLAGS.contains(&word) => {}
                _ => return None,
            }
        } else {
            // A flag that takes a value takes the rest of its cluster, so
            // `-in` backs up to a suffix of `n` rather than adding `-n`.
            let cluster = &word[1..];
            for (index, flag) in cluster.char_indices() {
                let rest = &cluster[index + flag.len_utf8()..];
                match flag {
                    IN_PLACE_FLAG => in_place = Some(rest),
                    EXPRESSION_FLAG if rest.is_empty() => scripts.push(words.next()?),
                    EXPRESSION_FLAG => scripts.push(rest),
                    flag if HARMLESS_SHORT_FLAGS.contains(flag) => continue,
                    _ => return None,
                }
                break;
            }
        }
    }
    if scripts.is_empty() {
        if operands.is_empty() {
            return None;
        }
        scripts.push(operands.remove(0));
    }
    if !scripts.into_iter().all(edits_only_its_input) {
        return None;
    }
    let Some(suffix) = in_place else {
        return Some(Vec::new());
    };
    if suffix.contains(BACKUP_PATTERN_CHARACTERS) {
        return None;
    }
    let backups = operands
        .iter()
        .filter(|_| !suffix.is_empty())
        .map(|file| format!("{file}{suffix}"));
    Some(
        operands
            .iter()
            .map(|file| (*file).to_owned())
            .chain(backups)
            .collect(),
    )
}

/// Whether every command in a script is one [`sed_written_files`] recognizes,
/// each ending at a separator or at the end of the script.
fn edits_only_its_input(script: &str) -> bool {
    let mut rest = script;
    loop {
        rest = rest.trim_start_matches(|character| {
            BLANKS.contains(&character) || COMMAND_SEPARATORS.contains(&character)
        });
        if rest.is_empty() {
            return true;
        }
        let Some(after) = command_end(skip_addresses(rest)) else {
            return false;
        };
        rest = after.trim_start_matches(BLANKS);
        if !rest.is_empty() && !rest.starts_with(COMMAND_SEPARATORS) {
            return false;
        }
    }
}

/// The text after an optional address or range of two.
fn skip_addresses(command: &str) -> &str {
    let Some(rest) = skip_address(command) else {
        return command;
    };
    match rest.strip_prefix(',') {
        Some(second) => skip_address(second).unwrap_or(second),
        None => rest,
    }
}

/// The text after a line, last-line or regex address, if the text starts
/// with one.
fn skip_address(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix(LAST_LINE) {
        return Some(rest);
    }
    if let Some(regex) = text.strip_prefix(REGEX_DELIMITER) {
        return delimited_regex(regex, REGEX_DELIMITER);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| &text[digits..])
}

/// The text after a recognized command, or `None` when the command is not
/// one: a substitution without `w` or `e`, a transliteration, a deletion or a
/// print.
fn command_end(command: &str) -> Option<&str> {
    let mut characters = command.chars();
    let name = characters.next()?;
    let body = characters.as_str();
    match name {
        DELETE | PRINT => Some(body),
        SUBSTITUTE => {
            let delimiter = delimiter(body)?;
            let replacement = delimited_regex(&body[delimiter.len_utf8()..], delimiter)?;
            let flags = delimited(replacement, delimiter)?;
            Some(flags.trim_start_matches(|flag| SUBSTITUTE_FLAGS.contains(flag)))
        }
        TRANSLITERATE => {
            let delimiter = delimiter(body)?;
            delimited(
                delimited(&body[delimiter.len_utf8()..], delimiter)?,
                delimiter,
            )
        }
        _ => None,
    }
}

fn delimiter(body: &str) -> Option<char> {
    body.chars()
        .next()
        .filter(|&delimiter| delimiter != ESCAPE && delimiter != '\n')
}

/// The text after the next delimiter no backslash escapes, which is where GNU
/// sed ends a part. An unescaped newline ends the command first.
fn delimited(text: &str, delimiter: char) -> Option<&str> {
    let mut characters = text.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            ESCAPE => {
                characters.next()?;
            }
            '\n' => return None,
            character if character == delimiter => {
                return Some(&text[index + delimiter.len_utf8()..]);
            }
            _ => {}
        }
    }
    None
}

/// [`delimited`] for a regex, which BSD sed reads differently: it keeps going
/// past the delimiter to close a bracket expression. A regex that leaves one
/// open would end in a different place on each, so it is refused.
fn delimited_regex(text: &str, delimiter: char) -> Option<&str> {
    let rest = delimited(text, delimiter)?;
    let regex = &text[..text.len() - rest.len() - delimiter.len_utf8()];
    brackets_close(regex).then_some(rest)
}

fn brackets_close(regex: &str) -> bool {
    let mut characters = regex.chars();
    while let Some(character) = characters.next() {
        match character {
            ESCAPE => {
                characters.next();
            }
            BRACKET_OPEN => {
                let body = characters.as_str();
                let body = body.strip_prefix(BRACKET_NEGATION).unwrap_or(body);
                let body = body.strip_prefix(BRACKET_CLOSE).unwrap_or(body);
                let Some(end) = body.find(BRACKET_CLOSE) else {
                    return false;
                };
                characters = body[end + BRACKET_CLOSE.len_utf8()..].chars();
            }
            _ => {}
        }
    }
    true
}

fn prints_every_address(script: &str) -> bool {
    script
        .split(';')
        .all(|command| command.strip_suffix(PRINT).is_some_and(is_address))
}

fn is_address(address: &str) -> bool {
    match address.split_once(',') {
        Some((first, last)) => is_line(first) && (last == LAST_LINE || is_line(last)),
        None => address == LAST_LINE || is_line(address),
    }
}

fn is_line(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{sed_only_prints, sed_written_files};

    #[test_case(&["-n", "1,140p", "f.rs"] => true ; "the_shape_models_actually_send")]
    #[test_case(&["-n", "1266,1276p;2046,2064p", "f.rs"] => true ; "several_ranges_in_one_script")]
    #[test_case(&["-n", "5p", "f.rs"] => true ; "a_single_line")]
    #[test_case(&["-n", "5,$p", "f.rs"] => true ; "a_range_ending_at_the_last_line")]
    #[test_case(&["-n", "$p", "f.rs"] => true ; "the_last_line_alone")]
    #[test_case(&["-nE", "1,2p", "f.rs"] => true ; "a_cluster_of_harmless_flags")]
    #[test_case(&["--quiet", "1,2p", "f.rs"] => true ; "the_long_spelling")]
    #[test_case(&["-n", "1,2p"] => true ; "reading_stdin_takes_no_operand")]
    #[test_case(&["-n", "1,2p", "/etc/passwd"] => true ; "an_operand_is_left_to_the_confinement_check")]
    #[test_case(&["-i", "s/a/b/", "f.rs"] => false ; "in_place_writes")]
    #[test_case(&["-ni", "1,2p", "f.rs"] => false ; "in_place_hiding_in_a_cluster")]
    #[test_case(&["--in-place", "s/a/b/", "f.rs"] => false ; "in_place_spelled_long")]
    #[test_case(&["-n", "1w /tmp/x", "f.rs"] => false ; "the_script_writes")]
    #[test_case(&["-n", "1,10p;1w /tmp/x", "f.rs"] => false ; "one_bad_command_taints_the_script")]
    #[test_case(&["-n", "1e rm -rf /", "f.rs"] => false ; "the_script_executes")]
    #[test_case(&["-n", "1r /etc/passwd", "f.rs"] => false ; "the_script_reads_a_path_confinement_cannot_see")]
    #[test_case(&["-n", "/^mod tests/,/^ use /p", "f.rs"] => false ; "a_regex_address_is_not_recognized")]
    #[test_case(&["-n", "1~2p", "f.rs"] => false ; "a_step_address_is_not_recognized")]
    #[test_case(&["-n", "1,+5p", "f.rs"] => false ; "a_relative_address_is_not_recognized")]
    #[test_case(&["-f", "script.sed", "f.rs"] => false ; "a_script_that_is_not_in_the_command")]
    #[test_case(&["-e", "1,2p", "f.rs"] => false ; "a_flag_that_takes_a_value")]
    #[test_case(&["--", "1,2p", "f.rs"] => false ; "end_of_options_is_not_recognized")]
    #[test_case(&["-n"] => false ; "no_script_at_all")]
    #[test_case(&[] => false ; "no_arguments_at_all")]
    fn sed_calls_are_classified(arguments: &[&str]) -> bool {
        sed_only_prints(arguments)
    }

    #[test_case(&["-i", "s/a/b/g", "f.rs"], Some(&["f.rs"]) ; "in_place")]
    #[test_case(&["-i.bak", "s/a/b/", "f.rs"], Some(&["f.rs", "f.rs.bak"]) ; "a_backup_beside_each_file")]
    #[test_case(&["-in", "s/a/b/", "f.rs"], Some(&["f.rs", "f.rsn"]) ; "the_suffix_takes_the_rest_of_its_cluster")]
    #[test_case(&["-ni", "s/a/b/p", "f.rs"], Some(&["f.rs"]) ; "in_place_closing_a_cluster")]
    #[test_case(&["--in-place=.orig", "-e", "s/a/b/", "--expression=/x/d", "a", "b"], Some(&["a", "b", "a.orig", "b.orig"]) ; "long_spellings_and_several_scripts")]
    #[test_case(&["-E", "-i", "1,$s/[0-9]+/N/g;/^$/d", "f"], Some(&["f"]) ; "ranges_regex_addresses_and_brackets")]
    #[test_case(&["-i", "s;a/b;c;g", "f"], Some(&["f"]) ; "a_separator_as_the_delimiter")]
    #[test_case(&["-i", "y/abc/xyz/", "f"], Some(&["f"]) ; "a_transliteration")]
    #[test_case(&["s/a/b/", "f.rs"], Some(&[]) ; "without_in_place_only_standard_output_changes")]
    #[test_case(&["-i", "s/a/b/w out", "f"], None ; "a_substitution_that_writes")]
    #[test_case(&["-i", "s/a/b/e", "f"], None ; "a_substitution_that_executes")]
    #[test_case(&["-i", "1w out", "f"], None ; "a_write_command")]
    #[test_case(&["s/a/b/;1w out", "f"], None ; "a_write_without_in_place")]
    #[test_case(&["-i", "{s/a/b/}", "f"], None ; "a_block")]
    #[test_case(&["-i", "s/[/]/g;/w out/d", "f"], None ; "a_bracket_bsd_would_close_past_the_delimiter")]
    #[test_case(&["-i", "-f", "d", "f"], None ; "a_script_file")]
    #[test_case(&["-i*.bak", "s/a/b/", "f"], None ; "a_suffix_that_builds_a_path")]
    #[test_case(&["--follow-symlinks", "-i", "s/a/b/", "f"], None ; "an_option_that_is_not_understood")]
    #[test_case(&["-i"], None ; "no_script")]
    fn sed_writes_are_named(arguments: &[&str], expected: Option<&[&str]>) {
        assert_eq!(
            sed_written_files(arguments),
            expected.map(|files| files.iter().map(ToString::to_string).collect())
        );
    }
}
