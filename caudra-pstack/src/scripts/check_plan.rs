//! Port of poteto-mode's `check-plan.mjs`: checks a multi-phase plan's structure and prose, prints
//! one box count line per PR section, and lists every problem as `file:line: message`.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::sync::LazyLock;

use regex::Regex;

use crate::host::{Host, Output};

const USAGE: &str = "Usage: node check-plan.mjs <plan.md>\n";
const RULE: &str = "Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.";
const FRONTMATTER: &str = "---";
const DEPENDS_ON: &str = "Depends on.";
const VERIFY_LIVE: &str = "Verify, live.";
const VERIFY_PERF: &str = "Verify, perf.";
const REVIEW_GATE: &str = "Review gate.";
const SUB_BLOCKS: [&str; 9] = [
    DEPENDS_ON,
    "Files.",
    "Build.",
    "You see.",
    "Verify, unit.",
    VERIFY_LIVE,
    VERIFY_PERF,
    REVIEW_GATE,
    "Merge.",
];
const BOXED_BLOCKS: [&str; 5] = ["Files.", "Build.", "You see.", "Verify, unit.", "Merge."];
const RULED_BLOCKS: [&str; 3] = ["Verify, unit.", VERIFY_LIVE, VERIFY_PERF];
const HOW_TO_READ: &str = "How to read this";
const PROGRAM_CHECKLIST: &str = "Program checklist";
const CLOSE_THE_PROGRAM: &str = "Close the program";
const PROGRAM_H3: [&str; 5] = [
    "Arm the program",
    "Spawn owners",
    "PR mechanics",
    "Verdict and merge",
    "Boot recipe",
];
const PROGRAM_MARKERS: [&str; 3] = ["git show origin/main:", "/loop 1h", "status message"];
const HOW_TO_READ_MARKERS: [&str; 5] = [
    "One box is one unit of work",
    "names the evidence",
    "Check a box only when its evidence exists",
    "playbooks/",
    RULE,
];
const PERF_ITEMS: [&str; 4] = ["Metric.", "Probe.", "Baseline.", "Rule."];
const GATE_WORDS: [&str; 3] = ["screenshot", "video", "operator"];
const GATE_NONE: &str = "None.";
const LANE_NUMBERS: &str = "1,2,3,4,5,6,7,8,9,10";
const PASS_PREDICATE: &str = "Pass when";
const APPENDIX: &str = "Appendix";
const PROTOTYPE_EVIDENCE: &str = "Prototype evidence";
const INTRO_LIMIT: usize = 10;
const LONG_DASHES: [char; 2] = ['\u{2013}', '\u{2014}'];
const CURLY_QUOTES: [char; 4] = ['\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}'];
/// What JavaScript's `.` refuses besides `\n`, which a split line never holds.
const JS_LINE_TERMINATORS: [char; 3] = ['\r', '\u{2028}', '\u{2029}'];

