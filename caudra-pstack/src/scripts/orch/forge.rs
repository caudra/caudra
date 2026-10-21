//! Resolves the stack frontier: walks the pull request base chain down to trunk and the open
//! children up to the tip, through `git` and the `gh` or `origin` forge CLI.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::host::Host;

use super::compat::safe_integer;
use super::model::{FrontierPr, OrchError, OrchResult, PrState};

pub const FORGES: [&str; 2] = ["gh", "origin"];
const DEFAULT_TRUNK: &str = "main";
const ORIGIN_PREFIX: &str = "origin/";

static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{40,64}$").expect("literal"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    Gh,
    Origin,
}

struct PullRequestQuery {
    state_field: &'static str,
    head_field: &'static str,
    base_field: &'static str,
    states: &'static [(&'static str, PrState)],
}

const GH_QUERY: PullRequestQuery = PullRequestQuery {
    state_field: "state",
    head_field: "headRefName",
    base_field: "baseRefName",
    states: &[
        ("OPEN", PrState::Open),
        ("MERGED", PrState::Merged),
        ("CLOSED", PrState::Closed),
    ],
};

const ORIGIN_QUERY: PullRequestQuery = PullRequestQuery {
    state_field: "status",
    head_field: "headRef",
    base_field: "baseRef",
    states: &[
        ("open", PrState::Open),
        ("draft", PrState::Open),
        ("merged", PrState::Merged),
        ("closed", PrState::Closed),
    ],
};

impl Forge {
    pub fn parse(value: &str) -> OrchResult<Self> {
        match value {
            "gh" => Ok(Self::Gh),
            "origin" => Ok(Self::Origin),
            _ => Err(OrchError::user("forge must be gh or origin")),
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Gh => "gh",
            Self::Origin => "origin",
        }
    }

    fn query(self) -> &'static PullRequestQuery {
        match self {
            Self::Gh => &GH_QUERY,
            Self::Origin => &ORIGIN_QUERY,
        }
    }
}

#[derive(Clone, Copy)]
enum Filter {
    Head,
    Base,
}

impl Filter {
    fn flag(self) -> &'static str {
        match self {
            Self::Head => "--head",
            Self::Base => "--base",
        }
    }
}

struct PullRequest {
    pr: u64,
    state: PrState,
    branch: String,
    base: String,
}

struct Resolver<'a> {
    host: &'a dyn Host,
    repo: &'a Path,
    forge: Forge,
}

