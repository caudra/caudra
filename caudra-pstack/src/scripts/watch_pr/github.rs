//! Reading a PR from GitHub through `gh`: the commands, their fail-closed JSON parsing, the checks
//! and review-thread fallbacks, and how a PR's context and connected stack are found.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::types::{
    Check, CheckKind, CheckRead, CheckSource, ChecksFastPath, CommitRollup, GitHubReader,
    OpenPullRequest, PrContext, PrNumber, PullRequestFacts, QueryFailure, RawReviewThread,
    Repository, ReviewComment, ReviewDecision, ReviewThread, ReviewThreadPage, RollupPage,
    WatchError, pr_number, string_to_number,
};
use crate::host::{Host, Output};

pub const REVIEW_THREADS_QUERY: &str = "\nquery ReviewThreads($owner: String!, $repo: String!, $pr: Int!, $after: String) {\n  repository(owner: $owner, name: $repo) {\n    pullRequest(number: $pr) {\n      reviewThreads(first: 100, after: $after) {\n        pageInfo {\n          hasNextPage\n          endCursor\n        }\n        nodes {\n          id\n          isResolved\n          comments(first: 10) {\n            nodes {\n              body\n              createdAt\n              path\n              line\n              author { login }\n            }\n          }\n        }\n      }\n    }\n  }\n}\n";
pub const PR_COMMIT_STATUS_QUERY: &str = "\nquery PrCommitStatuses($owner: String!, $repo: String!, $pr: Int!) {\n  repository(owner: $owner, name: $repo) {\n    pullRequest(number: $pr) {\n      commits(last: 50) {\n        nodes {\n          commit {\n            oid\n            statusCheckRollup {\n              state\n            }\n          }\n        }\n      }\n    }\n  }\n}\n";
pub const PR_CHECK_ROLLUP_QUERY: &str = "\nquery PrCheckRollup($owner: String!, $repo: String!, $pr: Int!, $after: String) {\n  repository(owner: $owner, name: $repo) {\n    pullRequest(number: $pr) {\n      commits(last: 1) {\n        nodes {\n          commit {\n            statusCheckRollup {\n              contexts(first: 100, after: $after) {\n                pageInfo {\n                  hasNextPage\n                  endCursor\n                }\n                nodes {\n                  __typename\n                  ... on CheckRun {\n                    name\n                    status\n                    conclusion\n                    detailsUrl\n                  }\n                  ... on StatusContext {\n                    context\n                    state\n                    targetUrl\n                  }\n                }\n              }\n            }\n          }\n        }\n      }\n    }\n  }\n}\n";
pub const PULL_REQUEST_FIELDS: &str = "mergeable,mergeStateStatus,reviewDecision,headRefOid,headRefName,baseRefName,state,mergedAt,isDraft";
pub const CHECK_FIELDS: &str = "name,state,description,link,workflow,bucket";
pub const CURRENT_PR_FIELDS: &str = "number,url";
pub const OPEN_PR_FIELDS: &str = "number,headRefName,baseRefName";
pub const CODE_REVIEW_GATE: &str = "Code Review Gate";
const OPEN_PR_LIMIT: &str = "300";
const FAST_PATH_CODES: [i32; 3] = [0, 1, 8];
const FIRST_LINE_LIMIT: usize = 240;
const GITHUB_HOST: &str = "github.com";
const HTTPS_SCHEME: &str = "https";
const HTTPS_PORT: &str = "443";
const SCP_REMOTE_PREFIX: &str = "git@github.com:";
const SSH_REMOTE_PREFIX: &str = "ssh://git@github.com/";
const GITHUB_ORIGIN: &str = "https://github.com/";
const GIT_SUFFIX: &str = ".git";
const PULL_SEGMENT: &str = "pull";
const INVALID_URL: &str = "Invalid URL";
const NOT_CANONICAL_PULL_URL: &str = "not a canonical GitHub pull URL";
const PR_NUMBER_INVALID: &str = "PR number must be a positive integer";
const BUGBOT: &str = "bugbot";
const CURSOR_AUTHOR: &str = "cursor";
const CURSOR_BUGBOT_TOKENS: [&str; 5] = [
    "bugbot",
    "cursor_automation_id",
    "agentic security review",
    "description start",
    "severity",
];
const CHECK_RUN: &str = "CheckRun";
const STATUS_CONTEXT: &str = "StatusContext";

static PASS_KEYS: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        Regex::new(r"RUN_ID:\s*([a-zA-Z0-9_.:-]+)").expect("literal"),
        Regex::new(r"CURSOR_AUTOMATION_ID:\s*([a-zA-Z0-9_.:-]+)").expect("literal"),
    ]
});

type Field<'v> = Option<&'v Value>;

pub fn first_line(value: &str) -> String {
    value
        .trim()
        .split('\n')
        .next()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or_default()
        .chars()
        .take(FIRST_LINE_LIMIT)
        .collect()
}

fn query_error<T>(failure: QueryFailure) -> Result<T, WatchError> {
    Err(WatchError::Query(failure))
}

fn missing<T>(path: &str, value: Field) -> Result<T, WatchError> {
    query_error(match value {
        None => QueryFailure::MissingKey {
            detail: format!("missing {path}"),
            raw_value: None,
        },
        Some(value) => QueryFailure::MissingKey {
            detail: format!("invalid {path}: {value}"),
            raw_value: Some(value.to_string()),
        },
    })
}

fn record<'v>(value: Field<'v>, path: &str) -> Result<&'v Map<String, Value>, WatchError> {
    match value {
        Some(Value::Object(object)) => Ok(object),
        _ => missing(path, value),
    }
}

fn list<'v>(value: Field<'v>, path: &str) -> Result<&'v [Value], WatchError> {
    match value {
        Some(Value::Array(items)) => Ok(items),
        _ => missing(path, value),
    }
}

fn at<'v>(value: Field<'v>, path: &[&str]) -> Result<&'v Value, WatchError> {
    let joined = path.join(".");
    let mut current = value;
    for key in path {
        let object = record(current, &joined)?;
        let Some(next) = object.get(*key) else {
            return missing(&joined, None);
        };
        current = Some(next);
    }
    current.map_or_else(|| missing(&joined, None), Ok)
}

fn string(value: Field, path: &str) -> Result<String, WatchError> {
    match value {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => missing(path, value),
    }
}