static OPENING_FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^ {0,3}(`{3,}|~{3,})([^\r\n\x{2028}\x{2029}]*)$").expect("literal")
});
static CLOSING_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(`+|~+)[ \t]*$").expect("literal"));
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]*`").expect("literal"));
static IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]*\)").expect("literal"));
static LINK_TARGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\]\([^)]*\)").expect("literal"));
static SUB_BLOCK_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*\*([^*]+)\*\*([^\r\n\x{2028}\x{2029}]*)$").expect("literal"));
static LANES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Ten lanes on `[^`<>]+` at the PR head").expect("literal"));
static LANE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Lane ([0-9]+)\. ").expect("literal"));
static SCREENSHOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Save `[^`]+`").expect("literal"));

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    let Some(file) = argv.first().filter(|file| !file.is_empty()) else {
        return Output::failure(2, USAGE);
    };
    let content = match fs::read(host.resolve(file)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => return Output::failure(1, format!("Error: {error}, open '{file}'\n")),
    };
    let checked = check(file, &content);
    Output {
        code: i32::from(!checked.problems.is_empty()),
        stdout: checked.stdout,
        stderr: checked
            .problems
            .iter()
            .map(|problem| format!("{problem}\n"))
            .collect(),
    }
}

struct Checked {
    stdout: String,
    problems: Vec<String>,
}

struct Problems<'a> {
    file: &'a str,
    found: Vec<String>,
}

impl Problems<'_> {
    fn fail(&mut self, line: usize, message: impl Display) {
        self.found.push(format!("{}:{line}: {message}", self.file));
    }
}

struct Line<'a> {
    n: usize,
    text: &'a str,
    code: bool,
}

struct Section<'a> {
    title: &'a str,
    n: usize,
    body: &'a [Line<'a>],
}

struct Fence {
    marker: char,
    length: usize,
}

impl Fence {
    fn opening(text: &str) -> Option<Self> {
        let captures = OPENING_FENCE.captures(text)?;
        let run = &captures[1];
        let marker = run.chars().next()?;
        (marker != '`' || !captures[2].contains('`')).then_some(Self {
            marker,
            length: run.len(),
        })
    }

    fn closed_by(&self, text: &str) -> bool {
        CLOSING_FENCE.captures(text).is_some_and(|captures| {
            let run = &captures[1];
            run.starts_with(self.marker) && run.len() >= self.length
        })
    }
}

struct SubBlock<'a> {
    name: &'a str,
    n: usize,
    rest: &'a str,
    lines: Vec<&'a Line<'a>>,
}

struct CheckBox<'a> {
    n: usize,
    text: &'a str,
}

fn check(file: &str, content: &str) -> Checked {
    let raw = split_lines(content);
    let mut problems = Problems {
        file,
        found: Vec::new(),
    };
    let start = if raw.first() == Some(&FRONTMATTER) {
        raw.iter()
            .skip(1)
            .position(|line| *line == FRONTMATTER)
            .map_or(0, |at| at + 2)
    } else {
        0
    };

    let mut lines = Vec::new();
    let mut fence: Option<Fence> = None;
    for (index, text) in raw.iter().enumerate().skip(start) {
        let n = index + 1;
        let code = if fence.as_ref().is_some_and(|open| open.closed_by(text)) {
            fence = None;
            false
        } else if fence.is_some() {
            true
        } else {
            fence = Fence::opening(text);
            fence.is_some()
        };
        lines.push(Line { n, text, code });
        if !code {
            check_prose(&mut problems, n, text);
        }
    }

    let headings: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| Some((index, h2(line)?)))
        .collect();
    let sections: Vec<Section> = headings
        .iter()
        .enumerate()
        .map(|(k, &(at, title))| {
            let end = headings.get(k + 1).map_or(lines.len(), |&(next, _)| next);
            Section {
                title,
                n: lines[at].n,
                body: &lines[at + 1..end],
            }
        })
        .collect();
    let find = |title: &str| sections.iter().position(|section| section.title == title);

    let h1 = lines
        .iter()
        .position(|line| !line.code && line.text.starts_with("# "));
    if h1.is_none() {
        problems.fail(1, "no H1 title");
    }
    let how_to_read = find(HOW_TO_READ).map(|at| &sections[at]);
    if how_to_read.is_none() {
        problems.fail(1, format!("no \"## {HOW_TO_READ}\" section"));
    }
    if let (Some(h1), Some(how_to_read)) = (h1, how_to_read) {
        let intro = lines[h1 + 1..]
            .iter()
            .filter(|line| line.n < how_to_read.n && !js_trim(line.text).is_empty())
            .count();
        if intro >= INTRO_LIMIT {
            problems.fail(
                lines[h1].n,
                format!("intro is {intro} lines, under ten required"),
            );
        }
        for marker in HOW_TO_READ_MARKERS {
            if !body_has(how_to_read, marker) {
                problems.fail(how_to_read.n, format!("{HOW_TO_READ} lacks \"{marker}\""));
            }
        }
    }

    let program_index = find(PROGRAM_CHECKLIST);
    match program_index.map(|at| &sections[at]) {
        None => problems.fail(1, format!("no \"## {PROGRAM_CHECKLIST}\" section")),
        Some(program) => check_program(&mut problems, program),
    }

    let close_index = find(CLOSE_THE_PROGRAM);
    if close_index.is_none() {
        problems.fail(1, format!("no \"## {CLOSE_THE_PROGRAM}\" section"));
    }
    let pr_sections = match (program_index, close_index) {
        (Some(program), Some(close)) => sections.get(program + 1..close).unwrap_or_default(),
        _ => &[],
    };
    if pr_sections.is_empty() {
        problems.fail(
            1,
            format!("no PR sections between {PROGRAM_CHECKLIST} and {CLOSE_THE_PROGRAM}"),
        );
    }

    let mut stdout: String = pr_sections
        .iter()
        .map(|pr| format!("{}\n", check_pr(&mut problems, pr)))
        .collect();

    if let Some(close) = close_index {
        let tail = &sections[close + 1..];
        for section in tail {
            if !section.title.starts_with(APPENDIX) {
                problems.fail(
                    section.n,
                    format!(
                        "\"## {}\" after {CLOSE_THE_PROGRAM} is not an appendix",
                        section.title
                    ),
                );
            }
        }
        if !tail
            .iter()
            .any(|section| section.title.contains(PROTOTYPE_EVIDENCE))
        {
            problems.fail(
                sections[close].n,
                format!("no \"## {APPENDIX} ... {PROTOTYPE_EVIDENCE}\" section"),
            );
        }
    }

    stdout.push_str(&format!(
        "{} PR sections, {} problems\n",
        pr_sections.len(),
        problems.found.len()
    ));
    Checked {
        stdout,
        problems: problems.found,
    }
}

/// Splits as JavaScript's `split(/\r?\n/)` does, so a final `\r` without `\n` stays.
fn split_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    let last = lines.len() - 1;
    for line in &mut lines[..last] {
        *line = line.strip_suffix('\r').unwrap_or(line);
    }
    lines
}

fn check_prose(problems: &mut Problems, n: usize, text: &str) {
    let prose = INLINE_CODE.replace_all(text, "`");
    let prose = IMAGE.replace_all(&prose, "");
    let prose = LINK_TARGET.replace_all(&prose, "]");
    if prose.contains(LONG_DASHES) {
        problems.fail(n, "long dash");
    }
    if prose.contains(CURLY_QUOTES) {
        problems.fail(n, "curly quote");
    }
    let mid_sentence_colon = prose.match_indices(": ").any(|(at, separator)| {
        prose[at + separator.len()..]
            .chars()
            .next()
            .is_some_and(|next| !is_js_space(next))
    });
    if mid_sentence_colon {
        problems.fail(n, "mid-sentence colon");
    }
}

