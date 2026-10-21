//! The `watch-pr` command line, parsed as the commander program upstream declares it (same flags,
//! help, messages, and precedence), and the run that resolves the PRs and prints the verdicts.

use std::sync::LazyLock;

use regex::Regex;

use super::github::{discover_stack, resolve_context};
use super::policy::{
    RunDependencies, VerdictFactory, run_queued, run_simple, status_query_verdict,
};
use super::render::render;
use super::types::{
    GitHubReader, PollingOptions, PrContext, PrNumber, Verdict, WatchError, WatchMode, pr_number,
    string_to_number,
};
use crate::host::{Host, Output};

pub const HELP: &str = "Usage: watch-pr [options]

Watch one pull request, a connected stack, or an immutable queued stack.
JSON (NDJSON while polling) is the default; --pretty renders human text.

Options:
  --owner <owner>             GitHub repository owner
  --repo <repo>               GitHub repository name
  --pr <number>               pull request number
  --stack                     watch the connected open stack (default: false)
  --queued-stack              watch the captured stack until all PRs merge
                              (default: false)
  --stack-prs <n,...>         frozen bottom-to-top queue (queued mode only)
  --interval <seconds>        poll interval (default: 60)
  --sweep-interval <seconds>  whole-stack sweep interval (default: 300)
  --timeout <seconds>         deadline; 0 disables it (default: 0)
  --max-query-errors <count>  consecutive query-error budget (default: 5)
  --status-only               print one status table and exit 0 (default: false)
  --allow-draft               do not treat a draft as a merge gate (default:
                              false)
  --pretty                    render human text instead of JSON (default: false)
  -h, --help                  display help for command
";
pub const EX_USAGE: i32 = 64;
/// A run the caller cancelled mid-poll ends like one interrupted by SIGINT.
pub const EXIT_CANCELLED: i32 = 130;
pub const EXIT_CRASHED: i32 = 1;
pub const CANCELLED_MESSAGE: &str = "watch-pr: cancelled\n";
const HELP_FLAGS: [&str; 2] = ["-h", "--help"];
const HELP_LONG: &str = "--help";
const LITERAL: &str = "--";
const STACK_PRS_REQUIRES_QUEUED: &str = "error: --stack-prs requires --queued-stack";
const MUST_BE_POSITIVE: &str = "must be greater than zero";
const MUST_BE_NON_NEGATIVE: &str = "must be zero or greater";
const MUST_BE_POSITIVE_INTEGER: &str = "must be a positive integer";
const DUPLICATE_PR: &str = "contains a duplicate PR";
const DEFAULT_INTERVAL: f64 = 60.0;
const DEFAULT_SWEEP_INTERVAL: f64 = 300.0;
const DEFAULT_TIMEOUT: f64 = 0.0;
const DEFAULT_MAX_QUERY_ERRORS: u64 = 5;
const SUGGESTION_MAX_DISTANCE: usize = 3;
const SUGGESTION_MIN_SIMILARITY: f64 = 0.4;

static NEGATIVE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-\d*\.?\d+(e[+-]?\d+)?$").expect("literal"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Owner,
    Repo,
    Pr,
    Stack,
    QueuedStack,
    StackPrs,
    Interval,
    SweepInterval,
    Timeout,
    MaxQueryErrors,
    StatusOnly,
    AllowDraft,
    Pretty,
}

impl Flag {
    const ALL: [Self; 13] = [
        Self::Owner,
        Self::Repo,
        Self::Pr,
        Self::Stack,
        Self::QueuedStack,
        Self::StackPrs,
        Self::Interval,
        Self::SweepInterval,
        Self::Timeout,
        Self::MaxQueryErrors,
        Self::StatusOnly,
        Self::AllowDraft,
        Self::Pretty,
    ];