impl Resolver<'_> {
    /// Runs a tool in the repository, failing with its exit status and stderr the way Bun's
    /// `execFileSync` describes them.
    fn run_tool(&self, command: &str, args: &[&str]) -> OrchResult<String> {
        let argv: Vec<&str> = [command].into_iter().chain(args.iter().copied()).collect();
        let failure = |reason: String| {
            OrchError::user(format!("{command} {} failed: {reason}", args.join(" ")))
        };
        match self.host.run(&argv, Some(self.repo)) {
            Ok(output) if output.code == 0 => Ok(output.stdout),
            Ok(output) => {
                let detail = if output.stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", output.stderr)
                };
                Err(failure(format!(
                    "Command failed: {}{detail}",
                    argv.join(" ")
                )))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Err(failure(format!(
                "Executable not found in $PATH: \"{command}\""
            ))),
            Err(error) => Err(failure(error.to_string())),
        }
    }

    fn git(&self, args: &[&str]) -> OrchResult<String> {
        Ok(self.run_tool("git", args)?.trim().to_owned())
    }

    fn list(&self, filter: Filter, branch: &str) -> OrchResult<Vec<PullRequest>> {
        let query = self.forge.query();
        let forge = self.forge.command();
        let command = format!("{forge} pr list {} {branch}", filter.flag());
        let fields = [
            "number",
            query.state_field,
            query.head_field,
            query.base_field,
        ]
        .join(",");
        // Every state is fetched because a merged or closed PR still anchors the chain, and a
        // draft counts as OPEN.
        let raw = self.run_tool(
            forge,
            &[
                "pr",
                "list",
                filter.flag(),
                branch,
                "--state",
                "all",
                "--json",
                &fields,
            ],
        )?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|_| OrchError::user(format!("{command} output is not valid JSON")))?;
        let Value::Array(rows) = value else {
            return Err(OrchError::user(format!(
                "{command} output is not a JSON array"
            )));
        };
        rows.iter()
            .map(|row| {
                pull_request(query, row).ok_or_else(|| {
                    OrchError::user(format!("{command} output has an invalid PR row"))
                })
            })
            .collect()
    }

    fn branch_pull_request(&self, branch: &str) -> OrchResult<PullRequest> {
        let mut rows = self.list(Filter::Head, branch)?;
        if rows.len() > 1 {
            let numbers: Vec<String> = rows.iter().map(|row| format!("#{}", row.pr)).collect();
            return Err(OrchError::user(format!(
                "branch {branch} has multiple pull requests ({}); close the stale ones first",
                numbers.join(", ")
            )));
        }
        rows.pop().ok_or_else(|| {
            OrchError::user(format!(
                "branch {branch} has no pull request; open one with {} pr create, or resolve the frontier from a branch whose chain is fully submitted",
                self.forge.command()
            ))
        })
    }

    fn open_children(&self, branch: &str) -> OrchResult<Vec<PullRequest>> {
        let mut rows = self.list(Filter::Base, branch)?;
        rows.retain(|row| row.state == PrState::Open);
        Ok(rows)
    }

    fn branch_sha(&self, branch: &str) -> OrchResult<String> {
        let sha = self.git(&["rev-parse", branch])?;
        if !SHA.is_match(&sha) {
            return Err(OrchError::user(format!(
                "git rev-parse {branch} returned an invalid SHA"
            )));
        }
        Ok(sha)
    }

    fn trunk(&self) -> String {
        // Init-plus-fetch checkouts and local fixtures lack origin/HEAD.
        self.git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .map(|trunk| {
                trunk
                    .strip_prefix(ORIGIN_PREFIX)
                    .map_or(trunk.clone(), str::to_owned)
            })
            .unwrap_or_else(|_| DEFAULT_TRUNK.to_owned())
    }

    fn resolve(&self) -> OrchResult<Vec<FrontierPr>> {
        let trunk = self.trunk();
        let current = self.git(&["symbolic-ref", "--short", "HEAD"])?;
        if current == trunk {
            return Ok(Vec::new());
        }
        let loops_at = |branch: &str| {
            OrchError::user(format!("pull request base chain loops at branch {branch}"))
        };
        let mut seen = HashSet::from([current.clone()]);
        let mut chain = vec![self.branch_pull_request(&current)?];
        while let Some(base) = chain.first().map(|bottom| bottom.base.clone())
            && base != trunk
        {
            if !seen.insert(base.clone()) {
                return Err(loops_at(&base));
            }
            chain.insert(0, self.branch_pull_request(&base)?);
        }
        let mut tip = current;
        loop {
            let mut children = self.open_children(&tip)?;
            if children.len() > 1 {
                let branches: Vec<&str> = children.iter().map(|row| row.branch.as_str()).collect();
                return Err(OrchError::user(format!(
                    "stack is not linear: branches {} all target {tip}",
                    branches.join(", ")
                )));
            }
            let Some(child) = children.pop() else {
                break;
            };
            if !seen.insert(child.branch.clone()) {
                return Err(loops_at(&child.branch));
            }
            tip = child.branch.clone();
            chain.push(child);
        }
        let distinct: HashSet<u64> = chain.iter().map(|row| row.pr).collect();
        if distinct.len() != chain.len() {
            return Err(OrchError::user(
                "pull request base chain contains duplicate pull requests",
            ));
        }
        chain
            .into_iter()
            .map(|row| {
                Ok(FrontierPr {
                    pr: row.pr,
                    sha: self.branch_sha(&row.branch)?,
                    branches: row.branch,
                    state: row.state,
                })
            })
            .collect()
    }
}

fn pull_request(query: &PullRequestQuery, row: &Value) -> Option<PullRequest> {
    let row = row.as_object()?;
    let status = row.get(query.state_field)?.as_str()?;
    let state = query
        .states
        .iter()
        .find(|(name, _)| *name == status)
        .map(|(_, state)| *state)?;
    let pr = safe_integer(row.get("number")?).filter(|pr| *pr >= 1)?;
    let text = |field: &str| {
        row.get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(PullRequest {
        pr: pr.unsigned_abs(),
        state,
        branch: text(query.head_field)?,
        base: text(query.base_field)?,
    })
}

pub fn resolve_frontier(host: &dyn Host, repo: &Path, forge: Forge) -> OrchResult<Vec<FrontierPr>> {
    Resolver { host, repo, forge }.resolve()
}

pub fn validate_pin(actual: &[u64], expected: &[u64]) -> OrchResult<()> {
    if actual == expected {
        return Ok(());
    }
    let join = |prs: &[u64]| prs.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let missing: Vec<u64> = expected
        .iter()
        .copied()
        .filter(|pr| !actual.contains(pr))
        .collect();
    let extra: Vec<u64> = actual
        .iter()
        .copied()
        .filter(|pr| !expected.contains(pr))
        .collect();
    let mut drift = Vec::new();
    if !missing.is_empty() {
        drift.push(format!("missing from resolved chain: {}", join(&missing)));
    }
    if !extra.is_empty() {
        drift.push(format!("extra in resolved chain: {}", join(&extra)));
    }
    if drift.is_empty() {
        drift.push(format!(
            "order differs: expected {}; resolved {}",
            join(expected),
            join(actual)
        ));
    }
    Err(OrchError::user(format!(
        "frontier pin mismatch: {}",
        drift.join("; ")
    )))
}
