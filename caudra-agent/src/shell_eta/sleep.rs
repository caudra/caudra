use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};

use tree_sitter::{Node, ParseOptions, ParseState, Parser};

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_ANALYSIS_DEPTH: usize = 32;
const PARSE_BUDGET: Duration = Duration::from_millis(10);
const MILLIS_PER_SECOND: u64 = 1_000;
const MILLIS_PER_MINUTE: u64 = 60 * MILLIS_PER_SECOND;
const MILLIS_PER_HOUR: u64 = 60 * MILLIS_PER_MINUTE;
const MILLIS_PER_DAY: u64 = 24 * MILLIS_PER_HOUR;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Timing {
    Exact(u64),
    AtLeast(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Analysis<'a> {
    pub timing: Timing,
    pub remainder: Option<&'a str>,
}

pub(super) fn analyze(command: &str) -> Option<Analysis<'_>> {
    if command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return None;
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let started = Instant::now();
    let mut progress = |_: &ParseState| {
        if started.elapsed() >= PARSE_BUDGET {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut input = |offset: usize, _| command.as_bytes().get(offset..).unwrap_or_default();
    let tree = parser.parse_with_options(
        &mut input,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )?;
    let root = tree.root_node();
    if root.has_error() || root.kind() != "program" {
        return None;
    }
    let mut analysis = analyze_program(root, command)?;
    if analysis.remainder.is_some() && !can_detach(root, 0) {
        analysis.remainder = None;
    }
    Some(analysis)
}

fn analyze_program<'a>(root: Node<'_>, source: &'a str) -> Option<Analysis<'a>> {
    let mut total = None;
    let mut remainder_start = 0;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        if child.kind() == ";" {
            remainder_start = child.end_byte();
            continue;
        }
        if child.next_sibling().is_some_and(|next| next.kind() == "&") {
            return total.map(|millis| Analysis {
                timing: Timing::AtLeast(millis),
                remainder: None,
            });
        }
        match analyze_node(child, source, 0) {
            Some(Analysis {
                timing: Timing::Exact(millis),
                ..
            }) => {
                total = Some(total.unwrap_or_default().checked_add(millis)?);
                remainder_start = child.end_byte();
            }
            Some(Analysis {
                timing: Timing::AtLeast(millis),
                remainder,
            }) => {
                return Some(Analysis {
                    timing: Timing::AtLeast(total.unwrap_or_default().checked_add(millis)?),
                    remainder,
                });
            }
            None => {
                return total.map(|millis| Analysis {
                    timing: Timing::AtLeast(millis),
                    remainder: Some(
                        source[remainder_start..].trim_start_matches([' ', '\t', '\n']),
                    ),
                });
            }
        }
    }
    total.map(|millis| Analysis {
        timing: Timing::Exact(millis),
        remainder: None,
    })
}

fn analyze_node<'a>(node: Node<'_>, source: &'a str, depth: usize) -> Option<Analysis<'a>> {
    if depth >= MAX_ANALYSIS_DEPTH {
        return None;
    }
    match node.kind() {
        "command" => sleep_millis(node, source).map(|millis| Analysis {
            timing: Timing::Exact(millis),
            remainder: None,
        }),
        "list" => {
            let mut cursor = node.walk();
            let mut children = node
                .children(&mut cursor)
                .filter(|child| child.kind() != "comment");
            let left = children.next()?;
            let operator = children.next()?;
            let right = children.next()?;
            if operator.kind() != "&&" || children.next().is_some() {
                return None;
            }
            let left = analyze_node(left, source, depth + 1)?;
            match left.timing {
                Timing::AtLeast(_) => Some(left),
                Timing::Exact(left_millis) => match analyze_node(right, source, depth + 1) {
                    Some(mut right) => {
                        right.timing = match right.timing {
                            Timing::Exact(millis) => {
                                Timing::Exact(left_millis.checked_add(millis)?)
                            }
                            Timing::AtLeast(millis) => {
                                Timing::AtLeast(left_millis.checked_add(millis)?)
                            }
                        };
                        Some(right)
                    }
                    None => Some(Analysis {
                        timing: Timing::AtLeast(left_millis),
                        remainder: Some(
                            source[operator.end_byte()..].trim_start_matches([' ', '\t', '\n']),
                        ),
                    }),
                },
            }
        }
        _ => None,
    }
}

fn can_detach(node: Node<'_>, depth: usize) -> bool {
    if depth >= MAX_ANALYSIS_DEPTH {
        return false;
    }
    match node.kind() {
        "command" | "comment" | ";" | "&&" => true,
        "program" | "list" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .all(|child| can_detach(child, depth + 1))
        }
        _ => false,
    }
}

