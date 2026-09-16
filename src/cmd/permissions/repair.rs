use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use caudra_agent::permissions::{
    PermissionArgumentConstraint, PermissionSubject, canonical_json_sha256,
    review::{review_from_candidates, visit_review_candidate_preimages},
    selected_input_digest,
};
use caudra_agent::tools::native;
use caudra_storage::StateDir;
use caudra_storage::paths::normalize_path;
use caudra_storage::permission_state::{
    PermissionHistoryScan, PermissionResourceKind, PermissionResourceSelector, PermissionReview,
    PermissionReviewSource, PermissionRuleRecord, RawPermissionSnapshot, inventory_fingerprint,
    read_repair_record, validate_conversation_record,
};
use caudra_storage::sessions::{SESSIONS_DB_FILE, SessionDatabase};
use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_HISTORY_ROWS: usize = 1_000_000;
const MAX_HISTORY_BYTES: usize = 2 * 1024 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 1024 * 1024;
const MAX_VALUE_BYTES: usize = 32_768;
const MAX_CANDIDATES: usize = 100_000;
const MAX_CANDIDATE_BYTES: usize = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 100_000;
const MAX_RULES: usize = 10_000;
const MAX_MATCHED_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_CANDIDATE_CHECKS: usize = 10_000_000;
const MAX_COMMAND_CANDIDATE_CHECKS: usize = 2_000_000;
const MAX_CANDIDATE_WORK_BYTES: usize = 2 * 1024 * 1024 * 1024;
const MAX_COMMAND_CANDIDATE_WORK_BYTES: usize = 512 * 1024 * 1024;
const RAW_CANDIDATE_WORK_FACTOR: usize = 2;
const URL_CANDIDATE_WORK_FACTOR: usize = 3 * (MAX_DEPTH + 4);
pub(super) const RULES_KEY: &str = "structured_permission_rules";
const INVALID_RECORDS: &str = "invalid permission records; repair refused without changing storage";
const UNAVAILABLE_TOOL: &str = "Unavailable (unrecognized tool contract)";
const MISSING_SCOPE: &str = "[scope unavailable:";
const OMITTED: &str = "[omitted:";
const MISSING_INPUT_AUTHORITY: &str = "; input scope unavailable: no verified input";
const MISSING_SCOPE_AUTHORITY: &str = "; some scope descriptions unavailable or omitted";

static NATIVE_CONTRACTS: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| native::review_contracts().into_iter().collect());

type ReviewLocation = (Option<usize>, usize);

#[derive(Default, Serialize)]
struct RepairReport {
    dry_run: bool,
    persistent_rules: usize,
    conversation_rules: usize,
    sessions: usize,
    already_typed: usize,
    retried: usize,
    repaired: usize,
    recovered: usize,
    unavailable: usize,
    unavailable_scopes: usize,
    unavailable_inputs: usize,
    history: PermissionHistoryScan,
    tool_calls: usize,
    exact_input_hashes: usize,
    selected_projection_hashes: usize,
    candidates: usize,
    candidate_bytes: usize,
    candidate_limit_reached: bool,
    candidate_digests_wanted: usize,
    candidate_digests_remaining: usize,
    candidate_values_processed: usize,
    candidate_values_discarded: usize,
    candidate_values_duplicate: usize,
    candidate_checks_skipped: usize,
    candidate_work_bytes: usize,
    path_candidate_work_limit_reached: bool,
    command_candidate_work_limit_reached: bool,
    candidate_depth_limit_reached: bool,
    value_limit_reached: bool,
    matched_input_limit_reached: bool,
    traversal_limit_reached: bool,
    invalid_history_rows: usize,
    backup: Option<String>,
}

struct RepairTarget {
    location: ReviewLocation,
    record: PermissionRuleRecord,
    input: Option<Arc<Value>>,
}

#[derive(Default)]
struct ToolInputLookup {
    exact: HashMap<String, Vec<usize>>,
    selected: HashMap<Vec<String>, HashMap<String, Vec<usize>>>,
}

#[derive(Default)]
struct InputLookup {
    tools: HashMap<String, ToolInputLookup>,
    exact_hashes: usize,
    projection_hashes: usize,
}

impl InputLookup {
    fn new(targets: &[RepairTarget]) -> Self {
        let mut lookup = Self::default();
        for (index, target) in targets.iter().enumerate() {
            let tool = tool_name(&target.record.rule.subject);
            if tool == UNAVAILABLE_TOOL {
                continue;
            }
            let entries = lookup.tools.entry(tool.into()).or_default();
            match &target.record.rule.arguments {
                PermissionArgumentConstraint::Exact { digest } => {
                    entries.exact.entry(digest.clone()).or_default().push(index)
                }
                PermissionArgumentConstraint::SelectedDigest { pointers, digest } => entries
                    .selected
                    .entry(pointers.clone())
                    .or_default()
                    .entry(digest.clone())
                    .or_default()
                    .push(index),
                _ => {}
            }
        }
        lookup
    }

    fn take_matches(&mut self, tool: &str, input: &Value) -> Vec<usize> {
        let Some(entries) = self.tools.get_mut(tool) else {
            return Vec::new();
        };
        let mut matched = Vec::new();
        if !entries.exact.is_empty() {
            self.exact_hashes += 1;
            if let Some(indices) = entries.exact.remove(&canonical_json_sha256(input)) {
                matched.extend(indices);
            }
        }
        entries.selected.retain(|pointers, digests| {
            self.projection_hashes += 1;
            if let Ok(digest) = selected_input_digest(input, pointers)
                && let Some(indices) = digests.remove(&digest)
            {
                matched.extend(indices);
            }
            !digests.is_empty()
        });
        matched
    }
}

#[derive(Default)]
struct CandidateWork {
    checks: usize,
    bytes: usize,
    skipped: usize,
    limited: bool,
}

impl CandidateWork {
    fn admit(&mut self, bytes: usize, max_checks: usize, max_bytes: usize) -> bool {
        if self.checks >= max_checks || bytes > max_bytes.saturating_sub(self.bytes) {
            self.skipped += 1;
            self.limited = true;
            return false;
        }
        self.checks += 1;
        self.bytes += bytes;
        true
    }
}

#[derive(Default)]
struct Candidates {
    wanted: HashSet<String>,
    urls: bool,
    values: HashSet<String>,
    bytes: usize,
    count_limited: bool,
    value_limited: bool,
    depth_limited: bool,
    discarded: usize,
    duplicate: usize,
    paths_work: CandidateWork,
    commands_work: CandidateWork,
}

impl Candidates {
    fn new(targets: &[RepairTarget]) -> Self {
        let mut candidates = Self::default();
        for target in targets {
            for resource in &target.record.rule.resources {
                candidates.want(
                    &resource.selector,
                    matches!(resource.kind, PermissionResourceKind::Url),
                );
                for selector in resource.attributes.values() {
                    candidates.want(selector, false);
                }
            }
        }
        candidates
    }

