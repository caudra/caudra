//! Port of poteto-mode's `watch-pr`: watches one pull request, its connected stack, or a captured
//! queued stack through `gh`, and prints a verdict as JSON (NDJSON while polling) or `--pretty`
//! text, exiting with the verdict's code. Each progress line also goes to [`Host::progress`] as it
//! is produced, and a cancelled sleep ends the run with [`cli::EXIT_CANCELLED`].

mod cli;
#[cfg(test)]
mod fakes;
mod github;
mod policy;
mod render;
mod types;

use crate::host::{Host, Output};

pub fn run(host: &dyn Host, argv: &[String]) -> Output {
    cli::main(host, &github::GhReader::new(host), argv)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use test_case::test_case;

    use super::cli::{CANCELLED_MESSAGE, EXIT_CANCELLED, EXIT_CRASHED};
    use super::fakes::{args, fake_host};
    use super::github::{
        PR_COMMIT_STATUS_QUERY, REVIEW_THREADS_QUERY, checks_args, graphql_args, pull_request_args,
    };
    use super::types::PrContext;
    use super::*;
    use crate::host::fake::FakeHost;

    const OWNER: &str = "o";
    const REPO: &str = "r";
    const EPOCH_OBSERVED_AT: &str = "1970-01-01T00:00:00.000Z";
    const AFTER_ONE_POLL_OBSERVED_AT: &str = "1970-01-01T00:01:00.000Z";
    const OPEN_PRS_COMMAND: &str =
        "gh pr list --repo o/r --state open --limit 300 --json number,headRefName,baseRefName";
    const PR_7_VIEW: &str = r#"{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"","headRefOid":"abc","headRefName":"feature","baseRefName":"main","state":"OPEN","mergedAt":null,"isDraft":false}"#;
    const PR_8_VIEW: &str = r#"{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":"REVIEW_REQUIRED","headRefOid":"def","headRefName":"upstack","baseRefName":"feature","state":"OPEN","mergedAt":null,"isDraft":true}"#;
    const PASSING_CHECKS: &str = r#"[{"name":"build","state":"SUCCESS","description":"ok","link":"https://ci/1","workflow":"CI","bucket":"pass"}]"#;
    const PENDING_CHECKS: &str = r#"[{"name":"build","state":"PENDING","description":"","link":"","workflow":"CI","bucket":"pending"}]"#;
    const OPEN_PRS: &str = r#"[{"number":7,"headRefName":"feature","baseRefName":"main"},{"number":8,"headRefName":"upstack","baseRefName":"feature"}]"#;
    const NO_THREADS: &str = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}"#;
    const BUGBOT_THREAD: &str = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":false,"comments":{"nodes":[{"body":"RUN_ID: r1\nsecond line","createdAt":"2026-01-01T00:00:00Z","path":"src/a.rs","line":12,"author":{"login":"cursor-bugbot"}}]}}]}}}}}"#;
    const COMMITS: &str = r#"{"data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":"old","statusCheckRollup":{"state":"SUCCESS"}}},{"commit":{"oid":"abc","statusCheckRollup":{"state":"SUCCESS"}}}]}}}}}"#;

    const READY_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"single","kind":"READY","terminal":true,"exitCode":0,"scope":{"kind":"single","pr":{"kind":"ready-pr","context":{"owner":"o","repo":"r","number":7},"proof":{"mergeability":"clear","threads":[],"ci":{"source":"gh-pr-checks","all":[{"name":"build","description":"ok","link":"https://ci/1","workflow":"CI","kind":"passed","reportedState":"SUCCESS"}],"hadPreviousPassingCi":true,"kind":"ci-clean","failed":[],"pending":[],"github":{"kind":"allowed","basis":"merge-state","mergeStateStatus":"CLEAN","headRollupState":"SUCCESS"}},"gate":{"state":"OPEN","reviewDecision":null,"draft":"not-draft"}}}}}
