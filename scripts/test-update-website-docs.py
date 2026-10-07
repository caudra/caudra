#!/usr/bin/env python3

import base64
import copy
import importlib.util
import json
import os
import subprocess
import unittest
from pathlib import Path
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
            "user": BOT.copy(),
            "head": {
                "ref": "automation/docs-source",
                "repo": {"full_name": "caudra/website"},
            },
            "base": {"ref": "main", "repo": {"full_name": "caudra/website"}},
        }

    def request(self, repo, path, method="GET", data=None, missing=False):
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
            raise AssertionError(f"Unexpected mutation {path}")
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


if __name__ == "__main__":
    unittest.main()