fn optional_string(value: Field, path: &str) -> Result<Option<String>, WatchError> {
    match value {
        Some(Value::Null) => Ok(None),
        _ => string(value, path).map(Some),
    }
}

fn boolean(value: Field, path: &str) -> Result<bool, WatchError> {
    match value {
        Some(Value::Bool(flag)) => Ok(*flag),
        _ => missing(path, value),
    }
}

fn enum_value<T: DeserializeOwned>(value: Field, path: &str) -> Result<T, WatchError> {
    match value {
        Some(text @ Value::String(_)) => {
            serde_json::from_value(text.clone()).or_else(|_| missing(path, value))
        }
        _ => missing(path, value),
    }
}

fn nullable_enum<T: DeserializeOwned>(value: Field, path: &str) -> Result<Option<T>, WatchError> {
    match value {
        Some(Value::Null) => Ok(None),
        _ => enum_value(value, path).map(Some),
    }
}

/// `gh pr view` reports no review decision as `""`, not null. Only this field does it, so the
/// normalization stays here, where it cannot hide a genuinely unexpected value elsewhere.
fn review_decision(value: Field) -> Result<Option<ReviewDecision>, WatchError> {
    let path = "pull request.reviewDecision";
    match value {
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        _ => nullable_enum(value, path),
    }
}

fn json_pr_number(value: Field, label: &str) -> Result<PrNumber, WatchError> {
    value
        .and_then(Value::as_f64)
        .and_then(pr_number)
        .ok_or_else(|| WatchError::Fatal(format!("{label} must be a positive integer")))
}

fn text_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

enum UrlProblem {
    Unparseable,
    NotCanonical,
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// The path of a plain `https://github.com/...` URL: no credentials, no explicit non-default
/// port, no query, no fragment.
fn github_path(value: &str) -> Result<&str, UrlProblem> {
    let (scheme, rest) = value.split_once(':').ok_or(UrlProblem::Unparseable)?;
    let mut scheme_chars = scheme.chars();
    let valid_scheme = scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid_scheme {
        return Err(UrlProblem::Unparseable);
    }
    if !scheme.eq_ignore_ascii_case(HTTPS_SCHEME) {
        return Err(UrlProblem::NotCanonical);
    }
    let rest = rest.strip_prefix("//").ok_or(UrlProblem::NotCanonical)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, rest) = rest.split_at(authority_end);
    if authority.is_empty() {
        return Err(UrlProblem::Unparseable);
    }
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
    if authority.contains('@')
        || !host.eq_ignore_ascii_case(GITHUB_HOST)
        || !(port.is_empty() || port == HTTPS_PORT)
    {
        return Err(UrlProblem::NotCanonical);
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !fragment.is_empty() || !query.is_empty() {
        return Err(UrlProblem::NotCanonical);
    }
    Ok(path)
}

pub fn parse_remote(value: &str) -> Option<Repository> {
    let trimmed = value.trim();
    let normalized = [SCP_REMOTE_PREFIX, SSH_REMOTE_PREFIX]
        .into_iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
        .map_or_else(
            || trimmed.to_owned(),
            |path| format!("{GITHUB_ORIGIN}{path}"),
        );
    let path = github_path(&normalized).ok()?;
    match segments(path.strip_suffix(GIT_SUFFIX).unwrap_or(path)).as_slice() {
        [owner, repo] => Some(Repository {
            owner: (*owner).to_owned(),
            repo: (*repo).to_owned(),
        }),
        _ => None,
    }
}

pub fn parse_pr_url(value: &str) -> Result<PrContext, WatchError> {
    let parsed = match github_path(value) {
        Ok(path) => match segments(path).as_slice() {
            [owner, repo, segment, number] if *segment == PULL_SEGMENT => {
                pr_number(string_to_number(number))
                    .map(|number| PrContext {
                        owner: (*owner).to_owned(),
                        repo: (*repo).to_owned(),
                        number,
                    })
                    .ok_or(PR_NUMBER_INVALID)
            }
            _ => Err(NOT_CANONICAL_PULL_URL),
        },
        Err(UrlProblem::Unparseable) => Err(INVALID_URL),
        Err(UrlProblem::NotCanonical) => Err(NOT_CANONICAL_PULL_URL),
    };
    parsed.or_else(|reason| {
        query_error(QueryFailure::InvalidContextUrl {
            raw_value: value.to_owned(),
            detail: format!("could not infer owner/repo from PR URL: {value} ({reason})"),
        })
    })
}

struct CheckDetails {
    name: String,
    description: String,
    link: String,
    workflow: String,
}

impl CheckDetails {
    fn read(object: &Map<String, Value>, name_key: &str) -> Result<Self, WatchError> {
        Ok(Self {
            name: string(object.get(name_key), name_key)?,
            description: text_field(object, "description").unwrap_or_default(),
            link: text_field(object, "link")
                .or_else(|| text_field(object, "detailsUrl"))
                .unwrap_or_default(),
            workflow: text_field(object, "workflow").unwrap_or_default(),
        })
    }

    fn check(self, kind: CheckKind, reported_state: &str) -> Check {
        Check {
            name: self.name,
            description: self.description,
            link: self.link,
            workflow: self.workflow,
            kind,
            reported_state: reported_state.to_owned(),
        }
    }

