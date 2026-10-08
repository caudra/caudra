#!/usr/bin/env python3

import base64
import copy
import importlib.util
import io
import json
import os
import subprocess
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "update_website_docs", Path(__file__).with_name("update-website-docs.py")
)
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load update-website-docs.py")
UPDATER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(UPDATER)

OLD = "1" * 40
NEW = "2" * 40
REVERT = "8" * 40
MAIN = "3" * 40
BRANCH = "4" * 40
TREE = "5" * 40
BLOB = "6" * 40
COMMIT = "7" * 40
BOT = {"login": "docs-app[bot]", "id": 123, "type": "Bot"}
INPUTS = (
    "docs/content/index.md",
    "docs/navigation.json",
    "docs/examples/config.example.toml",
    "install.sh",
    "install.ps1",
)


def manifest(revision):
    return {"version": 1, "repository": "caudra/caudra", "revision": revision}


def entry(path, sha=BLOB, mode="100644"):
    return {"path": path, "type": "blob", "mode": mode, "sha": sha}


class FakeAPI:
    def __init__(self, source=False):
        self.source = source
        self.mutations = []
        self.before_request = None
        self.branch = None
        self.main = NEW if source else MAIN
        self.prs = []
        self.workflow = {
            "id": 99,
            "path": ".github/workflows/rust.yml",
            "state": "active",
        }
        self.run = {
            "id": 42,
            "workflow_id": 99,
            "path": ".github/workflows/rust.yml",
            "repository": {"full_name": "caudra/caudra"},
            "head_repository": {"full_name": "caudra/caudra"},
            "event": "push",
            "head_branch": "main",
            "head_sha": NEW,
            "status": "completed",
            "conclusion": "success",
            "run_attempt": 1,
        }
        self.jobs: list[dict] = [
            {"name": name, "status": "completed", "conclusion": "success"}
            for name in ("Format (Rust)", "Lint", "Test")
        ]
        self.jobs[-1]["steps"] = [
            {
                "name": "Run make gen-docs-check",
                "status": "completed",
                "conclusion": "success",
            }
        ]
        self.trees = {
            OLD: [entry(path) for path in INPUTS],
            NEW: [entry(path, NEW if path == INPUTS[0] else BLOB) for path in INPUTS],
            MAIN: [entry("docs-source.json", OLD), entry("package.json")],
            BRANCH: [entry("docs-source.json", NEW), entry("package.json")],
        }
        self.blobs = {OLD: manifest(OLD), NEW: manifest(NEW)}
        self.commits = {}
        self.comparison = None
        self.truncated = False
        self.site_workflow = {
            "id": 100,
            "name": "Website",
            "path": UPDATER.WEBSITE_WORKFLOW,
            "state": "active",
        }
        self.site_run: dict = {}
        self.site_jobs: list[dict] = [
            {
                "name": "build",
                "status": "completed",
                "conclusion": "success",
                "steps": [
                    {"name": name, "status": "completed", "conclusion": "success"}
                    for name in sorted(UPDATER.WEBSITE_STEPS)
                ],
            }
        ]
        self.repo_settings = {"allow_squash_merge": True, "allow_merge_commit": True}

    def bot_commit(self, sha):
        return {
            "sha": sha,
            "author": BOT.copy(),
            "committer": BOT.copy(),
            "commit": {"message": f"docs: update source to {NEW}"},
        }

    def pull(self):
        return {
            "number": 10,
            "state": "open",
            "draft": False,
            "user": BOT.copy(),
            "head": {
                "ref": "automation/docs-source",
                "repo": {"full_name": "caudra/website"},
                "sha": self.branch,
            },
            "base": {
                "ref": "main",
                "repo": {"full_name": "caudra/website"},
                "sha": self.main,
            },
        }

    def website_run(self):
        return {
            **self.run,
            "id": 101,
            "workflow_id": 100,
            "path": UPDATER.WEBSITE_WORKFLOW,
            "repository": {"full_name": UPDATER.WEBSITE},
            "head_repository": {"full_name": UPDATER.WEBSITE},
            "event": "pull_request",
            "head_branch": UPDATER.BRANCH,
            "head_sha": self.branch,
            "pull_requests": [self.pull()],
        }

    def request(self, repo, path, method="GET", data=None, missing=False):
        if repo is None and path == f"users/{BOT['login']}":
            return BOT
        expected = "caudra/caudra" if self.source else "caudra/website"
        if repo != expected:
            raise AssertionError(f"Token used for wrong repository: {repo}")
        if self.before_request:
            self.before_request(path, method)
        if method != "GET":
            assert data is not None
            self.mutations.append((path, method, data))
            if path == "git/trees":
                self.trees[TREE] = copy.deepcopy(self.trees[MAIN])
                self.trees[TREE][0] = entry("docs-source.json", COMMIT)
                self.blobs[COMMIT] = json.loads(data["tree"][0]["content"])
                return {"sha": TREE}
            if path == "git/commits":
                self.trees[COMMIT] = self.trees[TREE]
                self.commits[COMMIT] = self.bot_commit(COMMIT)
                return {"sha": COMMIT}
            if path in ("git/refs", "git/refs/heads/automation/docs-source"):
                if data.get("force") is True:
                    raise AssertionError("Force update forbidden")
                self.branch = data["sha"]
                self.comparison = None
                for pull in self.prs:
                    pull["head"]["sha"] = self.branch
                return {}
            if path == "pulls":
                self.prs.append(self.pull())
                self.prs[0].update(
                    {
                        key: data[key]
                        for key in ("title", "body", "maintainer_can_modify")
                    }
                )
                return self.prs[0]
            if path == "pulls/10":
                self.prs[0].update(data)
                return self.prs[0]
            if path == "pulls/10/merge":
                self.main = self.branch
                self.prs.clear()
                return {"merged": True}
            if path == "dispatches":
                return None
            raise AssertionError(f"Unexpected mutation {path}")
        if path == "":
            return self.repo_settings
        if path == "actions/workflows/site.yml":
            return self.site_workflow
        if path.startswith("actions/workflows/100/runs?"):
            run = self.site_run or self.website_run()
            return {"total_count": 1, "workflow_runs": [run]}
        if path == "actions/runs/101":
            return self.site_run or self.website_run()
        if path.startswith("actions/runs/101/attempts/1/jobs?"):
            page = int(path.rsplit("=", 1)[1])
            return {
                "total_count": len(self.site_jobs),
                "jobs": self.site_jobs[(page - 1) * 100 : page * 100],
            }
        if path == "pulls/10":
            return self.prs[0]
        if path == "actions/workflows/rust.yml":
            return self.workflow
        if path == "actions/runs/42":
            return self.run
        if path.startswith("actions/runs/42/attempts/1/jobs?"):
            page = int(path.rsplit("=", 1)[1])
            return {
                "total_count": len(self.jobs),
                "jobs": self.jobs[(page - 1) * 100 : page * 100],
            }
        if path == "git/ref/heads/main":
            return {"object": {"type": "commit", "sha": self.main}}
        if path == "git/ref/heads/automation/docs-source":
            if self.branch is None:
                if not missing:
                    raise AssertionError("Expected optional branch lookup")
                return None
            return {"object": {"type": "commit", "sha": self.branch}}
        if path.startswith("git/commits/"):
            sha = path.removeprefix("git/commits/")
            return {"sha": sha, "tree": {"sha": sha}}
        if path.startswith("git/trees/"):
            sha = path.removeprefix("git/trees/").split("?")[0]
            return {"truncated": self.truncated, "tree": self.trees[sha]}
        if path.startswith("git/blobs/"):
            sha = path.removeprefix("git/blobs/")
            return {
                "encoding": "base64",
                "content": base64.b64encode(
                    json.dumps(self.blobs[sha]).encode()
                ).decode(),
            }
        if path.startswith("compare/"):
            old, new = path.removeprefix("compare/").split("...")
            if self.comparison is not None:
                return self.comparison
            return {
                "status": "identical" if old == new else "ahead",
                "merge_base_commit": {"sha": old},
                "total_commits": 1,
                "commits": [self.commits.get(new, self.bot_commit(new))],
            }
        if path.startswith("commits/"):
            sha = path.removeprefix("commits/")
            return self.commits.get(sha, self.bot_commit(sha))
        if path.startswith("pulls?"):
            return self.prs
        raise AssertionError(f"Unexpected read {path}")