    fn want(&mut self, selector: &PermissionResourceSelector, url: bool) {
        let digest = match selector {
            PermissionResourceSelector::Digest { digest } => {
                self.urls |= url;
                digest
            }
            PermissionResourceSelector::FilesystemSubtreeDigest { digest } => digest,
            PermissionResourceSelector::UrlSubtreeDigest { digest }
            | PermissionResourceSelector::UrlOriginDigest { digest } => {
                self.urls = true;
                digest
            }
            _ => return,
        };
        self.wanted.insert(digest.clone());
    }

    fn insert(&mut self, value: &str) {
        self.check(value, false);
    }

    fn check(&mut self, value: &str, command: bool) {
        if value.len() > MAX_VALUE_BYTES {
            self.value_limited = true;
            return;
        }
        if value.is_empty() || self.wanted.is_empty() {
            return;
        }
        let work_bytes = value.len()
            * if self.urls && value.contains("://") {
                URL_CANDIDATE_WORK_FACTOR
            } else {
                RAW_CANDIDATE_WORK_FACTOR
            };
        let admitted = if command {
            self.commands_work.admit(
                work_bytes,
                MAX_COMMAND_CANDIDATE_CHECKS,
                MAX_COMMAND_CANDIDATE_WORK_BYTES,
            )
        } else {
            self.paths_work
                .admit(work_bytes, MAX_CANDIDATE_CHECKS, MAX_CANDIDATE_WORK_BYTES)
        };
        if !admitted {
            return;
        }
        if self.values.contains(value) {
            self.duplicate += 1;
            return;
        }
        let mut matched = false;
        let limited = visit_review_candidate_preimages(
            value,
            self.urls.then_some(MAX_DEPTH),
            |digest, preimage| {
                if self.wanted.contains(digest) {
                    matched = true;
                    if self.retain(preimage) {
                        self.wanted.remove(digest);
                    }
                }
            },
        );
        self.depth_limited |= limited;
        if !matched {
            self.discarded += 1;
        }
    }

    fn retain(&mut self, value: &str) -> bool {
        if self.values.contains(value) {
            return true;
        }
        if value.len() > MAX_VALUE_BYTES {
            self.value_limited = true;
            return false;
        }
        if self.values.len() >= MAX_CANDIDATES || self.bytes + value.len() > MAX_CANDIDATE_BYTES {
            self.count_limited = true;
            return false;
        }
        self.bytes += value.len();
        self.values.insert(value.into());
        true
    }

    fn path(&mut self, value: &str, cwd: &str) {
        if value.len() > MAX_VALUE_BYTES || cwd.len() > MAX_VALUE_BYTES {
            self.value_limited = true;
            return;
        }
        if self.wanted.is_empty()
            || self.paths_work.checks >= MAX_CANDIDATE_CHECKS
            || self.paths_work.bytes >= MAX_CANDIDATE_WORK_BYTES
        {
            if !self.wanted.is_empty() {
                self.paths_work.limited = true;
                self.paths_work.skipped += 1;
            }
            return;
        }
        let path = Path::new(cwd).join(value);
        if path.is_absolute() {
            let normalized = normalize_path(&path);
            for (index, variant) in [&path, &normalized].into_iter().enumerate() {
                if index != 0 && path.as_os_str() == normalized.as_os_str() {
                    continue;
                }
                for (index, path) in variant.ancestors().enumerate() {
                    if index == MAX_DEPTH {
                        self.depth_limited = true;
                        break;
                    }
                    if let Some(path) = path.to_str() {
                        self.insert(path);
                    }
                }
            }
        }
    }

    fn input(&mut self, input: &Value, cwd: &str) {
        if self.wanted.is_empty() {
            return;
        }
        self.path(cwd, cwd);
        let workdir = input
            .get("workdir")
            .or_else(|| input.get("cwd"))
            .and_then(Value::as_str);
        let base = workdir.map(|workdir| Path::new(cwd).join(workdir));
        let base = base.as_deref().and_then(Path::to_str).unwrap_or(cwd);
        self.path(base, cwd);
        if let Some(fields) = input.as_object() {
            for (key, value) in fields {
                let Some(value) = value.as_str() else {
                    continue;
                };
                match key.as_str() {
                    "filePath" | "file_path" | "path" | "root" | "directory" => {
                        self.insert(value);
                        self.path(value, base);
                    }
                    "cwd" | "workdir" => {
                        self.insert(value);
                        self.path(value, cwd);
                    }
                    "url" | "query" | "pattern" => self.insert(value),
                    "command" => self.command(value, base),
                    _ => {}
                }
            }
        }
    }

    fn command(&mut self, command: &str, cwd: &str) {
        self.check(command, true);
        if command.len() > MAX_VALUE_BYTES || self.wanted.is_empty() {
            return;
        }
        for fragment in command.split([';', '|', '&', '\n', '(', ')']) {
            let fragment = fragment.trim();
            if fragment != command {
                self.check(fragment, true);
            }
            let normalized = fragment.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized != fragment {
                self.check(&normalized, true);
            }
            for token in fragment.split_whitespace() {
                let token = token.trim_matches(['\'', '"']);
                if token.contains("://") {
                    self.check(token, false);
                } else if token != fragment {
                    self.check(token, true);
                }
                if token.starts_with('/') || token.starts_with("./") || token.starts_with("../") {
                    self.path(token, cwd);
                }
            }
        }
    }
}

pub(super) fn tool_name(subject: &PermissionSubject) -> &str {
    match subject {
        PermissionSubject::Native { owner, contract } if owner == native::OWNER => NATIVE_CONTRACTS
            .get(contract)
            .map(String::as_str)
            .unwrap_or(UNAVAILABLE_TOOL),
        PermissionSubject::Native { owner, contract } if owner == "workcell" => {
            match contract.as_str() {
                "file.read.v1" => "file_read",
                "file.glob.v1" => "file_glob",
                "file.grep.v1" => "file_grep",
                "file.write.v1" => "file_write",
                "file.edit.v1" => "file_edit",
                "file.patch.v1" => "file_apply_patch",
                "file.index.v1" => "file_index",
                "shell.execution.v1" => "shell",
                "python.execution.v1" => "python_execution",
                "web.search.v1" => "websearch",
                "web.fetch.v1" => "webfetch",
                "code.map.v1" => "code_map",
                "code.context.v1" => "code_context",
                "code.refs.v1" => "code_refs",
                "code.impact.v1" => "code_impact",
                "code.expand.v1" => "code_expand",
                "execution-environment.snapshot.v1" => "execution_environment",
                _ => UNAVAILABLE_TOOL,
            }
        }
        PermissionSubject::Lua { tool, .. }
        | PermissionSubject::Mcp { tool, .. }
        | PermissionSubject::RemoteWorkcell { tool, .. } => tool,
        _ => UNAVAILABLE_TOOL,
    }
}

pub(super) fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| eyre!(INVALID_RECORDS))
}

