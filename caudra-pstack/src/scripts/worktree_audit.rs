//! Port of poteto-mode's `worktree-audit.sh`: a read-only table of every linked worktree with its
//! size, merge, dirty, remote, and PR state, the last session that worked in it, and a suggested
//! prune bucket. A worktree's last session comes from [`Host::last_session_at`] where upstream
//! searches Cursor transcripts.

use std::cmp::Ordering;
use std::path::Path;

use jiff::Timestamp;
use serde_json::Value;

use crate::host::{Host, Output};

const HELP: &str = "Read-only worktree prune audit. Classifies every git worktree by size, merge
state, uncommitted work, remote/PR state, and the most recent chat that
operated in it. Emits a table sorted by size with a suggested bucket. Never
deletes anything; deletion stays a human-gated step in the playbook.

Usage: worktree-audit.sh [-b|--base <ref>] [repo-path]
  repo-path defaults to the current repo.
  -b sets the ref the MERGED column checks against (e.g. origin/integration).
  Without it the base is origin/HEAD, then origin/main, then origin/master.
  origin/HEAD is the remote's default branch, not necessarily the branch PRs
  merge into; pass -b when they differ.
";
const TABLE_HEADER: &str = "SIZE\tAGE\tMERGED\tDIRTY\tREMOTE\tPR\tLAST_CHAT\tBUCKET\tWORKTREE\n";
const FALLBACK_BASES: [&str; 2] = ["origin/main", "origin/master"];
const REMOTE_PREFIX: &str = "origin/";
const WORKTREE_PREFIX: &str = "worktree ";
const UNTRACKED_PREFIX: &str = "??";
const UNKNOWN_AGE: &str = "?";
const NO_VALUE: &str = "-";
const OPEN_PR_SUFFIXES: [&str; 2] = ["/open", "/draft"];
const SECONDS_PER_DAY: i64 = 86_400;
const RECENT_CHAT_DAYS: i64 = 4;
const DATE_FORMAT: &str = "%Y-%m-%d";
const PR_LIST: [&str; 9] = [
    "pr", "list", "--author", "@me", "--state", "all", "--limit", "1000", "--json",
];
const ORIGIN_PR_FIELDS: PrFields = PrFields {
    json: "number,status,headRef",
    state: "status",
    head: "headRef",
    lowercase_state: false,
};
const GH_PR_FIELDS: PrFields = PrFields {
    json: "number,state,headRefName",
    state: "state",
    head: "headRefName",
    lowercase_state: true,
};
/// `sort -h`'s unit letters, smallest first; `k` ranks with `K`.
const UNIT_LETTERS: &str = "KMGTPEZYRQ";

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    match audit(host, argv) {
        Ok(output) | Err(output) => output,
    }
}

/// Which fields a PR CLI's `--json` names and how its state reads.
struct PrFields {
    json: &'static str,
    state: &'static str,
    head: &'static str,
    lowercase_state: bool,
}

struct PullRequest {
    head: Value,
    label: String,
}

struct Row {
    size: String,
    line: String,
}

/// Runs every command from the audited repo, reading output as the script's `$(...)` does.
struct Repo<'a> {
    host: &'a dyn Host,
    dir: &'a Path,
}

impl Repo<'_> {
    fn succeeds(&self, argv: &[&str]) -> bool {
        self.output_if_ok(argv).is_some()
    }

    fn output_if_ok(&self, argv: &[&str]) -> Option<String> {
        self.host
            .run(argv, Some(self.dir))
            .ok()
            .filter(|output| output.code == 0)
            .map(|output| substitution(&output.stdout))
    }

    fn stdout(&self, argv: &[&str]) -> String {
        self.host
            .run(argv, Some(self.dir))
            .map(|output| substitution(&output.stdout))
            .unwrap_or_default()
    }
}

