use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use caudra_storage::{log, paths};
use color_eyre::eyre::{Result, bail, eyre};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;

const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LINES: usize = 50_000;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_TIMESTAMP_BYTES: usize = 64;
const P90_PERCENT: usize = 90;
const PERCENT: usize = 100;
const UNKNOWN: &str = "other_or_unknown";
const EXPLICIT: &str = "explicit";
const RULE_SETTLED: &str = "rule_settled";
const POLICY_DENIED: &str = "policy_denied";
const ABANDONED: &str = "abandoned";
const DECISIONS: &[&str] = &[EXPLICIT, RULE_SETTLED, POLICY_DENIED, ABANDONED];
const REASONS: &[&str] = &[
    "force_prompt",
    "protected",
    "requires_prompt",
    "ask_rule",
    "uncovered",
];
const LIFETIMES: &[&str] = &["once", "conversation", "project", "global", "not_recorded"];
const TOOLS: &[&str] = &[
    "file_read",
    "file_glob",
    "file_grep",
    "file_write",
    "file_edit",
    "file_apply_patch",
    "index",
    "file_index",
    "shell",
    "python_execution",
    "execution_environment",
    "webfetch",
    "websearch",
    "code_map",
    "code_context",
    "code_refs",
    "code_impact",
    "code_expand",
    "batch",
];
const NOTES: &[&str] = &[
    "Counts are log events, not deduplicated requests or a prompt rate; no invocation denominator is available.",
    "Only the selected file's bounded tail is sampled; rotated logs and history are not scanned.",
    "The line cap processes complete records forward within the byte-tail; remaining complete bytes are reported as unprocessed.",
    "Since filters both prompts and decisions independently; boundary-crossing pairs can be incomplete.",
    "Pairing uses manager_id and request_id, or request_id alone when manager_id is absent. IDs are not globally unique; duplicate/colliding keys within the sample are excluded from waits.",
    "Waits are derived from timestamps of unambiguous pairs. They include unattended time, may overlap, and are not active human time.",
    "Unknown values are grouped into fixed categories. Commands, resources, identifiers and arbitrary strings are never emitted.",
    "The opened file length bounds the read; concurrent append, rotation or truncation can make this an incomplete sample.",
];

type Counts = BTreeMap<&'static str, u64>;

#[derive(Default, Serialize)]
struct Sample {
    since_inclusive: Option<String>,
    effective_max_bytes: u64,
    max_lines: usize,
    max_line_bytes: usize,
    file_bytes_at_open: u64,
    read_offset: u64,
    bytes_read: u64,
    boundary_probe_bytes: u64,
    leading_partial_bytes_skipped: usize,
    trailing_partial_bytes_skipped: usize,
    lines_considered: usize,
    complete_bytes_not_processed: usize,
    oversized_lines_skipped: u64,
    invalid_json_lines: u64,
    non_permission_lines: u64,
    invalid_timestamp_events: u64,
    events_before_since: u64,
    earliest_event_unix_ms: Option<i64>,
    latest_event_unix_ms: Option<i64>,
    file_shrank_during_read: bool,
}

#[derive(Default, Serialize)]
struct Pairing {
    events_without_manager_id: u64,
    events_with_invalid_pair_identifiers: u64,
    duplicate_or_colliding_keys: u64,
    ambiguous_prompt_events: u64,
    ambiguous_decision_events: u64,
    prompts_missing_decision: u64,
    decisions_missing_prompt: u64,
    reversed_timestamp_pairs: u64,
}

#[derive(Default, Serialize)]
struct WaitSummary {
    paired_events: usize,
    min_ms: Option<u64>,
    median_ms: Option<f64>,
    p90_ms: Option<u64>,
    max_ms: Option<u64>,
}

#[derive(Serialize)]
struct Audit {
    read_only: bool,
    sample: Sample,
    prompt_events: u64,
    decision_events: u64,
    decisions: Counts,
    prompt_tools: Counts,
    prompt_reasons: Counts,
    explicit_answer_lifetimes: Counts,
    pairing: Pairing,
    paired_waits_ms: BTreeMap<&'static str, WaitSummary>,
    notes: &'static [&'static str],
}