fn collect_targets(
    value: &Value,
    session: Option<usize>,
    retry_unavailable: bool,
    targets: &mut Vec<RepairTarget>,
    report: &mut RepairReport,
) -> Result<()> {
    let rows = value.as_array().ok_or_else(|| eyre!(INVALID_RECORDS))?;
    let mut ids = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        if report.persistent_rules + report.conversation_rules >= MAX_RULES {
            bail!("permission inventory exceeds {MAX_RULES} records; repair refused");
        }
        let persistent = session.is_none();
        let mut record = read_repair_record(row, persistent)?;
        if !ids.insert(record.id.clone()) {
            bail!(INVALID_RECORDS);
        }
        if persistent {
            report.persistent_rules += 1;
        } else {
            report.conversation_rules += 1;
        }
        record.review = typed_review(row, persistent);
        if row
            .get("review")
            .and_then(|review| review.get("source"))
            .and_then(Value::as_str)
            == Some("approved")
            && record.review.is_none()
        {
            bail!(INVALID_RECORDS);
        }
        let retry = retry_unavailable
            && record.review.as_ref().is_some_and(|review| {
                review.source == PermissionReviewSource::Unavailable
                    || (review.source == PermissionReviewSource::Recovered
                        && incomplete_review(&record, review))
            });
        if record.review.is_none() || retry {
            if retry {
                report.retried += 1;
            }
            targets.push(RepairTarget {
                location: (session, index),
                record,
                input: None,
            });
        } else {
            report.already_typed += 1;
        }
    }
    Ok(())
}

fn typed_review(row: &Value, persistent: bool) -> Option<PermissionReview> {
    let Ok(record) = serde_json::from_value::<PermissionRuleRecord>(row.clone()) else {
        return None;
    };
    let valid = if persistent {
        inventory_fingerprint(std::slice::from_ref(&record)).is_ok()
    } else {
        validate_conversation_record(&record).is_ok()
    };
    valid.then_some(record.review).flatten()
}

fn incomplete_value(value: &Value) -> bool {
    match value {
        Value::String(value) => missing_label(value),
        Value::Array(values) => values.iter().any(incomplete_value),
        Value::Object(values) => values.values().any(incomplete_value),
        _ => false,
    }
}

fn missing_label(value: &str) -> bool {
    value.starts_with(MISSING_SCOPE) || value.starts_with(OMITTED)
}

fn incomplete_review(record: &PermissionRuleRecord, review: &PermissionReview) -> bool {
    review.tool == UNAVAILABLE_TOOL
        || missing_label(&review.tool)
        || (!matches!(
            record.rule.arguments,
            PermissionArgumentConstraint::Unconstrained
        ) && review.input.as_ref().is_none_or(incomplete_value))
        || incomplete_scopes(record, review)
}

fn incomplete_scopes(record: &PermissionRuleRecord, review: &PermissionReview) -> bool {
    record
        .rule
        .resources
        .iter()
        .enumerate()
        .any(|(index, resource)| {
            let Some(label) = review.resources.iter().find(|label| label.index == index) else {
                return true;
            };
            label.value.as_deref().is_none_or(missing_label)
                || resource.attributes.keys().any(|key| {
                    label
                        .attributes
                        .get(key)
                        .is_none_or(|value| missing_label(value))
                })
        })
}

fn retain_recovered_labels(
    record: &PermissionRuleRecord,
    review: &mut PermissionReview,
    previous: &PermissionReview,
) {
    if review.tool == UNAVAILABLE_TOOL && previous.tool != UNAVAILABLE_TOOL {
        review.tool.clone_from(&previous.tool);
    }
    if review.input.as_ref().is_none_or(incomplete_value)
        && previous
            .input
            .as_ref()
            .is_some_and(|input| review.input.is_none() || !incomplete_value(input))
    {
        review.input.clone_from(&previous.input);
    }
    for resource in &mut review.resources {
        let Some(old) = previous
            .resources
            .iter()
            .find(|old| old.index == resource.index)
        else {
            continue;
        };
        if resource.value.as_deref().is_none_or(missing_label)
            && old
                .value
                .as_deref()
                .is_some_and(|value| !missing_label(value))
        {
            resource.value.clone_from(&old.value);
        }
        for (key, value) in &mut resource.attributes {
            if missing_label(value)
                && let Some(old) = old
                    .attributes
                    .get(key)
                    .filter(|value| !missing_label(value))
            {
                value.clone_from(old);
            }
        }
    }
    if review.input.is_some() {
        review.authority = review.authority.replace(MISSING_INPUT_AUTHORITY, "");
    }
    if !incomplete_scopes(record, review) {
        review.authority = review.authority.replace(MISSING_SCOPE_AUTHORITY, "");
    }
}

fn visit_calls(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    limited: &mut bool,
    visit: &mut impl FnMut(&str, &Value),
) {
    if depth > MAX_DEPTH || *nodes == 0 {
        *limited = true;
        return;
    }
    *nodes -= 1;
    if let Some(object) = value.as_object() {
        if let Some(name) = object
            .get("name")
            .or_else(|| object.get("tool"))
            .or_else(|| object.get("recipient_name"))
            .and_then(Value::as_str)
            && let Some(input) = object
                .get("input")
                .or_else(|| object.get("parameters"))
                .or_else(|| object.get("arguments"))
        {
            let name = name
                .strip_prefix("functions.")
                .or_else(|| name.strip_prefix("multi_tool_use."))
                .unwrap_or(name);
            let parsed = input
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let input = parsed.as_ref().unwrap_or(input);
            visit(name, input);
            if matches!(name, "batch" | "parallel") {
                visit_calls(input, depth + 1, nodes, limited, visit);
            }
            return;
        }
        if let Some(name) = object.get("tool").and_then(Value::as_str) {
            let mut input = object.clone();
            input.remove("tool");
            visit(name, &Value::Object(input));
            return;
        }
        for (key, value) in object {
            if matches!(
                key.as_str(),
                "content" | "message" | "tool_calls" | "tool_uses" | "function"
            ) {
                visit_calls(value, depth + 1, nodes, limited, visit);
            }
        }
    } else if let Some(values) = value.as_array() {
        for value in values {
            visit_calls(value, depth + 1, nodes, limited, visit);
            if *nodes == 0 {
                *limited = true;
                break;
            }
        }
    }
}