    fn long(self) -> &'static str {
        match self {
            Self::Owner => "--owner",
            Self::Repo => "--repo",
            Self::Pr => "--pr",
            Self::Stack => "--stack",
            Self::QueuedStack => "--queued-stack",
            Self::StackPrs => "--stack-prs",
            Self::Interval => "--interval",
            Self::SweepInterval => "--sweep-interval",
            Self::Timeout => "--timeout",
            Self::MaxQueryErrors => "--max-query-errors",
            Self::StatusOnly => "--status-only",
            Self::AllowDraft => "--allow-draft",
            Self::Pretty => "--pretty",
        }
    }

    fn placeholder(self) -> Option<&'static str> {
        match self {
            Self::Owner => Some("<owner>"),
            Self::Repo => Some("<repo>"),
            Self::Pr => Some("<number>"),
            Self::StackPrs => Some("<n,...>"),
            Self::Interval | Self::SweepInterval | Self::Timeout => Some("<seconds>"),
            Self::MaxQueryErrors => Some("<count>"),
            Self::Stack
            | Self::QueuedStack
            | Self::StatusOnly
            | Self::AllowDraft
            | Self::Pretty => None,
        }
    }

    fn flags(self) -> String {
        match self.placeholder() {
            Some(placeholder) => format!("{} {placeholder}", self.long()),
            None => self.long().to_owned(),
        }
    }

    fn find(arg: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|flag| flag.long() == arg)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CliOptions {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<PrNumber>,
    pub mode: WatchMode,
    pub stack_prs: Vec<PrNumber>,
    pub status_only: bool,
    pub pretty: bool,
    pub polling: PollingOptions,
}

/// Why parsing stopped before a run: help was asked for, or the command line is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliExit {
    Help,
    Usage(String),
}

#[derive(Default)]
struct RawOptions {
    owner: Option<String>,
    repo: Option<String>,
    pr: Option<PrNumber>,
    stack: bool,
    queued_stack: bool,
    stack_prs: Option<Vec<PrNumber>>,
    interval: Option<f64>,
    sweep_interval: Option<f64>,
    timeout: Option<f64>,
    max_query_errors: Option<u64>,
    status_only: bool,
    allow_draft: bool,
    pretty: bool,
}

fn positive_number(value: &str) -> Result<f64, &'static str> {
    let parsed = string_to_number(value);
    (parsed.is_finite() && parsed > 0.0)
        .then_some(parsed)
        .ok_or(MUST_BE_POSITIVE)
}

fn non_negative_number(value: &str) -> Result<f64, &'static str> {
    let parsed = string_to_number(value);
    (parsed.is_finite() && parsed >= 0.0)
        .then_some(parsed)
        .ok_or(MUST_BE_NON_NEGATIVE)
}

fn positive_integer(value: &str) -> Result<u64, &'static str> {
    pr_number(string_to_number(value)).ok_or(MUST_BE_POSITIVE_INTEGER)
}

fn parse_pr_number(value: &str) -> Result<PrNumber, &'static str> {
    positive_integer(value.strip_prefix('#').unwrap_or(value))
}