/// `Err` is an early exit, such as help or a usage error, with what the script prints.
fn audit(host: &dyn Host, argv: &[String]) -> Result<Output, Output> {
    let (base, repo) = parse_args(argv)?;
    let repo = match repo {
        Some(repo) => repo.to_owned(),
        None => Repo {
            host,
            dir: host.cwd(),
        }
        .stdout(&["git", "rev-parse", "--show-toplevel"]),
    };
    if repo.is_empty() {
        return Err(Output::failure(1, "not in a git repo; pass a repo path\n"));
    }
    let dir = host.resolve(&repo);
    if !dir.is_dir() {
        return Err(Output::failure(
            1,
            format!("cd: {repo}: No such file or directory\n"),
        ));
    }
    let git = Repo { host, dir: &dir };
    if !git.succeeds(&["git", "rev-parse", "--git-dir"]) {
        return Err(Output::failure(
            1,
            format!("error: not a git repo: {repo}\n"),
        ));
    }

    let listing = git.stdout(&["git", "worktree", "list", "--porcelain"]);
    let mut worktrees = listing
        .lines()
        .filter_map(|line| line.strip_prefix(WORKTREE_PREFIX));
    let main_worktree = worktrees.next();

    let mut warnings = String::new();
    let base = match base {
        Some(base) => base.to_owned(),
        None => {
            let base = default_base(&git).ok_or_else(|| {
                Output::failure(
                    1,
                    "error: no base ref (origin/HEAD, origin/main, origin/master all missing); pass -b <ref>\n",
                )
            })?;
            warnings.push_str(&format!(
                "warn: no -b given; MERGED checks against {base}\n"
            ));
            base
        }
    };
    if let Some(branch) = base.strip_prefix(REMOTE_PREFIX) {
        let refspec = format!("+refs/heads/{branch}:refs/remotes/{base}");
        if !git.succeeds(&["git", "fetch", "origin", &refspec, "--quiet"]) {
            warnings.push_str(&format!(
                "warn: could not fetch {base}; merged column may be stale\n"
            ));
        }
    }
    if !commit_exists(&git, &base) {
        return Err(Output {
            code: 1,
            stdout: String::new(),
            stderr: format!("{warnings}error: base ref not found: {base}\n"),
        });
    }

    let pull_requests = pull_requests(&git);
    let now = host.now();
    let mut rows: Vec<Row> = worktrees
        .filter(|worktree| Some(*worktree) != main_worktree)
        .map(|worktree| audit_worktree(&git, &base, &pull_requests, now, worktree))
        .collect();
    rows.sort_by(|a, b| human_numeric_cmp(&b.size, &a.size).then_with(|| b.line.cmp(&a.line)));

    let mut stdout = format!("# base: {base}\n{TABLE_HEADER}");
    for row in rows {
        stdout.push_str(&row.line);
    }
    Ok(Output {
        code: 0,
        stdout,
        stderr: warnings,
    })
}

/// The base given by `-b` and the repo path, either absent when empty as the script reads them.
fn parse_args(argv: &[String]) -> Result<(Option<&str>, Option<&str>), Output> {
    let mut base = None;
    let mut rest = argv;
    while let [flag, tail @ ..] = rest {
        match flag.as_str() {
            "-b" | "--base" => {
                let [reference, tail @ ..] = tail else {
                    return Err(Output::failure(2, format!("error: {flag} needs a ref\n")));
                };
                base = Some(reference.as_str());
                rest = tail;
            }
            "-h" | "--help" => return Err(Output::success(HELP)),
            option if option.starts_with('-') => {
                return Err(Output::failure(
                    2,
                    format!("error: unknown option: {option}\n"),
                ));
            }
            _ => break,
        }
    }
    let repo = rest.first().map(String::as_str);
    Ok((
        base.filter(|base| !base.is_empty()),
        repo.filter(|repo| !repo.is_empty()),
    ))
}