fn sleep_millis(node: Node<'_>, source: &str) -> Option<u64> {
    let name = node.child_by_field_name("name")?;
    let executable = literal(name, source)?;
    if !matches!(
        executable.as_str(),
        "sleep" | "/bin/sleep" | "/usr/bin/sleep"
    ) {
        return None;
    }
    let mut total = None;
    let mut cursor = node.walk();
    for (index, child) in node.children(&mut cursor).enumerate() {
        if child == name || child.kind() == "comment" {
            continue;
        }
        if node.field_name_for_child(index as u32) != Some("argument") {
            return None;
        }
        let millis = duration_millis(&literal(child, source)?)?;
        total = Some(total.unwrap_or(0_u64).checked_add(millis)?);
    }
    total
}

fn literal(node: Node<'_>, source: &str) -> Option<String> {
    let mut words = shell_words::split(source.get(node.byte_range())?).ok()?;
    if words.len() != 1 {
        return None;
    }
    let word = words.pop()?;
    word.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/'))
        .then_some(word)
}

fn duration_millis(literal: &str) -> Option<u64> {
    let (number, multiplier) = match literal.as_bytes().last()? {
        b's' => (&literal[..literal.len() - 1], MILLIS_PER_SECOND),
        b'm' => (&literal[..literal.len() - 1], MILLIS_PER_MINUTE),
        b'h' => (&literal[..literal.len() - 1], MILLIS_PER_HOUR),
        b'd' => (&literal[..literal.len() - 1], MILLIS_PER_DAY),
        _ => (literal, MILLIS_PER_SECOND),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.bytes().try_fold(0_u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })?;
    let fractional_millis = fraction.bytes().rev().fold(0_u64, |value, digit| {
        (u64::from(digit - b'0') * multiplier + value) / 10
    });
    whole
        .checked_mul(multiplier)?
        .checked_add(fractional_millis)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{Analysis, MAX_ANALYSIS_DEPTH, MAX_COMMAND_BYTES, Timing, analyze};

    const POLLING_LOOP: &str = r#"for i in $(seq 1 11); do L=$(cut -d' ' -f1 /proc/loadavg); P=$(ps -eo args | grep -E 'stdio_perf|chain2?\.sh|fanout|perf-lane|p3r7' | grep -v grep | wc -l); echo "$(date +%T) load=$L p3=$P"; awk -v l=$L 'BEGIN{exit !(l<12)}' && [ $P -eq 0 ] && break; sleep 10; done; ps -eo pcpu,args --sort=-pcpu | head -5 | cut -c1-150"#;

    #[test_case("sleep 30", 30_000; "seconds")]
    #[test_case("sleep 1m 5", 65_000; "multiple_operands")]
    #[test_case("sleep 1d 2h 3m 4s", 93_784_000; "all_units")]
    #[test_case("sleep .5 1.25s .001m", 1_810; "fractions")]
    #[test_case("sleep 1. 0002", 3_000; "decimal_forms")]
    #[test_case("sleep 0", 0; "zero")]
    #[test_case("sleep 0.0001", 0; "submillisecond")]
    #[test_case("sleep 0.999999999999999999999999999999", 999; "long_fraction")]
    #[test_case("'sleep' \"1m\" '5'", 65_000; "quoted_literals")]
    #[test_case("sl'ee'p 1'0'", 10_000; "concatenated_literals")]
    #[test_case("\\sleep \\1", 1_000; "escaped_literals")]
    #[test_case("/bin/sleep 2", 2_000; "bin_path")]
    #[test_case("\"/usr/bin/sleep\" 2", 2_000; "usr_bin_path")]
    #[test_case("sleep 10; sleep 20", 30_000; "sequence")]
    #[test_case("sleep 10 && sleep 20", 30_000; "and_sequence")]
    #[test_case("sleep 1 && sleep 2 && sleep 3", 6_000; "nested_and")]
    #[test_case("# sleep 90\nsleep 1 # sleep 80\nsleep 2;", 3_000; "comments")]
    #[test_case("sleep 18446744073709551.615", u64::MAX; "maximum_millis")]
    fn exact_sleep(command: &str, millis: u64) {
        let analysis = analyze(command);
        assert_eq!(
            analysis.as_ref().and_then(|analysis| analysis.remainder),
            None
        );
        assert_eq!(
            analysis.map(|analysis| analysis.timing),
            Some(Timing::Exact(millis))
        );
    }

    #[test_case("sleep 10; cargo test", 10_000; "mixed_sequence")]
    #[test_case("sleep 10 && cargo test", 10_000; "mixed_and")]
    #[test_case("sleep 1; sleep 2; cargo test; sleep 100", 3_000; "stop_at_unknown")]
    #[test_case("sleep 1 && cargo test && sleep 100", 1_000; "stop_at_unknown_and")]
    #[test_case("sleep 1; exit; sleep 100", 1_000; "early_exit")]
    #[test_case("sleep 1; sleep $D", 1_000; "dynamic_tail")]
    #[test_case("sleep 1; if true; then sleep 100; fi", 1_000; "conditional_tail")]
    #[test_case("sleep 1; sleep 100 &", 1_000; "background_tail")]
    #[test_case("sleep 1; sleep 100 | cat", 1_000; "pipeline_tail")]
    #[test_case("sleep 1; echo $(sleep 100)", 1_000; "substitution_tail")]
    #[test_case("sleep 1; timeout 2 sleep 100", 1_000; "timeout_tail")]
    fn mandatory_prefix(command: &str, millis: u64) {
        assert_eq!(
            analyze(command).map(|analysis| analysis.timing),
            Some(Timing::AtLeast(millis))
        );
    }

    #[test_case("sleep 30 && make build", 30_000, "make build"; "build_tail")]
    #[test_case("sleep 10&&sleep 20&&make build", 30_000, "make build"; "combined_prefix")]
    #[test_case("sleep 30;make build;echo done", 30_000, "make build;echo done"; "full_sequence")]
    #[test_case("sleep 10; sleep 20\nmake build && echo done", 30_000, "make build && echo done"; "mixed_separators")]
    #[test_case("sleep 10 && make build && sleep 20; echo done", 10_000, "make build && sleep 20; echo done"; "unknown_stops_prefix")]
    #[test_case("sleep 10 && echo 'a && b; sleep 20'\necho \"done\" # keep\n", 10_000, "echo 'a && b; sleep 20'\necho \"done\" # keep\n"; "quoted_and_trailing_source")]
    #[test_case("sleep 10; # keep this\nmake build", 10_000, "# keep this\nmake build"; "semicolon_comment")]
    #[test_case("sleep 10 # keep this\nmake build", 10_000, "# keep this\nmake build"; "newline_comment")]
    #[test_case("sleep 10 && # keep this\nmake build", 10_000, "# keep this\nmake build"; "and_comment")]
    #[test_case("sleep 10 && sleep 20; # keep this\nmake build\n", 30_000, "# keep this\nmake build\n"; "list_comment")]
    #[test_case("sleep 0 && make build", 0, "make build"; "zero_prefix")]
    #[test_case("sleep 1; sleep $D", 1_000, "sleep $D"; "dynamic_tail")]
    #[test_case("sleep 1 && \u{2003}make build", 1_000, "\u{2003}make build"; "non_shell_whitespace")]
    #[test_case("sleep 1 && echo $(sleep 100)", 1_000, "echo $(sleep 100)"; "substitution_not_stripped")]
    fn detached_remainder(command: &str, millis: u64, remainder: &str) {
        assert_eq!(
            analyze(command),
            Some(Analysis {
                timing: Timing::AtLeast(millis),
                remainder: Some(remainder),
            })
        );
    }

    #[test_case("sleep 1; sleep 100 &", 1_000; "background_tail")]
    #[test_case("sleep 1; sleep 100 | cat", 1_000; "pipeline_tail")]
    #[test_case("sleep 1; sleep 100 || make build", 1_000; "or_tail")]
    #[test_case("sleep 1; if true; then sleep 100; fi", 1_000; "conditional_tail")]
    #[test_case("sleep 1; while false; do sleep 100; done", 1_000; "loop_tail")]
    #[test_case("sleep 1; (sleep 100)", 1_000; "subshell_tail")]
    #[test_case("sleep 1; { sleep 100; }", 1_000; "group_tail")]
    #[test_case("sleep 1 && make build; sleep 100 &", 1_000; "background_after_unknown")]
    #[test_case("sleep 1 && make build; false || sleep 100", 1_000; "or_after_unknown")]
    #[test_case("sleep 1 && make build | cat", 1_000; "and_pipeline")]
    #[test_case("sleep 1; sleep 2 && make build &", 1_000; "background_list")]
    #[test_case("sleep 1; sleep 2 || make build", 1_000; "or_prefix_not_stripped")]
    fn conservative_remainder(command: &str, millis: u64) {
        assert_eq!(
            analyze(command),
            Some(Analysis {
                timing: Timing::AtLeast(millis),
                remainder: None,
            })
        );
    }

    #[test_case(POLLING_LOOP; "polling_tail")]
    fn polling_remainder(command: &str) {
        assert_eq!(
            analyze(&format!("sleep 1; {command}")),
            Some(Analysis {
                timing: Timing::AtLeast(1_000),
                remainder: None,
            })
        );
    }

    #[test_case(""; "empty")]
    #[test_case("# sleep 30"; "comment_only")]
    #[test_case("echo 'sleep 30'"; "quoted_echo")]
    #[test_case("echo \"$(sleep 30)\""; "substitution")]
    #[test_case("sh -c 'sleep 30'"; "embedded_script")]
    #[test_case("cat <<'EOF'\nsleep 30\nEOF"; "heredoc")]
    #[test_case("cat <<EOF\n$(sleep 30)\nEOF"; "heredoc_substitution")]
    #[test_case("sleep"; "no_operand")]
    #[test_case("sleep ''"; "empty_operand")]
    #[test_case("sleep -1"; "negative")]
    #[test_case("sleep +1"; "signed")]
    #[test_case("sleep 1e2"; "exponent")]
    #[test_case("sleep inf"; "infinite")]
    #[test_case("sleep NaN"; "nan")]
    #[test_case("sleep ."; "dot")]
    #[test_case("sleep 1.2.3"; "multiple_dots")]
    #[test_case("sleep 1ms"; "invalid_unit")]
    #[test_case("sleep 1 invalid"; "invalid_later_operand")]
    #[test_case("sleep 18446744073709551.616"; "millis_overflow")]
    #[test_case("sleep 18446744073709551616"; "integer_overflow")]
    #[test_case("sleep 18446744073709551.615 1"; "operand_sum_overflow")]
    #[test_case("sleep 18446744073709551.615; sleep 1"; "sequence_overflow")]
    #[test_case("sleep $SECONDS"; "variable")]
    #[test_case("sleep \"$SECONDS\""; "quoted_variable")]
    #[test_case("sleep $(echo 1)"; "dynamic_operand")]
    #[test_case("sleep `echo 1`"; "backtick_operand")]
    #[test_case("sleep $((1+2))"; "arithmetic")]
    #[test_case("sleep {1,2}"; "brace_expansion")]
    #[test_case("sleep [1]"; "glob")]
    #[test_case("sleep $'1'"; "ansi_quote")]
    #[test_case("./sleep 1"; "relative_binary")]
    #[test_case("/tmp/sleep 1"; "arbitrary_binary")]
    #[test_case("timeout 2 sleep 10"; "timeout_wrapper")]
    #[test_case("env sleep 10"; "env_wrapper")]
    #[test_case("X=1 sleep 10"; "assignment_prefix")]
    #[test_case("sleep 10 > /missing/output"; "redirect")]
    #[test_case("sleep 10 &"; "background")]
    #[test_case("sleep 10 && sleep 20 &"; "background_and")]
    #[test_case("{ sleep 10; sleep 20; } &"; "background_group")]
    #[test_case("sleep 10 | cat"; "pipeline")]
    #[test_case("sleep 10 |& cat"; "stderr_pipeline")]
    #[test_case("sleep 10 || echo failed"; "or_list")]
    #[test_case("sleep 10 && make build || echo failed"; "outer_or_list")]
    #[test_case("sleep 10 || make build && echo done"; "inner_or_list")]
    #[test_case("false && sleep 10"; "conditional_sleep")]
    #[test_case("if true; then sleep 10; fi"; "conditional")]
    #[test_case("while false; do sleep 10; done"; "loop")]
    #[test_case("sleep() { :; }; sleep 10"; "function_override")]
    #[test_case("function sleep { :; }; sleep 10"; "alternate_function")]
    #[test_case("alias sleep=true; sleep 10"; "alias_override")]
    #[test_case("eval 'sleep 10'"; "eval")]
    #[test_case("source script; sleep 10"; "sourced_override")]
    #[test_case("exec sleep 10"; "exec_wrapper")]
    #[test_case("(sleep 10)"; "subshell")]
    #[test_case("sleep 10 &&"; "truncated_list")]
    #[test_case("sleep 10; echo '"; "unterminated_quote")]
    #[test_case("sleep 10\0"; "nul")]
    #[test_case(POLLING_LOOP; "polling_regression")]
    fn unknown(command: &str) {
        assert_eq!(analyze(command), None);
    }

    #[test_case(MAX_COMMAND_BYTES + 1; "input_limit")]
    #[test_case(MAX_COMMAND_BYTES * 2; "larger_input")]
    fn bounded_input(length: usize) {
        assert_eq!(analyze(&format!("sleep 1 #{}", "x".repeat(length))), None);
    }

    #[test_case(MAX_ANALYSIS_DEPTH + 1; "depth_limit")]
    #[test_case(MAX_ANALYSIS_DEPTH * 2; "deeper_chain")]
    fn bounded_analysis(depth: usize) {
        assert_eq!(analyze(&vec!["sleep 1"; depth + 1].join(" && ")), None);
    }

    #[test_case(MAX_ANALYSIS_DEPTH + 1; "depth_limit")]
    #[test_case(MAX_ANALYSIS_DEPTH * 2; "deeper_chain")]
    fn bounded_remainder(depth: usize) {
        let command = format!("sleep 1; {}", vec!["make build"; depth + 1].join(" && "));
        assert_eq!(
            analyze(&command),
            Some(Analysis {
                timing: Timing::AtLeast(1_000),
                remainder: None,
            })
        );
    }
}