fn check_program(problems: &mut Problems, program: &Section) {
    let h3s: Vec<&str> = program
        .body
        .iter()
        .filter(|line| !line.code)
        .filter_map(|line| line.text.strip_prefix("### "))
        .map(js_trim)
        .collect();
    let mut cursor = 0;
    for name in PROGRAM_H3 {
        match h3s[cursor..]
            .iter()
            .position(|title| title.starts_with(name))
        {
            Some(at) => cursor += at + 1,
            None => problems.fail(
                program.n,
                format!("{PROGRAM_CHECKLIST} lacks \"### {name}\" in order"),
            ),
        }
    }
    for marker in PROGRAM_MARKERS {
        if !body_has(program, marker) {
            problems.fail(program.n, format!("{PROGRAM_CHECKLIST} lacks \"{marker}\""));
        }
    }
}

/// Checks one PR section and returns its report line.
fn check_pr(problems: &mut Problems, pr: &Section) -> String {
    let title = pr.title;
    let mut heads: Vec<SubBlock> = Vec::new();
    for line in pr.body.iter().filter(|line| !line.code) {
        let head = SUB_BLOCK_HEAD
            .captures(line.text)
            .filter(|captures| SUB_BLOCKS.contains(&&captures[1]));
        match (head, heads.last_mut()) {
            (Some(captures), _) => heads.push(SubBlock {
                name: captures.get(1).map_or("", |name| name.as_str()),
                n: line.n,
                rest: js_trim(captures.get(2).map_or("", |rest| rest.as_str())),
                lines: Vec::new(),
            }),
            (None, Some(last)) => last.lines.push(line),
            (None, None) => {}
        }
    }
    let names: Vec<&str> = heads.iter().map(|head| head.name).collect();
    if names != SUB_BLOCKS {
        problems.fail(
            pr.n,
            format!(
                "{title}: sub-blocks are [{}], expected [{}]",
                names.join(", "),
                SUB_BLOCKS.join(", ")
            ),
        );
    }
    let block = |name: &str| heads.iter().find(|head| head.name == name);
    let counts: HashMap<&str, usize> = heads
        .iter()
        .map(|head| (head.name, boxes(head.lines.iter().copied()).len()))
        .collect();

    if let Some(depends) = block(DEPENDS_ON)
        && depends.rest.is_empty()
    {
        problems.fail(depends.n, format!("{title}: Depends on names nothing"));
    }
    for name in BOXED_BLOCKS {
        if let Some(head) = block(name)
            && boxes(head.lines.iter().copied()).is_empty()
        {
            problems.fail(head.n, format!("{title}: {name} has no box"));
        }
    }
    for name in RULED_BLOCKS {
        if let Some(head) = block(name)
            && !head.rest.starts_with(RULE)
        {
            problems.fail(
                head.n,
                format!("{title}: {name} does not open with the rule"),
            );
        }
    }
    if let Some(live) = block(VERIFY_LIVE) {
        check_live(problems, title, live);
    }
    if let Some(perf) = block(VERIFY_PERF) {
        let items: Vec<&str> = boxes(perf.lines.iter().copied())
            .iter()
            .map(|item| item.text.split(' ').next().unwrap_or_default())
            .collect();
        if items != PERF_ITEMS {
            problems.fail(
                perf.n,
                format!(
                    "{title}: perf boxes are [{}], expected [{}]",
                    items.join(", "),
                    PERF_ITEMS.join(", ")
                ),
            );
        }
    }
    if let Some(gate) = block(REVIEW_GATE) {
        check_gate(problems, title, gate);
    }

    let total = boxes(pr.body).len();
    let cells: Vec<String> = SUB_BLOCKS
        .iter()
        .filter(|name| **name != DEPENDS_ON)
        .map(|name| {
            format!(
                "{}={}",
                report_key(name),
                counts.get(name).copied().unwrap_or_default()
            )
        })
        .collect();
    format!("{title}  boxes={total}  {}", cells.join(" "))
}