#[derive(Default)]
struct Pair {
    prompts: u64,
    decisions: u64,
    prompt: Option<Timestamp>,
    decision: Option<(Timestamp, &'static str)>,
}

pub(super) fn run(
    log_path: Option<PathBuf>,
    since: Option<String>,
    max_bytes: Option<u64>,
) -> Result<()> {
    let since = since.as_deref().map(parse_since).transpose()?;
    let path = match log_path {
        Some(path) => path,
        None => log::file_path(
            &paths::logs_dir_path()
                .map_err(|_| eyre!("cannot determine existing log directory"))?,
            0,
        ),
    };
    let audit = read_sample(&path, since, max_bytes.unwrap_or(DEFAULT_MAX_BYTES))?;
    println!("{}", serde_json::to_string_pretty(&audit)?);
    Ok(())
}

fn parse_since(value: &str) -> Result<Timestamp> {
    if value.len() > MAX_TIMESTAMP_BYTES {
        bail!("--since must be an RFC3339 timestamp");
    }
    value
        .parse()
        .map_err(|_| eyre!("--since must be an RFC3339 timestamp"))
}

fn read_sample(path: &Path, since: Option<Timestamp>, max_bytes: u64) -> Result<Audit> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .map_err(|error| eyre!("cannot open audit log: {}", error.kind()))?;
    let metadata = file
        .metadata()
        .map_err(|error| eyre!("cannot inspect audit log: {}", error.kind()))?;
    if !metadata.is_file() {
        bail!("audit log must be a regular file");
    }
    let budget = max_bytes.clamp(1, MAX_BYTES);
    let offset = metadata.len().saturating_sub(budget);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| eyre!("cannot seek audit log: {}", error.kind()))?;
    let mut bytes = Vec::new();
    file.take(metadata.len() - offset)
        .read_to_end(&mut bytes)
        .map_err(|error| eyre!("cannot read audit log: {}", error.kind()))?;
    let mut audit = Audit {
        read_only: true,
        sample: Sample {
            since_inclusive: since.map(|value| value.to_string()),
            effective_max_bytes: budget,
            max_lines: MAX_LINES,
            max_line_bytes: MAX_LINE_BYTES,
            file_bytes_at_open: metadata.len(),
            read_offset: offset,
            bytes_read: bytes.len() as u64,
            file_shrank_during_read: (bytes.len() as u64) < metadata.len() - offset,
            ..Sample::default()
        },
        prompt_events: 0,
        decision_events: 0,
        decisions: counts(DECISIONS),
        prompt_tools: counts(TOOLS),
        prompt_reasons: counts(REASONS),
        explicit_answer_lifetimes: counts(LIFETIMES),
        pairing: Pairing::default(),
        paired_waits_ms: BTreeMap::new(),
        notes: NOTES,
    };
    let mut content = bytes.as_slice();
    if offset > 0
        && let Some((&probe, remaining)) = content.split_first()
    {
        audit.sample.boundary_probe_bytes = 1;
        content = remaining;
        if probe != b'\n' {
            let skipped = content
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(content.len(), |index| index + 1);
            audit.sample.leading_partial_bytes_skipped = skipped;
            content = &content[skipped..];
        }
    }
    let complete = content
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    audit.sample.trailing_partial_bytes_skipped = content.len() - complete;
    let mut pairs = HashMap::new();
    let mut processed = 0;
    for line in content[..complete]
        .split_inclusive(|byte| *byte == b'\n')
        .take(MAX_LINES)
    {
        audit.sample.lines_considered += 1;
        processed += line.len();
        if line.len() > MAX_LINE_BYTES {
            audit.sample.oversized_lines_skipped += 1;
            continue;
        }
        include_event(line, since, &mut audit, &mut pairs);
    }
    audit.sample.complete_bytes_not_processed = complete - processed;
    finish_pairs(&mut audit, pairs);
    Ok(audit)
}

