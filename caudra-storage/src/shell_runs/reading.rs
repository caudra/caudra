//! What the shell ETA model reads from a call: a few numbers about the
//! command's shape, hashed labels of the programs it runs, and its pipelines as
//! steps. Heredoc bodies and arguments other than flags never reach a label, and
//! the command itself is never kept.

use std::mem::take;
use std::sync::LazyLock;

use regex::Regex;

use super::lex;
use crate::sessions::SessionError;

pub const NUMBERS: usize = 13;
pub const READING_VERSION: u8 = 1;
const MAX_CHARS: usize = 8192;
const MAX_STEP_KEY_CHARS: usize = 200;
const MAX_SUB_CHARS: usize = 25;
const MAX_FLAG_CHARS: usize = 30;
const DEFAULT_LOOP_ITEMS: usize = 5;
const MISSING: f32 = -1.0;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

const WRAPPERS: &[&str] = &[
    "env", "nohup", "time", "exec", "command", "nice", "sudo", "stdbuf", "setsid", "xargs",
];
const FILTERS: &[&str] = &[
    "grep", "head", "tail", "sed", "awk", "cut", "sort", "uniq", "wc", "tr", "tee", "jq", "less",
    "cat", "column", "rg", "fold", "nl",
];
const CONTROL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "in", "case",
    "esac", "{", "}", "(", ")", "!", "[[", "]]", "function",
];
const QUICK: &[&str] = &[
    "echo", "printf", "true", "false", "test", "[", "cd", "export", "set", "unset", "ls", "pwd",
    "mkdir", "rm", "cp", "mv", "chmod", "touch", "ln", "basename", "dirname", "readlink",
    "realpath", "which", "type", "date", "source", ".", "local", "return", "exit", "read", "shift",
    "wait", "ulimit", "trap", "umask", "alias",
];
const LOOPS: &[&str] = &["for", "while", "until"];
const REDIRECTS: &[&str] = &["<", ">", ">>", "<<", ">&", "<&", "&>", "<<<"];
const PIPES: &[&str] = &["|", "|&"];
const NEWLINE: &str = "\n";
const SUBSHELL_OPEN: &str = "(";
const SUBSHELL_CLOSE: &str = ")";
const BACKGROUND: &str = "&";
const PIPE: &str = "|";
const FOR: &str = "for";
const IN: &str = "in";
const DO: &str = "do";
const DONE: &str = "done";
const SEMICOLON: &str = ";";
const TIMEOUT: &str = "timeout";
const SLEEP: &str = "sleep";
const HEAD: &str = "head";
const HEREDOC_WORD: &str = "__HEREDOC__";
const TIMEOUT_LOOKAHEAD: usize = 3;
const SUB_LOOKAHEAD: usize = 3;
const MINUTE_SECS: f64 = 60.0;
const HOUR_SECS: f64 = 3600.0;
const DAY_SECS: f64 = 86400.0;
const DECIMAL_RADIX: u32 = 10;

const HEAD_PREFIX: &str = "H:";
const MAIN_PREFIX: &str = "M:";
const FLAG_PREFIX: &str = "F:";
const FIRST_PREFIX: &str = "FIRST:";
const LAST_PREFIX: &str = "LAST:";
const PROJECT_PREFIX: &str = "P:";
const SPACE: &str = " ";

const READING_FIELD: &str = "shell_runs.reading";
const TRUNCATED: &str = "reading ends early";
const TRAILING: &str = "bytes follow the reading";
const STEPS_ABSENT: u8 = 0;
const STEPS_PRESENT: u8 = 1;

static SECONDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+(?:\.\d+)?)([smhd]?)$").expect("literal"));
static DECIMAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d$").expect("literal"));
static REDIRECT_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d*[<>]+&?\d*$").expect("literal"));
static ASSIGNMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=.*$").expect("literal"));

/// A shell call as the model sees it before it runs.
pub struct Call<'a> {
    pub command: &'a str,
    pub workdir: Option<&'a str>,
    /// The session's working directory, which names the project when the call
    /// gives no `workdir`.
    pub cwd: Option<&'a str>,
    pub timeout_secs: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub numbers: [f32; NUMBERS],
    /// Sorted, distinct FNV-1a hashes of the call's labels.
    pub tokens: Vec<u64>,
    /// Each step is one pipeline's parts; `None` when loops or backgrounding
    /// make a sum of parts meaningless.
    pub steps: Option<Vec<Vec<Part>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub key: u64,
    /// A builtin or text filter, quick wherever it runs.
    pub quick: bool,
}

struct Segment<'a> {
    words: Vec<&'a str>,
    op: &'a str,
    depth: usize,
}