    /// Every read path classifies the owner-approval gate here, so the rule has one home. A gate
    /// that is pending or waits on an owner approval must not count as pending or failed, or the
    /// watcher waits on a human.
    fn gate_or(self, kind: CheckKind, reported_state: &str) -> Check {
        let kind = if self.name == CODE_REVIEW_GATE {
            CheckKind::CodeReviewGate
        } else {
            kind
        };
        self.check(kind, reported_state)
    }
}

pub fn parse_fast_check(value: &Value) -> Result<Check, WatchError> {
    let object = record(Some(value), "check")?;
    let details = CheckDetails::read(object, "name")?;
    let state = string(object.get("state"), "check.state")?.to_uppercase();
    let bucket = string(object.get("bucket"), "check.bucket")?;
    Ok(match (state.as_str(), bucket.as_str()) {
        ("ACTION_REQUIRED", _) => details.gate_or(CheckKind::Failed, &state),
        ("FAILURE" | "ERROR", _) | (_, "fail") => details.check(CheckKind::Failed, &state),
        (_, "pending") => details.gate_or(CheckKind::Pending, &state),
        (_, "pass") => details.check(CheckKind::Passed, &state),
        (_, "skipping") => details.check(CheckKind::Skipped, &state),
        _ => details.check(CheckKind::Failed, &state),
    })
}

fn upper_text(object: &Map<String, Value>, key: &str) -> String {
    text_field(object, key)
        .map(|text| text.to_uppercase())
        .unwrap_or_default()
}

pub fn map_rollup_node(value: &Value) -> Result<Option<Check>, WatchError> {
    let object = record(Some(value), "rollup node")?;
    let typename = object.get("__typename").and_then(Value::as_str);
    let name_key = match typename {
        Some(CHECK_RUN) => "name",
        Some(STATUS_CONTEXT) => "context",
        _ => return Ok(None),
    };
    let mut details = CheckDetails::read(object, name_key)?;
    if let Some(target) = text_field(object, "targetUrl") {
        details.link = target;
    }
    if typename == Some(CHECK_RUN) {
        let conclusion = upper_text(object, "conclusion");
        return Ok(Some(if upper_text(object, "status") != "COMPLETED" {
            details.gate_or(CheckKind::Pending, "PENDING")
        } else {
            match conclusion.as_str() {
                "SUCCESS" => details.check(CheckKind::Passed, "SUCCESS"),
                "NEUTRAL" | "SKIPPED" => details.check(CheckKind::Skipped, &conclusion),
                "ACTION_REQUIRED" => details.gate_or(CheckKind::Failed, &conclusion),
                _ => details.check(CheckKind::Failed, "FAILURE"),
            }
        }));
    }
    let state = upper_text(object, "state");
    Ok(Some(match state.as_str() {
        "PENDING" | "EXPECTED" => details.gate_or(CheckKind::Pending, "PENDING"),
        "SUCCESS" => details.check(CheckKind::Passed, &state),
        "" => details.check(CheckKind::Failed, "FAILURE"),
        _ => details.check(CheckKind::Failed, &state),
    }))
}

fn parse_comment(value: &Value) -> Result<ReviewComment, WatchError> {
    let object = record(Some(value), "review comment")?;
    let author = match object.get("author") {
        Some(Value::Null) => None,
        author => Some(record(author, "review comment.author")?),
    };
    Ok(ReviewComment {
        author_login: author
            .map(|author| optional_string(author.get("login"), "review comment.author.login"))
            .transpose()?
            .flatten(),
        body: string(object.get("body"), "review comment.body")?,
        path: optional_string(object.get("path"), "review comment.path")?,
        line: match object.get("line") {
            Some(Value::Null) => None,
            line => match line.and_then(Value::as_f64) {
                Some(number) if number.fract() == 0.0 => Some(number as i64),
                _ => return missing("review comment.line", line),
            },
        },
        created_at: string(object.get("createdAt"), "review comment.createdAt")?,
    })
}

fn is_bugbot(comment: Option<&ReviewComment>) -> bool {
    let Some(comment) = comment else {
        return false;
    };
    let author = comment
        .author_login
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    let body = comment.body.to_lowercase();
    author.contains(BUGBOT)
        || (author == CURSOR_AUTHOR
            && CURSOR_BUGBOT_TOKENS
                .iter()
                .any(|token| body.contains(token)))
}

fn pass_key(comment: Option<&ReviewComment>) -> Option<&str> {
    let body = &comment?.body;
    PASS_KEYS.iter().find_map(|pattern| {
        pattern
            .captures(body)
            .and_then(|captures| captures.get(1))
            .map(|key| key.as_str())
    })
}

/// A page's cursor when another page follows, which `hasNextPage` must state outright.
fn next_cursor(page_info: Field, path: &str) -> Result<Option<String>, WatchError> {
    let page = record(page_info, path)?;
    let has_next_page = boolean(page.get("hasNextPage"), &format!("{path}.hasNextPage"))?;
    let cursor = optional_string(page.get("endCursor"), &format!("{path}.endCursor"))?;
    Ok(cursor.filter(|cursor| has_next_page && !cursor.is_empty()))
}

pub fn parse_review_thread_page(value: &Value) -> Result<ReviewThreadPage, WatchError> {
    let review_threads = record(
        Some(at(
            Some(value),
            &["data", "repository", "pullRequest", "reviewThreads"],
        )?),
        "reviewThreads",
    )?;
    let mut threads = Vec::new();
    for node in list(review_threads.get("nodes"), "reviewThreads.nodes")? {
        let thread = record(Some(node), "review thread")?;
        let resolved = boolean(thread.get("isResolved"), "review thread.isResolved")?;
        let comments = list(
            Some(at(Some(node), &["comments", "nodes"])?),
            "review thread.comments.nodes",
        )?;
        threads.push(RawReviewThread {
            id: string(thread.get("id"), "review thread.id")?,
            first_comment: comments.first().map(parse_comment).transpose()?,
            resolved,
        });
    }
    Ok(ReviewThreadPage {
        threads,
        end_cursor: next_cursor(review_threads.get("pageInfo"), "reviewThreads.pageInfo")?,
    })
}

pub fn build_review_threads(threads: Vec<RawReviewThread>) -> Vec<ReviewThread> {
    let mut keys = HashSet::new();
    let mut keyless = false;
    for thread in &threads {
        let comment = thread.first_comment.as_ref();
        if !is_bugbot(comment) {
            continue;
        }
        match pass_key(comment) {
            Some(key) => {
                keys.insert(key.to_owned());
            }
            None => keyless = true,
        }
    }
    let passes = if keys.is_empty() {
        usize::from(keyless)
    } else {
        keys.len()
    };
    threads
        .into_iter()
        .filter(|thread| !thread.resolved)
        .map(|thread| ReviewThread {
            is_bugbot: is_bugbot(thread.first_comment.as_ref()),
            id: thread.id,
            first_comment: thread.first_comment,
            bugbot_review_passes: passes,
        })
        .collect()
}

pub fn parse_pull_request(
    value: &Value,
    context: &PrContext,
) -> Result<PullRequestFacts, WatchError> {
    let object = record(Some(value), "pull request")?;
    let is_draft = boolean(object.get("isDraft"), "pull request.isDraft")?;
    Ok(PullRequestFacts {
        context: context.clone(),
        mergeable: enum_value(object.get("mergeable"), "pull request.mergeable")?,
        merge_state_status: enum_value(
            object.get("mergeStateStatus"),
            "pull request.mergeStateStatus",
        )?,
        review_decision: review_decision(object.get("reviewDecision"))?,
        head_ref_oid: optional_string(object.get("headRefOid"), "pull request.headRefOid")?,
        head_ref_name: string(object.get("headRefName"), "pull request.headRefName")?,
        base_ref_name: string(object.get("baseRefName"), "pull request.baseRefName")?,
        state: enum_value(object.get("state"), "pull request.state")?,
        merged_at: optional_string(object.get("mergedAt"), "pull request.mergedAt")?,
        is_draft,
    })
}

pub fn graphql_args(query: &str, context: &PrContext, after: Option<&str>) -> Vec<String> {
    let mut argv = [
        "gh",
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-f",
        &format!("owner={}", context.owner),
        "-f",
        &format!("repo={}", context.repo),
        "-F",
        &format!("pr={}", context.number),
    ]
    .map(str::to_owned)
    .to_vec();
    if let Some(after) = after {
        argv.extend(["-f".to_owned(), format!("after={after}")]);
    }
    argv
}

pub fn repo_slug(context: &PrContext) -> String {
    format!("{}/{}", context.owner, context.repo)
}

pub fn pull_request_args(context: &PrContext) -> Vec<String> {
    let number = context.number.to_string();
    let slug = repo_slug(context);
    [
        "gh",
        "pr",
        "view",
        &number,
        "--repo",
        &slug,
        "--json",
        PULL_REQUEST_FIELDS,
    ]
    .map(str::to_owned)
    .to_vec()
}

pub fn checks_args(context: &PrContext) -> Vec<String> {
    let number = context.number.to_string();
    let slug = repo_slug(context);
    [
        "gh",
        "pr",
        "checks",
        &number,
        "--repo",
        &slug,
        "--json",
        CHECK_FIELDS,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// [`GitHubReader`] over the `gh` and `git` commands the host runs.
pub struct GhReader<'a> {
    host: &'a dyn Host,
}

impl<'a> GhReader<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        Self { host }
    }

