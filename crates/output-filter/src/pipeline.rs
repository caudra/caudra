//! The ordered transformation pipeline applied by a matched rule.
//!
//! Stages run in a fixed order because later stages assume earlier ones have
//! run: substitutions must land before short-circuit matching, line selection
//! before per-line truncation, and the absolute cap after the head/tail window
//! so omission markers are themselves counted.

use std::sync::LazyLock;

use regex::Regex;

use crate::compile::Rule;

/// Input beyond this size is reduced to its tail before filtering. The bound
/// matches the shell tool's per-stream capture ring, so the normal path never
/// trims and an unexpected caller still cannot make filtering unbounded.
const MAX_INPUT_BYTES: usize = 1_048_576;
const MAX_INPUT_LINES: usize = 100_000;

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("ANSI pattern is valid"));

/// Result of applying a rule.
pub struct Filtered {
    /// The rendered text. May be a synthetic message rather than command output.
    pub text: String,
    /// Whether any line was dropped, trimmed, or replaced by a message.
    pub lossy: bool,
    /// Whether stderr was folded into `text`. When false the caller still owes
    /// the reader stderr, because a rule that only describes stdout must not
    /// cause a diagnostic written to stderr to disappear.
    pub consumed_stderr: bool,
}

impl Rule {
    /// Applies this rule to captured output.
    ///
    /// `exit_code` gates the two stages that can replace real output with a
    /// success message. A command that failed never has its output collapsed to
    /// a synthetic "ok", because the caller cannot tell such a message apart
    /// from a genuine success line.
    #[must_use]
    pub fn apply(&self, stdout: &str, stderr: &str, exit_code: Option<i32>) -> Filtered {
        let succeeded = exit_code == Some(0);
        let source = if self.filter_stderr && !stderr.is_empty() {
            if stdout.is_empty() {
                stderr.to_owned()
            } else {
                format!("{stdout}\n{stderr}")
            }
        } else {
            stdout.to_owned()
        };

        let (source, mut lossy) = bound_input(&source);
        let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();

        if self.strip_ansi {
            lines = lines.iter().map(|line| strip_ansi(line)).collect();
        }

        if !self.replace.is_empty() {
            lines = lines
                .into_iter()
                .map(|mut line| {
                    for rule in &self.replace {
                        line = rule
                            .pattern
                            .replace_all(&line, rule.replacement.as_str())
                            .into_owned();
                    }
                    line
                })
                .collect();
        }

        if succeeded && !self.match_output.is_empty() {
            let blob = lines.join("\n");
            for rule in &self.match_output {
                if !rule.pattern.is_match(&blob) {
                    continue;
                }
                if rule.unless.as_ref().is_some_and(|re| re.is_match(&blob)) {
                    continue;
                }
                return Filtered {
                    text: rule.message.clone(),
                    lossy: true,
                    consumed_stderr: self.filter_stderr,
                };
            }
        }

        if !self.strip_lines_matching.is_empty() {
            let before = lines.len();
            lines.retain(|line| !self.strip_lines_matching.iter().any(|re| re.is_match(line)));
            lossy |= lines.len() != before;
        } else if !self.keep_lines_matching.is_empty() {
            let before = lines.len();
            lines.retain(|line| self.keep_lines_matching.iter().any(|re| re.is_match(line)));
            lossy |= lines.len() != before;
        }

        if let Some(limit) = self.truncate_lines_at {
            lines = lines
                .into_iter()
                .map(|line| {
                    let truncated = truncate_chars(&line, limit);
                    lossy |= truncated != line;
                    truncated
                })
                .collect();
        }

        let total = lines.len();
        match (self.head_lines, self.tail_lines) {
            (Some(head), Some(tail)) if total > head + tail => {
                let mut result = lines[..head].to_vec();
                result.push(omitted(total - head - tail));
                result.extend_from_slice(&lines[total - tail..]);
                lines = result;
                lossy = true;
            }
            (Some(head), None) if total > head => {
                lines.truncate(head);
                lines.push(omitted(total - head));
                lossy = true;
            }
            (None, Some(tail)) if total > tail => {
                let dropped = total - tail;
                lines = lines.split_off(dropped);
                lines.insert(0, omitted(dropped));
                lossy = true;
            }
            _ => {}
        }

        if let Some(max) = self.max_lines
            && lines.len() > max
        {
            let dropped = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("... ({dropped} lines truncated)"));
            lossy = true;
        }