fn stack_pr_list(value: &str) -> Result<Vec<PrNumber>, &'static str> {
    let numbers = value
        .split(',')
        .map(|part| parse_pr_number(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() == numbers.len() {
        Ok(numbers)
    } else {
        Err(DUPLICATE_PR)
    }
}

impl RawOptions {
    fn set_value(&mut self, flag: Flag, value: &str) -> Result<(), CliExit> {
        let invalid = |reason: &str| {
            CliExit::Usage(format!(
                "error: option '{}' argument '{value}' is invalid. {reason}",
                flag.flags()
            ))
        };
        match flag {
            Flag::Owner => self.owner = Some(value.to_owned()),
            Flag::Repo => self.repo = Some(value.to_owned()),
            Flag::Pr => self.pr = Some(parse_pr_number(value).map_err(invalid)?),
            Flag::StackPrs => self.stack_prs = Some(stack_pr_list(value).map_err(invalid)?),
            Flag::Interval => self.interval = Some(positive_number(value).map_err(invalid)?),
            Flag::SweepInterval => {
                self.sweep_interval = Some(positive_number(value).map_err(invalid)?);
            }
            Flag::Timeout => self.timeout = Some(non_negative_number(value).map_err(invalid)?),
            Flag::MaxQueryErrors => {
                self.max_query_errors = Some(positive_integer(value).map_err(invalid)?);
            }
            Flag::Stack
            | Flag::QueuedStack
            | Flag::StatusOnly
            | Flag::AllowDraft
            | Flag::Pretty => {
                self.set_switch(flag);
            }
        }
        Ok(())
    }

    fn set_switch(&mut self, flag: Flag) {
        match flag {
            Flag::Stack => self.stack = true,
            Flag::QueuedStack => self.queued_stack = true,
            Flag::StatusOnly => self.status_only = true,
            Flag::AllowDraft => self.allow_draft = true,
            Flag::Pretty => self.pretty = true,
            Flag::Owner
            | Flag::Repo
            | Flag::Pr
            | Flag::StackPrs
            | Flag::Interval
            | Flag::SweepInterval
            | Flag::Timeout
            | Flag::MaxQueryErrors => {}
        }
    }
}

fn maybe_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// Commander's optimal string alignment distance, which gives up past three edits.
fn edit_distance(a: &[char], b: &[char]) -> usize {
    if a.len().abs_diff(b.len()) > SUGGESTION_MAX_DISTANCE {
        return a.len().max(b.len());
    }
    let mut distance = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in distance.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in distance[0].iter_mut().enumerate() {
        *cell = j;
    }
    for j in 1..=b.len() {
        for i in 1..=a.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (distance[i - 1][j] + 1)
                .min(distance[i][j - 1] + 1)
                .min(distance[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(distance[i - 2][j - 2] + 1);
            }
            distance[i][j] = best;
        }
    }
    distance[a.len()][b.len()]
}

/// Commander's "Did you mean" hint for an unknown `--` option.
fn suggestion(flag: &str) -> String {
    let Some(word) = flag.strip_prefix("--") else {
        return String::new();
    };
    let word: Vec<char> = word.chars().collect();
    let mut similar = Vec::new();
    let mut best_distance = SUGGESTION_MAX_DISTANCE;
    for candidate in Flag::ALL.map(Flag::long).into_iter().chain([HELP_LONG]) {
        let name: Vec<char> = candidate[2..].chars().collect();
        if name.len() <= 1 {
            continue;
        }
        let distance = edit_distance(&word, &name);
        let length = word.len().max(name.len());
        let similarity = (length - distance.min(length)) as f64 / length as f64;
        if similarity <= SUGGESTION_MIN_SIMILARITY {
            continue;
        }
        if distance < best_distance {
            best_distance = distance;
            similar = vec![candidate];
        } else if distance == best_distance {
            similar.push(candidate);
        }
    }
    similar.sort_unstable();
    match similar.as_slice() {
        [] => String::new(),
        [only] => format!("\n(Did you mean {only}?)"),
        many => format!("\n(Did you mean one of {}?)", many.join(", ")),
    }
}

/// Parses `argv` with commander's precedence: a bad option value fails at once, then help, then
/// conflicting modes, then the first unknown option, then stray operands.
pub fn parse_args(argv: &[String]) -> Result<CliOptions, CliExit> {
    let mut raw = RawOptions::default();
    let mut operands = Vec::new();
    let mut unknown = Vec::new();
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == LITERAL {
            if unknown.is_empty() {
                operands.extend(args.by_ref());
            } else {
                unknown.push(arg);
                unknown.extend(args.by_ref());
            }
            break;
        }
        if maybe_option(arg)
            && let Some(flag) = Flag::find(arg)
        {
            if flag.placeholder().is_none() {
                raw.set_switch(flag);
                continue;
            }
            let Some(value) = args.next() else {
                return Err(CliExit::Usage(format!(
                    "error: option '{}' argument missing",
                    flag.flags()
                )));
            };
            raw.set_value(flag, value)?;
            continue;
        }
        if let Some((name, value)) = arg.split_once('=')
            && name.len() > 2
            && name.starts_with("--")
            && let Some(flag) = Flag::find(name)
            && flag.placeholder().is_some()
        {
            raw.set_value(flag, value)?;
            continue;
        }
        let unknown_option = maybe_option(arg) && !NEGATIVE_NUMBER.is_match(arg);
        if unknown.is_empty() && !unknown_option {
            operands.push(arg);
        } else {
            unknown.push(arg);
        }
    }
    if unknown.iter().any(|arg| HELP_FLAGS.contains(&arg.as_str())) {
        return Err(CliExit::Help);
    }
    if raw.stack && raw.queued_stack {
        return Err(CliExit::Usage(format!(
            "error: option '{}' cannot be used with option '{}'",
            Flag::Stack.flags(),
            Flag::QueuedStack.flags()
        )));
    }
    if let Some(flag) = unknown.first() {
        return Err(CliExit::Usage(format!(
            "error: unknown option '{flag}'{}",
            suggestion(flag)
        )));
    }
    if !operands.is_empty() {
        return Err(CliExit::Usage(format!(
            "error: too many arguments. Expected 0 arguments but got {}.",
            operands.len()
        )));
    }
    if raw.stack_prs.is_some() && !raw.queued_stack {
        return Err(CliExit::Usage(STACK_PRS_REQUIRES_QUEUED.to_owned()));
    }
    Ok(CliOptions {
        mode: if raw.queued_stack {
            WatchMode::QueuedStack
        } else if raw.stack {
            WatchMode::Stack
        } else {
            WatchMode::Single
        },
        owner: raw.owner,
        repo: raw.repo,
        pr: raw.pr,
        stack_prs: raw.stack_prs.unwrap_or_default(),
        status_only: raw.status_only,
        pretty: raw.pretty,
        polling: PollingOptions {
            interval: raw.interval.unwrap_or(DEFAULT_INTERVAL),
            sweep_interval: raw.sweep_interval.unwrap_or(DEFAULT_SWEEP_INTERVAL),
            timeout: raw.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_query_errors: raw.max_query_errors.unwrap_or(DEFAULT_MAX_QUERY_ERRORS),
            allow_draft: raw.allow_draft,
        },
    })
}