fn include_event(
    line: &[u8],
    since: Option<Timestamp>,
    audit: &mut Audit,
    pairs: &mut HashMap<(Option<String>, String), Pair>,
) {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        audit.sample.invalid_json_lines += 1;
        return;
    };
    let fields = &record["fields"];
    let prompt = match fields["event"].as_str() {
        Some("permission_prompt") => true,
        Some("permission_decision") => false,
        _ => {
            audit.sample.non_permission_lines += 1;
            return;
        }
    };
    let Some(timestamp) = record["timestamp"]
        .as_str()
        .filter(|value| value.len() <= MAX_TIMESTAMP_BYTES)
        .and_then(|value| value.parse::<Timestamp>().ok())
    else {
        audit.sample.invalid_timestamp_events += 1;
        return;
    };
    if since.is_some_and(|since| timestamp < since) {
        audit.sample.events_before_since += 1;
        return;
    }
    let millis = timestamp.as_millisecond();
    audit.sample.earliest_event_unix_ms = Some(
        audit
            .sample
            .earliest_event_unix_ms
            .map_or(millis, |old| old.min(millis)),
    );
    audit.sample.latest_event_unix_ms = Some(
        audit
            .sample
            .latest_event_unix_ms
            .map_or(millis, |old| old.max(millis)),
    );
    let kind = decision_kind(fields);
    if prompt {
        audit.prompt_events += 1;
        *audit
            .prompt_tools
            .entry(category(fields["tool"].as_str(), TOOLS))
            .or_default() += 1;
        *audit
            .prompt_reasons
            .entry(category(fields["forcing_reason"].as_str(), REASONS))
            .or_default() += 1;
    } else {
        audit.decision_events += 1;
        *audit.decisions.entry(kind).or_default() += 1;
        if kind == EXPLICIT {
            let lifetime = fields["lifetime"]
                .as_str()
                .filter(|value| !value.is_empty())
                .unwrap_or("not_recorded");
            *audit
                .explicit_answer_lifetimes
                .entry(category(Some(lifetime), LIFETIMES))
                .or_default() += 1;
        }
    }
    let manager = match fields.get("manager_id") {
        None => {
            audit.pairing.events_without_manager_id += 1;
            None
        }
        Some(value) => match identifier(value) {
            Some(value) => Some(value),
            None => {
                audit.pairing.events_with_invalid_pair_identifiers += 1;
                return;
            }
        },
    };
    let Some(request) = identifier(&fields["request_id"]) else {
        audit.pairing.events_with_invalid_pair_identifiers += 1;
        return;
    };
    let pair = pairs.entry((manager, request)).or_default();
    if prompt {
        pair.prompts += 1;
        pair.prompt.get_or_insert(timestamp);
    } else {
        pair.decisions += 1;
        pair.decision.get_or_insert((timestamp, kind));
    }
}

fn decision_kind(fields: &Value) -> &'static str {
    match fields["answer"].as_str() {
        Some("matched_rule") => RULE_SETTLED,
        Some("policy_denied") => POLICY_DENIED,
        Some("abandoned") => ABANDONED,
        Some(
            "allow"
            | "allow_session"
            | "allow_always_local"
            | "allow_always_global"
            | "allow_option"
            | "allow_composed"
            | "deny"
            | "deny_guidance"
            | "deny_always_local"
            | "deny_always_global",
        ) if fields.get("source").is_none()
            || matches!(
                fields["source"].as_str(),
                Some("user_once" | "user_session" | "user_always")
            ) =>
        {
            EXPLICIT
        }
        _ => UNKNOWN,
    }
}

fn identifier(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES => {
            Some(value.clone())
        }
        Value::Number(value) => value.as_u64().map(|value| value.to_string()),
        _ => None,
    }
}

fn category(value: Option<&str>, known: &'static [&'static str]) -> &'static str {
    known
        .iter()
        .copied()
        .find(|candidate| Some(*candidate) == value)
        .unwrap_or(UNKNOWN)
}

fn counts(known: &'static [&'static str]) -> Counts {
    known
        .iter()
        .copied()
        .chain([UNKNOWN])
        .map(|key| (key, 0))
        .collect()
}