    fn run(&self, argv: &[String]) -> Result<Output, WatchError> {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.host.run(&argv, None).map_err(|error| {
            WatchError::Fatal(format!(
                "{}: {error}",
                argv.first().copied().unwrap_or_default()
            ))
        })
    }

    fn run_json(&self, argv: &[String]) -> Result<Value, WatchError> {
        let result = self.run(argv)?;
        let label = argv.join(" ");
        if result.code != 0 {
            let line = first_line(&result.stderr);
            return query_error(QueryFailure::CommandExit {
                code: result.code,
                detail: if line.is_empty() {
                    format!("{label} exited {}", result.code)
                } else {
                    line
                },
            });
        }
        parse_json(&result.stdout, &label)
    }
}

fn parse_json(text: &str, label: &str) -> Result<Value, WatchError> {
    serde_json::from_str(text).or_else(|error| {
        query_error(QueryFailure::JsonParse {
            detail: format!("{label}: {error}"),
        })
    })
}

fn commit_nodes(value: &Value) -> Result<&[Value], WatchError> {
    list(
        Some(at(
            Some(value),
            &["data", "repository", "pullRequest", "commits", "nodes"],
        )?),
        "commits.nodes",
    )
}

impl GitHubReader for GhReader<'_> {
    fn origin_repo(&self) -> Result<Option<Repository>, WatchError> {
        let result = self.run(&["git", "remote", "get-url", "origin"].map(str::to_owned))?;
        Ok((result.code == 0)
            .then(|| parse_remote(&result.stdout))
            .flatten())
    }

    fn current_pr(&self, pr: Option<PrNumber>) -> Result<PrContext, WatchError> {
        let mut argv = ["gh", "pr", "view"].map(str::to_owned).to_vec();
        argv.extend(pr.map(|pr| pr.to_string()));
        argv.extend(["--json", CURRENT_PR_FIELDS].map(str::to_owned));
        let value = self.run_json(&argv)?;
        let object = record(Some(&value), "current PR")?;
        let parsed = parse_pr_url(&string(object.get("url"), "current PR.url")?)?;
        let number = match pr {
            Some(pr) => pr,
            None => json_pr_number(object.get("number"), "current PR.number")?,
        };
        Ok(PrContext { number, ..parsed })
    }

    fn pull_request(&self, context: &PrContext) -> Result<PullRequestFacts, WatchError> {
        parse_pull_request(&self.run_json(&pull_request_args(context))?, context)
    }

    fn open_pull_requests(&self, context: &PrContext) -> Result<Vec<OpenPullRequest>, WatchError> {
        let slug = repo_slug(context);
        let argv = [
            "gh",
            "pr",
            "list",
            "--repo",
            &slug,
            "--state",
            "open",
            "--limit",
            OPEN_PR_LIMIT,
            "--json",
            OPEN_PR_FIELDS,
        ]
        .map(str::to_owned);
        let value = self.run_json(&argv)?;
        list(Some(&value), "open PRs")?
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let path = format!("open PRs[{index}]");
                let object = record(Some(item), &path)?;
                Ok(OpenPullRequest {
                    number: json_pr_number(object.get("number"), &format!("{path}.number"))?,
                    head_ref_name: string(
                        object.get("headRefName"),
                        &format!("{path}.headRefName"),
                    )?,
                    base_ref_name: string(
                        object.get("baseRefName"),
                        &format!("{path}.baseRefName"),
                    )?,
                })
            })
            .collect()
    }

    fn checks_fast_path(&self, context: &PrContext) -> Result<ChecksFastPath, WatchError> {
        let result = self.run(&checks_args(context))?;
        if FAST_PATH_CODES.contains(&result.code)
            && !result.stdout.trim().is_empty()
            && let Ok(Value::Array(items)) = parse_json(&result.stdout, "gh pr checks")
            && let Ok(checks) = items.iter().map(parse_fast_check).collect()
        {
            return Ok(ChecksFastPath::Checks(checks));
        }
        Ok(ChecksFastPath::Unusable {
            exit_code: result.code,
            stderr: result.stderr,
        })
    }

    fn check_rollup_page(
        &self,
        context: &PrContext,
        after: Option<&str>,
    ) -> Result<RollupPage, WatchError> {
        let value = self.run_json(&graphql_args(PR_CHECK_ROLLUP_QUERY, context, after))?;
        let Some(last) = commit_nodes(&value)?.last() else {
            return Ok(RollupPage::default());
        };
        let commit = at(Some(last), &["commit"])?;
        if record(Some(commit), "commit")?.get("statusCheckRollup") == Some(&Value::Null) {
            return Ok(RollupPage::default());
        }
        let contexts = record(
            Some(at(Some(commit), &["statusCheckRollup", "contexts"])?),
            "contexts",
        )?;
        let mut checks = Vec::new();
        for node in list(contexts.get("nodes"), "contexts.nodes")? {
            checks.extend(map_rollup_node(node)?);
        }
        Ok(RollupPage {
            checks,
            end_cursor: next_cursor(contexts.get("pageInfo"), "contexts.pageInfo")?,
        })
    }

    fn review_thread_page(
        &self,
        context: &PrContext,
        after: Option<&str>,
    ) -> Result<ReviewThreadPage, WatchError> {
        parse_review_thread_page(&self.run_json(&graphql_args(
            REVIEW_THREADS_QUERY,
            context,
            after,
        ))?)
    }

    fn commit_rollups(&self, context: &PrContext) -> Result<Vec<CommitRollup>, WatchError> {
        let value = self.run_json(&graphql_args(PR_COMMIT_STATUS_QUERY, context, None))?;
        commit_nodes(&value)?
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let path = format!("commits[{index}]");
                let commit = record(
                    Some(at(Some(item), &["commit"])?),
                    &format!("{path}.commit"),
                )?;
                let oid = string(commit.get("oid"), &format!("{path}.oid"))?;
                let state = match commit.get("statusCheckRollup") {
                    Some(Value::Null) => None,
                    rollup => nullable_enum(
                        Some(at(rollup, &["state"])?),
                        &format!("{path}.statusCheckRollup.state"),
                    )?,
                };
                Ok(CommitRollup { oid, state })
            })
            .collect()
    }
}