"#;
    const READY_PRETTY: &str = "READY: no merge conflicts, no unresolved review threads, no failing or pending checks\nmergeStateStatus=CLEAN\nreviewDecision=null\nisDraft=false\n";
    const STACK_STATUS_ONLY_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"stack","kind":"STATUS","terminal":true,"exitCode":0,"reason":"status-only","rows":[{"kind":"open","context":{"owner":"o","repo":"r","number":7},"facts":{"context":{"owner":"o","repo":"r","number":7},"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":null,"headRefOid":"abc","headRefName":"feature","baseRefName":"main","state":"OPEN","mergedAt":null,"isDraft":false},"threads":[],"ci":{"source":"gh-pr-checks","all":[{"name":"build","description":"ok","link":"https://ci/1","workflow":"CI","kind":"passed","reportedState":"SUCCESS"}],"hadPreviousPassingCi":true,"kind":"ci-clean","failed":[],"pending":[],"github":{"kind":"allowed","basis":"merge-state","mergeStateStatus":"CLEAN","headRollupState":"SUCCESS"}},"reviewAutomationRunning":false},{"kind":"open","context":{"owner":"o","repo":"r","number":8},"facts":{"context":{"owner":"o","repo":"r","number":8},"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":"REVIEW_REQUIRED","headRefOid":"def","headRefName":"upstack","baseRefName":"feature","state":"OPEN","mergedAt":null,"isDraft":true},"threads":[{"id":"T1","firstComment":{"authorLogin":"cursor-bugbot","body":"RUN_ID: r1\nsecond line","path":"src/a.rs","line":12,"createdAt":"2026-01-01T00:00:00Z"},"isBugbot":true,"bugbotReviewPasses":1}],"ci":{"source":"gh-pr-checks","all":[{"name":"build","description":"ok","link":"https://ci/1","workflow":"CI","kind":"passed","reportedState":"SUCCESS"}],"hadPreviousPassingCi":true,"kind":"ci-clean","failed":[],"pending":[],"github":{"kind":"allowed","basis":"rollup","mergeStateStatus":"BLOCKED","headRollupState":null}},"reviewAutomationRunning":false}]}
"#;
    const STACK_TABLE: &str = "| PR | CI | Review | Merge |\n| --- | --- | --- | --- |\n| [#7](https://github.com/o/r/pull/7) | ✅ | ✅ | ✅ |\n| [#8](https://github.com/o/r/pull/8) | ✅ | 📝 1 open | ⏸ draft |\n";
    const THREADS_BLOCKER_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"single","kind":"BLOCKER","terminal":true,"exitCode":3,"blocker":{"kind":"review-threads","pr":{"owner":"o","repo":"r","number":8},"threads":[{"id":"T1","firstComment":{"authorLogin":"cursor-bugbot","body":"RUN_ID: r1\nsecond line","path":"src/a.rs","line":12,"createdAt":"2026-01-01T00:00:00Z"},"isBugbot":true,"bugbotReviewPasses":1}]}}
"#;
    const THREADS_BLOCKER_PRETTY: &str = "BLOCKER: review-threads\npr=8\nunresolved=1\nT1 src/a.rs 12 cursor-bugbot isBugBot=true bugbotReviewPasses=1 RUN_ID: r1\n";
    const WAITING_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"single","kind":"WAITING","terminal":false,"frontier":{"owner":"o","repo":"r","number":7},"reason":{"kind":"pending-checks","pending":[{"name":"build","description":"","link":"","workflow":"CI","kind":"pending","reportedState":"PENDING"}]}}"#;
    const QUEUED_PENDING_JSON: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"queued-stack","kind":"QUEUE","terminal":false,"queue":[{"owner":"o","repo":"r","number":7}]}
{"schemaVersion":1,"sequence":2,"observedAt":"1970-01-01T00:00:00.000Z","mode":"queued-stack","kind":"STATUS","terminal":false,"reason":"whole-stack-sweep","rows":[{"kind":"open","context":{"owner":"o","repo":"r","number":7},"facts":{"context":{"owner":"o","repo":"r","number":7},"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":null,"headRefOid":"abc","headRefName":"feature","baseRefName":"main","state":"OPEN","mergedAt":null,"isDraft":false},"threads":[],"ci":{"kind":"ci-pending","source":"gh-pr-checks","all":[{"name":"build","description":"","link":"","workflow":"CI","kind":"pending","reportedState":"PENDING"}],"failed":[],"pending":[{"name":"build","description":"","link":"","workflow":"CI","kind":"pending","reportedState":"PENDING"}],"hadPreviousPassingCi":false},"reviewAutomationRunning":false}]}
{"schemaVersion":1,"sequence":3,"observedAt":"1970-01-01T00:00:00.000Z","mode":"queued-stack","kind":"WAITING","terminal":false,"frontier":{"owner":"o","repo":"r","number":7},"reason":{"kind":"pending-checks","pending":[{"name":"build","description":"","link":"","workflow":"CI","kind":"pending","reportedState":"PENDING"}]}}
"#;
    const QUEUED_PRETTY_BLOCKER: &str = "QUEUE: captured 2 PRs bottom-to-top: #7,#8\n| PR | CI | Review | Merge |\n| --- | --- | --- | --- |\n| [#7](https://github.com/o/r/pull/7) | ⏳ 1 pending | ✅ | ✅ |\n| [#8](https://github.com/o/r/pull/8) | ⏳ 1 pending | 📝 1 open | ⏸ draft |\nBLOCKER: review-threads\npr=8\nunresolved=1\nT1 src/a.rs 12 cursor-bugbot isBugBot=true bugbotReviewPasses=1 RUN_ID: r1\n";
    const STATUS_QUERY_BLOCKER: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"single","kind":"BLOCKER","terminal":true,"exitCode":7,"blocker":{"kind":"status-query","failures":1,"failure":{"kind":"command-exit","retryable":true,"code":1,"detail":"HTTP 502"}}}
