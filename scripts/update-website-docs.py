#!/usr/bin/env python3

import argparse
import base64
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from urllib.parse import quote

SOURCE = "caudra/caudra"
WEBSITE = "caudra/website"
WORKFLOW = ".github/workflows/rust.yml"
RELEASE_WORKFLOW = ".github/workflows/release.yml"
RELEASE_TAG = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)
BRANCH = "automation/docs-source"
MANIFEST = "docs-source.json"
SHA = re.compile(r"[0-9a-f]{40}")
INPUT = re.compile(r"docs/content/[^/]+\.md|docs/examples/[^/]+\.example\.toml")
SAFE_PATH = re.compile(r"[a-zA-Z0-9_./-]+")
REQUIRED_INPUTS = {"docs/navigation.json", "install.sh", "install.ps1"}
REQUIRED_JOBS = {"Format (Rust)", "Lint", "Test"}
DOCGEN_STEP = "Run make gen-docs-check"
COMMIT_PREFIX = "docs: update source to "
PAGE_SIZE = 100
MAX_PAGES = 100
MAX_TAG_DEPTH = 16
WEBSITE_WORKFLOW = ".github/workflows/site.yml"
WEBSITE_STEPS = {
    "Run bun run test:import",
    "Run bun run check",
    "Run bun run test",
    "Run bun run build",
    "Run bun run test:output",
    "Run bun run workers:check",
    "Run bun run test:browser",
    "Verify incomplete analytics configuration stays disabled",
    "Verify enabled analytics with a mocked tracker",
    "Build and verify deployment artifact",
}
CI_WAIT_SECONDS = 30 * 60
CI_POLL_SECONDS = 30
CI_TIMEOUT = "Timed out waiting for canonical Website CI; PR left open, retry run_id"


class Refusal(RuntimeError):
    pass


class PathFilteredRun(Refusal):
    pass


class SupersededRun(Refusal):
    pass


def require(condition, message):
    if not condition:
        raise Refusal(message)


def sha(value):
    require(
        isinstance(value, str) and SHA.fullmatch(value),
        "Expected a full commit/object SHA",
    )
    return value