pub fn resolve_checks(
    reader: &dyn GitHubReader,
    context: &PrContext,
) -> Result<CheckRead, WatchError> {
    let fast = reader.checks_fast_path(context)?;
    if let ChecksFastPath::Checks(checks) = &fast
        && !checks.is_empty()
    {
        return Ok(CheckRead {
            source: CheckSource::GhPrChecks,
            checks: checks.clone(),
        });
    }
    let mut checks = Vec::new();
    let mut after = None;
    loop {
        let page = reader.check_rollup_page(context, after.as_deref())?;
        checks.extend(page.checks);
        after = page.end_cursor;
        if after.is_none() {
            break;
        }
    }
    if !checks.is_empty() {
        return Ok(CheckRead {
            source: CheckSource::GraphqlRollup,
            checks,
        });
    }
    let suffix = match fast {
        ChecksFastPath::Unusable { exit_code, stderr } => {
            let line = first_line(&stderr);
            let detail = if line.is_empty() {
                String::new()
            } else {
                format!("; {line}")
            };
            format!("fast path exit={exit_code}; GraphQL rollup was empty{detail}")
        }
        ChecksFastPath::Checks(_) => "fast path and GraphQL rollup were empty".to_owned(),
    };
    query_error(QueryFailure::ChecksUnavailable {
        detail: format!("could not read PR checks: {suffix}"),
    })
}

pub fn resolve_review_threads(
    reader: &dyn GitHubReader,
    context: &PrContext,
) -> Result<Vec<ReviewThread>, WatchError> {
    let mut threads = Vec::new();
    let mut after = None;
    loop {
        let page = reader.review_thread_page(context, after.as_deref())?;
        threads.extend(page.threads);
        after = page.end_cursor;
        if after.is_none() {
            break;
        }
    }
    Ok(build_review_threads(threads))
}

pub fn resolve_context(
    reader: &dyn GitHubReader,
    owner: Option<&str>,
    repo: Option<&str>,
    pr: Option<PrNumber>,
) -> Result<PrContext, WatchError> {
    let context = |inferred_owner: &str, inferred_repo: &str, number| PrContext {
        owner: owner.unwrap_or(inferred_owner).to_owned(),
        repo: repo.unwrap_or(inferred_repo).to_owned(),
        number,
    };
    if let (Some(pr), Some(owner), Some(repo)) = (pr, owner, repo) {
        return Ok(context(owner, repo, pr));
    }
    if let Some(pr) = pr
        && let Some(origin) = reader.origin_repo()?
    {
        return Ok(context(&origin.owner, &origin.repo, pr));
    }
    let inferred = reader.current_pr(pr)?;
    Ok(context(
        &inferred.owner,
        &inferred.repo,
        pr.unwrap_or(inferred.number),
    ))
}

/// The connected open stack around `context`, bottom to top: its ancestors by base branch, then
/// every descendant depth first, siblings by PR number.
pub fn order_stack(context: &PrContext, open: &[OpenPullRequest]) -> Vec<PrContext> {
    let by_head: HashMap<&str, &OpenPullRequest> = open
        .iter()
        .map(|pr| (pr.head_ref_name.as_str(), pr))
        .collect();
    let mut children: HashMap<&str, Vec<&OpenPullRequest>> = HashMap::new();
    for pr in open {
        children
            .entry(pr.base_ref_name.as_str())
            .or_default()
            .push(pr);
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(|pr| pr.number);
    }
    let Some(start) = open.iter().rev().find(|pr| pr.number == context.number) else {
        return vec![context.clone()];
    };
    let mut seen = HashSet::from([start.number]);
    let mut down = Vec::new();
    let mut current = start;
    while let Some(&parent) = by_head.get(current.base_ref_name.as_str()) {
        if !seen.insert(parent.number) {
            break;
        }
        down.push(parent);
        current = parent;
    }
    let mut up = Vec::new();
    visit_children(start, &children, &mut seen, &mut up);
    down.into_iter()
        .rev()
        .chain([start])
        .chain(up)
        .map(|pr| context.with_number(pr.number))
        .collect()
}

fn visit_children<'p>(
    parent: &OpenPullRequest,
    children: &HashMap<&str, Vec<&'p OpenPullRequest>>,
    seen: &mut HashSet<PrNumber>,
    up: &mut Vec<&'p OpenPullRequest>,
) {
    for child in children
        .get(parent.head_ref_name.as_str())
        .into_iter()
        .flatten()
    {
        if seen.insert(child.number) {
            up.push(child);
            visit_children(child, children, seen, up);
        }
    }
}