fn finish_pairs(audit: &mut Audit, pairs: HashMap<(Option<String>, String), Pair>) {
    let mut waits: BTreeMap<_, Vec<u64>> = DECISIONS
        .iter()
        .copied()
        .chain([UNKNOWN])
        .map(|key| (key, Vec::new()))
        .collect();
    for pair in pairs.into_values() {
        if pair.prompts > 1 || pair.decisions > 1 {
            audit.pairing.duplicate_or_colliding_keys += 1;
            audit.pairing.ambiguous_prompt_events += pair.prompts;
            audit.pairing.ambiguous_decision_events += pair.decisions;
            continue;
        }
        match (pair.prompt, pair.decision) {
            (Some(prompt), Some((decision, kind))) if decision >= prompt => {
                waits
                    .entry(kind)
                    .or_default()
                    .push((decision.as_millisecond() - prompt.as_millisecond()) as u64);
            }
            (Some(_), Some(_)) => audit.pairing.reversed_timestamp_pairs += 1,
            (Some(_), None) => audit.pairing.prompts_missing_decision += 1,
            (None, Some(_)) => audit.pairing.decisions_missing_prompt += 1,
            (None, None) => {}
        }
    }
    audit.paired_waits_ms = waits
        .into_iter()
        .map(|(kind, mut values)| {
            values.sort_unstable();
            let count = values.len();
            let median = if count == 0 {
                None
            } else {
                Some((values[(count - 1) / 2] as f64 + values[count / 2] as f64) / 2.0)
            };
            (
                kind,
                WaitSummary {
                    paired_events: count,
                    min_ms: values.first().copied(),
                    median_ms: median,
                    p90_ms: values
                        .get((count * P90_PERCENT).div_ceil(PERCENT).saturating_sub(1))
                        .copied(),
                    max_ms: values.last().copied(),
                },
            )
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::fs;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::{env, process::Command};
    use test_case::test_case;

    use super::{
        ABANDONED, Audit, DEFAULT_MAX_BYTES, EXPLICIT, MAX_BYTES, MAX_LINE_BYTES, MAX_LINES,
        POLICY_DENIED, RULE_SETTLED, UNKNOWN, parse_since, read_sample,
    };

    const START: &str = "2026-01-01T00:00:00Z";
    const END: &str = "2026-01-01T00:00:02Z";
    const SECRET: &str = "never-output-this-private-value";
    #[cfg(unix)]
    const PATH_TEST: &str = "CAUDRA_PERMISSION_AUDIT_PATH_TEST";

    fn event(timestamp: &str, request: &str, manager: Option<u64>, answer: Option<&str>) -> String {
        let mut fields = json!({
            "event": if answer.is_some() { "permission_decision" } else { "permission_prompt" },
            "request_id": request, "tool": "shell", "forcing_reason": "uncovered",
        });
        if let Some(manager) = manager {
            fields["manager_id"] = json!(manager);
        }
        if let Some(answer) = answer {
            fields["answer"] = json!(answer);
            fields["source"] = json!(match answer {
                "matched_rule" | "policy_denied" => "rule",
                "abandoned" => "user_abort",
                _ => "user_once",
            });
            fields["lifetime"] = json!(if answer == "allow" { "once" } else { "" });
        }
        format!("{}\n", json!({ "timestamp": timestamp, "fields": fields }))
    }

    fn sample(input: &str, since: Option<&str>, budget: u64) -> Audit {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.log");
        fs::write(&path, input).unwrap();
        let audit = read_sample(
            &path,
            since.map(|value| parse_since(value).unwrap()),
            budget,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), input);
        audit
    }

    #[test]
    fn explicit_answers_are_not_rule_settlements() {
        let input = [
            event(START, "explicit-request", Some(1), None),
            event(END, "explicit-request", Some(1), Some("allow")),
            event(START, "settled-request", Some(1), None),
            event(END, "settled-request", Some(1), Some("matched_rule")),
        ]
        .concat();
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.prompt_events, 2);
        assert_eq!(audit.decision_events, 2);
        assert_eq!(audit.decisions[EXPLICIT], 1);
        assert_eq!(audit.decisions[RULE_SETTLED], 1);
        assert_eq!(audit.explicit_answer_lifetimes["once"], 1);
        assert_eq!(audit.paired_waits_ms[EXPLICIT].paired_events, 1);
        assert_eq!(audit.paired_waits_ms[RULE_SETTLED].paired_events, 1);
        assert_eq!(audit.paired_waits_ms[EXPLICIT].median_ms, Some(2000.0));
        assert_eq!(audit.prompt_tools["shell"], 2);
        assert_eq!(audit.prompt_reasons["uncovered"], 2);
    }

    #[test_case("abandoned", ABANDONED; "abandoned")]
    #[test_case("policy_denied", POLICY_DENIED; "policy_denied")]
    #[test_case("unexpected-answer", UNKNOWN; "unknown_answer")]
    fn nonexplicit_decisions_do_not_count_as_user_answers(answer: &str, category: &str) {
        let input = [
            event(START, "request", None, None),
            event(END, "request", None, Some(answer)),
        ]
        .concat();
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.decisions[category], 1);
        assert_eq!(audit.decisions[EXPLICIT], 0);
        assert_eq!(audit.explicit_answer_lifetimes.values().sum::<u64>(), 0);
        assert_eq!(audit.pairing.events_without_manager_id, 2);
    }

    #[test]
    fn paired_wait_quantiles_use_only_unambiguous_pairs() {
        let mut input = String::new();
        for seconds in 1..=10 {
            let id = seconds.to_string();
            let end = format!("2026-01-01T00:00:{seconds:02}Z");
            input.push_str(&event(START, &id, Some(1), None));
            input.push_str(&event(&end, &id, Some(1), Some("allow")));
        }
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        let waits = &audit.paired_waits_ms[EXPLICIT];
        assert_eq!(waits.paired_events, 10);
        assert_eq!(waits.median_ms, Some(5500.0));
        assert_eq!(waits.p90_ms, Some(9000));
        assert_eq!(waits.min_ms, Some(1000));
        assert_eq!(waits.max_ms, Some(10000));
    }

    #[test_case(Some(1), Some(2), 2; "distinct_managers")]
    #[test_case(Some(1), Some(1), 0; "colliding_manager_and_request")]
    #[test_case(None, None, 0; "colliding_legacy_ids")]
    fn ambiguous_keys_are_excluded_from_pairing(
        first: Option<u64>,
        second: Option<u64>,
        expected: usize,
    ) {
        let input = [
            event(START, "shared-request", first, None),
            event(END, "shared-request", first, Some("allow")),
            event(START, "shared-request", second, None),
            event(END, "shared-request", second, Some("allow")),
        ]
        .concat();
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.prompt_events, 2);
        assert_eq!(audit.paired_waits_ms[EXPLICIT].paired_events, expected);
        assert_eq!(
            audit.pairing.duplicate_or_colliding_keys,
            u64::from(expected == 0)
        );
        assert_eq!(
            audit.pairing.ambiguous_prompt_events,
            if expected == 0 { 2 } else { 0 }
        );
    }

    #[test]
    fn since_reports_incomplete_pairs_instead_of_inventing_answers() {
        let input = [
            event(START, "outside", Some(1), None),
            event(END, "outside", Some(1), Some("allow")),
            event(END, "unanswered", Some(1), None),
        ]
        .concat();
        let audit = sample(&input, Some(END), DEFAULT_MAX_BYTES);
        assert_eq!(audit.sample.events_before_since, 1);
        assert_eq!(audit.prompt_events, 1);
        assert_eq!(audit.decision_events, 1);
        assert_eq!(audit.pairing.prompts_missing_decision, 1);
        assert_eq!(audit.pairing.decisions_missing_prompt, 1);
        assert_eq!(audit.paired_waits_ms[EXPLICIT].paired_events, 0);
    }

    #[test]
    fn reversed_timestamps_are_not_negative_waits() {
        let input = [
            event(END, "request", None, None),
            event(START, "request", None, Some("allow")),
        ]
        .concat();
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.pairing.reversed_timestamp_pairs, 1);
        assert_eq!(audit.paired_waits_ms[EXPLICIT].paired_events, 0);
    }

    #[test_case(1; "newline_boundary")]
    #[test_case(8; "partial_first_line")]
    fn bounded_tail_drops_only_partial_records(prefix_bytes: usize) {
        const PREFIX: &str = "discard this partial prefix line\n";
        const PARTIAL: &str = "{\"unfinished\":";
        let events = [
            event(START, "request", None, None),
            event(END, "request", None, Some("allow")),
        ]
        .concat();
        let budget = (events.len() + PARTIAL.len() + prefix_bytes) as u64;
        let audit = sample(&format!("{PREFIX}{events}{PARTIAL}"), None, budget);
        assert_eq!(audit.sample.bytes_read, budget);
        assert_eq!(audit.sample.boundary_probe_bytes, 1);
        assert_eq!(
            audit.sample.leading_partial_bytes_skipped > 0,
            prefix_bytes > 1
        );
        assert_eq!(audit.sample.trailing_partial_bytes_skipped, PARTIAL.len());
        assert_eq!(audit.prompt_events, 1);
        assert_eq!(audit.decision_events, 1);
        assert_eq!(audit.sample.invalid_json_lines, 0);
    }

    #[test]
    fn huge_lines_and_line_counts_are_bounded() {
        let input = format!(
            "{}\n{}",
            "x".repeat(MAX_LINE_BYTES + 1),
            event(START, "request", None, None)
        );
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.sample.oversized_lines_skipped, 1);
        assert_eq!(audit.prompt_events, 1);
        let audit = sample(&"{}\n".repeat(MAX_LINES + 1), None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.sample.lines_considered, MAX_LINES);
        assert_eq!(audit.sample.complete_bytes_not_processed, "{}\n".len());
    }

    #[test_case(0, 1; "minimum")]
    #[test_case(u64::MAX, MAX_BYTES; "maximum")]
    fn byte_budget_is_clamped(requested: u64, effective: u64) {
        let audit = sample("", None, requested);
        assert_eq!(audit.sample.effective_max_bytes, effective);
        assert_eq!(audit.sample.bytes_read, 0);
    }

    #[test]
    fn arbitrary_fields_are_never_serialized_or_used_as_categories() {
        let mut prompt: Value = serde_json::from_str(&event(START, SECRET, None, None)).unwrap();
        for field in ["tool", "forcing_reason", "uncovered", "command", "resource"] {
            prompt["fields"][field] = json!(SECRET);
        }
        let mut decision: Value =
            serde_json::from_str(&event(END, SECRET, None, Some("allow"))).unwrap();
        decision["fields"]["lifetime"] = json!(SECRET);
        let input = format!("{prompt}\n{decision}\n");
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.prompt_tools[UNKNOWN], 1);
        assert_eq!(audit.prompt_reasons[UNKNOWN], 1);
        assert_eq!(audit.explicit_answer_lifetimes[UNKNOWN], 1);
        assert!(!serde_json::to_string(&audit).unwrap().contains(SECRET));
        let error = parse_since(SECRET).unwrap_err();
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn malformed_events_are_counted_without_leaking_parser_errors() {
        let input = format!(
            "{{bad-json}}\n{}{{}}\n",
            event(SECRET, "request", None, None)
        );
        let audit = sample(&input, None, DEFAULT_MAX_BYTES);
        assert_eq!(audit.sample.invalid_json_lines, 1);
        assert_eq!(audit.sample.invalid_timestamp_events, 1);
        assert_eq!(audit.sample.non_permission_lines, 1);
        assert_eq!(audit.prompt_events, 0);
        assert!(!serde_json::to_string(&audit).unwrap().contains(SECRET));
    }

    #[cfg(unix)]
    #[test]
    fn default_log_resolution_is_noncreating() {
        if let Some(root) = env::var_os(PATH_TEST) {
            let root = PathBuf::from(root);
            assert!(super::run(None, None, None).is_err());
            assert!(!root.exists());
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("uncreated");
        let result = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "cmd::permissions::audit::tests::default_log_resolution_is_noncreating",
            ])
            .env(PATH_TEST, &root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!root.exists());
    }
}