fn prepare(
    database: &SessionDatabase,
    snapshot: &RawPermissionSnapshot,
    retry_unavailable: bool,
) -> Result<(RawPermissionSnapshot, RepairReport)> {
    let mut report = RepairReport {
        dry_run: true,
        sessions: snapshot.sessions.len(),
        ..RepairReport::default()
    };
    let mut targets = Vec::new();
    if let Some(value) = &snapshot.persistent {
        collect_targets(
            &parse_json(value)?,
            None,
            retry_unavailable,
            &mut targets,
            &mut report,
        )?;
    }
    for (index, session) in snapshot.sessions.iter().enumerate() {
        if let Some(rules) = parse_json(&session.metadata)?.get(RULES_KEY) {
            collect_targets(
                rules,
                Some(index),
                retry_unavailable,
                &mut targets,
                &mut report,
            )?;
        }
    }
    let mut candidates = Candidates::new(&targets);
    report.candidate_digests_wanted = candidates.wanted.len();
    for session in &snapshot.sessions {
        candidates.path(&session.cwd, &session.cwd);
    }
    for target in &targets {
        if let Some(project) = target.record.project.as_deref().and_then(Path::to_str) {
            candidates.path(project, project);
        }
    }
    if !targets.is_empty() {
        let mut matched_bytes = 0;
        let mut lookup = InputLookup::new(&targets);
        report.history = database.visit_permission_history(
            MAX_HISTORY_ROWS,
            MAX_HISTORY_BYTES,
            MAX_ROW_BYTES,
            |cwd, payload| {
                let Ok(value) = serde_json::from_str::<Value>(payload) else {
                    report.invalid_history_rows += 1;
                    return;
                };
                let mut nodes = MAX_NODES;
                visit_calls(
                    &value,
                    0,
                    &mut nodes,
                    &mut report.traversal_limit_reached,
                    &mut |name, input| {
                        report.tool_calls += 1;
                        candidates.input(input, cwd);
                        if !lookup.tools.contains_key(name) {
                            return;
                        }
                        let bytes =
                            serde_json::to_vec(input).map_or(usize::MAX, |bytes| bytes.len());
                        if bytes > MAX_VALUE_BYTES {
                            candidates.value_limited = true;
                            return;
                        }
                        if bytes > MAX_MATCHED_INPUT_BYTES - matched_bytes {
                            report.matched_input_limit_reached = true;
                            return;
                        }
                        let matched = lookup.take_matches(name, input);
                        if !matched.is_empty() {
                            matched_bytes += bytes;
                            let input = Arc::new(input.clone());
                            for index in matched {
                                targets[index].input = Some(Arc::clone(&input));
                            }
                        }
                    },
                );
            },
        )?;
        report.exact_input_hashes = lookup.exact_hashes;
        report.selected_projection_hashes = lookup.projection_hashes;
    }
    report.candidates = candidates.values.len();
    report.candidate_bytes = candidates.bytes;
    report.candidate_limit_reached = candidates.count_limited;
    report.candidate_digests_remaining = candidates.wanted.len();
    report.candidate_values_processed =
        candidates.paths_work.checks + candidates.commands_work.checks;
    report.candidate_values_discarded = candidates.discarded;
    report.candidate_values_duplicate = candidates.duplicate;
    report.candidate_checks_skipped =
        candidates.paths_work.skipped + candidates.commands_work.skipped;
    report.candidate_work_bytes = candidates.paths_work.bytes + candidates.commands_work.bytes;
    report.path_candidate_work_limit_reached = candidates.paths_work.limited;
    report.command_candidate_work_limit_reached = candidates.commands_work.limited;
    report.candidate_depth_limit_reached = candidates.depth_limited;
    report.value_limit_reached = candidates.value_limited;
    let mut candidates = candidates.values.into_iter().collect::<Vec<_>>();
    candidates.sort();
    let mut reviews = HashMap::new();
    for target in targets {
        let rule = &target.record.rule;
        let mut review = review_from_candidates(
            rule,
            tool_name(&rule.subject),
            target.input.as_deref(),
            &candidates,
            PermissionReviewSource::Recovered,
        );
        if let Some(previous) = &target.record.review {
            retain_recovered_labels(&target.record, &mut review, previous);
        }
        let verified_scope = review.resources.iter().any(|resource| {
            resource.value.is_some()
                || resource
                    .attributes
                    .values()
                    .any(|value| !missing_label(value))
        });
        if review.input.is_some() || verified_scope {
            report.recovered += 1;
        } else {
            review.source = PermissionReviewSource::Unavailable;
            report.unavailable += 1;
        }
        report.unavailable_scopes += review
            .resources
            .iter()
            .filter(|resource| resource.value.is_none())
            .count();
        report.unavailable_scopes += review
            .resources
            .iter()
            .flat_map(|resource| resource.attributes.values())
            .filter(|value| missing_label(value))
            .count();
        if review.input.is_none()
            && !matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained)
        {
            report.unavailable_inputs += 1;
        }
        if target.record.review.as_ref() != Some(&review) {
            let mut checked = target.record.clone();
            checked.review = Some(review.clone());
            if target.location.0.is_none() {
                inventory_fingerprint(std::slice::from_ref(&checked))
                    .map_err(|_| eyre!(INVALID_RECORDS))?;
            } else {
                validate_conversation_record(&checked).map_err(|_| eyre!(INVALID_RECORDS))?;
            }
            report.repaired += 1;
            reviews.insert(target.location, serde_json::to_value(review)?);
        }
    }
    let mut replacement = snapshot.clone();
    if let Some(raw) = &mut replacement.persistent {
        let mut value = parse_json(raw)?;
        if replace_reviews(&mut value, None, &reviews)? {
            *raw = serde_json::to_string(&value)?;
        }
    }
    for (index, session) in replacement.sessions.iter_mut().enumerate() {
        let mut metadata = parse_json(&session.metadata)?;
        if let Some(rules) = metadata.get_mut(RULES_KEY)
            && replace_reviews(rules, Some(index), &reviews)?
        {
            session.metadata = serde_json::to_string(&metadata)?;
        }
    }
    Ok((replacement, report))
}

fn replace_reviews(
    value: &mut Value,
    session: Option<usize>,
    reviews: &HashMap<ReviewLocation, Value>,
) -> Result<bool> {
    let mut changed = false;
    for (index, row) in value
        .as_array_mut()
        .ok_or_else(|| eyre!(INVALID_RECORDS))?
        .iter_mut()
        .enumerate()
    {
        if let Some(review) = reviews.get(&(session, index)) {
            row["review"] = review.clone();
            changed = true;
        }
    }
    Ok(changed)
}