pub fn discover_stack(
    reader: &dyn GitHubReader,
    context: &PrContext,
) -> Result<Vec<PrContext>, WatchError> {
    Ok(order_stack(context, &reader.open_pull_requests(context)?))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::super::fakes::{
        CI, FakeReader, context, failed_check, fake_host, passing_check, pending_check,
    };
    use super::super::types::{MergeStateStatus, RawReviewThread, ReviewComment};
    use super::*;

    const PR: PrNumber = 42;
    const FAST: &str = "fast";
    const CHECKS_EMPTY: &str = "could not read PR checks: fast path exit=8; GraphQL rollup was empty; credential cannot read checks";

    fn names(checks: &[Check]) -> Vec<&str> {
        checks.iter().map(|check| check.name.as_str()).collect()
    }

    fn ids(threads: &[ReviewThread]) -> Vec<&str> {
        threads.iter().map(|thread| thread.id.as_str()).collect()
    }

    fn page(checks: Vec<Check>, end_cursor: Option<&str>) -> RollupPage {
        RollupPage {
            checks,
            end_cursor: end_cursor.map(str::to_owned),
        }
    }

    fn unusable(stderr: &str) -> ChecksFastPath {
        ChecksFastPath::Unusable {
            exit_code: 8,
            stderr: stderr.to_owned(),
        }
    }

    #[test]
    fn uses_a_non_empty_fast_path_result_without_a_rollup_query() {
        let reader = FakeReader {
            fast_path: ChecksFastPath::Checks(vec![passing_check(FAST)]),
            ..FakeReader::default()
        };
        let read = resolve_checks(&reader, &context(PR)).expect("checks");
        assert_eq!(read.source, CheckSource::GhPrChecks);
        assert_eq!(names(&read.checks), [FAST]);
        assert_eq!(reader.calls(), ["checksFastPath"]);
    }

    #[test]
    fn paginates_graphql_when_the_fast_path_is_unusable() {
        let reader = FakeReader {
            fast_path: unusable(""),
            rollup_pages: vec![
                page(vec![passing_check("first")], Some("next")),
                page(vec![failed_check("second")], None),
            ],
            ..FakeReader::default()
        };
        let read = resolve_checks(&reader, &context(PR)).expect("checks");
        assert_eq!(read.source, CheckSource::GraphqlRollup);
        assert_eq!(names(&read.checks), ["first", "second"]);
        assert_eq!(
            reader.calls(),
            [
                "checksFastPath",
                "checkRollupPage:null",
                "checkRollupPage:next"
            ]
        );
    }

    #[test]
    fn falls_back_when_valid_fast_path_json_represented_an_empty_list() {
        let reader = FakeReader {
            fast_path: ChecksFastPath::Checks(Vec::new()),
            rollup_pages: vec![page(vec![pending_check("fallback")], None)],
            ..FakeReader::default()
        };
        let read = resolve_checks(&reader, &context(PR)).expect("checks");
        assert_eq!(names(&read.checks), ["fallback"]);
        assert_eq!(reader.calls(), ["checksFastPath", "checkRollupPage:null"]);
    }

    #[test]
    fn fails_closed_when_both_paths_are_empty() {
        let reader = FakeReader {
            fast_path: unusable("credential cannot read checks"),
            ..FakeReader::default()
        };
        let error = resolve_checks(&reader, &context(PR)).expect_err("checks are unavailable");
        assert!(matches!(
            error,
            WatchError::Query(QueryFailure::ChecksUnavailable { ref detail }) if detail == CHECKS_EMPTY
        ));
        assert_eq!(reader.calls(), ["checksFastPath", "checkRollupPage:null"]);
    }

    #[test_case("IN_PROGRESS", None, CheckKind::Pending, "PENDING" ; "in_progress_is_pending")]
    #[test_case("COMPLETED", Some("SUCCESS"), CheckKind::Passed, "SUCCESS" ; "success_passes")]
    #[test_case("COMPLETED", Some("NEUTRAL"), CheckKind::Skipped, "NEUTRAL" ; "neutral_skips")]
    #[test_case("COMPLETED", Some("SKIPPED"), CheckKind::Skipped, "SKIPPED" ; "skipped_skips")]
    #[test_case("COMPLETED", Some("ACTION_REQUIRED"), CheckKind::Failed, "ACTION_REQUIRED" ; "action_required_fails")]
    #[test_case("COMPLETED", Some("TIMED_OUT"), CheckKind::Failed, "FAILURE" ; "timed_out_fails")]
    #[test_case("COMPLETED", Some("FUTURE_VALUE"), CheckKind::Failed, "FAILURE" ; "unknown_conclusion_fails_closed")]
    fn maps_check_run_states_fail_closed(
        status: &str,
        conclusion: Option<&str>,
        kind: CheckKind,
        reported_state: &str,
    ) {
        let check = map_rollup_node(&json!({
            "__typename": "CheckRun",
            "name": CI,
            "status": status,
            "conclusion": conclusion,
        }))
        .expect("node")
        .expect("check");
        assert_eq!(
            (check.kind, check.reported_state.as_str()),
            (kind, reported_state)
        );
    }

    #[test]
    fn classifies_an_in_progress_code_review_gate_from_the_rollup_as_the_gate() {
        for node in [
            json!({"__typename": "CheckRun", "name": CODE_REVIEW_GATE, "status": "IN_PROGRESS", "conclusion": null}),
            json!({"__typename": "StatusContext", "context": CODE_REVIEW_GATE, "state": "PENDING"}),
        ] {
            let check = map_rollup_node(&node).expect("node").expect("check");
            assert_eq!(check.kind, CheckKind::CodeReviewGate);
        }
    }

    #[test]
    fn classifies_a_code_review_gate_waiting_on_an_owner_approval_as_the_gate() {
        let check = map_rollup_node(&json!({
            "__typename": "CheckRun",
            "name": CODE_REVIEW_GATE,
            "status": "COMPLETED",
            "conclusion": "ACTION_REQUIRED",
            "description": "1 of 2 policies",
        }))
        .expect("node")
        .expect("check");
        assert_eq!(
            (check.kind, check.reported_state.as_str()),
            (CheckKind::CodeReviewGate, "ACTION_REQUIRED")
        );
    }

    #[test_case(CODE_REVIEW_GATE, CheckKind::CodeReviewGate ; "gate")]
    #[test_case(CI, CheckKind::Failed ; "ordinary_check")]
    fn classifies_a_fast_path_action_required_check(name: &str, kind: CheckKind) {
        let check =
            parse_fast_check(&json!({"name": name, "state": "ACTION_REQUIRED", "bucket": "fail"}))
                .expect("check");
        assert_eq!(
            (check.kind, check.reported_state.as_str()),
            (kind, "ACTION_REQUIRED")
        );
    }

    #[test_case("EXPECTED", CheckKind::Pending, "PENDING" ; "expected_is_pending")]
    #[test_case("FUTURE_VALUE", CheckKind::Failed, "FUTURE_VALUE" ; "unknown_state_fails")]
    fn maps_status_context_states(state: &str, kind: CheckKind, reported_state: &str) {
        let check =
            map_rollup_node(&json!({"__typename": "StatusContext", "context": CI, "state": state}))
                .expect("node")
                .expect("check");
        assert_eq!(
            (check.kind, check.reported_state.as_str()),
            (kind, reported_state)
        );
    }

    #[test]
    fn drops_unknown_typenames() {
        assert_eq!(
            map_rollup_node(&json!({"__typename": "FutureNode"})).expect("node"),
            None
        );
    }

    fn raw_pull_request(key: &str, value: Value) -> Value {
        let mut raw = json!({
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "headRefOid": "head",
            "headRefName": "feature",
            "baseRefName": "main",
            "state": "OPEN",
            "mergedAt": null,
            "isDraft": false,
        });
        raw[key] = value;
        raw
    }

    #[test]
    fn accepts_merge_state_status_conflicting() {
        let facts = parse_pull_request(
            &raw_pull_request("mergeStateStatus", json!("CONFLICTING")),
            &context(PR),
        )
        .expect("facts");
        assert_eq!(facts.merge_state_status, MergeStateStatus::Conflicting);
    }

    #[test]
    fn reads_ghs_empty_review_decision_as_no_decision() {
        let facts =
            parse_pull_request(&raw_pull_request("reviewDecision", json!("")), &context(PR))
                .expect("facts");
        assert_eq!(facts.review_decision, None);
    }

    #[test]
    fn still_rejects_an_unknown_review_decision() {
        let error = parse_pull_request(
            &raw_pull_request("reviewDecision", json!("MAYBE")),
            &context(PR),
        )
        .expect_err("unknown decision");
        assert!(matches!(
            error,
            WatchError::Query(QueryFailure::MissingKey { .. })
        ));
    }

    #[test]
    fn rejects_unknown_enum_values_as_retryable_errors_carrying_the_raw_value() {
        let error = parse_pull_request(
            &raw_pull_request("mergeStateStatus", json!("FUTURE_STATE")),
            &context(PR),
        )
        .expect_err("unknown state");
        let WatchError::Query(failure) = error else {
            panic!("expected a query failure, got {error:?}");
        };
        assert!(failure.retryable());
        assert_eq!(
            failure,
            QueryFailure::MissingKey {
                detail: "invalid pull request.mergeStateStatus: \"FUTURE_STATE\"".to_owned(),
                raw_value: Some("\"FUTURE_STATE\"".to_owned()),
            }
        );
    }

    fn thread_node(
        id: &str,
        resolved: bool,
        body: &str,
        path: Value,
        line: Value,
        login: &str,
    ) -> Value {
        json!({
            "id": id,
            "isResolved": resolved,
            "comments": {"nodes": [{
                "body": body,
                "createdAt": "now",
                "path": path,
                "line": line,
                "author": {"login": login},
            }]},
        })
    }

    #[test]
    fn annotates_bugbot_threads_with_distinct_review_pass_counts() {
        let response = json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                thread_node("one", false, "RUN_ID: run-1", json!("a.ts"), json!(1), "bugbot"),
                thread_node("two", false, "CURSOR_AUTOMATION_ID: run-2 severity high", Value::Null, Value::Null, "cursor"),
                thread_node("resolved", true, "RUN_ID: run-3", Value::Null, Value::Null, "bugbot"),
            ],
        }}}}});
        let threads =
            build_review_threads(parse_review_thread_page(&response).expect("page").threads);
        assert_eq!(ids(&threads), ["one", "two"]);
        assert!(threads.iter().all(|thread| thread.is_bugbot));
        assert!(
            threads
                .iter()
                .all(|thread| thread.bugbot_review_passes == 3)
        );
    }

    fn raw_thread(id: &str, author: &str, body: &str, resolved: bool) -> RawReviewThread {
        RawReviewThread {
            id: id.to_owned(),
            first_comment: Some(ReviewComment {
                author_login: Some(author.to_owned()),
                body: body.to_owned(),
                path: Some("a.ts".to_owned()),
                line: Some(1),
                created_at: "now".to_owned(),
            }),
            resolved,
        }
    }

    fn thread_page(threads: Vec<RawReviewThread>, end_cursor: Option<&str>) -> ReviewThreadPage {
        ReviewThreadPage {
            threads,
            end_cursor: end_cursor.map(str::to_owned),
        }
    }

    fn human(id: &str, resolved: bool) -> RawReviewThread {
        raw_thread(id, "someone", "please fix", resolved)
    }

    #[test]
    fn keeps_walking_pages_until_the_cursor_is_exhausted() {
        let reader = FakeReader {
            thread_pages: vec![
                thread_page(vec![human("one", false)], Some("next")),
                thread_page(vec![human("two", false)], Some("last")),
                thread_page(vec![human("three", false)], None),
            ],
            ..FakeReader::default()
        };
        let threads = resolve_review_threads(&reader, &context(PR)).expect("threads");
        assert_eq!(ids(&threads), ["one", "two", "three"]);
        assert_eq!(
            reader.calls(),
            [
                "reviewThreadPage:null",
                "reviewThreadPage:next",
                "reviewThreadPage:last"
            ]
        );
    }

    #[test_case(vec![human("early", true)], vec![human("late", false)], &["late"] ; "sees_an_unresolved_thread_past_the_first_page")]
    #[test_case(vec![human("one", false)], vec![human("two", true)], &["one"] ; "drops_resolved_threads_from_later_pages_too")]
    fn filters_resolved_threads_across_pages(
        first: Vec<RawReviewThread>,
        second: Vec<RawReviewThread>,
        expected: &[&str],
    ) {
        let reader = FakeReader {
            thread_pages: vec![thread_page(first, Some("next")), thread_page(second, None)],
            ..FakeReader::default()
        };
        let threads = resolve_review_threads(&reader, &context(PR)).expect("threads");
        assert_eq!(ids(&threads), expected);
    }

    #[test]
    fn stops_after_a_single_page_when_the_cursor_is_exhausted() {
        let reader = FakeReader {
            thread_pages: vec![thread_page(vec![human("only", false)], None)],
            ..FakeReader::default()
        };
        assert_eq!(
            resolve_review_threads(&reader, &context(PR))
                .expect("threads")
                .len(),
            1
        );
        assert_eq!(reader.calls(), ["reviewThreadPage:null"]);
    }

    #[test]
    fn counts_bugbot_review_passes_across_every_page() {
        let reader = FakeReader {
            thread_pages: vec![
                thread_page(
                    vec![raw_thread("one", "bugbot", "RUN_ID: run-1", false)],
                    Some("next"),
                ),
                thread_page(
                    vec![raw_thread("two", "bugbot", "RUN_ID: run-2", false)],
                    None,
                ),
            ],
            ..FakeReader::default()
        };
        let threads = resolve_review_threads(&reader, &context(PR)).expect("threads");
        assert!(
            threads
                .iter()
                .all(|thread| thread.bugbot_review_passes == 2)
        );
    }

    #[test]
    fn rejects_a_page_that_omits_page_info_instead_of_assuming_one_page() {
        let response =
            json!({"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}});
        let error = parse_review_thread_page(&response).expect_err("missing pageInfo");
        assert!(matches!(
            error,
            WatchError::Query(QueryFailure::MissingKey { ref detail, raw_value: None }) if detail == "missing reviewThreads.pageInfo"
        ));
    }

    #[test]
    fn returns_a_fully_explicit_context_without_any_reader_call() {
        let reader = FakeReader::default();
        let resolved =
            resolve_context(&reader, Some("explicit"), Some("repo"), Some(PR)).expect("context");
        assert_eq!(
            resolved,
            PrContext {
                owner: "explicit".to_owned(),
                repo: "repo".to_owned(),
                number: PR
            }
        );
        assert!(reader.calls().is_empty());
    }

    #[test]
    fn uses_the_local_origin_before_current_pr_for_an_explicit_number() {
        let reader = FakeReader {
            origin: Some(Repository {
                owner: "local".to_owned(),
                repo: "checkout".to_owned(),
            }),
            ..FakeReader::default()
        };
        let resolved = resolve_context(&reader, None, None, Some(PR)).expect("context");
        assert_eq!(
            (
                resolved.owner.as_str(),
                resolved.repo.as_str(),
                resolved.number
            ),
            ("local", "checkout", PR)
        );
        assert_eq!(reader.calls(), ["originRepo"]);
    }

    fn open_pr(number: PrNumber, head: &str, base: &str) -> OpenPullRequest {
        OpenPullRequest {
            number,
            head_ref_name: head.to_owned(),
            base_ref_name: base.to_owned(),
        }
    }

    fn numbers(contexts: &[PrContext]) -> Vec<PrNumber> {
        contexts.iter().map(|context| context.number).collect()
    }

    #[test]
    fn orders_the_connected_stack_bottom_to_top() {
        let ordered = order_stack(
            &context(PR),
            &[
                open_pr(41, "base-feature", "main"),
                open_pr(PR, "feature", "base-feature"),
                open_pr(43, "upstack", "feature"),
            ],
        );
        assert_eq!(numbers(&ordered), [41, 42, 43]);
    }

    #[test]
    fn stops_at_a_base_cycle_instead_of_walking_it_forever() {
        let ordered = order_stack(
            &context(PR),
            &[open_pr(41, "a", "b"), open_pr(PR, "b", "a")],
        );
        assert_eq!(numbers(&ordered), [41, 42]);
    }

    #[test_case("git@github.com:owner/repo.git\n", Some(("owner", "repo")) ; "scp_remote")]
    #[test_case("ssh://git@github.com/owner/repo.git", Some(("owner", "repo")) ; "ssh_remote")]
    #[test_case("https://GitHub.com:443/owner/repo", Some(("owner", "repo")) ; "default_port_and_case")]
    #[test_case("https://github.com:8443/owner/repo", None ; "other_port")]
    #[test_case("https://token@github.com/owner/repo", None ; "credentials")]
    #[test_case("https://gitlab.com/owner/repo", None ; "other_host")]
    #[test_case("https://github.com/owner/repo/extra", None ; "too_many_segments")]
    #[test_case("https://github.com/owner/repo?tab=1", None ; "query")]
    fn parses_only_plain_github_remotes(remote: &str, expected: Option<(&str, &str)>) {
        let parsed = parse_remote(remote);
        assert_eq!(
            parsed
                .as_ref()
                .map(|repository| (repository.owner.as_str(), repository.repo.as_str())),
            expected
        );
    }

    #[test_case("https://github.com/o/r/pull/7", Ok(7) ; "canonical")]
    #[test_case("https://github.com/o/r/issues/7", Err(NOT_CANONICAL_PULL_URL) ; "not_a_pull")]
    #[test_case("https://github.com/o/r/pull/0", Err(PR_NUMBER_INVALID) ; "zero")]
    #[test_case("not a url", Err(INVALID_URL) ; "unparseable")]
    fn parses_pr_urls(url: &str, expected: Result<PrNumber, &str>) {
        let parsed = parse_pr_url(url)
            .map(|context| context.number)
            .map_err(|error| match error {
                WatchError::Query(QueryFailure::InvalidContextUrl { detail, .. }) => detail,
                other => other.to_string(),
            });
        assert_eq!(
            parsed,
            expected.map_err(|reason| format!(
                "could not infer owner/repo from PR URL: {url} ({reason})"
            ))
        );
    }

    #[test]
    fn reports_a_failed_gh_command_by_its_first_stderr_line() {
        let host = fake_host();
        host.script(
            &pull_request_args(&context(PR)).join(" "),
            Output::failure(1, "HTTP 502\nretry later"),
        );
        let error = GhReader::new(&host)
            .pull_request(&context(PR))
            .expect_err("gh failed");
        assert!(matches!(
            error,
            WatchError::Query(QueryFailure::CommandExit { code: 1, ref detail }) if detail == "HTTP 502"
        ));
    }

    #[test]
    fn treats_unparseable_fast_path_output_as_unusable() {
        let host = fake_host();
        host.script(
            &checks_args(&context(PR)).join(" "),
            Output::success("[{\"name\": 1}]"),
        );
        assert_eq!(
            GhReader::new(&host)
                .checks_fast_path(&context(PR))
                .expect("fast path"),
            ChecksFastPath::Unusable {
                exit_code: 0,
                stderr: String::new()
            }
        );
    }
}