class UpdateTests(unittest.TestCase):
    def setUp(self):
        self.source = FakeAPI(source=True)
        self.website = FakeAPI()
        merge = patch.object(UPDATER, "merge_verified_pull")
        self.merge = merge.start()
        self.addCleanup(merge.stop)

    def update(self):
        return UPDATER.update(self.source, self.website, 42, BOT)

    def refuse(self):
        with self.assertRaises(UPDATER.Refusal):
            self.update()
        self.assertEqual(self.website.mutations, [])

    def test_successful_new_pr_changes_only_manifest(self):
        self.update()
        writes = self.website.mutations
        self.assertEqual(
            [w[0] for w in writes], ["git/trees", "git/commits", "git/refs", "pulls"]
        )
        self.assertEqual(writes[0][2]["base_tree"], MAIN)
        self.assertEqual(len(writes[0][2]["tree"]), 1)
        self.assertEqual(writes[0][2]["tree"][0]["path"], "docs-source.json")
        self.assertEqual(json.loads(writes[0][2]["tree"][0]["content"]), manifest(NEW))
        self.assertEqual(writes[1][2]["parents"], [MAIN])
        self.assertFalse(writes[-1][2]["maintainer_can_modify"])
        for text in (OLD, NEW, INPUTS[0]):
            self.assertIn(text, writes[-1][2]["body"])
        self.assertEqual(self.source.mutations, [])

    def test_invalid_runs_and_workflow_identity(self):
        for key, value in (
            ("repository", {"full_name": "attacker/caudra"}),
            ("head_repository", {"full_name": "attacker/caudra"}),
            ("event", "pull_request"),
            ("head_branch", "feature"),
            ("path", ".github/workflows/other.yml"),
            ("workflow_id", 12),
            ("conclusion", "failure"),
            ("status", "in_progress"),
            ("head_sha", "main"),
        ):
            with self.subTest(key=key):
                original = self.source.run[key]
                self.source.run[key] = value
                self.refuse()
                self.source.run[key] = original

    def test_successful_gate_cannot_hide_skipped_jobs_or_docgen(self):
        for job in self.source.jobs:
            with self.subTest(job=job["name"]):
                job["conclusion"] = "skipped"
                self.refuse()
                job["conclusion"] = "success"
        self.source.jobs[-1]["steps"][0]["conclusion"] = "skipped"
        self.refuse()

    def test_docgen_requires_one_exact_make_step(self):
        step = self.source.jobs[-1]["steps"][0]
        for steps in (
            [],
            [step, step],
            [{**step, "name": "Run just gen-docs-check"}],
        ):
            with self.subTest(steps=steps):
                self.source.jobs[-1]["steps"] = steps
                self.refuse()

    def test_no_content_change_ignores_non_imported_paths(self):
        self.source.trees[NEW] = copy.deepcopy(self.source.trees[OLD]) + [
            entry(path, NEW)
            for path in (
                "src/main.rs",
                "docs/AGENTS.md",
                "docs/content/nested/page.md",
                "docs/content/page.mdx",
                "docs/examples/not-an-example.toml",
            )
        ]
        self.update()
        self.assertEqual(self.website.mutations, [])

    def reverted_source(self):
        self.source.main = REVERT
        self.source.run["head_sha"] = REVERT
        self.source.trees[REVERT] = copy.deepcopy(self.source.trees[OLD])
        self.website.branch = BRANCH

    def test_reverted_inputs_advance_pending_branch_and_pr(self):
        for existing_pr in (False, True):
            with self.subTest(existing_pr=existing_pr):
                self.source = FakeAPI(source=True)
                self.website = FakeAPI()
                self.reverted_source()
                if existing_pr:
                    self.website.prs = [self.website.pull()]
                self.update()
                writes = self.website.mutations
                self.assertEqual(
                    [write[0] for write in writes],
                    [
                        "git/trees",
                        "git/commits",
                        "git/refs/heads/automation/docs-source",
                        "pulls/10" if existing_pr else "pulls",
                    ],
                )
                self.assertEqual(
                    json.loads(writes[0][2]["tree"][0]["content"]), manifest(REVERT)
                )
                self.assertEqual(writes[1][2]["parents"], [BRANCH])
                self.assertEqual(writes[2][2], {"sha": COMMIT, "force": False})
                self.assertIn(REVERT, self.website.prs[0]["body"])
                self.assertIn(
                    "None; supersedes the pending source update.",
                    self.website.prs[0]["body"],
                )
                self.website.mutations.clear()
                self.update()
                self.assertEqual(self.website.mutations, [])

    def test_reverted_inputs_still_refuse_human_edits(self):
        self.reverted_source()
        self.website.trees[BRANCH].append(entry("human-edit.txt"))
        self.refuse()

    def test_reverted_inputs_cannot_move_pending_pin_backwards(self):
        self.reverted_source()
        original = self.source.request

        def request(repo, path, **kwargs):
            if path == f"compare/{NEW}...{REVERT}":
                return {"status": "behind", "merge_base_commit": {"sha": REVERT}}
            return original(repo, path, **kwargs)

        with patch.object(self.source, "request", side_effect=request):
            self.refuse()

    def test_unchanged_inputs_with_merged_branch_remain_noop(self):
        self.reverted_source()
        self.website.comparison = {
            "status": "behind",
            "merge_base_commit": {"sha": BRANCH},
            "total_commits": 0,
            "commits": [],
        }
        self.update()
        self.assertEqual(self.website.mutations, [])

    def test_stale_source_run(self):
        self.source.main = OLD
        self.refuse()

    def test_unavailable_or_reversed_source(self):
        for status in ("behind", "diverged"):
            self.source.comparison = {
                "status": status,
                "merge_base_commit": {"sha": NEW},
            }
            self.refuse()
        self.source.comparison = None
        with patch.object(
            self.source, "request", side_effect=UPDATER.Refusal("Unavailable")
        ):
            self.refuse()

    def test_truncated_tree_and_invalid_modes(self):
        for api in (self.source, self.website):
            api.truncated = True
            self.refuse()
            api.truncated = False
        for mode in ("120000", "160000"):
            self.source.trees[NEW][0]["mode"] = mode
            self.refuse()

    def test_required_inputs_and_manifest_validation(self):
        for path in INPUTS:
            self.source.trees[NEW] = [entry(item) for item in INPUTS if item != path]
            self.refuse()
        self.source = FakeAPI(source=True)
        for value in (
            manifest("main"),
            {**manifest(OLD), "repository": "other/repo"},
            {**manifest(OLD), "version": 2},
        ):
            self.website.blobs[OLD] = value
            self.refuse()

    def test_partial_branch_publication_recovers_one_pr(self):
        self.website.branch = BRANCH
        self.update()
        self.assertEqual([w[0] for w in self.website.mutations], ["pulls"])
        self.website.mutations.clear()
        self.update()
        self.assertEqual(self.website.mutations, [])
        self.assertEqual(len(self.website.prs), 1)

    def test_refuses_human_branch_pr_and_extra_changes(self):
        self.website.branch = BRANCH
        human = self.website.bot_commit(BRANCH)
        human["committer"] = {"login": "human", "id": 456, "type": "User"}
        self.website.commits[BRANCH] = human
        self.refuse()
        self.website.commits.clear()
        self.website.trees[BRANCH].append(entry("surprise.txt"))
        self.refuse()
        self.website.trees[BRANCH].pop()
        self.website.prs = [self.website.pull()]
        self.website.prs[0]["user"] = human["committer"]
        self.refuse()

    def test_main_advance_uses_two_parents_and_current_main_tree(self):
        self.website.branch = BRANCH
        self.website.trees[BRANCH][0] = entry("docs-source.json", OLD)
        self.website.trees[OLD] = [
            entry("docs-source.json", OLD),
            entry("package.json"),
        ]
        self.website.trees[MAIN].append(entry("new-website-file"))
        self.website.comparison = {
            "status": "diverged",
            "merge_base_commit": {"sha": OLD},
            "total_commits": 1,
            "commits": [self.website.bot_commit(BRANCH)],
        }
        self.update()
        self.assertEqual(self.website.mutations[0][2]["base_tree"], MAIN)
        self.assertEqual(self.website.mutations[1][2]["parents"], [BRANCH, MAIN])
        self.assertEqual(self.website.mutations[2][2], {"sha": COMMIT, "force": False})

    def test_source_race_before_mutation_is_refused(self):
        reads = 0

        def race(path, method):
            nonlocal reads
            if path == "git/ref/heads/main":
                reads += 1
                if reads == 2:
                    self.source.main = OLD

        self.source.before_request = race
        self.refuse()

    def test_branch_race_before_ref_publication_is_refused(self):
        def race(path, method):
            if path == "git/commits" and method == "POST":
                self.website.branch = BRANCH

        self.website.before_request = race
        with self.assertRaises(UPDATER.Refusal):
            self.update()
        self.assertEqual(
            [w[0] for w in self.website.mutations], ["git/trees", "git/commits"]
        )

    def test_gh_error_never_prints_credentials(self):
        secret = "test-super-secret"
        result = subprocess.CompletedProcess([], 1, stdout="", stderr=secret)
        with (
            patch.object(UPDATER.subprocess, "run", return_value=result) as run,
            self.assertRaises(UPDATER.Refusal) as error,
        ):
            UPDATER.GitHub(secret).request("caudra/caudra", "git/ref/heads/main")
        self.assertNotIn(secret, str(error.exception))
        self.assertNotIn(secret, str(run.call_args.args))
        self.assertEqual(run.call_args.kwargs["env"]["GH_TOKEN"], secret)
        self.assertNotIn("SOURCE_TOKEN", run.call_args.kwargs["env"])

    def test_gh_does_not_inherit_other_token_or_debug(self):
        result = subprocess.CompletedProcess(
            [], 0, stdout="HTTP/2.0 200 OK\r\n\r\n{}", stderr=""
        )
        with (
            patch.dict(
                os.environ,
                {
                    "SOURCE_TOKEN": "source",
                    "WEBSITE_TOKEN": "website",
                    "GH_DEBUG": "api",
                },
            ),
            patch.object(UPDATER.subprocess, "run", return_value=result) as run,
        ):
            UPDATER.GitHub("selected").request("caudra/caudra", "git/ref/heads/main")
        env = run.call_args.kwargs["env"]
        self.assertNotIn("GH_DEBUG", env)
        self.assertNotIn("WEBSITE_TOKEN", env)

    def test_wrong_workflow_and_missing_or_duplicate_jobs(self):
        for key, value in (
            ("path", ".github/workflows/other.yml"),
            ("state", "disabled_manually"),
        ):
            original = self.source.workflow[key]
            self.source.workflow[key] = value
            self.refuse()
            self.source.workflow[key] = original
        self.source.jobs.append(copy.deepcopy(self.source.jobs[0]))
        self.refuse()
        self.source.jobs = self.source.jobs[:1]
        self.refuse()

    def test_required_job_on_later_page(self):
        self.source.jobs = [
            {"name": f"other-{i}"} for i in range(101)
        ] + self.source.jobs
        self.update()
        self.assertEqual(len(self.website.prs), 1)

    def test_newer_pending_pin_cannot_regress(self):
        self.website.branch = BRANCH
        self.website.trees[BRANCH][0] = entry("docs-source.json", COMMIT)
        self.website.blobs[COMMIT] = manifest(COMMIT)
        original = self.source.request

        def request(repo, path, **kwargs):
            if path == f"compare/{COMMIT}...{NEW}":
                return {"status": "behind", "merge_base_commit": {"sha": NEW}}
            return original(repo, path, **kwargs)

        with patch.object(self.source, "request", side_effect=request):
            self.refuse()

    def test_mode_addition_and_deletion_changes_are_provenance(self):
        self.source.trees[NEW] = copy.deepcopy(self.source.trees[OLD])
        self.source.trees[NEW][-1]["mode"] = "100755"
        self.source.trees[OLD].append(entry("docs/content/deleted.md"))
        self.source.trees[NEW].append(entry("docs/content/added.md"))
        self.update()
        body = self.website.prs[0]["body"]
        for path in ("install.ps1", "docs/content/deleted.md", "docs/content/added.md"):
            self.assertIn(path, body)
        self.assertNotIn("docs/content/index.md", body)

    def test_already_merged_branch_can_advance_without_duplicate_parents(self):
        self.website.branch = MAIN
        self.update()
        self.assertEqual(self.website.mutations[1][2]["parents"], [MAIN])

    def test_full_history_ownership_and_truncation(self):
        self.website.branch = BRANCH
        human = self.website.bot_commit(OLD)
        human["author"] = {"login": "human", "id": 456, "type": "User"}
        self.website.comparison = {
            "status": "ahead",
            "merge_base_commit": {"sha": MAIN},
            "total_commits": 2,
            "commits": [human, self.website.bot_commit(BRANCH)],
        }
        self.refuse()
        self.website.comparison["commits"] = [self.website.bot_commit(BRANCH)]
        self.website.comparison["total_commits"] = 251
        self.refuse()

    def test_website_main_race_refuses_all_mutations(self):
        reads = 0

        def race(path, method):
            nonlocal reads
            if path == "git/ref/heads/main":
                reads += 1
                if reads == 2:
                    self.website.main = OLD

        self.website.before_request = race
        self.refuse()

    def test_partial_pr_failure_recovers_then_noops(self):
        def fail_pr(path, method):
            if path == "pulls" and method == "POST":
                raise UPDATER.Refusal("Simulated PR outage")

        self.website.before_request = fail_pr
        with self.assertRaises(UPDATER.Refusal):
            self.update()
        self.assertEqual(self.website.branch, COMMIT)
        self.website.before_request = None
        self.website.mutations.clear()
        self.update()
        self.assertEqual([w[0] for w in self.website.mutations], ["pulls"])
        self.website.mutations.clear()
        self.update()
        self.assertEqual(self.website.mutations, [])

    def test_ref_race_is_not_retried_or_forced(self):
        def fail_ref(path, method):
            if path == "git/refs" and method == "POST":
                raise UPDATER.Refusal("Reference already exists")

        self.website.before_request = fail_ref
        with self.assertRaises(UPDATER.Refusal):
            self.update()
        self.assertEqual(
            [w[0] for w in self.website.mutations], ["git/trees", "git/commits"]
        )
        self.assertEqual(self.website.prs, [])

    def test_gh_missing_ref_only_accepts_404(self):
        for status in (404, 401, 403, 500):
            result = subprocess.CompletedProcess(
                [], 1, stdout=f"HTTP/2.0 {status} Error\n\n{{}}", stderr="sensitive"
            )
            with patch.object(UPDATER.subprocess, "run", return_value=result):
                if status == 404:
                    self.assertIsNone(
                        UPDATER.GitHub("secret").request(
                            "caudra/website",
                            "git/ref/heads/automation/docs-source",
                            missing=True,
                        )
                    )
                else:
                    with self.assertRaises(UPDATER.Refusal):
                        UPDATER.GitHub("secret").request(
                            "caudra/website",
                            "git/ref/heads/automation/docs-source",
                            missing=True,
                        )