fn default_base(git: &Repo) -> Option<String> {
    let remote_head = git.stdout(&["git", "symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    [remote_head.as_str()]
        .into_iter()
        .chain(FALLBACK_BASES)
        .find(|candidate| !candidate.is_empty() && commit_exists(git, candidate))
        .map(str::to_owned)
}

fn commit_exists(git: &Repo, reference: &str) -> bool {
    git.succeeds(&[
        "git",
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{reference}^{{commit}}"),
    ])
}

/// The author's PRs from Origin's CLI when it can list them, otherwise from `gh`, otherwise none.
fn pull_requests(git: &Repo) -> Vec<PullRequest> {
    let list = |program: &str, fields: &PrFields| {
        let argv: Vec<&str> = [program]
            .into_iter()
            .chain(PR_LIST)
            .chain([fields.json])
            .collect();
        git.output_if_ok(&argv)
    };
    let parsed = match list("origin", &ORIGIN_PR_FIELDS) {
        Some(raw) => parse_pull_requests(&raw, &ORIGIN_PR_FIELDS),
        None => list("gh", &GH_PR_FIELDS).and_then(|raw| parse_pull_requests(&raw, &GH_PR_FIELDS)),
    };
    parsed.unwrap_or_default()
}

/// Reads a PR list as upstream's `jq` filter does, which yields nothing at all when any entry
/// fails it.
fn parse_pull_requests(raw: &str, fields: &PrFields) -> Option<Vec<PullRequest>> {
    let entries: Vec<Value> = serde_json::from_str(raw).ok()?;
    entries
        .iter()
        .map(|entry| {
            let entry = entry.as_object()?;
            let field = |name: &str| entry.get(name).cloned().unwrap_or(Value::Null);
            let state = if fields.lowercase_state {
                entry.get(fields.state)?.as_str()?.to_ascii_lowercase()
            } else {
                interpolate(&field(fields.state))
            };
            Some(PullRequest {
                head: field(fields.head),
                label: format!("#{}/{state}", interpolate(&field("number"))),
            })
        })
        .collect()
}

/// How `jq` writes a value inside a string interpolation.
fn interpolate(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn audit_worktree(
    git: &Repo,
    base: &str,
    pull_requests: &[PullRequest],
    now: Timestamp,
    worktree: &str,
) -> Row {
    let size = git
        .stdout(&["du", "-sh", worktree])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let head = git.stdout(&git_in(worktree, &["rev-parse", "HEAD"]));
    let head_seconds = git
        .output_if_ok(&git_in(worktree, &["log", "-1", "--format=%ct", "HEAD"]))
        .and_then(|seconds| seconds.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds > 0);
    let age = head_seconds.map_or_else(
        || UNKNOWN_AGE.to_owned(),
        |seconds| format!("{}d", (now.as_second() - seconds) / SECONDS_PER_DAY),
    );
    let merged = if git.succeeds(&["git", "merge-base", "--is-ancestor", &head, base]) {
        "YES"
    } else {
        "no"
    };
    let dirty = dirty_state(&git.stdout(&git_in(worktree, &["status", "--porcelain"])));

    let branch = git
        .output_if_ok(&git_in(
            worktree,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        ))
        .unwrap_or_default();
    let remote = if branch.is_empty() {
        "detached".to_owned()
    } else if git.succeeds(&git_in(
        worktree,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
    )) {
        let remote_branch = format!("{REMOTE_PREFIX}{branch}");
        if git.stdout(&git_in(worktree, &["rev-parse", &remote_branch])) == head {
            "pushed".to_owned()
        } else {
            let range = format!("{remote_branch}..HEAD");
            format!(
                "ahead{}",
                git.stdout(&git_in(worktree, &["rev-list", "--count", &range]))
            )
        }
    } else {
        "no-remote".to_owned()
    };

    let pr = pull_requests
        .iter()
        .find(|pr| !branch.is_empty() && pr.head.as_str() == Some(branch.as_str()))
        .map_or(NO_VALUE, |pr| pr.label.as_str());

    let last_session = git.host.last_session_at(Path::new(worktree));
    let last_chat = last_session.map_or_else(
        || NO_VALUE.to_owned(),
        |at| at.strftime(DATE_FORMAT).to_string(),
    );
    let recent = last_session.is_some_and(|at| {
        at.as_second() > 0
            && (now.as_second() - at.as_second()) / SECONDS_PER_DAY <= RECENT_CHAT_DAYS
    });

    let bucket = if dirty.starts_with("wip:") {
        "hold-wip"
    } else if OPEN_PR_SUFFIXES.iter().any(|suffix| pr.ends_with(suffix)) {
        "hold-open-pr"
    } else if recent {
        "verify-recent-chat"
    } else if merged == "YES" || pr != NO_VALUE {
        "safe"
    } else {
        "review"
    };

    let line = format!(
        "{size}\t{age}\t{merged}\t{dirty}\t{remote}\t{pr}\t{last_chat}\t{bucket}\t{worktree}\n"
    );
    Row { size, line }
}

fn git_in<'a>(worktree: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    ["git", "-C", worktree]
        .into_iter()
        .chain(args.iter().copied())
        .collect()
}

/// Tracked edits are real work in progress; untracked files alone are disposable scratch.
fn dirty_state(porcelain: &str) -> String {
    if porcelain.is_empty() {
        return "clean".to_owned();
    }
    let total = porcelain.split('\n').count();
    let untracked = porcelain
        .split('\n')
        .filter(|line| line.starts_with(UNTRACKED_PREFIX))
        .count();
    match total - untracked {
        0 => format!("scratch:{untracked}"),
        tracked => format!("wip:{tracked}"),
    }
}

/// What `$(...)` keeps of a command's output: everything but trailing newlines.
fn substitution(stdout: &str) -> String {
    stdout.trim_end_matches('\n').to_owned()
}

/// GNU `sort -h` on one field: the unit letter outranks the number, so `900K` sorts below `1M`,
/// and a zero has no unit.
fn human_numeric_cmp(left: &str, right: &str) -> Ordering {
    let (left_unit, left_value) = human_numeric_key(left);
    let (right_unit, right_value) = human_numeric_key(right);
    left_unit.cmp(&right_unit).then_with(|| {
        left_value
            .partial_cmp(&right_value)
            .unwrap_or(Ordering::Equal)
    })
}

fn human_numeric_key(field: &str) -> (i32, f64) {
    let field = field.trim_start_matches([' ', '\t']);
    let (negative, unsigned) = match field.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, field),
    };
    let number_end = unsigned
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(unsigned.len());
    let number = &unsigned[..number_end];
    let unit = unsigned[number_end..]
        .chars()
        .next()
        .and_then(|letter| UNIT_LETTERS.find(letter.to_ascii_uppercase()))
        .filter(|_| number.bytes().any(|digit| (b'1'..=b'9').contains(&digit)))
        .and_then(|index| i32::try_from(index + 1).ok())
        .unwrap_or_default();
    let magnitude = number.parse::<f64>().unwrap_or_default();
    if negative {
        (-unit, -magnitude)
    } else {
        (unit, magnitude)
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{HELP, Ordering, Output, SECONDS_PER_DAY, human_numeric_cmp, run};
    use crate::host::fake::FakeHost;

    const NOW: &str = "2026-10-20T12:00:00Z";
    const MAIN: &str = "/repo";
    const BASE_WARNING: &str = "warn: no -b given; MERGED checks against origin/main\n";
    const NO_BASE: &str =
        "error: no base ref (origin/HEAD, origin/main, origin/master all missing); pass -b <ref>\n";
    const NOT_IN_REPO: &str = "not in a git repo; pass a repo path\n";
    const TABLE_HEADER: &str =
        "# base: origin/main\nSIZE\tAGE\tMERGED\tDIRTY\tREMOTE\tPR\tLAST_CHAT\tBUCKET\tWORKTREE\n";

    struct Worktree<'a> {
        path: &'a str,
        size: &'a str,
        head: &'a str,
        age_days: i64,
        merged: bool,
        porcelain: &'a str,
        branch: Option<&'a str>,
        remote_head: Option<&'a str>,
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn now() -> Timestamp {
        NOW.parse().expect("fixed timestamp")
    }

    fn days(count: i64) -> SignedDuration {
        SignedDuration::from_secs(count * SECONDS_PER_DAY)
    }

    fn repo_host(dir: &TempDir, worktrees: &[Worktree]) -> FakeHost {
        let host = FakeHost::new(dir.path()).at(now());
        let listing: String = [MAIN]
            .into_iter()
            .chain(worktrees.iter().map(|worktree| worktree.path))
            .map(|path| format!("worktree {path}\nHEAD abc\n\n"))
            .collect();
        host.script("git rev-parse --git-dir", Output::success(".git\n"))
            .script("git worktree list --porcelain", Output::success(listing))
            .script(
                "git symbolic-ref --short refs/remotes/origin/HEAD",
                Output::success("origin/main\n"),
            )
            .script(
                "git rev-parse --verify --quiet origin/main^{commit}",
                Output::success("base\n"),
            )
            .script(
                "git fetch origin +refs/heads/main:refs/remotes/origin/main --quiet",
                Output::success(""),
            );
        for worktree in worktrees {
            script_worktree(&host, worktree);
        }
        host
    }

    fn script_worktree(host: &FakeHost, worktree: &Worktree) {
        let path = worktree.path;
        let git = |args: &str| format!("git -C {path} {args}");
        host.script(
            &format!("du -sh {path}"),
            Output::success(format!("{}\t{path}\n", worktree.size)),
        )
        .script(
            &git("rev-parse HEAD"),
            Output::success(format!("{}\n", worktree.head)),
        )
        .script(
            &git("log -1 --format=%ct HEAD"),
            Output::success(format!(
                "{}\n",
                now().as_second() - worktree.age_days * SECONDS_PER_DAY
            )),
        )
        .script(
            &git("status --porcelain"),
            Output::success(worktree.porcelain),
        );
        if worktree.merged {
            host.script(
                &format!("git merge-base --is-ancestor {} origin/main", worktree.head),
                Output::success(""),
            );
        }
        if let Some(branch) = worktree.branch {
            host.script(
                &git("symbolic-ref --quiet --short HEAD"),
                Output::success(format!("{branch}\n")),
            );
            if let Some(remote_head) = worktree.remote_head {
                host.script(
                    &git(&format!(
                        "show-ref --verify --quiet refs/remotes/origin/{branch}"
                    )),
                    Output::success(""),
                )
                .script(
                    &git(&format!("rev-parse origin/{branch}")),
                    Output::success(format!("{remote_head}\n")),
                )
                .script(
                    &git(&format!("rev-list --count origin/{branch}..HEAD")),
                    Output::success("2\n"),
                );
            }
        }
    }

    fn worktree(path: &str) -> Worktree<'_> {
        Worktree {
            path,
            size: "1.0M",
            head: "h0",
            age_days: 3,
            merged: false,
            porcelain: "",
            branch: None,
            remote_head: None,
        }
    }

    #[test]
    fn help_prints_header_comment() {
        let output = run(&FakeHost::new("/"), &argv(&["-b", "origin/x", "--help"]));
        assert_eq!(output, Output::success(HELP));
    }

    #[test_case(&["-b"], "error: -b needs a ref\n"; "base without ref")]
    #[test_case(&["--base"], "error: --base needs a ref\n"; "long base without ref")]
    #[test_case(&["--all"], "error: unknown option: --all\n"; "unknown option")]
    #[test_case(&["-"], "error: unknown option: -\n"; "lone dash")]
    fn bad_flags_exit_two(args: &[&str], message: &str) {
        let host = FakeHost::new("/");
        let output = run(&host, &argv(args));
        assert_eq!(output, Output::failure(2, message));
        assert!(host.ran().is_empty());
    }

    #[test]
    fn outside_a_repo_asks_for_a_path() {
        let output = run(&FakeHost::new("/"), &[]);
        assert_eq!(output, Output::failure(1, NOT_IN_REPO));
    }

    #[test]
    fn missing_base_refs_fail() {
        let dir = TempDir::new().expect("tempdir");
        let host = FakeHost::new(dir.path());
        host.script("git rev-parse --git-dir", Output::success(".git\n"));
        let output = run(&host, &argv(&["."]));
        assert_eq!(output, Output::failure(1, NO_BASE));
    }

    #[test]
    fn unknown_explicit_base_fails_after_fetch_warning() {
        let dir = TempDir::new().expect("tempdir");
        let host = repo_host(&dir, &[]);
        let output = run(&host, &argv(&["-b", "origin/integration", "."]));
        assert_eq!(
            output,
            Output::failure(
                1,
                "warn: could not fetch origin/integration; merged column may be stale\nerror: base ref not found: origin/integration\n"
            )
        );
    }

    #[test]
    fn audit_buckets_and_sorts_worktrees() {
        let dir = TempDir::new().expect("tempdir");
        let worktrees = [
            Worktree {
                porcelain: " M src/lib.rs\n?? scratch.txt\n",
                branch: Some("wip"),
                ..worktree("/wt/wip")
            },
            Worktree {
                size: "2.0G",
                branch: Some("open"),
                remote_head: Some("h0"),
                ..worktree("/wt/open")
            },
            Worktree {
                size: "900K",
                head: "h1",
                merged: true,
                porcelain: "?? a\n?? b\n",
                branch: Some("merged"),
                remote_head: Some("old"),
                age_days: 40,
                ..worktree("/wt/merged")
            },
            Worktree {
                size: "0",
                ..worktree("/wt/recent")
            },
            Worktree {
                size: "12M",
                head: "h9",
                ..worktree("/wt/review")
            },
        ];
        let host = repo_host(&dir, &worktrees)
            .with_session("/wt/recent", now() - days(4))
            .with_session("/wt/review", now() - days(5));
        host.script("origin pr list --author @me --state all --limit 1000 --json number,status,headRef", Output::failure(1, "no auth"))
            .script(
                "gh pr list --author @me --state all --limit 1000 --json number,state,headRefName",
                Output::success(
                    r#"[{"number":7,"state":"OPEN","headRefName":"open"},{"number":5,"state":"MERGED","headRefName":"merged"}]"#,
                ),
            );

        let output = run(&host, &argv(&[dir.path().to_str().expect("utf-8 path")]));

        assert_eq!(output.stderr, BASE_WARNING);
        assert_eq!(
            output.stdout,
            format!(
                "{TABLE_HEADER}\
                 2.0G\t3d\tno\tclean\tpushed\t#7/open\t-\thold-open-pr\t/wt/open\n\
                 12M\t3d\tno\tclean\tdetached\t-\t2026-10-15\treview\t/wt/review\n\
                 1.0M\t3d\tno\twip:1\tno-remote\t-\t-\thold-wip\t/wt/wip\n\
                 900K\t40d\tYES\tscratch:2\tahead2\t#5/merged\t-\tsafe\t/wt/merged\n\
                 0\t3d\tno\tclean\tdetached\t-\t2026-10-16\tverify-recent-chat\t/wt/recent\n"
            )
        );
    }

    #[test]
    fn unreadable_pr_list_leaves_prs_empty() {
        let dir = TempDir::new().expect("tempdir");
        let host = repo_host(
            &dir,
            &[Worktree {
                branch: Some("feature"),
                ..worktree("/wt/feature")
            }],
        );
        host.script(
            "origin pr list --author @me --state all --limit 1000 --json number,status,headRef",
            Output::success("not json"),
        );
        let output = run(&host, &argv(&["-b", "origin/main", "."]));
        assert_eq!(
            (output.stdout, output.stderr.as_str()),
            (
                format!(
                    "{TABLE_HEADER}1.0M\t3d\tno\tclean\tno-remote\t-\t-\treview\t/wt/feature\n"
                ),
                ""
            )
        );
        assert!(!host.ran().iter().any(|command| command.starts_with("gh ")));
    }

    #[test_case("2.0G", "900M", Ordering::Greater; "unit outranks number")]
    #[test_case("1.5K", "1.2K", Ordering::Greater; "same unit compares numbers")]
    #[test_case("0", "4.0K", Ordering::Less; "zero has no unit")]
    #[test_case("0K", "1", Ordering::Less; "zero with a unit has no unit")]
    #[test_case("", "0", Ordering::Equal; "empty is zero")]
    #[test_case("12", "9", Ordering::Greater; "plain numbers")]
    #[test_case("1k", "1K", Ordering::Equal; "lowercase k")]
    fn sizes_sort_as_sort_h(left: &str, right: &str, expected: Ordering) {
        assert_eq!(human_numeric_cmp(left, right), expected);
    }
}