pub(super) fn run(
    state_dir: &StateDir,
    apply: bool,
    json: bool,
    retry_unavailable: bool,
) -> Result<()> {
    let path = state_dir.path().join(SESSIONS_DB_FILE);
    let display_path = super::inventory::terminal_text(&path.display().to_string());
    if !json {
        println!("Database: {display_path}");
    }
    let database = SessionDatabase::open_read_only(state_dir)
        .with_context(|| format!("open permission database {display_path}"))?;
    let snapshot = database.raw_permission_snapshot()?;
    let (replacement, mut report) = prepare(&database, &snapshot, retry_unavailable)?;
    drop(database);
    if apply && report.repaired > 0 {
        report.backup = Some(
            SessionDatabase::repair_permission_reviews(state_dir, &snapshot, &replacement)?
                .display()
                .to_string(),
        );
    }
    report.dry_run = !apply;
    if json {
        let mut output = serde_json::to_value(&report)?;
        output["database"] = json!(path);
        output["retry_unavailable"] = json!(retry_unavailable);
        output["limits"] = json!({
            "history_rows": MAX_HISTORY_ROWS, "history_bytes": MAX_HISTORY_BYTES,
            "row_bytes": MAX_ROW_BYTES, "value_bytes": MAX_VALUE_BYTES,
            "candidates": MAX_CANDIDATES, "candidate_bytes": MAX_CANDIDATE_BYTES,
            "depth": MAX_DEPTH, "nodes_per_row": MAX_NODES,
            "rules": MAX_RULES, "matched_input_bytes": MAX_MATCHED_INPUT_BYTES,
            "path_candidate_checks": MAX_CANDIDATE_CHECKS,
            "command_candidate_checks": MAX_COMMAND_CANDIDATE_CHECKS,
            "path_candidate_work_bytes": MAX_CANDIDATE_WORK_BYTES,
            "command_candidate_work_bytes": MAX_COMMAND_CANDIDATE_WORK_BYTES,
            "raw_candidate_work_factor": RAW_CANDIDATE_WORK_FACTOR,
            "url_candidate_work_factor": URL_CANDIDATE_WORK_FACTOR,
        });
        println!("{}", super::inventory::terminal_json(&output)?);
    } else {
        println!(
            "Retry unavailable: {retry_unavailable}; {} reviews retried",
            report.retried
        );
        println!(
            "Permission review repair — {}",
            if apply {
                "applied"
            } else {
                "dry run; no database changes"
            }
        );
        println!(
            "{} persistent rules, {} conversation rules in {} sessions; {} already typed",
            report.persistent_rules,
            report.conversation_rules,
            report.sessions,
            report.already_typed
        );
        println!(
            "{} reviews changed in this plan; attempted recovery: {} recovered, {} unavailable; {} scope labels and {} inputs unavailable",
            report.repaired,
            report.recovered,
            report.unavailable,
            report.unavailable_scopes,
            report.unavailable_inputs
        );
        println!(
            "Scanned {} history rows ({} bytes), {} tool calls; {} candidates ({} bytes)",
            report.history.rows,
            report.history.bytes,
            report.tool_calls,
            report.candidates,
            report.candidate_bytes
        );
        println!(
            "Limits: {MAX_HISTORY_ROWS} rows, {MAX_HISTORY_BYTES} scan bytes, {MAX_ROW_BYTES} bytes/row, {MAX_VALUE_BYTES} bytes/value, {MAX_DEPTH} depth, {MAX_NODES} nodes/row, {MAX_CANDIDATES} candidates, {MAX_CANDIDATE_BYTES} candidate bytes, {MAX_MATCHED_INPUT_BYTES} matched-input bytes."
        );
        println!(
            "Candidate checks: {} processed, {} discarded, {} repeated, {} skipped; {} useful preimages retained, {} of {} wanted digests still unavailable",
            report.candidate_values_processed,
            report.candidate_values_discarded,
            report.candidate_values_duplicate,
            report.candidate_checks_skipped,
            report.candidates,
            report.candidate_digests_remaining,
            report.candidate_digests_wanted
        );
        println!(
            "Candidate work: {} budgeted bytes (raw multiplier {RAW_CANDIDATE_WORK_FACTOR}, URL multiplier {URL_CANDIDATE_WORK_FACTOR}); path/URL limits {MAX_CANDIDATE_CHECKS} checks / {MAX_CANDIDATE_WORK_BYTES} bytes, command limits {MAX_COMMAND_CANDIDATE_CHECKS} checks / {MAX_COMMAND_CANDIDATE_WORK_BYTES} bytes",
            report.candidate_work_bytes
        );
        println!(
            "Candidate work truncation: paths/URLs={}, command text={}, depth={}",
            report.path_candidate_work_limit_reached,
            report.command_candidate_work_limit_reached,
            report.candidate_depth_limit_reached
        );
        println!(
            "Truncation: history={}, oversized rows={}, candidates={}, values={}, matched inputs={}, traversal={}; invalid rows={}",
            report.history.truncated,
            report.history.oversized_rows,
            report.candidate_limit_reached,
            report.value_limit_reached,
            report.matched_input_limit_reached,
            report.traversal_limit_reached,
            report.invalid_history_rows
        );
        println!(
            "Only review metadata changes. No commands run, history paths resolved, or authority transferred. Unverified values remain unavailable."
        );
        if let Some(backup) = &report.backup {
            println!("SQLite backup: {}", super::inventory::terminal_text(backup));
        }
        if !apply {
            println!(
                "To apply, stop all sessions and storage readers, then rerun with --apply, retaining this database selection and retry options."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Candidates, INVALID_RECORDS, InputLookup, MAX_CANDIDATE_WORK_BYTES, MAX_CANDIDATES,
        MAX_COMMAND_CANDIDATE_CHECKS, MAX_DEPTH, MAX_NODES, MAX_RULES, MAX_VALUE_BYTES,
        RepairTarget, UNAVAILABLE_TOOL, prepare, tool_name, visit_calls,
    };
    use caudra_agent::permissions::{
        PermissionArgumentConstraint, PermissionSubject, canonical_json_sha256,
        review::review_from_candidates, selected_input_digest,
    };
    use caudra_agent::tools::native;
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_state::{
        PermissionResourceSelector, PermissionReviewSource, PermissionRuleRecord,
        RawPermissionSession, RawPermissionSnapshot, read_inventory, read_repair_record,
    };
    use caudra_storage::sessions::{Session, SessionDatabase, TitleSource};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    #[cfg(unix)]
    use std::{fs, os::unix::fs::symlink};
    use test_case::test_case;

    const PATH: &str = "/historical/nonexistent/private.rs";
    const CWD: &str = "/historical/nonexistent";
    const STATE_KEY: &str = "permission.rules";
    const SECRET: &str = "old-review-secret";
    const URL: &str = "http://EXAMPLE.COM/a/b?limit=2#ignored";
    const URL_EXACT: &str = "https://example.com/a/b?limit=2";
    const URL_ORIGIN: &str = "https://example.com";
    const URL_SUBTREE: &str = "https://example.com/a";

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(transparent)]
    struct Message(Value);

    impl TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    fn record(input: &Value, selected: bool) -> Value {
        let arguments = if selected {
            json!({"constraint": "selected_digest", "pointers": ["/filePath", "/limit"], "digest": selected_input_digest(input, &["/filePath", "/limit"]).unwrap()})
        } else {
            json!({"constraint": "exact", "digest": canonical_json_sha256(input)})
        };
        json!({"id": CaudraId::generate().to_string(), "created_at": 1,
            "rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "file.read.v1"},
                "executor": "native", "resources": [{"kind": {"kind": "file"}, "selector": {"match": "digest", "digest": canonical_json_sha256(&json!(PATH))}, "access": "read"}],
                "arguments": arguments, "lifetime": "global", "effect": "deny"},
            "review": {"tool": SECRET, "input_summary": SECRET}, "unknown_field": {"keep": true}})
    }

    fn filesystem_target(root: &str, file: &str) -> RepairTarget {
        let mut raw = record(&json!({}), false);
        raw["rule"]["resources"][0]["selector"]["digest"] =
            json!(canonical_json_sha256(&json!(file)));
        raw["rule"]["resources"][0]["attributes"] =
            json!({"workdir": {"match": "digest", "digest": canonical_json_sha256(&json!(root))}});
        raw["rule"]["resources"].as_array_mut().unwrap().push(json!({
            "kind": {"kind": "directory"}, "access": "read",
            "selector": {"match": "filesystem_subtree_digest", "digest": canonical_json_sha256(&json!(["filesystem_subtree", root]))}
        }));
        RepairTarget {
            location: (None, 0),
            record: read_repair_record(&raw, true).unwrap(),
            input: None,
        }
    }

    #[test_case(MAX_CANDIDATES + 1; "late_filesystem_and_workdir_survive_command_noise")]
    fn irrelevant_commands_cannot_fill_candidate_retention(noise: usize) {
        let target = filesystem_target(CWD, PATH);
        let mut candidates = Candidates::new(std::slice::from_ref(&target));
        for index in 0..noise {
            candidates.command(&format!("irrelevant_{index}"), CWD);
        }
        assert!(candidates.values.is_empty());
        assert!(candidates.discarded >= noise);
        assert!(!candidates.count_limited);
        candidates.input(&json!({"filePath": PATH}), CWD);
        assert_eq!(candidates.values.len(), 2);
        assert!(candidates.wanted.is_empty());
        let values = candidates.values.into_iter().collect::<Vec<_>>();
        let review = review_from_candidates(
            &target.record.rule,
            "file_read",
            None,
            &values,
            PermissionReviewSource::Recovered,
        );
        assert_eq!(
            review.resources[0].value.as_deref(),
            Some(format!("Exact: {PATH}").as_str())
        );
        assert_eq!(
            review.resources[0].attributes["workdir"],
            format!("Exact: {CWD}")
        );
        assert_eq!(
            review.resources[1].value.as_deref(),
            Some(format!("Filesystem subtree: {CWD}").as_str())
        );
    }

    #[test_case("private.rs"; "relative")]
    #[test_case("./private.rs"; "current_component")]
    #[test_case("nested/../private.rs"; "parent_component")]
    #[test_case("/historical/nonexistent/nested/../private.rs"; "absolute_parent_component")]
    #[test_case(PATH; "absolute")]
    fn path_candidates_keep_verified_lexical_preimages(value: &str) {
        let target = filesystem_target(CWD, PATH);
        let mut candidates = Candidates::new(&[target]);
        candidates.path(value, CWD);
        assert!(candidates.values.contains(PATH));
        assert!(candidates.values.contains(CWD));
        assert!(candidates.wanted.is_empty());
    }

    #[test_case("/historical/nonexistent/nested/../private.rs"; "original_and_normalized_values")]
    fn normalization_adds_a_candidate_without_replacing_the_raw_preimage(raw: &str) {
        let mut candidates = Candidates::new(&[filesystem_target(CWD, PATH)]);
        candidates.want(
            &PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(raw)),
            },
            false,
        );
        candidates.path(raw, CWD);
        assert!(candidates.values.contains(raw));
        assert!(candidates.values.contains(PATH));
        assert!(candidates.wanted.is_empty());
    }

    #[cfg(unix)]
    #[test_case("maki"; "old_root_symlink_is_never_followed")]
    fn lexical_candidates_do_not_resolve_current_symlinks(name: &str) {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("different-checkout");
        fs::create_dir(&destination).unwrap();
        let old = temp.path().join(name);
        symlink(&destination, &old).unwrap();
        let file = old.join("private.rs");
        let physical = destination.join("private.rs");
        let target = filesystem_target(old.to_str().unwrap(), file.to_str().unwrap());
        let mut candidates = Candidates::new(&[target]);
        let unrelated = canonical_json_sha256(&json!(physical));
        candidates.wanted.insert(unrelated.clone());
        candidates.path("nested/../private.rs", old.to_str().unwrap());
        assert!(candidates.values.contains(file.to_str().unwrap()));
        assert!(!candidates.values.contains(physical.to_str().unwrap()));
        assert_eq!(candidates.wanted.len(), 1);
        assert!(candidates.wanted.contains(&unrelated));
    }

    #[test_case(false; "command_budget_does_not_consume_path_budget")]
    #[test_case(true; "path_work_bytes_are_bounded")]
    fn candidate_work_limits_are_independent_and_reported(exhaust_paths: bool) {
        let mut candidates = Candidates::new(&[filesystem_target(CWD, PATH)]);
        if exhaust_paths {
            candidates.paths_work.bytes = MAX_CANDIDATE_WORK_BYTES;
            candidates.path(PATH, CWD);
            assert!(candidates.paths_work.limited);
            assert!(candidates.paths_work.skipped > 0);
            assert!(candidates.values.is_empty());
        } else {
            candidates.commands_work.checks = MAX_COMMAND_CANDIDATE_CHECKS;
            candidates.command(&format!("arbitrary_noise {PATH}"), CWD);
            assert!(candidates.commands_work.limited);
            assert!(candidates.commands_work.skipped > 0);
            assert!(candidates.values.contains(PATH));
            assert!(candidates.wanted.is_empty());
            assert!(!candidates.paths_work.limited);
        }
    }

    #[test_case(false; "normalized_url_exact_origin_subtree")]
    #[test_case(true; "late_url_after_command_budget")]
    fn url_preimages_use_shared_normalization(command_budget_exhausted: bool) {
        let mut raw = record(&json!({}), false);
        raw["rule"]["resources"] = json!([
            {"kind": {"kind": "url"}, "selector": {"match": "digest", "digest": canonical_json_sha256(&json!(URL_EXACT))}},
            {"kind": {"kind": "url"}, "selector": {"match": "url_origin_digest", "digest": canonical_json_sha256(&json!(["url_origin", URL_ORIGIN]))}},
            {"kind": {"kind": "url"}, "selector": {"match": "url_subtree_digest", "digest": canonical_json_sha256(&json!(["url_subtree", URL_SUBTREE]))}}
        ]);
        let target = RepairTarget {
            location: (None, 0),
            record: read_repair_record(&raw, true).unwrap(),
            input: None,
        };
        let mut candidates = Candidates::new(std::slice::from_ref(&target));
        if command_budget_exhausted {
            candidates.commands_work.checks = MAX_COMMAND_CANDIDATE_CHECKS;
            candidates.command(&format!("curl '{URL}'"), CWD);
        } else {
            candidates.insert(URL);
        }
        assert!(candidates.wanted.is_empty());
        assert_eq!(candidates.values.len(), 3);
        assert!(candidates.values.contains(URL_EXACT));
        assert!(candidates.values.contains(URL_ORIGIN));
        assert!(candidates.values.contains(URL_SUBTREE));
        let review = review_from_candidates(
            &target.record.rule,
            "webfetch",
            None,
            &candidates.values.into_iter().collect::<Vec<_>>(),
            PermissionReviewSource::Recovered,
        );
        assert!(
            review
                .resources
                .iter()
                .all(|resource| resource.value.is_some())
        );
    }

    #[test_case("echo safe"; "visible_command_patterns_need_no_candidates")]
    fn command_candidates_require_matching_digests(command: &str) {
        let mut candidates = Candidates::default();
        candidates.want(
            &PermissionResourceSelector::CommandPattern {
                pattern: command.into(),
            },
            false,
        );
        candidates.command(command, CWD);
        assert!(candidates.values.is_empty());
        assert_eq!(candidates.commands_work.checks, 0);
        candidates.want(
            &PermissionResourceSelector::Digest {
                digest: canonical_json_sha256(&json!(command)),
            },
            false,
        );
        candidates.command(&format!("pwd && {command}"), CWD);
        assert_eq!(candidates.values.len(), 1);
        assert!(candidates.values.contains(command));
    }

    #[test_case(MAX_DEPTH + 1; "url_depth_limit_is_reported")]
    fn url_depth_limit_preserves_bounded_origin_recovery(depth: usize) {
        let mut candidates = Candidates::default();
        candidates.want(
            &PermissionResourceSelector::UrlOriginDigest {
                digest: canonical_json_sha256(&json!(["url_origin", URL_ORIGIN])),
            },
            true,
        );
        candidates.want(
            &PermissionResourceSelector::UrlSubtreeDigest {
                digest: canonical_json_sha256(&json!(["url_subtree", URL_SUBTREE])),
            },
            true,
        );
        candidates.insert(&format!("{URL_ORIGIN}{}", "/a".repeat(depth)));
        assert!(candidates.depth_limited);
        assert!(candidates.values.contains(URL_ORIGIN));
        assert_eq!(candidates.wanted.len(), 1);
    }

    #[test_case("view_image"; "image_contract")]
    #[test_case("tool_output"; "output_contract")]
    fn native_contract_lookup_recovers_exact_inputs_without_invocation(name: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let mut database = SessionDatabase::open(&state).unwrap();
        let (contract, _) = native::review_contracts()
            .into_iter()
            .find(|(_, tool)| tool == name)
            .unwrap();
        let input = if name == "view_image" {
            json!({"path": PATH})
        } else {
            json!({"output_id": "historical-output", "limit": 17})
        };
        let mut raw = record(&input, false);
        raw["rule"]["subject"] = json!({"kind": "native", "owner": "caudra", "contract": contract});
        database.global_state_set(STATE_KEY, &json!([raw])).unwrap();
        let mut session = Session::<Message, Value, Value>::new("test", CWD);
        session.push_message(Message(
            json!({"content": [{"type": "tool_use", "name": name, "input": input}]}),
        ));
        database.save(&session, None).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        let (after, report) = prepare(&database, &before, false).unwrap();
        let repaired: Value = serde_json::from_str(after.persistent.as_ref().unwrap()).unwrap();
        assert_eq!(repaired[0]["review"]["tool"], name);
        assert_eq!(repaired[0]["review"]["input"], input);
        assert_eq!(report.exact_input_hashes, 1);
        let unknown = PermissionSubject::Native {
            owner: "caudra".into(),
            contract: "unrecognized-contract".into(),
        };
        assert_eq!(tool_name(&unknown), UNAVAILABLE_TOOL);
    }

    #[test_case(PermissionReviewSource::Approved, true, false, false; "approved_untouched")]
    #[test_case(PermissionReviewSource::Recovered, false, false, false; "retry_opt_in")]
    #[test_case(PermissionReviewSource::Recovered, true, false, true; "retry_incomplete")]
    #[test_case(PermissionReviewSource::Recovered, true, true, false; "complete_untouched")]
    #[test_case(PermissionReviewSource::Unavailable, true, false, true; "retry_unavailable")]
    fn retry_only_recovers_nonapproved_incomplete_reviews(
        source: PermissionReviewSource,
        retry: bool,
        complete: bool,
        changed: bool,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let mut database = SessionDatabase::open(&state).unwrap();
        let input = json!({"filePath": PATH, "limit": 17});
        let mut raw = record(&input, false);
        let rule = read_repair_record(&raw, true).unwrap().rule;
        let candidates = if complete {
            vec![PATH.into()]
        } else {
            Vec::new()
        };
        raw["review"] = serde_json::to_value(review_from_candidates(
            &rule,
            "file_read",
            complete.then_some(&input),
            &candidates,
            source,
        ))
        .unwrap();
        database
            .global_state_set(STATE_KEY, &json!([raw.clone()]))
            .unwrap();
        let mut session = Session::<Message, Value, Value>::new("test", CWD);
        session.push_message(Message(
            json!({"content": [{"name": "file_read", "input": input}]}),
        ));
        database.save(&session, None).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        let (after, report) = prepare(&database, &before, retry).unwrap();
        assert_eq!(report.retried, usize::from(changed));
        if changed {
            let repaired: Value = serde_json::from_str(after.persistent.as_ref().unwrap()).unwrap();
            assert_eq!(repaired[0]["id"], raw["id"]);
            assert_eq!(repaired[0]["rule"], raw["rule"]);
            assert_eq!(repaired[0]["review"]["input"], input);
        } else {
            assert_eq!(after, before);
        }
    }

    #[test_case(PATH; "retain_verified_scope_when_history_is_gone")]
    fn retry_cannot_discard_previously_recovered_labels(path: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let database = SessionDatabase::open(&state).unwrap();
        let mut raw = record(&json!({"filePath": path}), false);
        let rule = read_repair_record(&raw, true).unwrap().rule;
        raw["review"] = serde_json::to_value(review_from_candidates(
            &rule,
            "file_read",
            None,
            &[path.into()],
            PermissionReviewSource::Recovered,
        ))
        .unwrap();
        database.global_state_set(STATE_KEY, &json!([raw])).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        let (after, report) = prepare(&database, &before, true).unwrap();
        assert_eq!(report.retried, 1);
        assert_eq!(report.repaired, 0);
        assert_eq!(after, before);
    }

    #[test_case(MAX_RULES; "one_hash_for_ten_thousand_rules")]
    fn input_index_hashes_once_per_call_and_ordered_pointer_set(copies: usize) {
        let input = json!({"filePath": PATH, "limit": 17});
        let raw = read_repair_record(&record(&input, false), true).unwrap();
        let mut targets = (0..copies)
            .map(|index| RepairTarget {
                location: (Some(index), 0),
                record: raw.clone(),
                input: None,
            })
            .collect::<Vec<_>>();
        for pointers in [
            ["/filePath", "/limit"],
            ["/limit", "/filePath"],
            ["/filePath", "/limit"],
        ] {
            let mut raw = raw.clone();
            raw.rule.arguments = PermissionArgumentConstraint::SelectedDigest {
                pointers: pointers.iter().map(|pointer| (*pointer).into()).collect(),
                digest: selected_input_digest(&input, &pointers).unwrap(),
            };
            targets.push(RepairTarget {
                location: (None, targets.len()),
                record: raw,
                input: None,
            });
        }
        let mut lookup = InputLookup::new(&targets);
        assert!(lookup.take_matches("file_write", &input).is_empty());
        assert_eq!(lookup.exact_hashes, 0);
        let matched = lookup.take_matches("file_read", &input);
        assert_eq!(matched.len(), targets.len());
        assert_eq!(lookup.exact_hashes, 1);
        assert_eq!(lookup.projection_hashes, 2);
        assert!(lookup.take_matches("file_read", &input).is_empty());
        assert_eq!(lookup.exact_hashes, 1);
    }

    #[test_case(false; "duplicate_ids_within_inventory_refused")]
    #[test_case(true; "same_ids_in_distinct_sessions_remain_distinct")]
    fn duplicate_rule_ids_do_not_cross_review_locations(separate_sessions: bool) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let database = SessionDatabase::open(&state).unwrap();
        let mut raw = record(&json!({}), false);
        if !separate_sessions {
            database
                .global_state_set(STATE_KEY, &json!([raw.clone(), raw]))
                .unwrap();
            let before = database.raw_permission_snapshot().unwrap();
            assert!(prepare(&database, &before, true).is_err());
            assert_eq!(database.raw_permission_snapshot().unwrap(), before);
            return;
        }
        raw["rule"]["lifetime"] = json!("conversation");
        raw["rule"]["arguments"] = json!({"constraint": "unconstrained"});
        raw["rule"]["resources"][0]["selector"]["digest"] =
            json!(canonical_json_sha256(&json!(CWD)));
        let rule = read_repair_record(&raw, false).unwrap().rule;
        raw["review"] = serde_json::to_value(review_from_candidates(
            &rule,
            "file_read",
            None,
            &[],
            PermissionReviewSource::Approved,
        ))
        .unwrap();
        let mut second = raw.clone();
        second["review"]["source"] = json!("unavailable");
        let session = |record: Value| RawPermissionSession {
            id: CaudraId::generate(),
            write_version: 0,
            cwd: CWD.into(),
            metadata: json!({"structured_permission_rules": [record], "unknown": true}).to_string(),
        };
        let before = RawPermissionSnapshot {
            persistent: None,
            sessions: vec![session(raw), session(second)],
        };
        let (after, report) = prepare(&database, &before, true).unwrap();
        assert_eq!(report.retried, 1);
        assert_eq!(after.sessions[0], before.sessions[0]);
        let second: Value = serde_json::from_str(&after.sessions[1].metadata).unwrap();
        assert_eq!(
            second["structured_permission_rules"][0]["review"]["source"],
            "recovered"
        );
        let first: Value = serde_json::from_str(&before.sessions[0].metadata).unwrap();
        assert_eq!(
            second["structured_permission_rules"][0]["id"],
            first["structured_permission_rules"][0]["id"]
        );
    }

    #[test_case(false, false; "exact_main_batch")]
    #[test_case(true, true; "selected_subagent_parallel")]
    fn repair_hash_lookup_preserves_denial_and_never_uses_old_labels(
        selected: bool,
        subagent: bool,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let mut database = SessionDatabase::open(&state).unwrap();
        let input = json!({"filePath": PATH, "limit": 12, "offset": 4});
        let raw = record(&input, selected);
        database
            .global_state_set(STATE_KEY, &json!([raw.clone()]))
            .unwrap();
        let payload = if subagent {
            json!({"content": [{"type": "tool_use", "name": "parallel", "input": {"tool_uses": [{"recipient_name": "functions.file_read", "parameters": input}]}}]})
        } else {
            json!({"content": [{"type": "tool_use", "name": "batch", "input": {"tool_calls": [{"tool": "file_read", "parameters": input}]}}]})
        };
        let mut session = Session::<Message, Value, Value>::new("test", CWD);
        if subagent {
            session.set_subagent_messages("child".into(), vec![Message(payload)]);
        } else {
            session.push_message(Message(payload));
        }
        database.save(&session, None).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        let (after, report) = prepare(&database, &before, false).unwrap();
        assert_eq!(database.raw_permission_snapshot().unwrap(), before);
        assert_eq!(report.recovered, 1);
        assert_eq!(report.tool_calls, 2);
        assert_eq!(report.exact_input_hashes, usize::from(!selected));
        assert_eq!(report.selected_projection_hashes, usize::from(selected));
        let repaired: Value = serde_json::from_str(after.persistent.as_ref().unwrap()).unwrap();
        assert_eq!(repaired[0]["rule"], raw["rule"]);
        assert_eq!(repaired[0]["id"], raw["id"]);
        assert_eq!(repaired[0]["unknown_field"], raw["unknown_field"]);
        assert!(!after.persistent.as_ref().unwrap().contains(SECRET));
        assert!(
            repaired[0]["review"]["resources"][0]["value"]
                .as_str()
                .unwrap()
                .contains(PATH)
        );
        let expected_input = if selected {
            json!([
                {"pointer": "/filePath", "present": true, "value": PATH},
                {"pointer": "/limit", "present": true, "value": 12}
            ])
        } else {
            input
        };
        assert_eq!(repaired[0]["review"]["input"], expected_input);
        assert!(!serde_json::to_string(&report).unwrap().contains(PATH));
        drop(database);
        let backup = SessionDatabase::repair_permission_reviews(&state, &before, &after).unwrap();
        assert!(backup.is_file());
        let rules = read_inventory(&state).unwrap();
        let typed: PermissionRuleRecord = serde_json::from_value(repaired[0].clone()).unwrap();
        assert_eq!(rules[0], typed);
    }

    #[test_case("different-input"; "unmatched_input")]
    fn unmatched_history_is_unavailable_not_a_nearest_call(path: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let mut database = SessionDatabase::open(&state).unwrap();
        database
            .global_state_set(
                STATE_KEY,
                &json!([record(&json!({"filePath": PATH}), false)]),
            )
            .unwrap();
        let mut session = Session::<Message, Value, Value>::new("test", CWD);
        session.push_message(Message(
            json!({"content": [{"name": "file_read", "input": {"filePath": path}}]}),
        ));
        database.save(&session, None).unwrap();
        let snapshot = database.raw_permission_snapshot().unwrap();
        let (after, report) = prepare(&database, &snapshot, false).unwrap();
        assert_eq!(report.unavailable_inputs, 1);
        assert_eq!(report.unavailable, 1);
        assert!(!after.persistent.unwrap().contains(SECRET));
    }

    #[test_case(0; "no_nodes")]
    #[test_case(MAX_DEPTH + 1; "depth")]
    fn traversal_bounds_are_reported(depth: usize) {
        let mut limited = false;
        let mut nodes = if depth == 0 { 0 } else { MAX_NODES };
        visit_calls(
            &json!({"name": "shell", "input": {"command": "echo test"}}),
            depth,
            &mut nodes,
            &mut limited,
            &mut |_, _| panic!("limited call visited"),
        );
        assert!(limited);
    }

    #[test_case(true; "oversized_value")]
    #[test_case(false; "candidate_count")]
    fn candidate_bounds_are_reported(oversized: bool) {
        let mut candidates = Candidates::default();
        if oversized {
            candidates.insert(&"x".repeat(MAX_VALUE_BYTES + 1));
            assert!(candidates.value_limited);
            assert!(candidates.values.is_empty());
        } else {
            for index in 0..=MAX_CANDIDATES {
                candidates
                    .wanted
                    .insert(canonical_json_sha256(&json!(index.to_string())));
            }
            for index in 0..=MAX_CANDIDATES {
                candidates.insert(&index.to_string());
            }
            assert!(candidates.count_limited);
            assert_eq!(candidates.values.len(), MAX_CANDIDATES);
        }
    }

    #[test_case("bad"; "invalid_id")]
    fn invalid_records_refuse_without_changes(id: &str) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().into());
        let database = SessionDatabase::open(&state).unwrap();
        let mut raw = record(&json!({}), false);
        raw["id"] = json!(id);
        database.global_state_set(STATE_KEY, &json!([raw])).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        assert!(
            prepare(&database, &before, false).is_err(),
            "{INVALID_RECORDS}"
        );
        assert_eq!(database.raw_permission_snapshot().unwrap(), before);
    }
}