class VerifyTests(unittest.TestCase):
    def setUp(self):
        self.source = FakeAPI(source=True)
        self.website = FakeAPI()
        self.source.jobs.extend(
            {"name": name, "status": "completed", "conclusion": "success"}
            for name in ("Detect changes", "CI")
        )

    def filtered(self):
        for job in self.source.jobs:
            if job["name"] in UPDATER.REQUIRED_JOBS:
                job["conclusion"] = "skipped"
                job.pop("steps", None)

    def cli(self, event="workflow_run", verify_only=True):
        with TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.write_text("existing=value\n")
            stdout, stderr = io.StringIO(), io.StringIO()
            args = ["update-website-docs.py", "--run-id", "42"]
            args += ["--verify-only"] if verify_only else ["--app-slug", "docs-app"]
            with (
                patch.dict(
                    os.environ,
                    {"GITHUB_EVENT_NAME": event, "GITHUB_OUTPUT": str(output)},
                    clear=True,
                ),
                patch.object(
                    UPDATER, "GitHub", side_effect=[self.source, self.website]
                ) as client,
                patch.object(UPDATER.sys, "argv", args),
                patch.object(UPDATER.sys, "stdout", stdout),
                patch.object(UPDATER.sys, "stderr", stderr),
            ):
                status = UPDATER.main()
            if verify_only:
                client.assert_called_once()
            self.assertEqual(self.website.mutations, [])
            return status, stdout.getvalue(), stderr.getvalue(), output.read_text()

    def assert_cli_refused(self, **kwargs):
        status, stdout, stderr, output = self.cli(**kwargs)
        self.assertEqual(status, 1)
        self.assertEqual(stdout, "")
        self.assertTrue(stderr.startswith("Refused: "), stderr)
        self.assertEqual(output, "existing=value\n")

    def test_filtered_is_never_verified_or_publishable(self):
        self.filtered()
        with self.assertRaises(UPDATER.PathFilteredRun):
            UPDATER.verified_source(self.source, 42)
        with self.assertRaises(UPDATER.PathFilteredRun):
            UPDATER.update(self.source, self.website, 42, BOT)
        self.assertEqual(self.website.mutations, [])
        self.assert_cli_refused(verify_only=False)

    def test_filtered_noop_only_for_automatic_preflight(self):
        self.filtered()
        status, stdout, stderr, output = self.cli()
        self.assertEqual(status, 0)
        self.assertIn("No-op:", stdout)
        self.assertIn("path filtering", stdout)
        self.assertNotIn(NEW, stdout)
        self.assertEqual(stderr, "")
        self.assertEqual(output, "existing=value\neligible=false\n")
        for event in ("workflow_dispatch", "", "push"):
            with self.subTest(event=event):
                self.assert_cli_refused(event=event)

    def test_verified_preflight_emits_positive_eligibility(self):
        for event in ("workflow_run", "workflow_dispatch", ""):
            with self.subTest(event=event):
                status, stdout, stderr, output = self.cli(event=event)
                self.assertEqual(status, 0)
                self.assertEqual(stdout, f"Verified source: {NEW}\n")
                self.assertEqual(stderr, "")
                self.assertEqual(output, "existing=value\neligible=true\nkind=rust\n")

    def test_superseded_run_is_only_an_automatic_preflight_noop(self):
        self.source.main = OLD
        for filtered in (False, True):
            with self.subTest(filtered=filtered):
                refusal = UPDATER.SupersededRun
                if filtered:
                    self.filtered()
                    refusal = UPDATER.PathFilteredRun
                with self.assertRaises(refusal):
                    UPDATER.verified_source(self.source, 42)
                with self.assertRaises(refusal):
                    UPDATER.update(self.source, self.website, 42, BOT)
                status, stdout, stderr, output = self.cli()
                self.assertEqual(status, 0)
                self.assertIn("No-op:", stdout)
                self.assertIn(
                    "path filtering" if filtered else "no longer current main", stdout
                )
                self.assertNotIn("Verified source:", stdout)
                self.assertEqual(stderr, "")
                self.assertEqual(output, "existing=value\neligible=false\n")
                for event in ("workflow_dispatch", "", "push"):
                    self.assert_cli_refused(event=event)
                self.assert_cli_refused(verify_only=False)

    def test_successful_jobs_without_docgen_never_emit_eligibility(self):
        step = self.source.jobs[2]["steps"][0]
        for steps in ([], [step, step], [{**step, "conclusion": "skipped"}]):
            for current in (NEW, OLD):
                with self.subTest(steps=steps, current=current):
                    self.source.main = current
                    self.source.jobs[2]["steps"] = steps
                    self.assert_cli_refused()

    def test_nonpublishable_runs_still_require_canonical_identity(self):
        for target, key, value in (
            ("workflow", "path", ".github/workflows/other.yml"),
            ("workflow", "state", "disabled_manually"),
            ("run", "id", 43),
            ("run", "workflow_id", 12),
            ("run", "path", ".github/workflows/other.yml"),
            ("run", "repository", {"full_name": "attacker/caudra"}),
            ("run", "head_repository", {"full_name": "attacker/caudra"}),
            ("run", "event", "pull_request"),
            ("run", "head_branch", "feature"),
            ("run", "head_sha", "main"),
            ("run", "head_repository", None),
            ("run", "conclusion", "failure"),
            ("run", "status", "in_progress"),
            ("run", "run_attempt", 0),
            ("run", "run_attempt", True),
            ("run", "run_attempt", "1"),
        ):
            for filtered, current in ((True, NEW), (True, OLD), (False, OLD)):
                with self.subTest(
                    target=target,
                    key=key,
                    value=value,
                    filtered=filtered,
                    current=current,
                ):
                    self.setUp()
                    if filtered:
                        self.filtered()
                    self.source.main = current
                    getattr(self.source, target)[key] = value
                    self.assert_cli_refused()

    def test_nonpublishable_runs_still_require_valid_current_main(self):
        for ref in (
            None,
            {"object": {"type": "tree", "sha": OLD}},
            {"object": {"type": "commit", "sha": "main"}},
        ):
            for filtered in (False, True):
                with self.subTest(ref=ref, filtered=filtered):
                    self.setUp()
                    if filtered:
                        self.filtered()
                    original = self.source.request

                    def request(repo, path, ref=ref, original=original, **kwargs):
                        return (
                            ref
                            if path == "git/ref/heads/main"
                            else original(repo, path, **kwargs)
                        )

                    with patch.object(self.source, "request", side_effect=request):
                        self.assert_cli_refused()

    def test_nonpublishable_runs_require_valid_jobs_and_filter_gates(self):
        for index in range(5):
            for outcome in (
                "missing",
                "duplicate",
                "failure",
                "cancelled",
                "in_progress",
                None,
                "opposite",
            ):
                for filtered, current in ((True, NEW), (True, OLD), (False, OLD)):
                    if not filtered and index >= 3:
                        continue
                    with self.subTest(
                        index=index, outcome=outcome, filtered=filtered, current=current
                    ):
                        self.setUp()
                        if filtered:
                            self.filtered()
                        self.source.main = current
                        job = self.source.jobs[index]
                        if outcome == "missing":
                            self.source.jobs.pop(index)
                        elif outcome == "duplicate":
                            self.source.jobs.append(job.copy())
                        elif outcome == "in_progress":
                            job["status"] = outcome
                        else:
                            job["conclusion"] = (
                                (
                                    "success"
                                    if job["conclusion"] == "skipped"
                                    else "skipped"
                                )
                                if outcome == "opposite"
                                else outcome
                            )
                        self.assert_cli_refused()

    def test_nonpublishable_classification_reads_all_attempt_pages(self):
        for filtered, current in ((True, NEW), (True, OLD), (False, OLD)):
            with self.subTest(filtered=filtered, current=current):
                self.setUp()
                if filtered:
                    self.filtered()
                self.source.main = current
                self.source.jobs = [
                    {"name": f"other-{i}"} for i in range(101)
                ] + self.source.jobs
                self.source.run["run_attempt"] = 2
                original = self.source.request

                def request(repo, path, original=original, **kwargs):
                    if "/jobs?" in path:
                        self.assertIn("/attempts/2/", path)
                        path = path.replace("/attempts/2/", "/attempts/1/")
                    return original(repo, path, **kwargs)

                with patch.object(self.source, "request", side_effect=request):
                    self.assertEqual(self.cli()[3], "existing=value\neligible=false\n")
                    self.source.jobs.insert(0, self.source.jobs[-3].copy())
                    self.assert_cli_refused()

    def test_nonpublishable_runs_cannot_hide_incomplete_or_excessive_pagination(self):
        for filtered, current in ((True, NEW), (True, OLD), (False, OLD)):
            self.setUp()
            if filtered:
                self.filtered()
            self.source.main = current
            original = self.source.request
            for total in (4, 6):
                with self.subTest(total=total, filtered=filtered, current=current):

                    def request(repo, path, total=total, original=original, **kwargs):
                        result = original(repo, path, **kwargs)
                        if "/jobs?" in path:
                            result["total_count"] = total
                        return result

                    with patch.object(self.source, "request", side_effect=request):
                        self.assert_cli_refused()
                        with patch.object(UPDATER, "MAX_PAGES", 1):
                            self.assert_cli_refused()

    def test_workflow_gates_both_privileged_steps_on_positive_eligibility(self):
        workflow = (
            Path(__file__).resolve().parents[1] / ".github/workflows/website-docs.yml"
        ).read_text()
        steps = workflow.split("\n      - ")
        preflight = next(step for step in steps if "--verify-only" in step)
        mint = next(
            step for step in steps if "actions/create-github-app-token@" in step
        )
        propose = next(step for step in steps if "--app-slug" in step)
        self.assertIn("id: preflight\n", preflight)
        self.assertNotIn("WEBSITE_TOKEN", preflight)
        self.assertLess(steps.index(preflight), steps.index(mint))
        self.assertLess(steps.index(mint), steps.index(propose))
        for step in (mint, propose):
            self.assertEqual(
                [
                    line.strip()
                    for line in step.splitlines()
                    if line.strip().startswith("if:")
                ],
                [
                    "if: steps.preflight.outputs.eligible == 'true' && steps.preflight.outputs.kind == 'rust'"
                ],
            )
        self.assertIn("app-id: ${{ vars.WEBSITE_APP_ID }}", mint)
        self.assertIn("private-key: ${{ secrets.WEBSITE_APP_PRIVATE_KEY }}", mint)
        self.assertNotIn("continue-on-error", workflow)
        self.assertIn("ref: ${{ github.workflow_sha }}", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("workflows: [Rust, Release]", workflow)
        self.assertIn(
            "group: website-docs-source-${{ github.event.workflow_run.name || format('manual-{0}', inputs.run_id) }}",
            workflow,
        )
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertNotIn("branches:", workflow)
        self.assertIn("permission-actions: read", mint)
        release_mint = next(step for step in steps if "id: release-token" in step)
        self.assertIn("permission-contents: write", release_mint)
        self.assertNotIn("permission-actions:", release_mint)
        self.assertNotIn("permission-pull-requests:", release_mint)
        self.assertIn("steps.preflight.outputs.kind == 'release'", release_mint)
        self.assertLess(steps.index(preflight), steps.index(release_mint))


class MergeTests(unittest.TestCase):
    def setUp(self):
        self.source = FakeAPI(source=True)
        self.website = FakeAPI()
        sleeper = patch.object(
            UPDATER.time, "sleep", side_effect=AssertionError("Unexpected wait")
        )
        sleeper.start()
        self.addCleanup(sleeper.stop)

    def update(self):
        return UPDATER.update(self.source, self.website, 42, BOT)

    def pending(self):
        self.website.branch = BRANCH
        self.website.prs = [self.website.pull()]
        self.website.site_run = self.website.website_run()

    def refused_merge(self):
        with self.assertRaises(UPDATER.Refusal):
            self.update()
        self.assertFalse(
            any(path.endswith("/merge") for path, _, _ in self.website.mutations)
        )

    def test_real_update_merges_only_after_ci_with_expected_sha_and_retries_noop(self):
        self.assertIn("merged after canonical CI", self.update())
        self.assertEqual(
            self.website.mutations[-1],
            ("pulls/10/merge", "PUT", {"sha": COMMIT, "merge_method": "squash"}),
        )
        self.assertEqual(self.source.mutations, [])
        self.website.mutations.clear()
        self.assertIn("no update needed", self.update())
        self.assertEqual(self.website.mutations, [])

    def test_merge_commit_fallback_and_no_rebase_or_admin_api(self):
        self.website.repo_settings["allow_squash_merge"] = False
        self.update()
        self.assertEqual(
            self.website.mutations[-1][2], {"sha": COMMIT, "merge_method": "merge"}
        )
        self.setUp()
        self.website.repo_settings = {}
        self.refused_merge()

    def test_wrong_workflow_identity_or_run_provenance_never_merges(self):
        cases = (
            ("workflow", "path", ".github/workflows/other.yml"),
            ("workflow", "name", "Other"),
            ("workflow", "state", "disabled_manually"),
            ("run", "workflow_id", 999),
            ("run", "path", ".github/workflows/other.yml"),
            ("run", "event", "push"),
            ("run", "head_branch", "main"),
            ("run", "head_sha", OLD),
            ("run", "repository", {"full_name": "attacker/website"}),
            ("run", "head_repository", {"full_name": "attacker/website"}),
            ("run", "pull_requests", []),
            ("run", "run_attempt", True),
        )
        for target, key, value in cases:
            with self.subTest(target=target, key=key):
                self.setUp()
                self.pending()
                item = (
                    self.website.site_workflow
                    if target == "workflow"
                    else self.website.site_run
                )
                item[key] = value
                self.refused_merge()

    def test_failed_or_skipped_website_verification_never_merges(self):
        for outcome in ("failure", "cancelled", "skipped", "neutral", "timed_out"):
            for scope in ("run", "job", "step"):
                with self.subTest(outcome=outcome, scope=scope):
                    self.setUp()
                    self.pending()
                    item = {
                        "run": self.website.site_run,
                        "job": self.website.site_jobs[0],
                        "step": self.website.site_jobs[0]["steps"][0],
                    }[scope]
                    item["conclusion"] = outcome
                    self.refused_merge()
        for scope in ("job", "step"):
            for change in ("missing", "duplicate"):
                with self.subTest(scope=scope, change=change):
                    self.setUp()
                    items = (
                        self.website.site_jobs
                        if scope == "job"
                        else self.website.site_jobs[0]["steps"]
                    )
                    if change == "missing":
                        items.clear()
                    else:
                        items.append(copy.deepcopy(items[0]))
                    self.refused_merge()

    def test_ci_at_old_base_or_wrong_pr_is_refused(self):
        for field, value in (("base", OLD), ("head", OLD), ("number", 11)):
            self.setUp()
            self.pending()
            pull = self.website.site_run["pull_requests"][0]
            if field == "number":
                pull[field] = value
            else:
                pull[field]["sha"] = value
            self.refused_merge()

    def test_pending_ci_wait_is_bounded_and_does_not_merge(self):
        self.pending()
        self.website.site_run["status"] = "in_progress"
        with (
            patch.object(
                UPDATER.time, "monotonic", side_effect=[0, 0, UPDATER.CI_WAIT_SECONDS]
            ),
            self.assertRaisesRegex(UPDATER.Refusal, "Timed out"),
        ):
            self.update()
        self.assertFalse(
            any(path.endswith("/merge") for path, _, _ in self.website.mutations)
        )

    def test_missing_then_pending_then_successful_ci_waits_without_real_sleep(self):
        original = UPDATER.verified_website_ci
        calls = 0

        def verify(*args):
            nonlocal calls
            calls += 1
            return None if calls <= 2 else original(*args)

        with (
            patch.object(UPDATER, "verified_website_ci", side_effect=verify),
            patch.object(UPDATER.time, "sleep") as sleeper,
            patch.object(UPDATER.sys, "stdout", io.StringIO()),
        ):
            self.update()
        self.assertEqual(sleeper.call_count, 2)
        self.assertTrue(
            all(
                0 <= call.args[0] <= UPDATER.CI_POLL_SECONDS
                for call in sleeper.call_args_list
            )
        )

    def test_ci_revalidation_catches_new_attempt(self):
        with patch.object(
            UPDATER, "verified_website_ci", side_effect=[(101, 1), (101, 2)]
        ):
            self.refused_merge()

    def test_source_run_revalidated_after_ci(self):
        def race(path, method):
            if path == "actions/runs/101":
                self.source.run["conclusion"] = "failure"

        self.website.before_request = race
        self.refused_merge()

    def test_missing_ci_waits_but_incomplete_pagination_fails_closed(self):
        original = self.website.request
        for total in (0, 1):
            with self.subTest(total=total):

                def request(repo, path, total=total, **kwargs):
                    if path.startswith("actions/workflows/100/runs?"):
                        return {"total_count": total, "workflow_runs": []}
                    return original(repo, path, **kwargs)

                with patch.object(self.website, "request", side_effect=request):
                    if total == 0:
                        self.assertIsNone(
                            UPDATER.verified_website_ci(self.website, 10, MAIN, BRANCH)
                        )
                    else:
                        with self.assertRaises(UPDATER.Refusal):
                            UPDATER.verified_website_ci(self.website, 10, MAIN, BRANCH)

    def test_merge_refusal_is_not_retried_or_reported_as_success(self):
        original = self.website.request

        def request(repo, path, **kwargs):
            result = original(repo, path, **kwargs)
            return {"merged": False} if path.endswith("/merge") else result

        with (
            patch.object(self.website, "request", side_effect=request),
            self.assertRaisesRegex(UPDATER.Refusal, "GitHub refused"),
        ):
            self.update()
        self.assertEqual(
            sum(path.endswith("/merge") for path, _, _ in self.website.mutations), 1
        )

    def test_newest_run_cannot_be_hidden_by_older_success(self):
        self.pending()
        original = self.website.request

        def request(repo, path, **kwargs):
            if path.startswith("actions/workflows/100/runs?"):
                return {"total_count": 2, "workflow_runs": [{"id": 100}, {"id": 101}]}
            return original(repo, path, **kwargs)

        self.website.site_run["conclusion"] = "failure"
        with patch.object(self.website, "request", side_effect=request):
            self.refused_merge()

    def test_final_head_base_source_and_human_edit_races_are_refused(self):
        for target in ("head", "base", "source", "human", "draft"):
            with self.subTest(target=target):
                self.setUp()

                def race(path, method, target=target):
                    if path == "pulls/10" and method == "GET":
                        if target == "head":
                            self.website.branch = OLD
                        elif target == "base":
                            self.website.main = OLD
                        elif target == "source":
                            self.source.main = OLD
                        elif target == "draft":
                            self.website.prs[0]["draft"] = True
                        else:
                            self.website.prs[0]["user"]["type"] = "User"

                self.website.before_request = race
                self.refused_merge()

    def test_successful_ci_cannot_hide_human_branch_commit(self):
        def race(path, method):
            if path == "actions/runs/101":
                commit = self.website.bot_commit(COMMIT)
                commit["author"]["type"] = "User"
                self.website.commits[COMMIT] = commit

        self.website.before_request = race
        self.refused_merge()

    def test_ci_job_pagination_is_complete(self):
        self.website.site_jobs = [
            {"name": f"other-{i}"} for i in range(101)
        ] + self.website.site_jobs
        self.update()

    def test_actions_permission_error_is_actionable_without_secrets(self):
        result = subprocess.CompletedProcess(
            [], 1, stdout="HTTP/2.0 403 Forbidden\n\n{}", stderr="secret"
        )
        with patch.object(UPDATER.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(UPDATER.Refusal, "Actions: read") as error:
                UPDATER.GitHub("secret").request(
                    UPDATER.WEBSITE, "actions/workflows/site.yml"
                )
            self.assertNotIn("secret", str(error.exception))


class ReleaseAPI(FakeAPI):
    def __init__(self):
        super().__init__(source=True)
        self.workflow["path"] = UPDATER.RELEASE_WORKFLOW
        self.run.update(path=UPDATER.RELEASE_WORKFLOW, head_branch="v0.2.0-preview.1")
        self.release = {
            "id": 50,
            "tag_name": self.run["head_branch"],
            "draft": False,
            "prerelease": True,
            "published_at": "2026-10-08T12:00:00Z",
        }
        self.releases = [self.release]
        self.tag = {"type": "commit", "sha": NEW}
        self.tags = {}

    def request(self, repo, path, method="GET", data=None, missing=False):
        if repo == UPDATER.SOURCE and method == "GET":
            if path == "actions/workflows/release.yml":
                return self.workflow
            if path.startswith("git/ref/tags/"):
                return {"object": self.tag}
            if path.startswith("git/tags/"):
                return {"object": self.tags[path.removeprefix("git/tags/")]}
            if path.startswith("releases/tags/"):
                return self.release
            if path.startswith("releases?"):
                page = int(path.rsplit("=", 1)[1])
                return self.releases[(page - 1) * 100 : page * 100]
        return super().request(repo, path, method, data, missing)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.source = ReleaseAPI()
        self.website = FakeAPI()

    def dispatch(self):
        return UPDATER.dispatch_release(self.source, self.website, 42)

    def refuse(self):
        with self.assertRaises((UPDATER.Refusal, ValueError)):
            self.dispatch()
        self.assertEqual(self.website.mutations, [])

    def test_exact_dispatch_contract_includes_prerelease(self):
        self.dispatch()
        self.assertEqual(
            self.website.mutations,
            [
                (
                    "dispatches",
                    "POST",
                    {
                        "event_type": "caudra-release",
                        "client_payload": {
                            "version": 1,
                            "run_id": 42,
                            "tag": "v0.2.0-preview.1",
                            "revision": NEW,
                        },
                    },
                )
            ],
        )
        self.assertEqual(self.source.mutations, [])

    def test_annotated_tag_is_peeled_and_retagged_or_cyclic_tag_refused(self):
        self.source.tag = {"type": "tag", "sha": BLOB}
        self.source.tags[BLOB] = {"type": "tag", "sha": TREE}
        self.source.tags[TREE] = {"type": "commit", "sha": NEW}
        self.dispatch()
        self.website.mutations.clear()
        self.source.tags[TREE] = {"type": "commit", "sha": OLD}
        self.refuse()
        self.source.tags[TREE] = {"type": "tag", "sha": BLOB}
        self.refuse()

    def test_release_identity_status_and_publication_are_required(self):
        for target, key, value in (
            ("workflow", "path", UPDATER.WORKFLOW),
            ("workflow", "state", "disabled_manually"),
            ("run", "workflow_id", 100),
            ("run", "id", 43),
            ("run", "path", UPDATER.WORKFLOW),
            ("run", "event", "workflow_dispatch"),
            ("run", "repository", {"full_name": "attacker/caudra"}),
            ("run", "head_repository", {"full_name": "attacker/caudra"}),
            ("run", "head_branch", "main"),
            ("run", "head_sha", "A" * 40),
            ("run", "status", "in_progress"),
            ("run", "conclusion", "failure"),
            ("release", "draft", True),
            ("release", "tag_name", "v0.3.0"),
            ("release", "published_at", None),
            ("release", "published_at", "2026-10-08"),
            ("release", "id", True),
            ("tag", "sha", OLD),
            ("tag", "type", "tree"),
        ):
            with self.subTest(target=target, key=key, value=value):
                self.setUp()
                getattr(self.source, target)[key] = value
                self.refuse()

    def test_strict_tags_and_positive_run_ids(self):
        for tag in ("v1.0.0", "v0.2.0-preview.1", "v1.2.3+build.4"):
            self.assertTrue(UPDATER.eligible_tag(tag))
        for tag in (
            "vfoo",
            "v01.0.0",
            "v1.0.0-01",
            "../v1.0.0",
            "v1.0.0/x",
            "v1.0.0?x",
            None,
        ):
            self.assertFalse(UPDATER.eligible_tag(tag))
        for run_id in (0, -1, True, "42"):
            with self.assertRaises(UPDATER.Refusal):
                UPDATER.verified_release(self.source, run_id)

    def test_latest_uses_publication_time_across_pages_including_prereleases(self):
        self.source.releases = [
            {"id": i, "tag_name": f"v1.0.{i}", "draft": True} for i in range(100)
        ] + [self.source.release]
        self.dispatch()
        self.website.mutations.clear()
        self.source.releases.append(
            {
                "id": 999,
                "tag_name": "v0.2.0-preview.2",
                "draft": False,
                "prerelease": True,
                "published_at": "2026-10-08T12:01:00Z",
            }
        )
        self.refuse()

    def test_ambiguous_or_unlisted_release_and_excessive_pagination_refused(self):
        self.source.releases.append({**self.source.release, "id": 51})
        self.refuse()
        self.source.releases = []
        self.refuse()
        self.source.releases = [self.source.release] * 100
        with patch.object(UPDATER, "MAX_PAGES", 1):
            self.refuse()

    def test_release_rechecked_immediately_before_dispatch(self):
        original = UPDATER.verified_release
        calls = 0

        def verify(*args):
            nonlocal calls
            calls += 1
            if calls == 2:
                self.source.tag["sha"] = OLD
            return original(*args)

        with patch.object(UPDATER, "verified_release", side_effect=verify):
            self.refuse()
        self.assertEqual(calls, 2)

    def test_dispatch_204_is_success_but_empty_200_is_not_json(self):
        for code in (204, 200):
            result = subprocess.CompletedProcess(
                [], 0, stdout=f"HTTP/2.0 {code} OK\r\n\r\n", stderr=""
            )
            with patch.object(UPDATER.subprocess, "run", return_value=result):
                if code == 204:
                    self.assertIsNone(
                        UPDATER.GitHub("secret").request(
                            UPDATER.WEBSITE, "dispatches", method="POST", data={}
                        )
                    )
                else:
                    with self.assertRaises(UPDATER.Refusal):
                        UPDATER.GitHub("secret").request(
                            UPDATER.WEBSITE, "dispatches", method="POST", data={}
                        )

    def test_release_preflight_emits_kind_without_website_credential(self):
        with TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with (
                patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True),
                patch.object(UPDATER, "GitHub", return_value=self.source) as client,
                patch.object(
                    UPDATER.sys, "argv", ["update", "--run-id", "42", "--verify-only"]
                ),
                patch.object(UPDATER.sys, "stdout", io.StringIO()),
            ):
                self.assertEqual(UPDATER.main(), 0)
            client.assert_called_once()
            self.assertEqual(output.read_text(), "eligible=true\nkind=release\n")

    def test_manual_release_dispatch_needs_no_pr_bot_identity(self):
        with (
            patch.dict(
                os.environ, {"GITHUB_EVENT_NAME": "workflow_dispatch"}, clear=True
            ),
            patch.object(UPDATER, "GitHub", side_effect=[self.source, self.website]),
            patch.object(UPDATER.sys, "argv", ["update", "--run-id", "42"]),
            patch.object(UPDATER.sys, "stdout", io.StringIO()),
        ):
            self.assertEqual(UPDATER.main(), 0)
        self.assertEqual(len(self.website.mutations), 1)
        self.assertEqual(self.website.mutations[0][0], "dispatches")

    def test_stale_release_manual_retry_refused_automatic_preflight_noop(self):
        self.source.releases.append(
            {**self.source.release, "id": 51, "published_at": "2026-10-09T12:00:00Z"}
        )
        for event, expected in (("workflow_dispatch", 1), ("workflow_run", 0)):
            with TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                with (
                    patch.dict(
                        os.environ,
                        {"GITHUB_OUTPUT": str(output), "GITHUB_EVENT_NAME": event},
                        clear=True,
                    ),
                    patch.object(UPDATER, "GitHub", return_value=self.source) as client,
                    patch.object(
                        UPDATER.sys,
                        "argv",
                        ["update", "--run-id", "42", "--verify-only"],
                    ),
                    patch.object(UPDATER.sys, "stdout", io.StringIO()),
                    patch.object(UPDATER.sys, "stderr", io.StringIO()),
                ):
                    self.assertEqual(UPDATER.main(), expected)
                client.assert_called_once()
                self.assertEqual(
                    output.read_text() if output.exists() else "",
                    "eligible=false\n" if expected == 0 else "",
                )


if __name__ == "__main__":
    unittest.main()