        let text = lines.join("\n");
        if succeeded
            && text.trim().is_empty()
            && let Some(message) = &self.on_empty
        {
            return Filtered {
                text: message.clone(),
                // The returned text is synthetic rather than command output, so
                // this is a replacement even when no line was dropped to reach
                // it. A caller distinguishing a rendered result from a
                // pass-through depends on that being reported.
                lossy: true,
                consumed_stderr: self.filter_stderr,
            };
        }
        Filtered {
            text,
            lossy,
            consumed_stderr: self.filter_stderr,
        }
    }
}

/// Keeps the newest content when input exceeds the processing bound, matching
/// the shell capture ring's rationale that failures and summaries appear last.
fn bound_input(source: &str) -> (String, bool) {
    let mut trimmed = false;
    let mut bounded = source;
    if bounded.len() > MAX_INPUT_BYTES {
        let start = bounded.len() - MAX_INPUT_BYTES;
        let start = (start..bounded.len())
            .find(|index| bounded.is_char_boundary(*index))
            .unwrap_or(bounded.len());
        bounded = &bounded[start..];
        trimmed = true;
    }
    let line_count = bounded.lines().count();
    if line_count > MAX_INPUT_LINES {
        let skip = line_count - MAX_INPUT_LINES;
        let kept = bounded.lines().skip(skip).collect::<Vec<_>>().join("\n");
        return (kept, true);
    }
    (bounded.to_owned(), trimmed)
}

fn strip_ansi(text: &str) -> String {
    ANSI.replace_all(text, "").into_owned()
}

/// Truncates by character count so multi-byte text is never split mid-scalar.
fn truncate_chars(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_owned();
    }
    if max < 3 {
        return "...".to_owned();
    }
    format!("{}...", line.chars().take(max - 3).collect::<String>())
}

fn omitted(count: usize) -> String {
    format!("... ({count} lines omitted)")
}

#[cfg(test)]
mod tests {
    use crate::builtin;

    #[test]
    fn failure_does_not_collapse_to_a_success_message() {
        let rule = builtin().rule("liquibase").expect("liquibase rule");
        let ok = rule.apply("", "", Some(0));
        assert_eq!(ok.text, "liquibase: ok");
        let failed = rule.apply("", "", Some(1));
        assert_eq!(failed.text, "");
    }

    #[test]
    fn failure_does_not_short_circuit_on_match_output() {
        let rule = builtin().rule("bundle-install").expect("bundle rule");
        let input = "Bundle complete!";
        assert_eq!(rule.apply(input, "", Some(0)).text, "ok bundle: complete");
        assert_eq!(rule.apply(input, "", Some(1)).text, input);
    }

    #[test]
    fn signalled_commands_are_treated_as_failures() {
        let rule = builtin().rule("liquibase").expect("liquibase rule");
        assert_eq!(rule.apply("", "", None).text, "");
    }

    #[test]
    fn a_synthetic_success_message_is_reported_as_a_replacement() {
        // A caller uses `lossy` to tell a rendered result from a pass-through.
        // An `on_empty` message is synthetic even when no line was dropped.
        let rule = builtin().rule("liquibase").expect("liquibase rule");
        let filtered = rule.apply("", "", Some(0));
        assert_eq!(filtered.text, "liquibase: ok");
        assert!(filtered.lossy);
    }

    #[test]
    fn oversized_input_is_reduced_to_its_tail() {
        let rule = builtin().rule("df").expect("df rule");
        let input = "line\n".repeat(super::MAX_INPUT_LINES + 10);
        let filtered = rule.apply(&input, "", Some(0));
        assert!(filtered.lossy);
        assert!(filtered.text.lines().count() <= super::MAX_INPUT_LINES);
    }
}
