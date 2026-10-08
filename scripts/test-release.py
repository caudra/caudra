#!/usr/bin/env python3

import copy
import hashlib
import importlib.util
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "release", Path(__file__).with_name("release.py")
)
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)

TAG = "v0.2.0-preview.1"
COMMIT = "a" * 40
OTHER_COMMIT = "b" * 40
TITLE = "Caudra 0.2 Preview 1"
DRAFT = {
    "id": 123,
    "tag_name": TAG,
    "target_commitish": COMMIT,
    "draft": True,
    "immutable": False,
    "prerelease": True,
    "assets": [],
}
PUBLISHED_ERROR = "Published releases must never be modified"
INVENTORY_ERROR = "Asset inventory mismatch"
UPLOAD_ERROR = "simulated interrupted upload"
WORKFLOWS = Path(__file__).resolve().parent.parent / ".github/workflows"
SMOKE_WORKER_WARNING = "Workcell python_execution is unavailable"
SMOKE_STARTUP_ERROR = "resolve data directory: Permission denied"
SMOKE_DIRECTORIES = (
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="release test ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "Cargo.toml").write_text(
            f'[workspace.package]\nversion = "{TAG[1:]}"\n'
        )
        for name in RELEASE.INSTALLERS:
            (self.root / name).write_bytes(f"installer source: {name}\n".encode())
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        self.remote = copy.deepcopy(DRAFT)
        self.uploaded = {}
        self.publications = []
        self.upload_calls = []
        self.corrupt_download = False
        self.publish_after_upload = False
        self.fail_upload = None

    def seed_archives(self):
        for name in RELEASE.asset_names(TAG):
            if name not in RELEASE.INSTALLERS:
                (self.artifacts / name).write_bytes(f"archive: {name}\n".encode())

    def fake_api(self, path, payload=None):
        if path.startswith("git/ref/tags/"):
            return {"object": {"type": "commit", "sha": COMMIT}}
        self.assertEqual(path, f"releases/{DRAFT['id']}")
        if payload is not None:
            self.publications.append(payload)
            self.remote.update(payload)
        return copy.deepcopy(self.remote)

    def fake_gh(self, *arguments):
        self.assertEqual(arguments[0], "release")
        if arguments[1] == "upload":
            source = Path(arguments[3])
            self.upload_calls.append(source.name)
            if source.name == self.fail_upload:
                raise RuntimeError(UPLOAD_ERROR)
            self.uploaded[source.name] = source.read_bytes()
            self.remote["assets"] = [
                {"name": name, "size": len(data), "state": "uploaded"}
                for name, data in self.uploaded.items()
            ]
            if self.publish_after_upload:
                self.remote["draft"] = False
        elif arguments[1] == "download":
            directory = Path(arguments[-1])
            for name, data in self.uploaded.items():
                (directory / name).write_bytes(data)
            if self.corrupt_download:
                (directory / "install.sh").write_bytes(b"corrupted installer")
        else:
            self.fail(f"Unexpected release operation: {arguments}")
        return ""

    def publish(self):
        with (
            patch.object(RELEASE, "ROOT", self.root),
            patch.object(RELEASE, "api", side_effect=self.fake_api),
            patch.object(RELEASE, "releases", side_effect=lambda: [self.remote]),
            patch.object(RELEASE, "gh", side_effect=self.fake_gh),
        ):
            RELEASE.publish(TAG, COMMIT, self.artifacts)

    def test_strict_semver(self):
        valid = (
            TAG,
            "v0.0.0",
            "v1.2.3",
            "v1.2.3-0",
            "v1.2.3-rc.10+build.001",
            "v1.2.3-alpha-beta.01a",
        )
        for tag in valid:
            with self.subTest(tag=tag):
                RELEASE.version(tag)
        invalid = (
            "0.2.0-preview.1",
            "V0.2.0",
            "v01.2.3",
            "v1.02.3",
            "v1.2.03",
            "v1.2",
            "v1.2.3.4",
            "v1.2.3-preview.01",
            "v1.2.3-01",
            "v1.2.3-",
            "v1.2.3+",
            "v1.2.3-a..b",
            "v1.2.3-α",
            "v1.2.3\n",
            "v1.2.3/other",
        )
        for tag in invalid:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                RELEASE.version(tag)

    def test_title_and_prerelease_classification(self):
        self.assertEqual(RELEASE.release_title(TAG), TITLE)
        self.assertEqual(RELEASE.release_title("v0.2.1"), "Caudra 0.2.1")
        self.assertEqual(
            RELEASE.release_title("v0.2.1-preview.2"), "Caudra 0.2.1 Preview 2"
        )
        self.assertIsNone(RELEASE.version("v0.2.0+build.1")[1])
        self.assertEqual(RELEASE.version("v0.2.0-rc.1")[1], "rc.1")

    def test_release_notes_state_preview_limits_migration_and_exact_provenance(self):
        notes = RELEASE.release_notes(TAG, COMMIT)
        for expected in (
            f"## {TITLE}",
            "Preview / prerelease.",
            "Experimental features remain subject to change",
            "manually running the external PowerShell installer",
            "Rollback is unsupported on Windows pending native verification",
            "do not imply complete native-platform or end-to-end verification",
            "Back up your data before upgrading",
            "data migrations are not reversed",
            "not restored",
            f"Tag: `{TAG}`",
            f"[{COMMIT}](https://github.com/caudra/caudra/commit/{COMMIT})",
            "[Canonical documentation](https://caudra.ai/docs/)",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, notes)
        self.assertEqual(notes, RELEASE.release_notes(TAG, COMMIT))
        self.assertNotIn(OTHER_COMMIT, notes)

    def test_stable_release_notes_do_not_claim_preview_status(self):
        notes = RELEASE.release_notes("v0.2.0", COMMIT)
        self.assertIn("Stable release.", notes)
        self.assertNotIn("Preview / prerelease", notes)

    def test_source_requires_canonical_repository_tag_version_and_exact_checkout(self):
        with patch.object(
            RELEASE.subprocess, "check_output", return_value=f"{COMMIT}\n"
        ):
            self.assertEqual(
                RELEASE.validate_source(
                    "caudra/caudra", f"refs/tags/{TAG}", COMMIT, self.root
                ),
                TAG,
            )
            invalid = (
                ("fork/caudra", f"refs/tags/{TAG}", COMMIT),
                ("caudra/caudra", f"refs/heads/{TAG}", COMMIT),
                ("caudra/caudra", "refs/tags/v0.2.0", COMMIT),
                ("caudra/caudra", f"refs/tags/{TAG}", "main"),
                ("caudra/caudra", f"refs/tags/{TAG}", OTHER_COMMIT),
            )
            for repository, ref, sha in invalid:
                with (
                    self.subTest(repository=repository, ref=ref, sha=sha),
                    self.assertRaises(ValueError),
                ):
                    RELEASE.validate_source(repository, ref, sha, self.root)

    def test_lightweight_and_annotated_tags_resolve_to_exact_commit(self):
        commit = {"object": {"type": "commit", "sha": COMMIT}}
        annotated = {"object": {"type": "tag", "sha": OTHER_COMMIT}}
        for responses in ([commit], [annotated, commit]):
            with patch.object(RELEASE, "api", side_effect=responses):
                RELEASE.assert_tag_commit(TAG, COMMIT)
        for responses in (
            [{"object": {"type": "commit", "sha": OTHER_COMMIT}}],
            [{"object": {"type": "tree", "sha": COMMIT}}],
            [annotated, annotated],
        ):
            with (
                patch.object(RELEASE, "api", side_effect=responses),
                self.assertRaises(ValueError),
            ):
                RELEASE.assert_tag_commit(TAG, COMMIT)

    def test_existing_release_must_be_mutable_draft_bound_to_source(self):
        RELEASE.assert_draft(DRAFT, TAG, COMMIT)
        for field, value in (
            ("draft", False),
            ("draft", None),
            ("immutable", True),
            ("immutable", None),
            ("target_commitish", "main"),
            ("target_commitish", OTHER_COMMIT),
            ("tag_name", "v0.2.0"),
            ("prerelease", False),
        ):
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                RELEASE.assert_draft({**DRAFT, field: value}, TAG, COMMIT)

    def test_prepare_reuses_matching_draft_without_mutation(self):
        with (
            patch.object(RELEASE, "existing_draft", return_value=DRAFT),
            patch.object(RELEASE, "gh") as gh,
        ):
            RELEASE.prepare(TAG, COMMIT)
            gh.assert_not_called()

    def test_prepare_creates_explicit_preview_not_latest(self):
        with (
            patch.object(RELEASE, "existing_draft", side_effect=[None, DRAFT]),
            patch.object(RELEASE, "gh") as gh,
        ):
            RELEASE.prepare(TAG, COMMIT)
            arguments = gh.call_args.args
            self.assertIn("--verify-tag", arguments)
            self.assertIn("--draft", arguments)
            self.assertIn("--latest=false", arguments)
            self.assertIn("--prerelease=true", arguments)
            self.assertEqual(arguments[arguments.index("--target") + 1], COMMIT)
            self.assertEqual(arguments[arguments.index("--title") + 1], TITLE)
            self.assertNotIn("--generate-notes", arguments)
            self.assertEqual(
                arguments[arguments.index("--notes") + 1],
                RELEASE.release_notes(TAG, COMMIT),
            )

    def test_latest_uses_semver_not_lexical_or_release_creation_order(self):
        def stable(tag, **overrides):
            return {"tag_name": tag, "draft": False, "prerelease": False, **overrides}

        cases = (
            (TAG, [], False),
            ("v0.2.0", [], True),
            ("v0.2.0", [stable("v0.1.99")], True),
            ("v0.2.0", [stable("v0.2.1")], False),
            ("v0.9.0", [stable("v0.10.0"), stable("v0.8.0")], False),
            ("v0.10.0", [stable("v0.9.0")], True),
            ("v0.2.0", [stable("v1.0.0", draft=True)], True),
            ("v0.2.0", [stable("v1.0.0-rc.1", prerelease=True)], True),
            ("v0.2.0", [stable("v1.0.0-rc.1")], True),
            ("v0.2.0", [stable("unrelated-tag")], True),
            ("v0.2.0+build.1", [stable("v0.3.0+build.2")], False),
        )
        for tag, published, expected in cases:
            with self.subTest(tag=tag, published=published):
                self.assertEqual(RELEASE.make_latest(tag, published), expected)

    def test_inventory_and_checksums_are_exact_and_deterministic(self):
        names = RELEASE.asset_names(TAG)
        self.assertEqual(len(names), 12)
        self.assertEqual(names, sorted(set(names)))
        self.assertEqual(sum(name.endswith("-symbols.tar.gz") for name in names), 5)
        self.assertIn(f"caudra-{TAG}-x86_64-pc-windows-msvc.zip", names)
        for name in reversed(names):
            (self.artifacts / name).write_bytes(name.encode())
        RELEASE.assert_inventory(self.artifacts, names)
        expected = "".join(
            f"{hashlib.sha256(name.encode()).hexdigest()}  {name}\n" for name in names
        ).encode()
        self.assertEqual(RELEASE.checksums(self.artifacts, TAG), expected)
        (self.artifacts / RELEASE.CHECKSUMS).write_bytes(expected)
        self.assertEqual(RELEASE.checksums(self.artifacts, TAG), expected)
        with self.assertRaisesRegex(ValueError, INVENTORY_ERROR):
            RELEASE.assert_inventory(self.artifacts, names)
        (self.artifacts / RELEASE.CHECKSUMS).unlink()
        (self.artifacts / names[0]).unlink()
        with self.assertRaisesRegex(ValueError, INVENTORY_ERROR):
            RELEASE.assert_inventory(self.artifacts, names)
        (self.artifacts / names[0]).touch()
        with self.assertRaisesRegex(ValueError, "nonempty regular file"):
            RELEASE.assert_inventory(self.artifacts, names)

    def test_complete_preview_publication_verifies_downloaded_bytes(self):
        self.seed_archives()
        self.publish()
        self.assertEqual(len(self.uploaded), 13)
        self.assertEqual(
            self.uploaded["install.sh"], (self.root / "install.sh").read_bytes()
        )
        self.assertEqual(
            self.uploaded[RELEASE.CHECKSUMS], RELEASE.checksums(self.artifacts, TAG)
        )
        self.assertEqual(
            self.publications,
            [
                {
                    "name": TITLE,
                    "body": RELEASE.release_notes(TAG, COMMIT),
                    "draft": False,
                    "prerelease": True,
                    "make_latest": "false",
                }
            ],
        )
        before = self.upload_calls.copy()
        with self.assertRaisesRegex(ValueError, PUBLISHED_ERROR):
            self.publish()
        self.assertEqual(self.upload_calls, before)

    def test_published_or_immutable_retries_never_upload(self):
        self.seed_archives()
        for changes in ({"draft": False}, {"immutable": True}):
            self.remote = {**copy.deepcopy(DRAFT), **changes}
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.publish()
            self.assertFalse(self.upload_calls)
            self.assertFalse(self.publications)

    def test_recheck_between_uploads_stops_if_draft_was_published(self):
        self.seed_archives()
        self.publish_after_upload = True
        with self.assertRaisesRegex(ValueError, PUBLISHED_ERROR):
            self.publish()
        self.assertEqual(len(self.upload_calls), 1)
        self.assertFalse(self.publications)

    def test_failed_upload_leaves_draft_and_fresh_retry_completes(self):
        self.seed_archives()
        self.fail_upload = "install.ps1"
        with self.assertRaisesRegex(RuntimeError, UPLOAD_ERROR):
            self.publish()
        self.assertTrue(self.remote["draft"])
        self.assertFalse(self.publications)
        self.assertTrue(self.uploaded)
        self.fail_upload = None
        for name in (*RELEASE.INSTALLERS, RELEASE.CHECKSUMS):
            (self.artifacts / name).unlink()
        self.publish()
        self.assertFalse(self.remote["draft"])

    def test_corrupted_remote_assets_are_never_published(self):
        self.seed_archives()
        self.corrupt_download = True
        with self.assertRaisesRegex(ValueError, "checksums do not match"):
            self.publish()
        self.assertFalse(self.publications)
        self.assertTrue(self.remote["draft"])

    def test_missing_archive_fails_before_any_upload(self):
        with self.assertRaisesRegex(ValueError, INVENTORY_ERROR):
            self.publish()
        self.assertFalse(self.upload_calls)

    def test_unexpected_remote_asset_is_not_deleted_or_published(self):
        self.seed_archives()
        self.remote["assets"] = [{"name": "unexpected.txt"}]
        with self.assertRaisesRegex(ValueError, "unexpected assets"):
            self.publish()
        self.assertFalse(self.upload_calls)
        self.assertFalse(self.publications)

    def test_api_failure_is_not_mistaken_for_missing_release(self):
        with (
            patch.object(RELEASE, "assert_tag_commit"),
            patch.object(
                RELEASE, "releases", side_effect=subprocess.CalledProcessError(1, "gh")
            ),
            patch.object(RELEASE, "gh") as gh,
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                RELEASE.prepare(TAG, COMMIT)
            gh.assert_not_called()

    def test_release_listing_includes_all_pages(self):
        with patch.object(
            RELEASE, "gh", return_value='[[{"id": 1}], [{"id": 2}]]'
        ) as gh:
            self.assertEqual(RELEASE.releases(), [{"id": 1}, {"id": 2}])
            self.assertIn("--paginate", gh.call_args.args)


class ReleaseWorkflowTests(unittest.TestCase):
    def test_actions_use_node24_versions_and_ubuntu_is_pinned(self):
        versions = {
            "actions/checkout": "v7",
            "dorny/paths-filter": "v4",
            "actions/upload-artifact": "v7",
            "actions/download-artifact": "v8",
            "actions/create-github-app-token": "v3",
        }
        for workflow in WORKFLOWS.glob("*.yml"):
            with self.subTest(workflow=workflow.name):
                text = workflow.read_text()
                self.assertNotRegex(
                    text, r"(?m)^\s*(?:runs-on|runner):\s*ubuntu-latest\s*$"
                )
                for action, version in re.findall(r"uses:\s*([^\s@]+)@([^\s]+)", text):
                    if action in versions:
                        self.assertEqual(version, versions[action], action)

    def workflow(self, name):
        text = (WORKFLOWS / f"{name}.yml").read_text()
        header, jobs = text.split("\njobs:\n", 1)
        sections = re.split(r"(?m)^  ([a-z][a-z0-9-]*):\n", jobs)[1:]
        return header, dict(zip(sections[::2], sections[1::2], strict=True))

    def test_release_requires_reusable_rust_nix_and_python_before_drafting(self):
        header, jobs = self.workflow("release")
        for name in ("rust", "nix"):
            job = jobs[f"verify-{name}"]
            self.assertIn(f"uses: ./.github/workflows/{name}.yml", job)
            self.assertIn("needs: validate", job)
            self.assertIn("source-sha: ${{ github.sha }}", job)
        self.assertIn(
            "needs: [verify-rust, verify-nix, verify-python]", jobs["create-release"]
        )
        self.assertIn("needs: validate", jobs["verify-python"])
        for command in (
            "ruff format --check scripts/",
            "make pylint distribution-tests",
            "python3 scripts/test-update-website-docs.py",
        ):
            self.assertIn(command, jobs["verify-python"])
        self.assertNotIn("cargo ", jobs["verify-python"])
        self.assertNotIn("nix build", jobs["verify-python"])
        self.assertNotIn("actions: write", header)
        self.assertEqual(
            [name for name, body in jobs.items() if "actions: write" in body],
            ["verify-nix"],
        )

    def test_reusable_calls_force_all_gates_at_the_callers_exact_commit(self):
        for name in ("rust", "nix"):
            with self.subTest(workflow=name):
                header, jobs = self.workflow(name)
                self.assertIn("workflow_call:", header)
                self.assertIn("source-sha:\n        required: true", header)
                self.assertIn('CAUDRA_ENABLE_UPDATE_CHECK: "0"', header)
                self.assertIn("pull_request:", header)
                self.assertIn("push:", header)
                changes = jobs["changes"]
                self.assertIn(
                    "code: ${{ inputs.source-sha != '' && 'true' || steps.filter.outputs.code }}",
                    changes,
                )
                self.assertIn('test "$SOURCE_SHA" = "$GITHUB_SHA"', changes)
                self.assertIn(
                    "SOURCE_SHA: ${{ inputs.source-sha || github.sha }}", changes
                )
                self.assertIn("uses: dorny/paths-filter@v4", changes)
                self.assertIn("if: inputs.source-sha == ''", changes)
                for job, body in jobs.items():
                    if job not in ("changes", "ci-pass"):
                        self.assertIn("needs: changes", body)
                        self.assertIn("if: needs.changes.outputs.code == 'true'", body)
                    for checkout in body.split("uses: actions/checkout@v7")[1:]:
                        self.assertTrue(
                            checkout.startswith(
                                "\n        with:\n          ref: ${{ github.sha }}\n"
                                "          persist-credentials: false\n"
                            ),
                            job,
                        )

    def test_reusable_aggregates_cannot_hide_failed_cancelled_or_skipped_gates(self):
        for name in ("rust", "nix"):
            with self.subTest(workflow=name):
                _, jobs = self.workflow(name)
                aggregate = jobs["ci-pass"]
                needs = re.search(r"needs: \[([^\]]+)\]", aggregate)
                if needs is None:
                    self.fail("Aggregate must declare every prerequisite")
                self.assertEqual(
                    {job.strip() for job in needs[1].split(",")},
                    set(jobs) - {"ci-pass"},
                )
                for guard in (
                    "if: always()",
                    "needs.changes.result != 'success'",
                    "contains(needs.*.result, 'failure')",
                    "contains(needs.*.result, 'cancelled')",
                    "needs.changes.outputs.code == 'true' && contains(needs.*.result, 'skipped')",
                    "inputs.source-sha != '' && needs.changes.outputs.code != 'true'",
                    "run: exit 1",
                ):
                    self.assertIn(guard, aggregate)

    def test_existing_platform_and_locked_source_gates_are_preserved(self):
        _, rust = self.workflow("rust")
        self.assertIn('".config/nextest.toml"', rust["changes"])
        self.assertEqual(
            set(rust),
            {
                "changes",
                "fmt",
                "fmt-lua",
                "lint",
                "test",
                "machete",
                "workcell-native",
                "build",
                "macos",
                "windows",
                "ci-pass",
            },
        )
        for command in (
            "staged_files_keep_exact_permission_bits",
            "metadata_descriptors_refuse_unsupported_platforms",
            "directory_ancestry_respects_sticky_permissions",
            "cargo clippy --locked -p caudra-sandbox --tests -- -D warnings",
            "cargo test --locked -p caudra-sandbox local_admin::image",
            "make check ARGS=--locked",
        ):
            self.assertIn(command, rust["macos"])
        for job in ("lint", "windows"):
            self.assertIn("make code-worker", rust[job])
            self.assertIn(
                "cargo clippy --locked --all --tests -- -D warnings", rust[job]
            )
        self.assertIn("make test ARGS=--locked", rust["test"])
        self.assertIn("cargo run --locked -p caudra-docgen -- --check", rust["test"])
        self.assertEqual(rust["test"].count("name: Run make gen-docs-check"), 1)
        self.assertIn("make check build workcell-build ARGS=--locked", rust["build"])
        self.assertIn("check-native test-optional-features", rust["workcell-native"])
        self.assertIn("mcp,bundled-worker --test execution", rust["workcell-native"])
        for body in rust.values():
            for command in re.findall(
                r"cargo (?:clippy|check|test|run|build) [^\n]+", body
            ):
                self.assertIn("--locked", command)
        _, nix = self.workflow("nix")
        for command in (
            "nix build --no-update-lock-file .#checks.x86_64-linux.git-dep-hashes",
            "nix build --no-update-lock-file .#checks.x86_64-linux.fmt",
            "run: nix build --no-update-lock-file -L\n",
        ):
            self.assertIn(command, nix["build"])

    def test_release_prefetches_both_locked_graphs_before_caudra_build(self):
        _, jobs = self.workflow("release")
        commands = (
            "cargo fetch --locked --manifest-path Cargo.toml",
            "python3 scripts/build-code-worker.py --target ${{ matrix.target }}",
            'cargo fetch --locked --manifest-path "$(cat target/code-worker/source-manifest-path)"',
            "cargo build --locked --release --package caudra --target ${{ matrix.target }}",
            "python3 scripts/build-attribution.py \\",
        )
        for job in ("build-linux", "build-other"):
            with self.subTest(job=job):
                lines = [line.strip() for line in jobs[job].splitlines()]
                for command in commands:
                    self.assertEqual(lines.count(command), 1, command)
                positions = [lines.index(command) for command in commands]
                self.assertEqual(positions, sorted(positions))

    def test_built_binary_version_must_match_release_tag(self):
        _, jobs = self.workflow("release")
        for job in ("build-linux", "build-other"):
            self.assertIn("--version)", jobs[job])
            self.assertIn("${GITHUB_REF_NAME#v}", jobs[job])
        self.assertIn("-e GITHUB_REF_NAME", jobs["build-linux"])


@unittest.skipUnless(
    os.name == "posix" and shutil.which("bash"), "Unix and Bash required"
)
class NixSmokeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="nix smoke ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.record = self.root / "environment"
        self.hooks = self.root / "hooks"
        binary = self.root / "out/bin/caudra"
        binary.parent.mkdir(parents=True)
        binary.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' "
            + " ".join(f'"${name}"' for name in SMOKE_DIRECTORIES)
            + ' "$*" > "$SMOKE_RECORD"\n'
            'printf "%s\\n" "$SMOKE_STDOUT"\n'
            'printf "%s\\n" "$SMOKE_STDERR" >&2\n'
            'exit "$SMOKE_STATUS"\n'
        )
        binary.chmod(0o755)
        phases = re.findall(
            r"installCheckPhase = ''\n(.*?)\n\s*'';",
            (WORKFLOWS.parent.parent / "flake.nix").read_text(),
            re.DOTALL,
        )
        self.assertEqual(len(phases), 1)
        self.script = (
            'runHook() { printf "%s\\n" "$1" >> "$SMOKE_HOOKS"; }\n' + phases[0]
        )

    def run_smoke(self, stdout, stderr="", status=0):
        self.hooks.unlink(missing_ok=True)
        return subprocess.run(
            ["bash", "-euo", "pipefail", "-c", self.script],
            cwd=self.root,
            env={
                **os.environ,
                **dict.fromkeys(SMOKE_DIRECTORIES, "/homeless-shelter"),
                "TMPDIR": str(self.root),
                "out": str(self.root / "out"),
                "SMOKE_RECORD": str(self.record),
                "SMOKE_HOOKS": str(self.hooks),
                "SMOKE_STDOUT": stdout,
                "SMOKE_STDERR": stderr,
                "SMOKE_STATUS": str(status),
            },
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )

    def test_success_uses_private_home_and_xdg_directories(self):
        result = self.run_smoke("file_read\npython_execution")
        self.assertEqual(result.returncode, 0, result.stderr)
        *directories, arguments = self.record.read_text().splitlines()
        self.assertEqual(len(directories), len(SMOKE_DIRECTORIES))
        for directory in directories:
            path = Path(directory)
            self.assertTrue(path.is_relative_to(self.root))
            self.assertTrue(path.is_dir())
            self.assertEqual(path.stat().st_mode & 0o777, 0o700)
        self.assertEqual(
            arguments, "--model openai/gpt-5.1 tools --enabled-only --names"
        )
        self.assertEqual(
            self.hooks.read_text().splitlines(), ["preInstallCheck", "postInstallCheck"]
        )

    def test_missing_worker_and_command_failures_report_captured_output(self):
        for stdout, stderr, status in (
            ("file_read", "", 0),
            ("python_execution_unavailable", "", 0),
            ("python_execution", SMOKE_WORKER_WARNING, 0),
            ("python_execution", SMOKE_STARTUP_ERROR, 7),
        ):
            with self.subTest(stdout=stdout, stderr=stderr, status=status):
                result = self.run_smoke(stdout, stderr, status)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(stdout, result.stderr)
                if stderr:
                    self.assertIn(stderr, result.stderr)
                self.assertEqual(
                    self.hooks.read_text().splitlines(), ["preInstallCheck"]
                )


if __name__ == "__main__":
    unittest.main()