struct Head<'w, 'a> {
    prog: &'a str,
    sub: Option<&'a str>,
    rest: &'w [&'a str],
}

impl Head<'_, '_> {
    fn hash(&self, prefix: &str) -> u64 {
        match self.sub {
            Some(sub) => fnv1a(&[prefix, self.prog, SPACE, sub]),
            None => fnv1a(&[prefix, self.prog]),
        }
    }

    fn is_main(&self) -> bool {
        !FILTERS.contains(&self.prog) && !QUICK.contains(&self.prog)
    }
}

pub fn read(call: &Call) -> Reading {
    let body = lex::strip_heredocs(truncate_chars(call.command, MAX_CHARS));
    let words = lex::tokens(&body);
    let (segments, loops) = segments(&words);
    let heads: Vec<Option<Head>> = segments
        .iter()
        .map(|segment| head(&segment.words))
        .collect();
    let mains: Vec<&Head> = heads
        .iter()
        .flatten()
        .filter(|head| head.is_main())
        .collect();
    let heredocs = body.matches(HEREDOC_WORD).count();
    Reading {
        numbers: numbers(call, &segments, &loops, &heads, mains.len(), heredocs),
        tokens: tokens(call, &heads, &mains),
        steps: steps(&segments, &loops, &heads),
    }
}

fn numbers(
    call: &Call,
    segments: &[Segment],
    loops: &[usize],
    heads: &[Option<Head>],
    mains: usize,
    heredocs: usize,
) -> [f32; NUMBERS] {
    let loop_items = loops.iter().copied().max().unwrap_or(0);
    let mut sleep_total = 0.0;
    let mut timeout_cap: Option<f64> = None;
    let mut background = 0;
    let mut piped_to_head = false;
    for (index, (segment, found)) in segments.iter().zip(heads).enumerate() {
        let Some(found) = found else { continue };
        let repeats = if segment.depth > 0 {
            loop_items.max(1) as f64
        } else {
            1.0
        };
        if found.prog == SLEEP
            && let Some(secs) = found.rest.get(1).and_then(|arg| seconds(arg))
        {
            sleep_total += secs * repeats;
        }
        if let Some(secs) = timeout_seconds(&segment.words) {
            timeout_cap = Some(timeout_cap.unwrap_or(0.0).max(secs));
        }
        if segment.op == BACKGROUND {
            background += 1;
        }
        if found.prog == HEAD && index > 0 && segments[index - 1].op == PIPE {
            piped_to_head = true;
        }
    }
    let length = call.command.chars().count();
    let lines = call.command.matches(NEWLINE).count() + 1;
    [
        segments.len() as f32,
        loops.len() as f32,
        loop_items as f32,
        heredocs as f32,
        log1(Some(length as f64)),
        lines as f32,
        log1(Some(sleep_total)),
        log1(timeout_cap),
        background as f32,
        f32::from(u8::from(piped_to_head)),
        mains as f32,
        f32::from(u8::from(call.workdir.is_some_and(|dir| !dir.is_empty()))),
        log1(call.timeout_secs),
    ]
}

fn tokens(call: &Call, heads: &[Option<Head>], mains: &[&Head]) -> Vec<u64> {
    let mut hashes: Vec<u64> = heads
        .iter()
        .flatten()
        .map(|head| head.hash(HEAD_PREFIX))
        .chain(mains.iter().map(|main| main.hash(MAIN_PREFIX)))
        .collect();
    hashes.extend(mains.iter().flat_map(|main| {
        main.rest[1..]
            .iter()
            .filter(|word| word.starts_with('-') && word.chars().count() < MAX_FLAG_CHARS)
            .map(|word| {
                let flag = word.split('=').next().unwrap_or_default();
                fnv1a(&[FLAG_PREFIX, main.prog, SPACE, flag])
            })
    }));
    if let (Some(first), Some(last)) = (mains.first(), mains.last()) {
        hashes.push(first.hash(FIRST_PREFIX));
        hashes.push(last.hash(LAST_PREFIX));
    }
    hashes.push(fnv1a(&[PROJECT_PREFIX, project(call)]));
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

/// The last component of the directory the call runs in.
fn project<'a>(call: &Call<'a>) -> &'a str {
    [call.workdir, call.cwd]
        .into_iter()
        .flatten()
        .find(|dir| !dir.is_empty())
        .unwrap_or_default()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn steps(segments: &[Segment], loops: &[usize], heads: &[Option<Head>]) -> Option<Vec<Vec<Part>>> {
    if !loops.is_empty() || segments.iter().any(|segment| segment.op == BACKGROUND) {
        return None;
    }
    let mut steps = Vec::new();
    let mut step = Vec::new();
    for (segment, found) in segments.iter().zip(heads) {
        if let Some(found) = found {
            step.push(Part {
                key: step_key(found),
                quick: !found.is_main(),
            });
        }
        if !PIPES.contains(&segment.op) && !step.is_empty() {
            steps.push(take(&mut step));
        }
    }
    if !step.is_empty() {
        steps.push(step);
    }
    (!steps.is_empty()).then_some(steps)
}

