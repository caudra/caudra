//! Upstream's `uncitedConditions`: each condition of an action that no comment ties to code. A
//! condition is a conjunct of an action that changes nothing and reads a variable the action does
//! not change, and a comment with a file and line, as in `\* pay.ts:120`, cites it. A rule can
//! hold only because the model waits for such a condition, so `from-spec` lists them when a rule
//! holds.

use std::collections::{HashMap, HashSet};
use std::iter;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::Serialize;

use super::text::collapse_spaces;

const LINE_COMMENT: &[u8] = b"\\*";
const BLOCK_OPEN: &[u8] = b"(*";
const BLOCK_CLOSE: &[u8] = b"*)";
const AND: &[u8] = b"/\\";
const OR: &[u8] = b"\\/";
const EXISTS: &[u8] = b"\\E";
const SEQUENCE_OPEN: &[u8] = b"<<";
const SEQUENCE_CLOSE: &[u8] = b">>";
const RECORD_FIELD: &[u8] = b"|->";
/// The operators that bind looser than `/\`, so a conjunct cannot be cut out around them.
const LOOSER_THAN_AND: [&[u8]; 5] = [b"\\/", b"=>", b"<=>", b"~>", b"-+->"];
const EQUIV: &[u8] = b"\\equiv";
/// The forms whose body runs to the end of the expression, so no `/\` after them is the action's.
const RUNS_TO_END: [&[u8]; 6] = [b"\\E", b"\\A", b"IF", b"LET", b"CASE", b"CHOOSE"];

static VARIABLES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bVARIABLES?\s+(\w+(?:\s*,\s*\w+)*)").expect("valid regex"));
static PRIMES_OR_UNCHANGED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\w)\]>}]'|\bUNCHANGED\b").expect("valid regex"));
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:LOCAL[ \t]+)?([A-Za-z_]\w*)[ \t]*(?:\([^)]*\)|\[[^\]]*\])?[ \t]*==")
        .expect("valid regex")
});
static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\w./-]+\.[A-Za-z]\w*:\d+|#L\d+").expect("valid regex"));
static STRING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:[^"\\\n]|\\.)*""#).expect("valid regex"));

/// One condition no comment cites, as upstream's `summary.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Uncited {
    /// From 1.
    pub line: usize,
    pub action: String,
    pub condition: String,
}

/// The names a spec declares with `VARIABLE` or `VARIABLES`.
pub fn tla_variables(code: &str) -> Vec<String> {
    VARIABLES
        .captures_iter(code)
        .filter_map(|found| found.get(1))
        .flat_map(|list| list.as_str().split(','))
        .map(|name| name.trim().to_owned())
        .collect()
}

/// Every condition of every action in `text` that no comment cites, in definition order.
pub fn uncited_conditions(text: &str) -> Vec<Uncited> {
    let (code, notes) = split_comments(text);
    let bare = STRING
        .replace_all(&code, |found: &Captures| " ".repeat(found[0].len()))
        .into_owned();
    Spec::new(&bare).uncited(&code, &notes)
}

/// The text twice, each the same length line for line: the code with comments blanked, and the
/// comments with the code blanked.
fn split_comments(text: &str) -> (String, String) {
    let bytes = text.as_bytes();
    let mut code = String::with_capacity(text.len());
    let mut notes = String::with_capacity(text.len());
    let mut block = 0usize;
    let mut line = false;
    let mut string = false;
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        let two = &bytes[at..bytes.len().min(at + 2)];
        let piece = if (block > 0 && (two == BLOCK_OPEN || two == BLOCK_CLOSE))
            || (block == 0 && !line && !string && (two == LINE_COMMENT || two == BLOCK_OPEN))
            || (string && c == '\\' && two.len() == 2 && two[1] != b'\n')
        {
            &text[at..at + text[at + 1..].chars().next().map_or(1, char::len_utf8) + 1]
        } else {
            &text[at..at + c.len_utf8()]
        };
        let blank = " ".repeat(piece.len());
        let is_code = if c == '\n' {
            line = false;
            string = false;
            code.push(c);
            notes.push(c);
            at += 1;
            continue;
        } else if block > 0 {
            if two == BLOCK_OPEN {
                block += 1;
            } else if two == BLOCK_CLOSE {
                block -= 1;
            }
            false
        } else if line {
            false
        } else if string {
            string = piece.len() > c.len_utf8() || c != '"';
            true
        } else if two == LINE_COMMENT {
            line = true;
            false
        } else if two == BLOCK_OPEN {
            block = 1;
            false
        } else {
            string = c == '"';
            true
        };
        let (kept, blanked) = if is_code {
            (&mut code, &mut notes)
        } else {
            (&mut notes, &mut code)
        };
        kept.push_str(piece);
        blanked.push_str(&blank);
        at += piece.len();
    }
    (code, notes)
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn starts_with_word(bytes: &[u8], word: &[u8]) -> bool {
    bytes.starts_with(word) && !bytes.get(word.len()).copied().is_some_and(is_word)
}