fn check_live(problems: &mut Problems, title: &str, live: &SubBlock) {
    if !LANES.is_match(live.rest) {
        problems.fail(
            live.n,
            format!(
                "{title}: Verify, live lacks \"Ten lanes on `<swarm workers model>` at the PR head\" with the model filled in"
            ),
        );
    }
    let lanes: Vec<(CheckBox, Option<&str>)> = boxes(live.lines.iter().copied())
        .into_iter()
        .map(|lane| {
            let number = LANE
                .captures(lane.text)
                .and_then(|captures| captures.get(1))
                .map(|number| number.as_str());
            (lane, number)
        })
        .collect();
    let mut numbers: Vec<f64> = lanes
        .iter()
        .filter_map(|(_, number)| number.and_then(|number| number.parse::<f64>().ok()))
        .collect();
    numbers.sort_by(f64::total_cmp);
    let numbers = numbers
        .iter()
        .map(f64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if numbers != LANE_NUMBERS {
        problems.fail(
            live.n,
            format!("{title}: lanes are [{numbers}], expected 1 to 10"),
        );
    }
    for (lane, number) in &lanes {
        match number {
            None => problems.fail(lane.n, format!("{title}: live box is not a lane")),
            Some(number) if !SCREENSHOT.is_match(lane.text) => {
                problems.fail(
                    lane.n,
                    format!("{title}: lane {number} names no screenshot"),
                );
            }
            Some(number) if !lane.text.contains(PASS_PREDICATE) => {
                problems.fail(
                    lane.n,
                    format!("{title}: lane {number} has no pass predicate"),
                );
            }
            Some(_) => {}
        }
    }
}

fn check_gate(problems: &mut Problems, title: &str, gate: &SubBlock) {
    let gate_boxes = boxes(gate.lines.iter().copied());
    if gate.rest.starts_with(GATE_NONE) {
        if !gate_boxes.is_empty() {
            problems.fail(
                gate.n,
                format!("{title}: Review gate says None but has boxes"),
            );
        }
        return;
    }
    if gate_boxes.is_empty() {
        problems.fail(gate.n, format!("{title}: Review gate has no box"));
    }
    for word in GATE_WORDS {
        if !gate.lines.iter().any(|line| line.text.contains(word)) {
            problems.fail(gate.n, format!("{title}: Review gate lacks \"{word}\""));
        }
    }
}

fn h2<'a>(line: &Line<'a>) -> Option<&'a str> {
    let title = line.text.strip_prefix("## ").filter(|_| !line.code)?;
    Some(js_trim(title))
}

/// A marker never spans lines, so searching each line matches upstream's search of the joined body.
fn body_has(section: &Section, marker: &str) -> bool {
    section.body.iter().any(|line| line.text.contains(marker))
}

/// The `- [ ] text` and `- [x] text` lines outside code, as upstream's
/// `/^\s*- \[[ x]\] (.*)$/` finds them.
fn boxes<'a>(lines: impl IntoIterator<Item = &'a Line<'a>>) -> Vec<CheckBox<'a>> {
    lines
        .into_iter()
        .filter(|line| !line.code)
        .filter_map(|line| {
            let rest = line.text.trim_start_matches(is_js_space);
            let text = rest
                .strip_prefix("- [ ] ")
                .or_else(|| rest.strip_prefix("- [x] "))?;
            (!text.contains(JS_LINE_TERMINATORS)).then_some(CheckBox { n: line.n, text })
        })
        .collect()
}

