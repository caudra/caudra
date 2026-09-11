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
const LAST_LINE: &str = "$";

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

    use super::sed_only_prints;

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
}
