#!/usr/bin/env python3

import argparse
import base64
import json
import os
import re
import subprocess
import sys

SOURCE = "caudra/caudra"
WEBSITE = "caudra/website"
WORKFLOW = ".github/workflows/rust.yml"
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
        endpoint = f"repos/{repo}/{path}" if repo else path
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
        require(
            result.returncode == 0 and 200 <= code < 300 and separator,
            f"GitHub API request failed: {method} {endpoint}",
        )
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
        f"Update `{MANIFEST}` only. Website CI must verify this pin before review/merge.\n\n"
        f"Source: `{SOURCE}`\n\nPrevious: `{old}`\n\nProposed: `{revision}`\n\n"
        f"Verified Rust push/main run: `{run_id}` (format, lint, tests, documentation drift).\n\n"
        "Changed publishing inputs (blob or mode):\n"
        + (
            "".join(f"- `{path}`\n" for path in changes)
            if changes
            else "None; supersedes the pending source update.\n"
        )
        + "\nNo automatic merge or deployment.\n"
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
        mutate(
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
    return f"Website source update proposed at {revision}"


def main():
    parser = argparse.ArgumentParser(
        description="Propose verified Caudra publishing inputs to caudra/website"
    )
    parser.add_argument(
        "--run-id",
        type=int,
        required=True,
        help="Canonical Rust push/main workflow run ID",
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
        if args.verify_only:
            try:
                revision = verified_source(source, args.run_id)
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