fn step_key(head: &Head) -> u64 {
    let mut key = head.prog.to_owned();
    for word in &head.rest[1..] {
        key.push_str(SPACE);
        key.push_str(word);
    }
    fnv1a(&[truncate_chars(&key, MAX_STEP_KEY_CHARS)])
}

fn fnv1a(parts: &[&str]) -> u64 {
    parts
        .iter()
        .flat_map(|part| part.bytes())
        .fold(FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
        })
}

fn log1(value: Option<f64>) -> f32 {
    value.map_or(MISSING, |value| (1.0 + value).log2() as f32)
}

fn truncate_chars(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    text.char_indices()
        .nth(max)
        .map_or(text, |(at, _)| &text[..at])
}

fn seconds(text: &str) -> Option<f64> {
    let captures = SECONDS.captures(text)?;
    let unit = match &captures[2] {
        "m" => MINUTE_SECS,
        "h" => HOUR_SECS,
        "d" => DAY_SECS,
        _ => 1.0,
    };
    let number = &captures[1];
    let value = if number.is_ascii() {
        number.parse::<f64>()
    } else {
        ascii_digits(number).parse::<f64>()
    };
    value.ok().map(|value| value * unit)
}

/// Python's `float()` accepts any Unicode decimal digit; each `Nd` block is a
/// contiguous 0..=9 run.
fn ascii_digits(number: &str) -> String {
    number
        .chars()
        .map(|c| {
            if c.is_ascii() {
                return c;
            }
            let code = c as u32;
            let run = (1..=code)
                .take_while(|back| char::from_u32(code - back).is_some_and(is_decimal))
                .count();
            char::from_digit(run as u32 % DECIMAL_RADIX, DECIMAL_RADIX).unwrap_or('0')
        })
        .collect()
}

fn is_decimal(c: char) -> bool {
    let mut buffer = [0; 4];
    DECIMAL.is_match(c.encode_utf8(&mut buffer))
}

fn is_separator(token: &str) -> bool {
    token == NEWLINE
        || (!token.is_empty() && token.bytes().all(|b| matches!(b, b';' | b'&' | b'|')))
}

fn is_redirect_word(token: &str) -> bool {
    token.bytes().any(|b| b == b'<' || b == b'>') && REDIRECT_WORD.is_match(token)
}

/// Simple commands with the operator that ended each and its loop depth, and
/// how many items each loop iterates over.
fn segments(tokens: &[String]) -> (Vec<Segment<'_>>, Vec<usize>) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut words = Vec::new();
    let mut depth = 0usize;
    let mut loops = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_separator(token) {
            if !words.is_empty() {
                segments.push(Segment {
                    words: take(&mut words),
                    op: token,
                    depth,
                });
            } else if token == BACKGROUND
                && let Some(last) = segments.last_mut()
            {
                last.op = token;
            }
        } else if token == SUBSHELL_OPEN || token == SUBSHELL_CLOSE {
            if !words.is_empty() {
                segments.push(Segment {
                    words: take(&mut words),
                    op: token,
                    depth,
                });
            }
        } else if REDIRECTS.contains(&token) {
            index += 1;
        } else if is_redirect_word(token) {
        } else if words.is_empty() && LOOPS.contains(&token) {
            if token == FOR {
                let mut next = index + 2;
                let mut items = 0;
                if tokens.get(next).is_some_and(|word| word == IN) {
                    next += 1;
                    while next < tokens.len()
                        && ![SEMICOLON, DO, NEWLINE].contains(&tokens[next].as_str())
                    {
                        items += 1;
                        next += 1;
                    }
                }
                loops.push(if items == 0 {
                    DEFAULT_LOOP_ITEMS
                } else {
                    items
                });
                index = next;
            } else {
                loops.push(DEFAULT_LOOP_ITEMS);
            }
            depth += 1;
        } else if words.is_empty() && token == DONE {
            depth = depth.saturating_sub(1);
        } else if !(words.is_empty() && CONTROL.contains(&token)) {
            words.push(token);
        }
        index += 1;
    }
    if !words.is_empty() {
        segments.push(Segment {
            words,
            op: "",
            depth,
        });
    }
    (segments, loops)
}