class GitHub:
    def __init__(self, token):
        require(bool(token), "Missing GitHub credential")
        self.token = token

    def request(self, repo, path, method="GET", data=None, missing=False):
        endpoint = "/".join(part for part in (repo and f"repos/{repo}", path) if part)
        command = [
            "gh",
            "api",
            "--hostname",
            "github.com",
            "--include",
            "--method",
            method,
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            endpoint,
        ]
        if data is not None:
            command += ["--input", "-"]
        env = {
            key: value
            for key, value in os.environ.items()
            if key
            not in {
                "SOURCE_TOKEN",
                "WEBSITE_TOKEN",
                "GITHUB_TOKEN",
                "GH_TOKEN",
                "GH_ENTERPRISE_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN",
                "GH_DEBUG",
            }
        }
        env.update(GH_TOKEN=self.token, GH_HOST="github.com", GH_PROMPT_DISABLED="1")
        try:
            result = subprocess.run(
                command,
                input=json.dumps(data) if data is not None else None,
                capture_output=True,
                text=True,
                env=env,
                timeout=60,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            raise Refusal("Could not execute GitHub API request") from None
        response = result.stdout.replace("\r\n", "\n")
        headers, separator, body = response.partition("\n\n")
        status = re.match(r"HTTP/\S+ (\d{3})", headers)
        code = int(status[1]) if status else 0
        if missing and code == 404:
            return None
        if repo == WEBSITE and path.startswith("actions/") and code == 403:
            raise Refusal(
                "Website CI is unreadable: grant the website GitHub App Actions: read "
                "and approve its installation permissions, then retry"
            )
        require(
            result.returncode == 0 and 200 <= code < 300 and separator,
            f"GitHub API request failed: {method} {endpoint}",
        )
        if code == 204 and not body.strip():
            return None
        try:
            return json.loads(body)
        except ValueError:
            raise Refusal("GitHub returned invalid JSON") from None


def head(api, repo, branch="main", missing=False):
    ref = api.request(repo, f"git/ref/heads/{branch}", missing=missing)
    if ref is None:
        return None
    require(ref["object"]["type"] == "commit", "Branch does not reference a commit")
    return sha(ref["object"]["sha"])


def successful(item):
    return item.get("status") == "completed" and item.get("conclusion") == "success"


def verified_source(source, run_id):
    require(type(run_id) is int and run_id > 0, "Expected a positive Rust run ID")
    workflow = source.request(SOURCE, "actions/workflows/rust.yml")
    require(
        workflow["path"] == WORKFLOW and workflow["state"] == "active",
        "Rust workflow identity is invalid",
    )
    run = source.request(SOURCE, f"actions/runs/{run_id}")
    require(
        run["id"] == run_id
        and run["workflow_id"] == workflow["id"]
        and run["path"] == WORKFLOW
        and run["repository"]["full_name"] == SOURCE
        and run["head_repository"]["full_name"] == SOURCE
        and run["event"] == "push"
        and run["head_branch"] == "main"
        and successful(run),
        "Run must be a successful canonical Rust push/main run",
    )
    revision = sha(run["head_sha"])
    current = head(source, SOURCE)
    require(current is not None, "Current source main is unavailable")
    attempt = run["run_attempt"]
    require(type(attempt) is int and attempt > 0, "Invalid workflow attempt")
    jobs = []
    for page in range(1, MAX_PAGES + 1):
        result = source.request(
            SOURCE,
            f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page={PAGE_SIZE}&page={page}",
        )
        jobs.extend(result["jobs"])
        if len(jobs) == result["total_count"]:
            break
        require(
            result["jobs"] and len(jobs) < result["total_count"],
            "Incomplete workflow jobs",
        )
    else:
        raise Refusal("Too many workflow jobs")
    required = []
    for name in REQUIRED_JOBS:
        matches = [job for job in jobs if job["name"] == name]
        require(len(matches) == 1, "Required Rust job is missing or duplicated")
        required.append(matches[0])
    if all(
        job.get("status") == "completed" and job.get("conclusion") == "skipped"
        for job in required
    ):
        for name in ("Detect changes", "CI"):
            matches = [job for job in jobs if job["name"] == name]
            require(
                len(matches) == 1 and successful(matches[0]),
                "Path-filter gate did not pass",
            )
        raise PathFilteredRun("Rust verification was skipped by path filtering")
    for job in required:
        require(
            successful(job),
            "Required Rust verification did not pass",
        )
        if job["name"] == "Test":
            steps = [
                step for step in job.get("steps", []) if step["name"] == DOCGEN_STEP
            ]
            require(
                len(steps) == 1 and successful(steps[0]),
                "Documentation drift check did not pass",
            )
    if current != revision:
        raise SupersededRun("Verified source is no longer current main")
    return revision


def eligible_tag(tag):
    match = RELEASE_TAG.fullmatch(tag) if isinstance(tag, str) else None
    return bool(match) and all(
        not part.isdigit() or part == "0" or not part.startswith("0")
        for part in (match[4] or "").split(".")
    )


def published_at(release):
    value = release.get("published_at")
    require(isinstance(value, str), "Published release is missing its publication time")
    timestamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(timestamp.tzinfo is not None, "Release publication time needs a timezone")
    return timestamp.astimezone(timezone.utc)


def verified_release(source, run_id):
    require(type(run_id) is int and run_id > 0, "Expected a positive Release run ID")
    workflow = source.request(SOURCE, "actions/workflows/release.yml")
    require(
        workflow["path"] == RELEASE_WORKFLOW and workflow["state"] == "active",
        "Release workflow identity is invalid",
    )
    run = source.request(SOURCE, f"actions/runs/{run_id}")
    require(
        run["id"] == run_id
        and run["workflow_id"] == workflow["id"]
        and run["path"] == RELEASE_WORKFLOW
        and run["repository"]["full_name"] == SOURCE
        and run["head_repository"]["full_name"] == SOURCE
        and run["event"] == "push"
        and successful(run),
        "Run must be a completed successful canonical Release push/tag run",
    )
    tag = run["head_branch"]
    require(eligible_tag(tag), "Release run must name a strict v-prefixed SemVer tag")
    revision = sha(run["head_sha"])
    target = source.request(SOURCE, f"git/ref/tags/{quote(tag, safe='')}")["object"]
    seen = set()
    while target["type"] == "tag":
        object_sha = sha(target["sha"])
        require(
            object_sha not in seen and len(seen) < MAX_TAG_DEPTH,
            "Cyclic or excessively nested annotated release tag",
        )
        seen.add(object_sha)
        target = source.request(SOURCE, f"git/tags/{object_sha}")["object"]
    require(
        target["type"] == "commit" and sha(target["sha"]) == revision,
        "Release tag no longer resolves to the verified run revision",
    )
    release = source.request(SOURCE, f"releases/tags/{quote(tag, safe='')}")
    require(
        release["tag_name"] == tag
        and release.get("draft") is False
        and type(release.get("id")) is int
        and release["id"] > 0,
        "Release must be published, non-draft, and match the run tag",
    )
    publication = published_at(release)
    candidates = []
    for page in range(1, MAX_PAGES + 1):
        releases = source.request(SOURCE, f"releases?per_page={PAGE_SIZE}&page={page}")
        require(isinstance(releases, list), "Invalid releases response")
        candidates.extend(
            item
            for item in releases
            if item.get("draft") is False and eligible_tag(item.get("tag_name"))
        )
        if len(releases) < PAGE_SIZE:
            break
    else:
        raise Refusal("Too many releases to verify latest publication")
    require(candidates, "No eligible published Caudra release found")
    latest_time = max(published_at(item) for item in candidates)
    latest = [item for item in candidates if published_at(item) == latest_time]
    require(len(latest) == 1, "Latest eligible published release is ambiguous")
    if latest[0]["id"] != release["id"] or latest_time != publication:
        raise SupersededRun("Release is no longer the latest eligible publication")
    require(latest[0]["tag_name"] == tag, "Published release identity changed")
    return {"version": 1, "run_id": run_id, "tag": tag, "revision": revision}


def run_kind(source, run_id):
    require(type(run_id) is int and run_id > 0, "Expected a positive workflow run ID")
    run = source.request(SOURCE, f"actions/runs/{run_id}")
    kinds = {WORKFLOW: "rust", RELEASE_WORKFLOW: "release"}
    require(run.get("path") in kinds, "Run must use canonical Rust or Release workflow")
    return kinds[run["path"]]


def dispatch_release(source, website, run_id):
    payload = verified_release(source, run_id)
    require(
        verified_release(source, run_id) == payload,
        "Release provenance changed before dispatch; retry its canonical run",
    )
    website.request(
        WEBSITE,
        "dispatches",
        method="POST",
        data={"event_type": "caudra-release", "client_payload": payload},
    )
    return (
        f"Website release dispatch sent for {payload['tag']} at {payload['revision']}"
    )


def action_items(api, repo, path, key):
    items = []
    separator = "&" if "?" in path else "?"
    for page in range(1, MAX_PAGES + 1):
        result = api.request(repo, f"{path}{separator}per_page={PAGE_SIZE}&page={page}")
        items.extend(result[key])
        if len(items) == result["total_count"]:
            return items
        require(
            result[key] and len(items) < result["total_count"],
            "Incomplete Actions response",
        )
    raise Refusal("Too many Actions results to verify safely")


def verified_website_ci(website, number, main, branch):
    workflow = website.request(WEBSITE, "actions/workflows/site.yml")
    require(
        workflow["path"] == WEBSITE_WORKFLOW
        and workflow["name"] == "Website"
        and workflow["state"] == "active",
        "Canonical Website workflow identity is invalid",
    )
    runs = action_items(
        website,
        WEBSITE,
        f"actions/workflows/{workflow['id']}/runs?event=pull_request&head_sha={branch}",
        "workflow_runs",
    )
    if not runs:
        return None
    require(
        all(type(run.get("id")) is int and run["id"] > 0 for run in runs),
        "Invalid Website workflow run ID",
    )
    run_id = max(run["id"] for run in runs)
    run = website.request(WEBSITE, f"actions/runs/{run_id}")
    require(
        run["id"] == run_id
        and run["workflow_id"] == workflow["id"]
        and run["path"] == WEBSITE_WORKFLOW
        and run["repository"]["full_name"] == WEBSITE
        and run["head_repository"]["full_name"] == WEBSITE
        and run["event"] == "pull_request"
        and run["head_branch"] == BRANCH
        and run["head_sha"] == branch,
        "Website CI must be a canonical pull_request run for this exact bot branch",
    )
    pulls = run["pull_requests"]
    require(
        len(pulls) == 1
        and pulls[0]["number"] == number
        and pulls[0]["head"]["sha"] == branch
        and pulls[0]["base"]["sha"] == main
        and pulls[0]["base"]["ref"] == "main",
        "Website CI did not verify this exact PR head and current main; rebase and retry",
    )
    if run["status"] != "completed":
        require(
            run["status"]
            in {"queued", "in_progress", "waiting", "requested", "pending"},
            "Unexpected Website CI status",
        )
        return None
    require(
        successful(run),
        "Website CI failed, was cancelled, or skipped; PR was not merged",
    )
    attempt = run["run_attempt"]
    require(type(attempt) is int and attempt > 0, "Invalid Website workflow attempt")
    jobs = action_items(
        website, WEBSITE, f"actions/runs/{run_id}/attempts/{attempt}/jobs", "jobs"
    )
    builds = [job for job in jobs if job["name"] == "build"]
    require(
        len(builds) == 1 and successful(builds[0]),
        "Required Website build job did not pass",
    )
    for name in WEBSITE_STEPS:
        steps = [step for step in builds[0].get("steps", []) if step["name"] == name]
        require(
            len(steps) == 1 and successful(steps[0]),
            "Required Website test/build step did not pass",
        )
    return run_id, attempt


def merge_verified_pull(source, website, run_id, bot, main, branch, pull, guard):
    deadline = time.monotonic() + CI_WAIT_SECONDS
    while True:
        require(time.monotonic() < deadline, CI_TIMEOUT)
        guard()
        verified = verified_website_ci(website, pull["number"], main, branch)
        require(time.monotonic() < deadline, CI_TIMEOUT)
        if verified:
            break
        print("Waiting for canonical Website pull-request verification", flush=True)
        time.sleep(min(CI_POLL_SECONDS, max(0, deadline - time.monotonic())))
    revision = verified_source(source, run_id)
    pending, ancestry = owned_branch(website, main, branch, bot)
    require(
        pending == revision and ancestry == "ahead",
        "Merge requires the current verified pin on a bot-owned branch",
    )
    _, main_tree = tree_at(website, WEBSITE, main)
    _, branch_tree = tree_at(website, WEBSITE, branch)
    require(
        changed_paths(main_tree, branch_tree) == [MANIFEST],
        "Merge requires an exact manifest-only change",
    )
    repo = website.request(WEBSITE, "")
    method = "squash" if repo.get("allow_squash_merge") is True else "merge"
    require(
        method == "squash" or repo.get("allow_merge_commit") is True,
        "Enable squash or merge commits for caudra/website; no rebase or bypass is used",
    )
    require(
        verified_website_ci(website, pull["number"], main, branch) == verified,
        "Website CI changed before merge; retry",
    )
    guard()
    current = website.request(WEBSITE, f"pulls/{pull['number']}")
    require(
        current["state"] == "open"
        and current.get("draft") is False
        and bot_owned(current.get("user"), bot)
        and current["head"]["sha"] == branch
        and current["head"]["ref"] == BRANCH
        and current["head"]["repo"]["full_name"] == WEBSITE
        and current["base"]["sha"] == main
        and current["base"]["ref"] == "main"
        and current["base"]["repo"]["full_name"] == WEBSITE,
        "Pull request changed before merge; retry without rebasing or bypassing",
    )
    guard()
    result = website.request(
        WEBSITE,
        f"pulls/{pull['number']}/merge",
        method="PUT",
        data={"sha": branch, "merge_method": method},
    )
    require(
        result.get("merged") is True,
        "GitHub refused the conditional PR merge; inspect CI and retry",
    )


def tree_at(api, repo, revision):
    commit = api.request(repo, f"git/commits/{sha(revision)}")
    require(commit["sha"] == revision, "Commit resolution mismatch")
    tree_sha = sha(commit["tree"]["sha"])
    result = api.request(repo, f"git/trees/{tree_sha}?recursive=1")
    require(result.get("truncated") is False, "Refusing a truncated Git tree")
    entries = {}
    for item in result["tree"]:
        path = item["path"]
        require(path not in entries, "Duplicate Git tree entry")
        entries[path] = (item["mode"], item["type"], sha(item["sha"]))
    return tree_sha, entries


def inputs(source, revision):
    _, tree = tree_at(source, SOURCE, revision)
    selected = {
        path: value
        for path, value in tree.items()
        if path in REQUIRED_INPUTS or INPUT.fullmatch(path)
    }
    require(
        REQUIRED_INPUTS <= selected.keys(),
        "Source is missing required publishing inputs",
    )
    require(
        any(path.startswith("docs/content/") for path in selected)
        and any(path.startswith("docs/examples/") for path in selected),
        "Source must contain documentation and example files",
    )
    for path, (mode, kind, _) in selected.items():
        require(
            SAFE_PATH.fullmatch(path)
            and mode in {"100644", "100755"}
            and kind == "blob",
            "Publishing inputs must be ordinary files with safe names",
        )
    return selected


def pin(api, tree):
    require(
        MANIFEST in tree and tree[MANIFEST][:2] == ("100644", "blob"),
        "Website manifest must be an ordinary non-executable file",
    )
    blob = api.request(WEBSITE, f"git/blobs/{tree[MANIFEST][2]}")
    require(blob["encoding"] == "base64", "Unexpected manifest encoding")
    try:
        value = json.loads(
            base64.b64decode("".join(blob["content"].split()), validate=True)
        )
    except ValueError:
        raise Refusal("Invalid website manifest JSON") from None
    require(
        isinstance(value, dict)
        and set(value) == {"version", "repository", "revision"}
        and type(value["version"]) is int
        and value["version"] == 1
        and value["repository"] == SOURCE,
        "Invalid website source contract",
    )
    return sha(value["revision"])


def descendant(source, old, new):
    result = source.request(SOURCE, f"compare/{sha(old)}...{sha(new)}")
    require(
        result["status"] in {"ahead", "identical"}
        and result["merge_base_commit"]["sha"] == old,
        "Source pin is unavailable, reversed, or unrelated",
    )


def changed_paths(old, new):
    return sorted(
        path for path in old.keys() | new.keys() if old.get(path) != new.get(path)
    )


def bot_owned(user, bot):
    return (
        user
        and user.get("id") == bot["id"]
        and user.get("login") == bot["login"]
        and user.get("type") == "Bot"
    )


def owned_commit(commit, bot):
    message = commit["commit"]["message"]
    require(
        bot_owned(commit.get("author"), bot)
        and bot_owned(commit.get("committer"), bot)
        and message.startswith(COMMIT_PREFIX)
        and SHA.fullmatch(message.removeprefix(COMMIT_PREFIX)),
        "Automation branch contains a commit not owned by this bot",
    )


def owned_branch(website, main, branch, bot):
    owned_commit(website.request(WEBSITE, f"commits/{branch}"), bot)
    comparison = website.request(WEBSITE, f"compare/{main}...{branch}")
    require(
        comparison["status"] in {"ahead", "behind", "diverged", "identical"},
        "Unexpected automation branch ancestry",
    )
    commits = comparison["commits"]
    require(
        len(commits) == comparison["total_commits"] and len(commits) <= 250,
        "Cannot validate complete automation branch history",
    )
    for commit in commits:
        owned_commit(commit, bot)
    _, base_tree = tree_at(
        website, WEBSITE, sha(comparison["merge_base_commit"]["sha"])
    )
    _, branch_tree = tree_at(website, WEBSITE, branch)
    require(
        set(changed_paths(base_tree, branch_tree)) <= {MANIFEST},
        "Automation branch changes files other than the source manifest",
    )
    return pin(website, branch_tree), comparison["status"]


def open_pr(website, bot):
    pulls = website.request(
        WEBSITE, f"pulls?state=open&head=caudra:{BRANCH}&per_page={PAGE_SIZE}"
    )
    require(len(pulls) <= 1, "Multiple automation pull requests exist")
    if not pulls:
        return None
    pull = pulls[0]
    require(
        pull["state"] == "open"
        and bot_owned(pull.get("user"), bot)
        and pull["head"]["ref"] == BRANCH
        and pull["head"]["repo"]["full_name"] == WEBSITE
        and pull["base"]["ref"] == "main"
        and pull["base"]["repo"]["full_name"] == WEBSITE,
        "Existing pull request is not owned by this automation",
    )
    return pull


def update(source, website, run_id, bot):
    require(
        bot.get("type") == "Bot" and type(bot.get("id")) is int and bot["id"] > 0,
        "Expected a GitHub App bot identity",
    )
    revision = verified_source(source, run_id)
    main = head(website, WEBSITE)
    main_tree_sha, main_tree = tree_at(website, WEBSITE, main)
    old = pin(website, main_tree)
    descendant(source, old, revision)
    changes = changed_paths(inputs(source, old), inputs(source, revision))
    branch = head(website, WEBSITE, BRANCH, missing=True)
    pull = open_pr(website, bot)
    require(
        branch is not None or pull is None,
        "Pull request exists without its automation branch",
    )
    pending = None
    ancestry = None
    if branch:
        pending, ancestry = owned_branch(website, main, branch, bot)
        descendant(source, pending, revision)
    if (
        not changes
        and not pull
        and not (ancestry in {"ahead", "diverged"} and pending != old)
    ):
        return "Publishing inputs unchanged; no update needed"

    def guard():
        require(
            head(source, SOURCE) == revision,
            "Source main advanced; retry with its verified run",
        )
        require(
            head(website, WEBSITE) == main,
            "Website main advanced; rerun to preserve its changes",
        )
        require(
            head(website, WEBSITE, BRANCH, missing=True) == branch,
            "Automation branch changed concurrently; rerun",
        )
        current = open_pr(website, bot)
        require(
            (current or {}).get("number") == (pull or {}).get("number"),
            "Automation pull request changed concurrently; rerun",
        )

    def mutate(path, method, data):
        guard()
        return website.request(WEBSITE, path, method=method, data=data)

    if pending != revision or ancestry != "ahead":
        content = (
            json.dumps(
                {"version": 1, "repository": SOURCE, "revision": revision}, indent=2
            )
            + "\n"
        )
        tree = mutate(
            "git/trees",
            "POST",
            {
                "base_tree": main_tree_sha,
                "tree": [
                    {
                        "path": MANIFEST,
                        "mode": "100644",
                        "type": "blob",
                        "content": content,
                    }
                ],
            },
        )
        parents = [branch] if branch else [main]
        if branch and branch != main and ancestry != "ahead":
            parents.append(main)
        identity = {
            "name": bot["login"],
            "email": f"{bot['id']}+{bot['login']}@users.noreply.github.com",
        }
        commit = mutate(
            "git/commits",
            "POST",
            {
                "message": COMMIT_PREFIX + revision,
                "tree": sha(tree["sha"]),
                "parents": parents,
                "author": identity,
                "committer": identity,
            },
        )
        commit_sha = sha(commit["sha"])
        if branch:
            mutate(
                f"git/refs/heads/{BRANCH}", "PATCH", {"sha": commit_sha, "force": False}
            )
        else:
            mutate(
                "git/refs", "POST", {"ref": f"refs/heads/{BRANCH}", "sha": commit_sha}
            )
        branch = commit_sha
    title = "docs: update verified Caudra source"
    body = (
        f"Update `{MANIFEST}` only. Canonical website CI must verify this pin before bot merge.\n\n"
        f"Source: `{SOURCE}`\n\nPrevious: `{old}`\n\nProposed: `{revision}`\n\n"
        f"Verified Rust push/main run: `{run_id}` (format, lint, tests, documentation drift).\n\n"
        "Changed publishing inputs (blob or mode):\n"
        + (
            "".join(f"- `{path}`\n" for path in changes)
            if changes
            else "None; supersedes the pending source update.\n"
        )
        + "\nThe App waits for canonical Website PR tests/build, then requests an ordinary "
        "expected-SHA merge without admin bypass. "
        "Main updates the automatic preview only. Production requires a separately "
        "verified successful Caudra Release run; preview1 remains immutable.\n"
    )
    if pull:
        if (
            pull.get("title") != title
            or pull.get("body") != body
            or pull.get("maintainer_can_modify")
        ):
            mutate(
                f"pulls/{pull['number']}",
                "PATCH",
                {
                    "title": title,
                    "body": body,
                    "maintainer_can_modify": False,
                },
            )
    else:
        pull = mutate(
            "pulls",
            "POST",
            {
                "title": title,
                "body": body,
                "head": BRANCH,
                "base": "main",
                "maintainer_can_modify": False,
            },
        )
    merge_verified_pull(source, website, run_id, bot, main, branch, pull, guard)
    return f"Website source update merged after canonical CI at {revision}"


def main():
    parser = argparse.ArgumentParser(
        description="Publish verified Caudra preview inputs or dispatch a verified release"
    )
    parser.add_argument(
        "--run-id",
        type=int,
        required=True,
        help="Canonical Rust push/main or Release push/tag workflow run ID",
    )
    parser.add_argument(
        "--verify-only",
        action="store_true",
        help="Validate source using SOURCE_TOKEN only; path-filtered or superseded workflow_run is a no-op",
    )
    parser.add_argument(
        "--app-slug", help="GitHub App slug from actions/create-github-app-token"
    )
    args = parser.parse_args()
    try:
        source = GitHub(os.environ.get("SOURCE_TOKEN"))
        kind = run_kind(source, args.run_id)
        if args.verify_only:
            try:
                revision = (
                    verified_source(source, args.run_id)
                    if kind == "rust"
                    else verified_release(source, args.run_id)["revision"]
                )
            except (PathFilteredRun, SupersededRun) as error:
                if os.environ.get("GITHUB_EVENT_NAME") != "workflow_run":
                    raise
                eligible = "false"
                print(f"No-op: {error}; source is not eligible for website publishing")
            else:
                eligible = "true"
                print(f"Verified source: {revision}")
            if output := os.environ.get("GITHUB_OUTPUT"):
                with open(output, "a", encoding="utf-8") as stream:
                    stream.write(f"eligible={eligible}\n")
                    if eligible == "true":
                        stream.write(f"kind={kind}\n")
        elif kind == "release":
            print(
                dispatch_release(
                    source, GitHub(os.environ.get("WEBSITE_TOKEN")), args.run_id
                )
            )
        else:
            require(
                args.app_slug and re.fullmatch(r"[a-z0-9][a-z0-9-]*", args.app_slug),
                "Invalid GitHub App slug",
            )
            login = args.app_slug + "[bot]"
            bot = source.request(None, f"users/{login}")
            require(bot.get("login") == login, "GitHub App identity mismatch")
            print(
                update(
                    source, GitHub(os.environ.get("WEBSITE_TOKEN")), args.run_id, bot
                )
            )
    except Refusal as error:
        print(f"Refused: {error}", file=sys.stderr)
        return 1
    except (KeyError, TypeError, ValueError):
        print("Refused: malformed GitHub API response", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