"#;
    const RETRY_PREFIX: &str = r#"{"schemaVersion":1,"sequence":1,"observedAt":"1970-01-01T00:00:00.000Z","mode":"single","kind":"RETRY","terminal":false,"failure":{"kind":"command-exit","retryable":true,"code":1,"detail":"HTTP 502"},"consecutiveFailures":1,"retryInSeconds":60}"#;

    fn repo_context(number: u64) -> PrContext {
        PrContext {
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            number,
        }
    }

    fn key(argv: Vec<String>) -> String {
        argv.join(" ")
    }

    fn script_pr(host: &FakeHost, number: u64, view: &str, threads: &str, checks: &str) {
        let context = repo_context(number);
        host.script(&key(pull_request_args(&context)), Output::success(view));
        host.script(
            &key(graphql_args(REVIEW_THREADS_QUERY, &context, None)),
            Output::success(threads),
        );
        host.script(&key(checks_args(&context)), Output::success(checks));
        host.script(
            &key(graphql_args(PR_COMMIT_STATUS_QUERY, &context, None)),
            Output::success(COMMITS),
        );
    }

    fn scripted_stack(checks: &str) -> FakeHost {
        let host = fake_host();
        script_pr(&host, 7, PR_7_VIEW, NO_THREADS, checks);
        script_pr(&host, 8, PR_8_VIEW, BUGBOT_THREAD, checks);
        host.script(OPEN_PRS_COMMAND, Output::success(OPEN_PRS));
        host
    }

    fn watch(host: &FakeHost, argv: &[&str]) -> Output {
        run(
            host,
            &args(&[&["--owner", OWNER, "--repo", REPO], argv].concat()),
        )
    }

    #[test_case(&["--pr", "7"], 0, READY_JSON ; "ready_json")]
    #[test_case(&["--pr", "7", "--pretty"], 0, READY_PRETTY ; "ready_pretty")]
    #[test_case(&["--pr", "7", "--stack", "--status-only"], 0, STACK_STATUS_ONLY_JSON ; "stack_status_json")]
    #[test_case(&["--pr", "7", "--stack", "--status-only", "--pretty"], 0, STACK_TABLE ; "stack_status_pretty")]
    #[test_case(&["--pr", "8"], 3, THREADS_BLOCKER_JSON ; "threads_blocker_json")]
    #[test_case(&["--pr", "8", "--pretty"], 3, THREADS_BLOCKER_PRETTY ; "threads_blocker_pretty")]
    fn prints_what_upstream_prints_for_the_same_github_answers(
        argv: &[&str],
        code: i32,
        stdout: &str,
    ) {
        let host = scripted_stack(PASSING_CHECKS);
        assert_eq!(
            watch(&host, argv),
            Output {
                code,
                stdout: stdout.to_owned(),
                stderr: String::new()
            }
        );
        assert!(host.progress_lines().is_empty());
    }

    #[test]
    fn shows_each_stack_poll_as_progress_before_the_verdict() {
        let host = scripted_stack(PASSING_CHECKS);
        let output = watch(
            &host,
            &["--pr", "7", "--stack", "--allow-draft", "--pretty"],
        );
        assert_eq!(output.code, 3);
        assert_eq!(
            output.stdout,
            format!("{STACK_TABLE}{THREADS_BLOCKER_PRETTY}")
        );
        assert_eq!(
            host.progress_lines(),
            STACK_TABLE.lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn writes_queued_pending_rows_in_upstreams_key_order() {
        let host = scripted_stack(PENDING_CHECKS);
        host.cancel_after(0);
        let output = watch(&host, &["--queued-stack", "--stack-prs", "7"]);
        assert_eq!(
            output,
            Output {
                code: EXIT_CANCELLED,
                stdout: QUEUED_PENDING_JSON.to_owned(),
                stderr: CANCELLED_MESSAGE.to_owned(),
            }
        );
        assert_eq!(
            host.progress_lines(),
            QUEUED_PENDING_JSON.lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn blocks_a_queued_stack_on_an_upstack_review_thread() {
        let host = scripted_stack(PENDING_CHECKS);
        let output = watch(
            &host,
            &[
                "--queued-stack",
                "--stack-prs",
                "7,8",
                "--allow-draft",
                "--pretty",
            ],
        );
        assert_eq!(output.code, 3);
        assert_eq!(output.stdout, QUEUED_PRETTY_BLOCKER);
    }

    #[test]
    fn polls_pending_checks_until_they_pass() {
        let host = scripted_stack(PENDING_CHECKS);
        host.script(
            &key(checks_args(&repo_context(7))),
            Output::success(PASSING_CHECKS),
        );
        let output = watch(&host, &["--pr", "7"]);
        assert_eq!(output.code, 0);
        let ready_after_sleep = READY_JSON
            .replace("\"sequence\":1", "\"sequence\":2")
            .replace(EPOCH_OBSERVED_AT, AFTER_ONE_POLL_OBSERVED_AT);
        assert_eq!(
            output.stdout,
            format!("{WAITING_JSON}\n{ready_after_sleep}")
        );
        assert_eq!(host.progress_lines(), [WAITING_JSON]);
        assert_eq!(host.slept(), [Duration::from_secs(60)]);
    }

    #[test]
    fn cancelling_a_sleep_ends_the_run_with_what_it_printed() {
        let host = scripted_stack(PENDING_CHECKS);
        host.cancel_after(0);
        let output = watch(&host, &["--pr", "7"]);
        assert_eq!(
            output,
            Output {
                code: EXIT_CANCELLED,
                stdout: format!("{WAITING_JSON}\n"),
                stderr: CANCELLED_MESSAGE.to_owned(),
            }
        );
    }

    #[test]
    fn reports_a_status_query_blocker_once_the_error_budget_is_spent() {
        let host = fake_host();
        host.script(
            &key(pull_request_args(&repo_context(7))),
            Output::failure(1, "HTTP 502\n"),
        );
        let output = watch(&host, &["--pr", "7", "--max-query-errors", "1"]);
        assert_eq!(output.code, 7);
        assert_eq!(output.stdout, STATUS_QUERY_BLOCKER);
    }

    #[test]
    fn retries_a_failed_query_after_its_backoff() {
        let host = fake_host();
        host.script(
            &key(pull_request_args(&repo_context(7))),
            Output::failure(1, "HTTP 502\n"),
        );
        host.cancel_after(0);
        let output = watch(&host, &["--pr", "7"]);
        assert_eq!(output.code, EXIT_CANCELLED);
        assert_eq!(host.progress_lines(), [RETRY_PREFIX]);
    }

    #[test]
    fn crashes_like_upstream_when_gh_cannot_start() {
        let host = fake_host();
        host.script_error(
            &key(pull_request_args(&repo_context(7))),
            io::ErrorKind::NotFound,
        );
        let output = watch(&host, &["--pr", "7"]);
        assert_eq!(output.code, EXIT_CRASHED);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.starts_with("error: gh: "));
    }
}