fn is_subcommand(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| lex::py_word(c) || matches!(c, ':' | '.' | '-'))
        && word.chars().count() < MAX_SUB_CHARS
}

/// The program a simple command runs, past wrappers, `timeout` and variable
/// assignments, with its subcommand when one follows.
fn head<'w, 'a>(words: &'w [&'a str]) -> Option<Head<'w, 'a>> {
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if WRAPPERS.contains(&word) {
            index += 1;
        } else if word == TIMEOUT {
            index += 1;
            while index < words.len() && words[index].starts_with('-') {
                index += 1;
            }
            index += 1;
        } else if word.contains('=') && ASSIGNMENT.is_match(word) {
            index += 1;
        } else {
            break;
        }
    }
    let rest = words.get(index..).filter(|rest| !rest.is_empty())?;
    let prog = rest[0].rsplit('/').next().unwrap_or_default();
    let sub = rest[1..rest.len().min(SUB_LOOKAHEAD)]
        .iter()
        .copied()
        .find(|word| is_subcommand(word));
    Some(Head { prog, sub, rest })
}

fn timeout_seconds(words: &[&str]) -> Option<f64> {
    let at = words
        .iter()
        .take(TIMEOUT_LOOKAHEAD)
        .position(|word| *word == TIMEOUT)?;
    let mut value = at + 1;
    while value < words.len() && words[value].starts_with('-') {
        value += 1;
    }
    seconds(words.get(value)?)
}

pub(crate) fn encode(reading: &Reading) -> Vec<u8> {
    let mut bytes = vec![READING_VERSION];
    for number in reading.numbers {
        bytes.extend(number.to_le_bytes());
    }
    let tokens = &reading.tokens[..usize::from(count(reading.tokens.len()))];
    bytes.extend(count(tokens.len()).to_le_bytes());
    for token in tokens {
        bytes.extend(token.to_le_bytes());
    }
    let Some(steps) = &reading.steps else {
        bytes.push(STEPS_ABSENT);
        return bytes;
    };
    bytes.push(STEPS_PRESENT);
    let steps = &steps[..usize::from(count(steps.len()))];
    bytes.extend(count(steps.len()).to_le_bytes());
    for step in steps {
        let parts = &step[..usize::from(count(step.len()))];
        bytes.extend(count(parts.len()).to_le_bytes());
        for part in parts {
            bytes.extend(part.key.to_le_bytes());
            bytes.push(u8::from(part.quick));
        }
    }
    bytes
}

/// A length as stored: more than fits is cut, never wrapped.
fn count(len: usize) -> u16 {
    u16::try_from(len).unwrap_or(u16::MAX)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Reading, SessionError> {
    let mut reader = Reader(bytes);
    let [version] = reader.take::<1>()?;
    if version != READING_VERSION {
        return Err(corrupt(format!(
            "version {version}, expected {READING_VERSION}"
        )));
    }
    let mut numbers = [0.0; NUMBERS];
    for number in &mut numbers {
        *number = f32::from_le_bytes(reader.take()?);
    }
    let tokens = (0..reader.count()?)
        .map(|_| reader.take().map(u64::from_le_bytes))
        .collect::<Result<_, _>>()?;
    let steps = match reader.take::<1>()? {
        [STEPS_ABSENT] => None,
        [STEPS_PRESENT] => Some(
            (0..reader.count()?)
                .map(|_| {
                    (0..reader.count()?)
                        .map(|_| {
                            Ok(Part {
                                key: u64::from_le_bytes(reader.take()?),
                                quick: reader.flag()?,
                            })
                        })
                        .collect::<Result<Vec<_>, SessionError>>()
                })
                .collect::<Result<_, _>>()?,
        ),
        [other] => return Err(corrupt(format!("steps marker {other}"))),
    };
    if !reader.0.is_empty() {
        return Err(corrupt(TRAILING.into()));
    }
    Ok(Reading {
        numbers,
        tokens,
        steps,
    })
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], SessionError> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or_else(|| corrupt(TRUNCATED.into()))?;
        self.0 = rest;
        Ok(*head)
    }

    fn count(&mut self) -> Result<u16, SessionError> {
        self.take().map(u16::from_le_bytes)
    }

    fn flag(&mut self) -> Result<bool, SessionError> {
        match self.take::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            [other] => Err(corrupt(format!("part flag {other}"))),
        }
    }
}