/// Collects what the run prints, and shows each progress verdict as it is produced.
struct Printer<'a> {
    host: &'a dyn Host,
    pretty: bool,
    stdout: String,
}

impl Printer<'_> {
    fn print(&mut self, verdict: &Verdict) {
        self.stdout.push_str(&render(verdict, self.pretty));
    }

    fn emit(&mut self, verdict: &Verdict) {
        let rendered = render(verdict, self.pretty);
        for line in rendered.lines() {
            self.host.progress(line);
        }
        self.stdout.push_str(&rendered);
    }

    fn finish(mut self, result: Result<Verdict, WatchError>) -> Output {
        let (code, stderr) = match result {
            Ok(verdict) => {
                self.print(&verdict);
                (verdict.event.exit_code().unwrap_or_default(), String::new())
            }
            Err(WatchError::Cancelled(_)) => (EXIT_CANCELLED, CANCELLED_MESSAGE.to_owned()),
            Err(error) => (EXIT_CRASHED, format!("error: {error}\n")),
        };
        Output {
            code,
            stdout: self.stdout,
            stderr,
        }
    }
}

fn resolve_contexts(
    reader: &dyn GitHubReader,
    options: &CliOptions,
) -> Result<Vec<PrContext>, WatchError> {
    let seed = resolve_context(
        reader,
        options.owner.as_deref(),
        options.repo.as_deref(),
        options.pr.or(options.stack_prs.first().copied()),
    )?;
    if !options.stack_prs.is_empty() {
        return Ok(options
            .stack_prs
            .iter()
            .map(|number| seed.with_number(*number))
            .collect());
    }
    match options.mode {
        WatchMode::Single => Ok(vec![seed]),
        WatchMode::Stack | WatchMode::QueuedStack => discover_stack(reader, &seed),
    }
}