fn depth_change(byte: u8) -> isize {
    match byte {
        b'(' | b'[' | b'{' => 1,
        b')' | b']' | b'}' => -1,
        _ => 0,
    }
}

/// What a stretch of code does on its own: the variables it reads and primes, whether it changes
/// any, and the definitions it names.
#[derive(Debug, Default, Clone)]
struct Direct {
    reads: Vec<String>,
    writes: Vec<String>,
    acts: bool,
    calls: Vec<String>,
}

/// The same, through every definition it names.
#[derive(Debug, Default)]
struct Through {
    reads: HashSet<String>,
    writes: HashSet<String>,
    acts: bool,
}

struct Definition {
    name: String,
    from: usize,
    to: usize,
    line: usize,
}

struct Spec<'a> {
    bare: &'a str,
    lines: Vec<&'a str>,
    variables: HashSet<String>,
    starts: Vec<usize>,
    definitions: Vec<Definition>,
    own: HashMap<String, Direct>,
}

impl<'a> Spec<'a> {
    fn new(bare: &'a str) -> Self {
        let starts: Vec<usize> = iter::once(0)
            .chain(bare.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        let mut spec = Self {
            bare,
            lines: bare.split('\n').collect(),
            variables: tla_variables(bare).into_iter().collect(),
            starts,
            definitions: Vec::new(),
            own: HashMap::new(),
        };
        for found in DEFINITION.captures_iter(bare) {
            let (Some(whole), Some(name)) = (found.get(0), found.get(1)) else {
                continue;
            };
            let definition = Definition {
                name: name.as_str().to_owned(),
                from: whole.end(),
                to: spec.next_item(whole.end()),
                line: spec.line_of(whole.start()),
            };
            match spec
                .definitions
                .iter_mut()
                .find(|known| known.name == definition.name)
            {
                Some(known) => *known = definition,
                None => spec.definitions.push(definition),
            }
        }
        let names: HashSet<&str> = spec.definitions.iter().map(|d| d.name.as_str()).collect();
        let own = spec
            .definitions
            .iter()
            .map(|d| (d.name.clone(), spec.direct(&bare[d.from..d.to], &names)))
            .collect();
        spec.own = own;
        spec
    }

    /// Where the definition that starts at `from` ends: the next line that starts at the margin
    /// with neither `/\` nor `\/`.
    fn next_item(&self, from: usize) -> usize {
        let bytes = self.bare.as_bytes();
        self.starts
            .iter()
            .copied()
            .filter(|&start| start > from)
            .find(|&start| {
                let rest = &bytes[start..];
                !matches!(rest.first(), Some(b' ' | b'\t' | b'\n'))
                    && !rest.starts_with(AND)
                    && !rest.starts_with(OR)
            })
            .unwrap_or(bytes.len())
    }

    fn line_of(&self, at: usize) -> usize {
        self.starts.partition_point(|&start| start <= at) - 1
    }

    fn line_start(&self, at: usize) -> usize {
        self.starts[self.line_of(at)]
    }

    fn direct(&self, body: &str, definitions: &HashSet<&str>) -> Direct {
        let mut direct = Direct {
            acts: PRIMES_OR_UNCHANGED.is_match(body),
            ..Direct::default()
        };
        for (name, after) in identifiers(body) {
            let rest = &body.as_bytes()[after..];
            if rest.first() == Some(&b'\'') && self.variables.contains(name) {
                direct.writes.push(name.to_owned());
            }
            if rest.trim_ascii_start().starts_with(RECORD_FIELD) {
                continue;
            }
            if self.variables.contains(name) {
                direct.reads.push(name.to_owned());
            }
            if definitions.contains(name) && !direct.calls.iter().any(|call| call == name) {
                direct.calls.push(name.to_owned());
            }
        }
        direct
    }

    fn through(&self, start: &Direct) -> Through {
        let mut out = Through {
            reads: start.reads.iter().cloned().collect(),
            writes: start.writes.iter().cloned().collect(),
            acts: start.acts,
        };
        let mut seen: HashSet<&str> = start.calls.iter().map(String::as_str).collect();
        let mut todo: Vec<&str> = start.calls.iter().map(String::as_str).collect();
        while let Some(name) = todo.pop() {
            let Some(called) = self.own.get(name) else {
                continue;
            };
            out.reads.extend(called.reads.iter().cloned());
            out.writes.extend(called.writes.iter().cloned());
            out.acts |= called.acts;
            for call in &called.calls {
                if seen.insert(call) {
                    todo.push(call);
                }
            }
        }
        out
    }

    /// The conjuncts of the expression in `from..to`, or `None` when it is not a conjunction the
    /// action's conditions can be read from.
    fn conjuncts(&self, from: usize, to: usize) -> Option<Vec<(usize, usize)>> {
        let bytes = self.bare.as_bytes();
        let first = from
            + bytes[from..to]
                .iter()
                .position(|b| !b.is_ascii_whitespace())?;
        if starts_with_word(&bytes[first..to], EXISTS) {
            let mut depth = 0;
            for (at, &byte) in bytes.iter().enumerate().take(to).skip(first) {
                depth += depth_change(byte);
                if depth == 0 && byte == b':' {
                    return self.conjuncts(at + 1, to);
                }
            }
            return None;
        }
        if bytes[first..].starts_with(AND) {
            return self.bulleted(first, to);
        }
        let mut cuts = Vec::new();
        let mut depth = 0isize;
        let mut at = first;
        while at < to {
            let rest = &bytes[at..];
            if rest.starts_with(SEQUENCE_OPEN) || rest.starts_with(SEQUENCE_CLOSE) {
                depth += if rest.starts_with(SEQUENCE_OPEN) {
                    1
                } else {
                    -1
                };
                at += 1;
            } else if depth_change(bytes[at]) != 0 {
                depth += depth_change(bytes[at]);
            } else if depth > 0 {
            } else if LOOSER_THAN_AND.iter().any(|op| rest.starts_with(op))
                || starts_with_word(rest, EQUIV)
            {
                return None;
            } else if !at
                .checked_sub(1)
                .is_some_and(|before| is_word(bytes[before]))
                && RUNS_TO_END.iter().any(|word| starts_with_word(rest, word))
            {
                break;
            } else if rest.starts_with(AND) {
                cuts.push(at);
                at += 1;
            }
            at += 1;
        }
        Some(
            iter::once(first)
                .chain(cuts.iter().map(|cut| cut + AND.len()))
                .enumerate()
                .map(|(index, start)| (start, cuts.get(index).copied().unwrap_or(to)))
                .collect(),
        )
    }

    /// A bulleted conjunction: one conjunct per `/\` at the first bullet's column.
    fn bulleted(&self, first: usize, to: usize) -> Option<Vec<(usize, usize)>> {
        let bytes = self.bare.as_bytes();
        let column = first - self.line_start(first);
        let mut bullets = vec![first];
        for start in self.starts.iter().copied().filter(|&s| s > first && s < to) {
            let indent = bytes[start..to]
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
            let at = start + indent;
            if at >= to || bytes[at].is_ascii_whitespace() {
                continue;
            }
            if indent < column {
                return None;
            }
            if indent == column && bytes[at..].starts_with(AND) {
                bullets.push(at);
            }
        }
        Some(
            bullets
                .iter()
                .enumerate()
                .flat_map(|(index, &bullet)| {
                    let end = bullets.get(index + 1).copied().unwrap_or(to);
                    let start = bullet + AND.len();
                    self.conjuncts(start, end)
                        .unwrap_or_else(|| vec![(start, end)])
                })
                .collect(),
        )
    }

    fn cited(&self, notes: &[&str], from: usize, to: usize, above: bool) -> bool {
        if (from..=to).any(|line| notes.get(line).is_some_and(|n| CITATION.is_match(n))) {
            return true;
        }
        above
            && (0..from)
                .rev()
                .take_while(|&line| {
                    self.lines[line].trim().is_empty() && !notes[line].trim().is_empty()
                })
                .any(|line| CITATION.is_match(notes[line]))
    }

    fn uncited(&self, code: &str, notes: &str) -> Vec<Uncited> {
        let notes: Vec<&str> = notes.split('\n').collect();
        let names: HashSet<&str> = self.definitions.iter().map(|d| d.name.as_str()).collect();
        let mut found = Vec::new();
        for definition in &self.definitions {
            let own = &self.own[&definition.name];
            if !own.acts {
                continue;
            }
            let changes = self.through(own).writes;
            for (start, end) in self
                .conjuncts(definition.from, definition.to)
                .unwrap_or_default()
            {
                let part = &self.bare[start..end];
                let reads = self.through(&self.direct(part, &names));
                if reads.acts || reads.reads.iter().all(|read| changes.contains(read)) {
                    continue;
                }
                let Some(lead) = part.find(|c: char| !c.is_whitespace()) else {
                    continue;
                };
                let first = self.line_of(start + lead);
                let last = self.line_of(start + part.trim_end().len() - 1);
                if self.cited(&notes, first, last, first > definition.line) {
                    continue;
                }
                found.push(Uncited {
                    line: first + 1,
                    action: definition.name.clone(),
                    condition: collapse_spaces(&code[start..end]),
                });
            }
        }
        found
    }
}

/// Each identifier that is not part of an operator such as `\in`, a field after `.`, or a longer
/// word, with where it ends.
fn identifiers(body: &str) -> impl Iterator<Item = (&str, usize)> {
    let bytes = body.as_bytes();
    let mut at = 0;
    iter::from_fn(move || {
        while at < bytes.len() {
            let start = at;
            let byte = bytes[at];
            if byte.is_ascii_alphabetic() || byte == b'_' {
                let end = start + bytes[start..].iter().take_while(|&&b| is_word(b)).count();
                at = end;
                let follows = start
                    .checked_sub(1)
                    .map(|before| bytes[before])
                    .is_some_and(|before| before == b'\\' || before == b'.' || is_word(before));
                if !follows {
                    return Some((&body[start..end], end));
                }
            } else {
                at += 1;
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::scripts::statechart::testing::fixture;

    const PAY: &str = r#"---- MODULE Pay ----
CONSTANT MaxAge
VARIABLES screen, sent,
          paid, clock
Ready == sent > 0
Tick == clock' = clock + 1 /\ UNCHANGED <<screen, sent, paid>>
Pay ==
  /\ screen = "Cart \* not a comment"
  /\ MaxAge > 0
  /\ clock < MaxAge (* pay.ts:120 *)
  /\ Ready
  \* checkout.ts:88
  /\ sent < 3
  /\ screen' = IF clock > 0 /\ sent > 0 THEN "Paid" ELSE screen /\ paid' = TRUE
  /\ UNCHANGED <<sent, clock>>
\* fill.ts:10
Fill == \E o \in 1..3 : /\ paid = TRUE
                        /\ clock > 0 /\ sent' = o + 1
                        /\ sent = o
                        /\ paid => clock > 0 /\ screen = "Cart"
                        /\ UNCHANGED <<screen, paid, clock>>
Next == Tick \/ Pay \/ Fill
===="#;

    fn uncited(line: usize, action: &str, condition: &str) -> Uncited {
        Uncited {
            line,
            action: action.to_owned(),
            condition: condition.to_owned(),
        }
    }

    #[test]
    fn a_condition_on_a_variable_the_action_does_not_change_needs_a_file_and_line() {
        assert_eq!(
            uncited_conditions(PAY),
            [
                uncited(11, "Pay", "Ready"),
                uncited(17, "Fill", "paid = TRUE"),
                uncited(18, "Fill", "clock > 0"),
                uncited(20, "Fill", r#"paid => clock > 0 /\ screen = "Cart""#),
            ]
        );
    }

    #[test]
    fn the_save_code_model_waits_for_two_conditions() {
        let text = fs::read_to_string(fixture("SaveCode.tla")).expect("the fixture reads");

        assert_eq!(
            uncited_conditions(&text),
            [
                uncited(8, "Retry", r#"screen = "Saving""#),
                uncited(9, "TimeOut", "retries = 1"),
            ]
        );
    }

    #[test]
    fn variables_span_lines() {
        assert_eq!(tla_variables(PAY), ["screen", "sent", "paid", "clock"]);
    }
}