fn corrupt(reason: String) -> SessionError {
    SessionError::CorruptDatabaseValue {
        field: READING_FIELD,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        Call, HEAD_PREFIX, NUMBERS, Part, READING_VERSION, Reading, decode, encode, fnv1a, read,
    };

    const PIPELINE: &str = "cd crate && cargo test -p core >out | tail -5; echo done";
    const SHAPED: &str = "for x in 1 2 3; do sleep 2; done\ntimeout 1m make | head &";
    const PROJECT_DIR: &str = "/work/project/";
    const TIMEOUT_SECS: f64 = 7.0;

    fn call(command: &str) -> Call<'_> {
        Call {
            command,
            workdir: None,
            cwd: Some(PROJECT_DIR),
            timeout_secs: None,
        }
    }

    fn keys(steps: &[&[(&str, bool)]]) -> Vec<Vec<Part>> {
        steps
            .iter()
            .map(|step| {
                step.iter()
                    .map(|(key, quick)| Part {
                        key: fnv1a(&[key]),
                        quick: *quick,
                    })
                    .collect()
            })
            .collect()
    }

    fn hashed(labels: &[&str]) -> Vec<u64> {
        let mut hashes: Vec<u64> = labels.iter().map(|label| fnv1a(&[label])).collect();
        hashes.sort_unstable();
        hashes
    }

    #[test]
    fn pipelines_become_steps_of_parts() {
        let steps = read(&call(PIPELINE)).steps;
        let expected = keys(&[
            &[("cd crate", true)],
            &[("cargo test -p core", false), ("tail -5", true)],
            &[("echo done", true)],
        ]);
        assert_eq!(steps, Some(expected));
    }

    #[test_case("for f in a b; do cargo test; done"; "loop")]
    #[test_case("cargo build & sleep 1"; "background")]
    #[test_case(""; "empty")]
    fn steps_are_absent_where_parts_do_not_add_up(command: &str) {
        assert_eq!(read(&call(command)).steps, None);
    }

    #[test]
    fn tokens_label_programs_flags_and_project() {
        let tokens = read(&call(
            "RUST_LOG=debug timeout 60 cargo test --release=x | head -3",
        ))
        .tokens;
        let expected = hashed(&[
            "H:cargo test",
            "H:head",
            "M:cargo test",
            "F:cargo --release",
            "FIRST:cargo test",
            "LAST:cargo test",
            "P:project",
        ]);
        assert_eq!(tokens, expected);
    }

    #[test]
    fn workdir_names_the_project_over_cwd() {
        let tokens = read(&Call {
            workdir: Some(PROJECT_DIR),
            cwd: Some("/elsewhere"),
            ..call("ls")
        })
        .tokens;
        assert_eq!(tokens, hashed(&["H:ls", "P:project"]));
    }

    #[test]
    fn numbers_describe_the_shape_of_the_call() {
        let reading = read(&Call {
            workdir: Some(PROJECT_DIR),
            timeout_secs: Some(TIMEOUT_SECS),
            ..call(SHAPED)
        });
        let length = SHAPED.chars().count() as f32;
        let expected: [f32; NUMBERS] = [
            3.0,
            1.0,
            3.0,
            0.0,
            (1.0 + length).log2(),
            2.0,
            7.0f32.log2(),
            61.0f32.log2(),
            1.0,
            1.0,
            2.0,
            1.0,
            3.0,
        ];
        assert_eq!(reading.numbers, expected);
    }

    #[test]
    fn heredoc_bodies_never_become_labels() {
        let reading = read(&call("python3 - <<'PY'\nimport secret\nPY"));
        assert_eq!(reading.numbers[3], 1.0);
        assert!(!reading.tokens.contains(&fnv1a(&[HEAD_PREFIX, "import"])));
    }

    #[test_case(PIPELINE; "steps")]
    #[test_case("for f in a; do b; done"; "no_steps")]
    fn readings_round_trip(command: &str) {
        let reading = read(&call(command));
        assert_eq!(decode(&encode(&reading)).unwrap(), reading);
    }

    #[test]
    fn another_version_is_unreadable() {
        let mut bytes = encode(&read(&call(PIPELINE)));
        bytes[0] = READING_VERSION + 1;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_cut_reading_is_unreadable() {
        let bytes = encode(&read(&call(PIPELINE)));
        assert!((0..bytes.len()).all(|len| decode(&bytes[..len]).is_err()));
    }

    #[test]
    fn trailing_bytes_are_unreadable() {
        let mut bytes = encode(&Reading {
            numbers: [0.0; NUMBERS],
            tokens: Vec::new(),
            steps: None,
        });
        bytes.push(0);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn fnv1a_matches_the_reference_vector() {
        assert_eq!(fnv1a(&["a"]), 0xaf63_dc4c_8601_ec8c);
    }
}