fn watch(
    host: &dyn Host,
    reader: &dyn GitHubReader,
    options: &CliOptions,
    printer: &mut Printer,
) -> Result<Verdict, WatchError> {
    let contexts = match resolve_contexts(reader, options) {
        Ok(contexts) => contexts,
        Err(WatchError::Query(failure)) => {
            let stamp = VerdictFactory::new(host, options.mode);
            return Ok(status_query_verdict(&stamp, 1, failure));
        }
        Err(error) => return Err(error),
    };
    let mut emit = |verdict: &Verdict| printer.emit(verdict);
    let mut dependencies = RunDependencies {
        reader,
        host,
        emit: &mut emit,
    };
    if options.mode == WatchMode::QueuedStack && !options.status_only {
        run_queued(&mut dependencies, contexts, &options.polling)
    } else {
        run_simple(
            &mut dependencies,
            &contexts,
            options.mode,
            options.status_only,
            &options.polling,
        )
    }
}

/// Upstream's `main`: usage errors exit 64 with only stderr, a run exits with its verdict's code,
/// a cancelled run exits [`EXIT_CANCELLED`] keeping what it printed, and a command that cannot
/// start exits [`EXIT_CRASHED`].
pub fn main(host: &dyn Host, reader: &dyn GitHubReader, argv: &[String]) -> Output {
    let options = match parse_args(argv) {
        Ok(options) => options,
        Err(CliExit::Help) => return Output::success(HELP),
        Err(CliExit::Usage(message)) => return Output::failure(EX_USAGE, format!("{message}\n")),
    };
    let mut printer = Printer {
        host,
        pretty: options.pretty,
        stdout: String::new(),
    };
    let result = watch(host, reader, &options, &mut printer);
    printer.finish(result)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::fakes::{
        CI, FakeReader, HEAD, args, context, default_facts, fake_host, passing_check,
    };
    use super::super::render::{render_json, render_pretty};
    use super::super::types::{
        ChecksFastPath, CommitRollup, Event, MergeStateStatus, PrSnapshot, PrState,
        PullRequestFacts, RollupState, StatusReason,
    };
    use super::*;

    const OBSERVED_AT: &str = "2026-07-26T00:00:00.000Z";
    const MERGED_TABLE_ROW: &str =
        "| [#1](https://github.com/owner/repo/pull/1) | \u{2014} | \u{2014} | ✅ merged |";
    const MERGED_STATUS_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"2026-07-26T00:00:00.000Z","mode":"single","kind":"STATUS","terminal":true,"exitCode":0,"reason":"status-only","rows":[{"kind":"merged","context":{"owner":"owner","repo":"repo","number":1},"facts":{"context":{"owner":"owner","repo":"repo","number":1},"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED","headRefOid":"head","headRefName":"feature","baseRefName":"main","state":"MERGED","mergedAt":"now","isDraft":false}}]}