fn report_key(name: &str) -> String {
    name.split([' ', ',', '.'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// JavaScript's `\s`, which `trim` also strips; it differs from Unicode `White_Space` at U+0085
/// and U+FEFF.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{RULE, USAGE, fs, run};
    use crate::host::Output;
    use crate::host::fake::FakeHost;

    const PLAN: &str = "plan.md";
    const NOT_AN_APPENDIX: &str = "\"## Example only\" after Close the program is not an appendix";
    const MID_SENTENCE_COLON: &str = "mid-sentence colon";
    const VALID_REPORT: &str = "PR 1  boxes=19  files=1 build=1 you-see=1 verify-unit=1 verify-live=10 verify-perf=4 review-gate=0 merge=1\n1 PR sections, 0 problems\n";

    fn valid_plan() -> String {
        let lanes: String = (1..=10)
            .map(|lane| {
                format!("- [ ] Lane {lane}. Save `lane-{lane}.png`. Pass when evidence exists.\n")
            })
            .collect();
        format!(
            "# Demo

## How to read this
One box is one unit of work and names the evidence. Check a box only when its evidence exists. See playbooks/.
{RULE}

## Program checklist
### Arm the program
### Spawn owners
### PR mechanics
### Verdict and merge
### Boot recipe
`git show origin/main:` /loop 1h status message

## PR 1
**Depends on.** None.
**Files.**
- [ ] Do the thing.
**Build.**
- [ ] Do the thing.
**You see.**
- [ ] Do the thing.
**Verify, unit.** {RULE}
- [ ] Check the thing.
**Verify, live.** {RULE} Ten lanes on `example-model` at the PR head
{lanes}**Verify, perf.** {RULE}
- [ ] Metric. Recorded.
- [ ] Probe. Recorded.
- [ ] Baseline. Recorded.
- [ ] Rule. Recorded.
**Review gate.** None.
**Merge.**
- [ ] Merge when approved.

## Close the program
Close.

## Appendix Prototype evidence
Recorded.
"
        )
    }

    fn check(content: &str) -> Output {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join(PLAN), content).expect("plan written");
        run(&FakeHost::new(dir.path()), &[PLAN.to_owned()])
    }

    fn run_plan(example: &str, expected_exit: i32) -> Output {
        let output = check(&format!("{}{example}", valid_plan()));
        assert_eq!(output.code, expected_exit, "{}", output.stderr);
        output
    }

    #[test]
    fn valid_plan_reports_its_boxes() {
        let output = run_plan("", 0);
        assert_eq!(
            (output.stdout.as_str(), output.stderr.as_str()),
            (VALID_REPORT, "")
        );
    }

    #[test_case(&[]; "no argument")]
    #[test_case(&[""]; "empty argument")]
    fn missing_plan_prints_usage(argv: &[&str]) {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        let output = run(&FakeHost::new("/"), &argv);
        assert_eq!((output.code, output.stderr.as_str()), (2, USAGE));
    }

    #[test_case("```markdown\n## Example only\n```\n"; "ordinary")]
    #[test_case(" ```markdown\n## Example only\n ```\n"; "one space")]
    #[test_case("  ```markdown\n## Example only\n  ```\n"; "two spaces")]
    #[test_case("   ```markdown\n## Example only\n   ```\n"; "three spaces")]
    #[test_case("```markdown\n## Example only\n````\n"; "longer closer")]
    #[test_case("````markdown\n```markdown\n## Example only\n```\n````\n"; "nested backticks")]
    #[test_case("~~~markdown `info`\n## Example only\n ~~~\n"; "tilde info string")]
    #[test_case("````markdown\n~~~\n## Example only\n````\n"; "mismatched closer")]
    #[test_case("````markdown\n```\n## Example only\n````\n"; "shorter closer")]
    #[test_case("````markdown\n``` info\n## Example only\n````\n"; "suffixed closer")]
    fn fenced_headings_are_ignored(example: &str) {
        run_plan(example, 0);
    }

    #[test_case("```markdown\n## Example only\n```  \n## Example only\n"; "spaces after closer")]
    #[test_case("```markdown\n## Example only\n```\t\n## Example only\n"; "tab after closer")]
    #[test_case("```markdown `invalid`\n## Example only\n```\n"; "backtick in backtick info string")]
    fn headings_outside_a_fence_are_checked(example: &str) {
        let output = run_plan(example, 1);
        assert!(output.stderr.contains(NOT_AN_APPENDIX), "{}", output.stderr);
    }

    #[test]
    fn prose_outside_a_fence_is_checked() {
        let output = run_plan(
            "```markdown\nThis prose has a mid-sentence colon: inside code\n```\nThis prose has a mid-sentence colon: bad.\n",
            1,
        );
        assert_eq!(output.stderr, format!("{PLAN}:53: {MID_SENTENCE_COLON}\n"));
    }

    #[test_case("Wait \u{2014} no.", "long dash"; "em dash")]
    #[test_case("It\u{2019}s here.", "curly quote"; "curly apostrophe")]
    #[test_case("Note: this.", MID_SENTENCE_COLON; "colon before a word")]
    #[test_case("Note: \u{feff}this.", ""; "colon before a byte order mark")]
    #[test_case("Note: \u{85}this.", MID_SENTENCE_COLON; "colon before next line")]
    fn prose_problems_name_their_line(prose: &str, message: &str) {
        let output = check(&format!("{}{prose}\n", valid_plan()));
        let expected = if message.is_empty() {
            String::new()
        } else {
            format!("{PLAN}:50: {message}\n")
        };
        assert_eq!(output.stderr, expected);
    }

    #[test_case("Ends with a colon:"; "colon at end")]
    #[test_case("Spaced colon:  two"; "colon before spaces")]
    #[test_case("`a: b` and [link](http://x: y) and ![img](a: b)"; "colon in code and links")]
    fn prose_without_problems_passes(prose: &str) {
        run_plan(&format!("{prose}\n"), 0);
    }

    #[test]
    fn frontmatter_is_skipped_and_lines_keep_their_numbers() {
        let output = check(&format!("---\ntitle: x\n---\n{}Bad: one\n", valid_plan()));
        assert_eq!(output.stderr, format!("{PLAN}:53: {MID_SENTENCE_COLON}\n"));
    }

    #[test]
    fn empty_plan_lists_every_missing_section() {
        let output = check("");
        assert_eq!(
            (output.code, output.stdout.as_str(), output.stderr.as_str()),
            (
                1,
                "0 PR sections, 5 problems\n",
                "plan.md:1: no H1 title\nplan.md:1: no \"## How to read this\" section\nplan.md:1: no \"## Program checklist\" section\nplan.md:1: no \"## Close the program\" section\nplan.md:1: no PR sections between Program checklist and Close the program\n"
            )
        );
    }

    #[test]
    fn broken_pr_lists_its_problems() {
        let plan = valid_plan()
            .replace("**Depends on.** None.", "**Depends on.**")
            .replace(
                "- [ ] Lane 3. Save `lane-3.png`. Pass when evidence exists.",
                "- [ ] Lane 3. Save it.",
            )
            .replace(
                "- [ ] Lane 4. Save `lane-4.png`. Pass when evidence exists.",
                "- [ ] Lane 4. Save `x`.",
            )
            .replace("- [ ] Lane 5.", "- [ ] Lane five.")
            .replace("- [ ] Probe. Recorded.\n", "")
            .replace(
                "**Review gate.** None.",
                "**Review gate.** Needs a human.\nscreenshot",
            );
        let output = check(&plan);
        assert_eq!(
            (output.code, output.stdout.as_str(), output.stderr.as_str()),
            (
                1,
                "PR 1  boxes=18  files=1 build=1 you-see=1 verify-unit=1 verify-live=10 verify-perf=3 review-gate=0 merge=1\n1 PR sections, 9 problems\n",
                "plan.md:16: PR 1: Depends on names nothing\nplan.md:25: PR 1: lanes are [1,2,3,4,6,7,8,9,10], expected 1 to 10\nplan.md:28: PR 1: lane 3 names no screenshot\nplan.md:29: PR 1: lane 4 has no pass predicate\nplan.md:30: PR 1: live box is not a lane\nplan.md:36: PR 1: perf boxes are [Metric., Baseline., Rule.], expected [Metric., Probe., Baseline., Rule.]\nplan.md:40: PR 1: Review gate has no box\nplan.md:40: PR 1: Review gate lacks \"video\"\nplan.md:40: PR 1: Review gate lacks \"operator\"\n"
            )
        );
    }

    #[test]
    fn missing_file_fails() {
        let dir = TempDir::new().expect("tempdir");
        let output = run(&FakeHost::new(dir.path()), &[PLAN.to_owned()]);
        assert_eq!(output.code, 1);
    }
}