"#;

    fn run_main(reader: &FakeReader, argv: &[&str]) -> Output {
        main(&fake_host(), reader, &args(argv))
    }

    #[test]
    fn uses_the_specified_defaults() {
        assert_eq!(
            parse_args(&[]),
            Ok(CliOptions {
                owner: None,
                repo: None,
                pr: None,
                mode: WatchMode::Single,
                stack_prs: Vec::new(),
                status_only: false,
                pretty: false,
                polling: PollingOptions {
                    interval: 60.0,
                    sweep_interval: 300.0,
                    timeout: 0.0,
                    max_query_errors: 5,
                    allow_draft: false,
                },
            })
        );
    }

    #[test]
    fn parses_a_frozen_queued_stack_bottom_to_top() {
        let parsed = parse_args(&args(&[
            "--queued-stack",
            "--stack-prs",
            "#10, 11,#12",
            "--interval",
            "2.5",
            "--sweep-interval",
            "30",
            "--timeout",
            "0",
            "--max-query-errors",
            "3",
            "--allow-draft",
            "--pretty",
        ]))
        .expect("options");
        assert_eq!(parsed.mode, WatchMode::QueuedStack);
        assert_eq!(parsed.stack_prs, [10, 11, 12]);
        assert_eq!(
            parsed.polling,
            PollingOptions {
                interval: 2.5,
                sweep_interval: 30.0,
                timeout: 0.0,
                max_query_errors: 3,
                allow_draft: true,
            }
        );
        assert!(parsed.pretty);
    }

    #[test_case(&["--interval", "0x10"], 16.0 ; "hex")]
    #[test_case(&["--interval=1e1"], 10.0 ; "inline_value_with_exponent")]
    #[test_case(&["--interval", " 7 "], 7.0 ; "padded")]
    fn reads_numbers_as_javascript_does(argv: &[&str], interval: f64) {
        assert_eq!(
            parse_args(&args(argv)).expect("options").polling.interval,
            interval
        );
    }

    #[test]
    fn strips_a_leading_hash_from_the_pr_number() {
        assert_eq!(
            parse_args(&args(&["--pr=#5"])).expect("options").pr,
            Some(5)
        );
    }

    #[test_case(&["--unknown"], "error: unknown option '--unknown'" ; "unknown_option")]
    #[test_case(&["--interval", "0"], "error: option '--interval <seconds>' argument '0' is invalid. must be greater than zero" ; "zero_interval")]
    #[test_case(&["--sweep-interval", "-1"], "error: option '--sweep-interval <seconds>' argument '-1' is invalid. must be greater than zero" ; "negative_sweep")]
    #[test_case(&["--timeout", "-1"], "error: option '--timeout <seconds>' argument '-1' is invalid. must be zero or greater" ; "negative_timeout")]
    #[test_case(&["--max-query-errors", "1.5"], "error: option '--max-query-errors <count>' argument '1.5' is invalid. must be a positive integer" ; "fractional_budget")]
    #[test_case(&["--stack", "--queued-stack"], "error: option '--stack' cannot be used with option '--queued-stack'" ; "conflicting_modes")]
    #[test_case(&["--stack-prs", "1,2"], "error: --stack-prs requires --queued-stack" ; "stack_prs_without_queue")]
    #[test_case(&["--queued-stack", "--stack-prs", "1,1"], "error: option '--stack-prs <n,...>' argument '1,1' is invalid. contains a duplicate PR" ; "duplicate_pr")]
    #[test_case(&["--queued-stack", "--stack-prs", ","], "error: option '--stack-prs <n,...>' argument ',' is invalid. must be a positive integer" ; "empty_stack_entry")]
    #[test_case(&["--pr", "#0"], "error: option '--pr <number>' argument '#0' is invalid. must be a positive integer" ; "zero_pr")]
    #[test_case(&["foo"], "error: too many arguments. Expected 0 arguments but got 1." ; "operand")]
    #[test_case(&["a", "b"], "error: too many arguments. Expected 0 arguments but got 2." ; "operands")]
    #[test_case(&["--owner"], "error: option '--owner <owner>' argument missing" ; "missing_value")]
    #[test_case(&["--stak"], "error: unknown option '--stak'\n(Did you mean --stack?)" ; "suggests_stack")]
    #[test_case(&["--pretty=1"], "error: unknown option '--pretty=1'\n(Did you mean --pretty?)" ; "switch_with_value")]
    #[test_case(&["--stackprs", "1"], "error: unknown option '--stackprs'\n(Did you mean --stack-prs?)" ; "suggests_stack_prs")]
    #[test_case(&["--interval", "--", "5"], "error: option '--interval <seconds>' argument '--' is invalid. must be greater than zero" ; "value_is_taken_verbatim")]
    #[test_case(&["--help", "--interval", "0"], "error: option '--interval <seconds>' argument '0' is invalid. must be greater than zero" ; "value_error_outranks_help")]
    #[test_case(&["--queued-stack", "--stack-prs", "3,4", "--stack"], "error: option '--stack' cannot be used with option '--queued-stack'" ; "conflict_after_values")]
    #[test_case(&["-hx"], "error: unknown option '-hx'" ; "combined_short_flags")]
    #[test_case(&["--version"], "error: unknown option '--version'" ; "no_version_flag")]
    fn rejects_every_invalid_mode_and_numeric_shape_as_usage(argv: &[&str], message: &str) {
        let reader = FakeReader::default();
        let output = run_main(&reader, argv);
        assert_eq!(output, Output::failure(EX_USAGE, format!("{message}\n")));
        assert!(reader.calls().is_empty());
    }

    #[test_case(&["--help"] ; "long")]
    #[test_case(&["-h"] ; "short")]
    #[test_case(&["--unknown", "-h"] ; "after_an_unknown_option")]
    #[test_case(&["--stack-prs", "1,2", "--help"] ; "before_the_queue_check")]
    #[test_case(&["--stack", "--queued-stack", "--help"] ; "before_the_conflict_check")]
    fn shows_help_without_touching_the_reader(argv: &[&str]) {
        let reader = FakeReader::default();
        assert_eq!(run_main(&reader, argv), Output::success(HELP));
        assert!(HELP.contains("JSON (NDJSON while polling)"));
        assert!(reader.calls().is_empty());
    }

    fn merged_status() -> Verdict {
        let facts = PullRequestFacts {
            state: PrState::Merged,
            merged_at: Some("now".to_owned()),
            ..default_facts()
        };
        Verdict {
            sequence: 1,
            observed_at: OBSERVED_AT.to_owned(),
            mode: WatchMode::Single,
            event: Event::Status {
                reason: StatusReason::StatusOnly,
                rows: vec![PrSnapshot::Merged {
                    context: context(1),
                    facts,
                }],
            },
        }
    }

    #[test]
    fn emits_compact_valid_json_by_default() {
        assert_eq!(render_json(&merged_status()), MERGED_STATUS_JSON);
    }

    #[test]
    fn renders_the_markdown_table_from_the_same_verdict_only() {
        let rendered = render_pretty(&merged_status());
        assert!(rendered.contains("| PR | CI | Review | Merge |"));
        assert!(rendered.contains(MERGED_TABLE_ROW));
    }

    #[test]
    fn bypasses_the_queue_machine_for_queued_stack_status_only() {
        let reader = FakeReader::default();
        let output = run_main(
            &reader,
            &[
                "--owner",
                "owner",
                "--repo",
                "repo",
                "--queued-stack",
                "--stack-prs",
                "1",
                "--status-only",
            ],
        );
        assert_eq!(output.code, 0);
        assert_eq!(output.stdout.lines().count(), 1);
        assert!(
            output
                .stdout
                .contains(r#""mode":"queued-stack","kind":"STATUS","terminal":true,"exitCode":0"#)
        );
        assert!(!output.stdout.contains(r#""kind":"QUEUE""#));
    }

    #[test]
    fn returns_exit_4_for_a_hidden_github_side_ci_refusal() {
        let reader = FakeReader {
            facts: PullRequestFacts {
                merge_state_status: MergeStateStatus::Blocked,
                ..default_facts()
            },
            fast_path: ChecksFastPath::Checks(vec![passing_check(CI)]),
            commit_rollups: vec![CommitRollup {
                oid: HEAD.to_owned(),
                state: Some(RollupState::Failure),
            }],
            ..FakeReader::default()
        };
        let output = run_main(
            &reader,
            &["--owner", "owner", "--repo", "repo", "--pr", "1"],
        );
        assert_eq!(output.code, 4);
        assert_eq!(output.stdout.lines().count(), 1);
        assert!(output.stdout.contains(
            r#""kind":"BLOCKER","terminal":true,"exitCode":4,"blocker":{"kind":"failing-checks""#
        ));
        assert!(output.stdout.contains(r#""kind":"ci-github-rejected""#));
    }
}
