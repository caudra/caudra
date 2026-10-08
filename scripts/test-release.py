#!/usr/bin/env python3

import copy
import hashlib
import importlib.util
import io
import json
import os
import re
import runpy
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
import zipfile
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
COMPACT_POLICY = "<!-- caudra-attribution-layout: compact -->"
DRAFT = {
    "id": 123,
    "tag_name": TAG,
    "target_commitish": COMMIT,
    "draft": True,
    "immutable": False,
    "prerelease": True,
    "assets": [],
    "body": COMPACT_POLICY,
}
PUBLISHED_ERROR = "Published releases must never be modified"
INVENTORY_ERROR = "Asset inventory mismatch"
UPLOAD_ERROR = "simulated interrupted upload"
WORKFLOWS = Path(__file__).resolve().parent.parent / ".github/workflows"
SMOKE_WORKER_WARNING = "Workcell python_execution is unavailable"
SMOKE_STARTUP_ERROR = "resolve data directory: Permission denied"
CURATED_NOTES = "### Highlights\n\nVersion-specific user-facing changes.\n"
WORKER_ACTION = WORKFLOWS.parent / "actions/code-worker/action.yml"
MUSL_WORKER_ACTION = WORKFLOWS.parent / "actions/musl-worker/action.yml"
WORKER_CACHE_PATHS = (
    "target/code-worker/bin",
    "target/code-worker/symbols",
    "target/code-worker/build-fingerprint",
    "target/code-worker/source-manifest-path",
    "${{ steps.identity.outputs.source }}",
)
ALPINE_CACHE_PATHS = (
    "${{ runner.temp }}/alpine-cargo/registry/cache",
    "${{ runner.temp }}/alpine-cargo/git/db",
)
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
        root_patch = patch.object(RELEASE, "ROOT", self.root)
        root_patch.start()
        self.addCleanup(root_patch.stop)
        notes = self.root / "release-notes"
        notes.mkdir()
        for tag in (TAG, "v0.2.0"):
            (notes / f"{tag[1:]}.md").write_text(CURATED_NOTES, encoding="utf-8")
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "caudra"\nversion.workspace = true\n'
            '[workspace]\nmembers = ["workcell", "workcell/crates/tool-contract"]\n'
            'exclude = ["vendor/example"]\n'
            f'[workspace.package]\nversion = "{TAG[1:]}"\n'
        )
        for member, name in (
            ("workcell", "workcell-mcp"),
            ("workcell/crates/tool-contract", "workcell-tool-contract"),
            ("vendor/example", "example"),
        ):
            directory = self.root / member
            directory.mkdir(parents=True)
            version = (
                'version = "1.0.0"' if name == "example" else "version.workspace = true"
            )
            (directory / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\n{version}\n'
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

    @staticmethod
    def file_records(files):
        return [
            {
                "path": name,
                "sha256": hashlib.sha256(data).hexdigest(),
                "size": len(data),
            }
            for name, data in sorted(files.items())
        ]

    @staticmethod
    def tar_bytes(files):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w:gz") as archive:
            for name, data in files.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        return output.getvalue()

    def attribution_files(self, target, layout="compact"):
        evidence = {
            "LICENSE": b"license text\n",
            "NOTICE": b"copyright notice\n",
            "ATTRIBUTION.txt": b"inventory\n",
            "policy.json": b"{}\n",
            "graphs/artifact/Cargo.lock": b"version = 4\n",
            "graphs/worker/Cargo.lock": b"version = 4\n",
            "rust-runtime/COPYRIGHT-library.html": b"runtime legal report\n",
        }
        graphs = [
            {
                "name": name,
                "package": package,
                "version": version,
                "default_features": name != "worker",
                "lockfile": f"graphs/{name}/Cargo.lock",
                "packages": ["fixture"],
            }
            for name, package, version in (
                ("artifact", "caudra", TAG[1:]),
                ("worker", "monty-runtime", "1.0.0"),
            )
        ]
        evidence["manifest.json"] = json.dumps(
            {
                "schema_version": 1,
                "target": target,
                "files": self.file_records(evidence),
                "graphs": graphs,
                "packages": [
                    {
                        "id": "fixture",
                        "notices": ["LICENSE"],
                        "selected_license": ["MIT"],
                    }
                ],
                "rust_runtime": [
                    {
                        "graphs": ["artifact", "worker"],
                        "report": "rust-runtime/COPYRIGHT-library.html",
                        "files": [{"path": "rust-runtime/COPYRIGHT-library.html"}],
                    }
                ],
            }
        ).encode()
        if layout == "expanded":
            return evidence
        files = {
            "LICENSE": evidence["LICENSE"],
            "NOTICE": evidence["NOTICE"],
            "THIRD_PARTY_NOTICES.txt": b"dependency license text\n",
            "ATTRIBUTION.txt": RELEASE.COMPACT_MARKER + b"See attribution.tar.gz\n",
            "attribution.tar.gz": self.tar_bytes(evidence),
        }
        files["manifest.json"] = json.dumps(
            {
                "schema_version": 2,
                "layout": "compact",
                "target": target,
                "files": self.file_records(files),
                "graphs": [
                    {key: graph[key] for key in RELEASE.GRAPH_IDENTITY}
                    for graph in graphs
                ],
                "evidence_manifest": {
                    "archive": "attribution.tar.gz",
                    "member": "manifest.json",
                    "sha256": hashlib.sha256(evidence["manifest.json"]).hexdigest(),
                    "size": len(evidence["manifest.json"]),
                },
            }
        ).encode()
        return files

    def seed_archives(self, layout="compact"):
        for target in RELEASE.TARGETS:
            files = {
                f"licenses/{name}": data
                for name, data in self.attribution_files(target, layout).items()
            }
            for name in RELEASE.asset_names(TAG):
                if target not in name:
                    continue
                path = self.artifacts / name
                payload = {
                    **files,
                    "caudra.exe"
                    if target.endswith("windows-msvc")
                    else "caudra": b"binary fixture",
                }
                if name.endswith("-symbols.tar.gz"):
                    payload[
                        "monty.exe" if target.endswith("windows-msvc") else "monty"
                    ] = b"worker symbols fixture"
                if name.endswith(".zip"):
                    with zipfile.ZipFile(path, "w") as archive:
                        for member, data in payload.items():
                            archive.writestr(member, data)
                else:
                    path.write_bytes(self.tar_bytes(payload))

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

    def publish(self, layout="compact"):
        with (
            patch.object(RELEASE, "ROOT", self.root),
            patch.object(RELEASE, "api", side_effect=self.fake_api),
            patch.object(RELEASE, "releases", side_effect=lambda: [self.remote]),
            patch.object(RELEASE, "gh", side_effect=self.fake_gh),
        ):
            RELEASE.publish(TAG, COMMIT, self.artifacts, layout)

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

    def test_release_notes_include_curated_content_and_exact_provenance(self):
        notes = RELEASE.release_notes(TAG, COMMIT)
        for expected in (
            f"## {TITLE}",
            "Preview / prerelease.",
            CURATED_NOTES.strip(),
            COMPACT_POLICY,
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

    def test_notes_must_exist_for_the_exact_version_and_be_reviewed(self):
        path = self.root / "release-notes" / f"{TAG[1:]}.md"
        for content in (
            "",
            "  \n",
            "TODO: write changes",
            "{{ highlights }}",
            COMPACT_POLICY,
        ):
            with self.subTest(content=content), self.assertRaises(ValueError):
                path.write_text(content)
                RELEASE.release_notes(TAG, COMMIT)
        path.unlink()
        with self.assertRaisesRegex(ValueError, "Missing version-specific"):
            RELEASE.release_notes(TAG, COMMIT)
        path.symlink_to(self.root / "release-notes" / "0.2.0.md")
        with self.assertRaisesRegex(ValueError, "Missing version-specific"):
            RELEASE.release_notes(TAG, COMMIT)

    def test_attribution_variable_has_a_compact_default_and_strict_values(self):
        for value in ("", "false", "0", " FALSE ", "\t0\n"):
            with self.subTest(value=value):
                self.assertEqual(RELEASE.attribution_layout(value), "compact")
        for value in ("true", "1", " TRUE "):
            with self.subTest(value=value):
                self.assertEqual(RELEASE.attribution_layout(value), "expanded")
        for value in ("yes", "2", "compact", "false\ntrue", "$(false)"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                RELEASE.attribution_layout(value)

    def test_draft_layout_cannot_change_between_attempts(self):
        for body in (
            "",
            COMPACT_POLICY * 2,
            COMPACT_POLICY.replace("compact", "expanded"),
        ):
            with self.subTest(body=body), self.assertRaisesRegex(ValueError, "layout"):
                RELEASE.assert_release_policy({**DRAFT, "body": body}, "compact")

    def test_expanded_archives_require_explicit_expanded_policy(self):
        self.seed_archives("expanded")
        with self.assertRaisesRegex(ValueError, "layout"):
            self.publish()
        self.assertFalse(self.upload_calls)
        self.remote["body"] = COMPACT_POLICY.replace("compact", "expanded")
        self.publish("expanded")
        self.assertFalse(self.remote["draft"])

    def test_expanded_manifest_requires_legal_evidence_and_resolvable_references(self):
        target = RELEASE.TARGETS[0]
        for mutation in ("empty", "graphs", "packages", "notice", "runtime", "source"):
            files = self.attribution_files(target, "expanded")
            manifest = json.loads(files["manifest.json"])
            if mutation == "empty":
                files = {}
                manifest["files"] = []
            elif mutation in ("graphs", "packages"):
                manifest[mutation] = []
            elif mutation == "notice":
                manifest["packages"][0]["notices"] = ["missing"]
            elif mutation == "runtime":
                manifest["rust_runtime"] = []
            else:
                manifest["packages"][0]["selected_license"] = ["MPL-2.0"]
            files["manifest.json"] = json.dumps(manifest).encode()
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                RELEASE.validate_attribution(files, target, "expanded")

    def test_compact_manifest_rejects_wrong_target_missing_and_corrupt_files(self):
        target = RELEASE.TARGETS[0]
        for mutation in (
            "target",
            "missing",
            "corrupt",
            "extra",
            "evidence",
            "duplicate",
        ):
            files = self.attribution_files(target)
            manifest = json.loads(files["manifest.json"])
            if mutation == "target":
                manifest["target"] = RELEASE.TARGETS[1]
            elif mutation == "missing":
                del files["NOTICE"]
            elif mutation == "corrupt":
                files["NOTICE"] = b"changed"
            elif mutation == "extra":
                files["unexpected"] = b"extra"
            elif mutation == "evidence":
                manifest["evidence_manifest"]["sha256"] = "0" * 64
            else:
                manifest["files"].append(manifest["files"][0])
            files["manifest.json"] = json.dumps(manifest).encode()
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                RELEASE.validate_attribution(files, target, "compact")

    def test_empty_ancillary_notice_is_preserved_alongside_license_text(self):
        target = RELEASE.TARGETS[0]
        files = self.attribution_files(target, "expanded")
        manifest = json.loads(files.pop("manifest.json"))
        files["packages/fixture/AUTHORS"] = b""
        manifest["packages"][0]["notices"].append("packages/fixture/AUTHORS")
        manifest["files"] = self.file_records(files)
        files["manifest.json"] = json.dumps(manifest).encode()
        RELEASE.validate_attribution(files, target, "expanded")
        manifest["packages"][0]["notices"] = ["packages/fixture/AUTHORS"]
        files["manifest.json"] = json.dumps(manifest).encode()
        with self.assertRaisesRegex(ValueError, "Missing package legal notices"):
            RELEASE.validate_attribution(files, target, "expanded")

    def test_archives_reject_unsafe_members_before_publication(self):
        for name in (
            "../escape",
            "/absolute",
            "licenses/../escape",
            "C:/escape",
            "licenses\\escape",
        ):
            with (
                self.subTest(name=name),
                self.assertRaisesRegex(ValueError, "Unsafe archive"),
                tarfile.open(
                    fileobj=io.BytesIO(self.tar_bytes({name: b"bad"})), mode="r:gz"
                ) as archive,
            ):
                RELEASE.read_attribution(archive, "licenses/")

    def test_archive_binary_is_required_before_any_upload(self):
        target = RELEASE.TARGETS[0]
        for payload in (
            {},
            {"caudra": b""},
            {"caudra": b"binary", "unexpected": b"extra"},
        ):
            self.seed_archives()
            files = {
                f"licenses/{name}": data
                for name, data in self.attribution_files(target).items()
            }
            path = self.artifacts / f"caudra-{TAG}-{target}.tar.gz"
            path.write_bytes(self.tar_bytes(files | payload))
            with (
                self.subTest(payload=payload),
                self.assertRaisesRegex(ValueError, "binary/symbol inventory"),
            ):
                self.publish()
            self.assertFalse(self.upload_calls)
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE):
            output = io.BytesIO()
            with tarfile.open(fileobj=output, mode="w:gz") as archive:
                member = tarfile.TarInfo("licenses/NOTICE")
                member.type = kind
                member.linkname = "outside"
                archive.addfile(member)
            with (
                self.subTest(kind=kind),
                self.assertRaisesRegex(ValueError, "Unsafe archive type"),
                tarfile.open(
                    fileobj=io.BytesIO(output.getvalue()), mode="r:gz"
                ) as archive,
            ):
                RELEASE.read_attribution(archive, "licenses/")

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

    def test_checked_in_workspace_packages_inherit_the_release_version(self):
        RELEASE.validate_workspace_versions(Path(__file__).resolve().parent.parent)

    def test_source_rejects_independent_package_versions_even_when_labels_match(self):
        for member in (".", "workcell", "workcell/crates/tool-contract"):
            path = Path(member) / "Cargo.toml"
            manifest = self.root / path
            original = manifest.read_text()
            for version in ('version = "0.1.0"', f'version = "{TAG[1:]}"', ""):
                with (
                    self.subTest(member=member, version=version),
                    patch.object(RELEASE.subprocess, "check_output") as checkout,
                    self.assertRaisesRegex(
                        ValueError,
                        re.escape(
                            f"{path.as_posix()}: {RELEASE.VERSION_INHERITANCE_ERROR}"
                        ),
                    ),
                ):
                    manifest.write_text(
                        original.replace("version.workspace = true", version)
                    )
                    RELEASE.validate_source(
                        RELEASE.REPOSITORY, f"refs/tags/{TAG}", COMMIT, self.root
                    )
                checkout.assert_not_called()
            manifest.write_text(original)

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
            "actions/setup-python": "v6",
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

    def test_heavy_jobs_use_resolved_runners_without_moving_control_jobs(self):
        roles = {
            "rust": {
                "lint": "linux_x64",
                "test": "linux_x64_heavy",
                "workcell-native": "linux_x64",
                "build": "linux_x64_heavy",
                "macos": "macos_arm64",
                "windows": "windows_x64",
            },
            "nix": {"build": "linux_x64"},
        }
        for workflow, runners in roles.items():
            _, jobs = self.workflow(workflow)
            for job, body in jobs.items():
                with self.subTest(workflow=workflow, job=job):
                    expected = "ubuntu-24.04"
                    if job in runners:
                        expected = (
                            "${{ fromJSON(needs.changes.outputs.runners)."
                            + runners[job]
                            + " }}"
                        )
                    self.assertEqual(
                        re.findall(r"(?m)^    runs-on: (.+)$", body), [expected]
                    )
        _, jobs = self.workflow("release")
        targets = {}
        for job, body in jobs.items():
            if job in ("build-linux", "build-other"):
                self.assertIn(
                    "runs-on: ${{ fromJSON(needs.validate.outputs.runners)[matrix.runner] }}",
                    body,
                )
                self.assertIn("needs: [validate, create-release]", body)
                entries = re.findall(r"- target: (\S+)\n\s+runner: (\S+)", body)
                for target, runner in entries:
                    self.assertNotIn(target, targets)
                    targets[target] = runner
            elif "runs-on:" in body:
                self.assertEqual(
                    re.findall(r"(?m)^    runs-on: (\S+)$", body),
                    ["ubuntu-24.04"],
                )
        self.assertEqual(
            targets,
            {
                "x86_64-unknown-linux-musl": "linux_x64_heavy",
                "aarch64-unknown-linux-musl": "linux_arm64",
                "x86_64-apple-darwin": "macos_x64",
                "aarch64-apple-darwin": "macos_arm64",
                "x86_64-pc-windows-msvc": "windows_x64",
            },
        )

    def test_runner_profiles_resolve_once_in_control_jobs_without_shell_interpolation(
        self,
    ):
        for workflow, control in (
            ("rust", "changes"),
            ("nix", "changes"),
            ("release", "validate"),
        ):
            with self.subTest(workflow=workflow):
                header, jobs = self.workflow(workflow)
                body = jobs[control]
                invocation = 'run: python3 scripts/ci-runners.py >> "$GITHUB_OUTPUT"'
                self.assertEqual("".join(jobs.values()).count(invocation), 1)
                self.assertIn(invocation, body)
                self.assertIn("run: python3 scripts/test-ci-runners.py", body)
                self.assertIn("id: runners", body)
                for output in ("profile", "runners"):
                    self.assertIn(
                        f"{output}: ${{{{ steps.runners.outputs.{output} }}}}", body
                    )
                selection = "vars.CAUDRA_RUNNER_PROFILE || 'github'"
                if control == "changes":
                    selection = "inputs.runner-profile || " + selection
                    self.assertIn(
                        "runner-profile:\n        required: false\n        type: string",
                        header,
                    )
                    for path in (
                        "scripts/ci-runners.py",
                        "scripts/test-ci-runners.py",
                        "scripts/test-release.py",
                    ):
                        self.assertIn(f'- "{path}"', body)
                    self.assertLess(body.index(invocation), body.index("id: filter"))
                self.assertIn(
                    "env:\n          CAUDRA_RUNNER_PROFILE: ${{ " + selection + " }}",
                    body,
                )
                self.assertEqual(
                    "".join(jobs.values()).count("vars.CAUDRA_RUNNER_PROFILE"), 1
                )
        _, release = self.workflow("release")
        for workflow in ("rust", "nix"):
            self.assertIn(
                "runner-profile: ${{ needs.validate.outputs.profile }}",
                release[f"verify-{workflow}"],
            )

    def test_native_jobs_select_supported_python_before_scripts_and_worker(self):
        setup = (
            "uses: actions/setup-python@v6\n"
            "        with:\n"
            '          python-version: "3.13"\n'
        )
        worker = "uses: ./.github/actions/code-worker"
        for workflow in ("rust", "release"):
            _, jobs = self.workflow(workflow)
            for job, body in jobs.items():
                if worker not in body:
                    continue
                with self.subTest(workflow=workflow, job=job):
                    self.assertEqual(body.count(setup), 1)
                    self.assertLess(body.index(setup), body.index(worker))
                    for invocation in re.finditer(r"\bpython3\s", body):
                        self.assertLess(body.index(setup), invocation.start())

    def test_release_requires_reusable_rust_nix_and_python_before_drafting(self):
        header, jobs = self.workflow("release")
        for name in ("rust", "nix"):
            job = jobs[f"verify-{name}"]
            self.assertIn(f"uses: ./.github/workflows/{name}.yml", job)
            self.assertIn("needs: validate", job)
            self.assertIn("source-sha: ${{ github.sha }}", job)
        self.assertIn(
            "needs: [validate, verify-rust, verify-nix, verify-python]",
            jobs["create-release"],
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
            'cargo fetch --locked --manifest-path "$(cat target/code-worker/source-manifest-path)"',
            "cargo build --locked --release --package caudra --target ${{ matrix.target }}",
            "python3 scripts/build-attribution.py \\",
        )
        for job in ("build-linux", "build-other"):
            with self.subTest(job=job):
                lines = [
                    re.sub(
                        r"^python3 scripts/time-command\.py [\w-]+ ", "", line.strip()
                    ).removesuffix(" --timings")
                    for line in jobs[job].splitlines()
                ]
                for command in commands:
                    self.assertEqual(lines.count(command), 1, command)
                positions = [lines.index(command) for command in commands]
                self.assertEqual(positions, sorted(positions))
                if job == "build-linux":
                    self.assertLess(
                        jobs[job].index("uses: ./.github/actions/musl-worker"),
                        jobs[job].index("Build in Alpine container"),
                    )
                else:
                    self.assertLess(
                        lines.index(
                            "python3 scripts/build-code-worker.py --target ${{ matrix.target }}"
                        ),
                        positions[1],
                    )

    def test_built_binary_version_must_match_release_tag(self):
        _, jobs = self.workflow("release")
        for job in ("build-linux", "build-other"):
            self.assertIn("--version)", jobs[job])
            self.assertIn("${GITHUB_REF_NAME#v}", jobs[job])
        self.assertIn("-e GITHUB_REF_NAME", jobs["build-linux"])

    def test_native_worker_cache_is_shared_and_saved_before_workspace_cleanup(self):
        action = WORKER_ACTION.read_text()
        restore, validate_and_save = action.split("    - shell: bash\n", 1)
        self.assertIn("uses: actions/cache/restore@v5", restore)
        self.assertNotIn("restore-keys:", restore)
        self.assertNotIn("if:", restore)
        validate, save = validate_and_save.split("uses: actions/cache/save@v5", 1)
        self.assertIn("python3 scripts/build-code-worker.py", validate)
        self.assertIn("TIME_WORKER: ${{ inputs.timing }}", validate)
        self.assertIn(
            "python3 scripts/time-command.py worker python3 scripts/build-code-worker.py",
            validate,
        )
        self.assertNotIn("if:", validate)
        self.assertIn("if: steps.cache.outputs.cache-hit != 'true'", save)
        self.assertIn("key: ${{ steps.cache.outputs.cache-primary-key }}", save)
        paths = re.findall(r"(?m)^          (\S.+)$", restore + save)
        self.assertEqual(paths, list(WORKER_CACHE_PATHS) * 2)
        for identity in (
            '["rustc", "-vV"]',
            'worker["fingerprint"](toolchain, target)',
            "Path.cwd()",
            "cargo_home",
            "hashFiles('.cargo/config.toml', '.github/actions/code-worker/action.yml')",
            "/registry/src/*/monty-runtime-{worker['VERSION']}",
        ):
            self.assertIn(identity, action)
        for isolated_key in ("github.ref", "github.sha", "github.job", "runner.temp"):
            self.assertNotIn(isolated_key, action)
        _, rust = self.workflow("rust")
        _, release = self.workflow("release")
        for job in ("lint", "test", "workcell-native", "build", "macos", "windows"):
            with self.subTest(job=job):
                self.assertEqual(
                    rust[job].count("uses: ./.github/actions/code-worker"), 1
                )
                self.assertLess(
                    rust[job].index("uses: Swatinem/rust-cache@v2"),
                    rust[job].index("uses: ./.github/actions/code-worker"),
                )
        native = release["build-other"]
        self.assertLess(
            native.index("uses: Swatinem/rust-cache@v2"),
            native.index("uses: ./.github/actions/code-worker"),
        )
        self.assertLess(
            native.index("uses: ./.github/actions/code-worker"),
            native.index("      - name: Build\n"),
        )
        self.assertIn('".github/actions/code-worker/**"', rust["changes"])
        self.assertIn('".cargo/config.toml"', rust["changes"])

    def test_native_release_dependency_cache_is_stable_and_survives_failure(self):
        _, release = self.workflow("release")
        cache = release["build-other"].split("uses: Swatinem/rust-cache@v2", 1)[1]
        cache = cache.split("\n      - ", 1)[0]
        self.assertIn("key: release-${{ matrix.target }}", cache)
        self.assertIn("cache-on-failure: true", cache)
        self.assertNotIn("github.ref", cache)

    def test_release_layout_is_frozen_and_passed_to_all_artifacts(self):
        _, jobs = self.workflow("release")
        self.assertEqual(
            "".join(jobs.values()).count("vars.CAUDRA_EXPANDED_ATTRIBUTION"), 1
        )
        self.assertIn(
            "attribution-layout: ${{ steps.source.outputs.attribution-layout }}",
            jobs["validate"],
        )
        for name in ("create-release", "build-linux", "build-other", "publish"):
            with self.subTest(job=name):
                self.assertIn(
                    "ATTRIBUTION_LAYOUT: ${{ needs.validate.outputs.attribution-layout }}",
                    jobs[name],
                )
                self.assertIn('--layout "$ATTRIBUTION_LAYOUT"', jobs[name])
        self.assertIn("-e ATTRIBUTION_LAYOUT", jobs["build-linux"])
        self.assertNotIn(
            "LICENSE NOTICE.md THIRD_PARTY_LICENSES licenses", "".join(jobs.values())
        )

    def test_release_executes_only_the_dedicated_worker_smoke_target(self):
        _, jobs = self.workflow("release")
        for name in ("build-linux", "build-other"):
            with self.subTest(job=name):
                body = jobs[name]
                self.assertEqual(body.count("--test embedded_worker"), 2)
                self.assertIn("--test embedded_worker --no-run --timings", body)
                self.assertIn(
                    "time-command.py smoke-execute cargo test --locked --release", body
                )
                self.assertNotIn("caudra-workcell production_host_executes", body)
                self.assertIn("path: target/cargo-timings/", body)
        self.assertIn('WORKCELL_REQUIRE_CODE_WORKER: "1"', self.workflow("release")[0])

    def test_musl_cache_keeps_worker_identity_and_source_without_build_targets(self):
        _, jobs = self.workflow("release")
        body = jobs["build-linux"]
        cache = MUSL_WORKER_ACTION.read_text()
        self.assertNotIn("restore-keys:", cache)
        self.assertIn("steps.identity.outputs.key", cache)
        self.assertIn("scripts/build-code-worker.py", cache)
        self.assertIn("musl-worker-v2-alpine-3.21-", cache)
        self.assertIn('"$ALPINE_IMAGE" sh -c', body)
        self.assertIn("source-manifest-path", cache)
        self.assertIn("steps.identity.outputs.source", cache)
        self.assertIn('worker["validate_version"](Path("Cargo.lock"))', cache)
        self.assertIn('python3 scripts/build-code-worker.py --target "$TARGET"', cache)
        self.assertNotIn("code-worker-build", cache)
        self.assertNotIn("target/release", cache)
        self.assertIn("uses: ./.github/actions/musl-worker", body)
        self.assertIn("ALPINE_IMAGE: ${{ steps.worker.outputs.image }}", body)
        for text in (body, cache):
            self.assertIn(
                '"$GITHUB_WORKSPACE/target/code-worker/source:/cargo/registry/src"',
                text,
            )

    def test_musl_warmer_only_produces_from_trusted_main_on_github(self):
        header, jobs = self.workflow("worker-cache")
        self.assertIn("workflow_dispatch:", header)
        self.assertIn("branches: [main]", header)
        self.assertNotIn("pull_request", header)
        self.assertIn(
            "github.repository == 'caudra/caudra' && github.ref == 'refs/heads/main'",
            jobs["warm"],
        )
        self.assertIn("uses: ./.github/actions/musl-worker", jobs["warm"])
        self.assertIn("runner: ubuntu-24.04\n", jobs["warm"])
        self.assertIn("runner: ubuntu-24.04-arm\n", jobs["warm"])
        self.assertNotIn("vars.CAUDRA_RUNNER_PROFILE", jobs["warm"])
        self.assertNotIn("cargo build", jobs["warm"])

    def test_worker_identity_reuses_hosts_but_separates_flags_toolchains_and_paths(
        self,
    ):
        script = WORKER_ACTION.read_text().split("      run: |\n", 1)[1]
        script = textwrap.dedent(script.split("    - uses:", 1)[0])
        toolchain = (
            "rustc 1.99.0\ncommit-hash: original\nhost: x86_64-unknown-linux-gnu\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            output = root / "output"
            identity_script = root / "identity.py"
            identity_script.write_text(script)

            def identity(ref, flags="", compiler=toolchain, cargo_home=root / "cargo"):
                output.write_text("")
                with (
                    patch.dict(
                        os.environ,
                        {
                            "GITHUB_OUTPUT": str(output),
                            "CARGO_HOME": str(cargo_home),
                            "GITHUB_REF": ref,
                            "RUSTFLAGS": flags,
                        },
                    ),
                    patch("subprocess.check_output", return_value=compiler),
                ):
                    runpy.run_path(str(identity_script))
                return dict(
                    line.split("=", 1) for line in output.read_text().splitlines()
                )

            main = identity("refs/heads/main")
            self.assertEqual(main, identity(f"refs/tags/{TAG}"))
            self.assertNotEqual(
                main["key"], identity("refs/heads/main", "-C opt-level=2")["key"]
            )
            self.assertNotEqual(
                main["key"],
                identity(
                    "refs/heads/main", compiler=toolchain.replace("original", "changed")
                )["key"],
            )
            self.assertNotEqual(
                main["key"],
                identity("refs/heads/main", cargo_home=root / "relocated")["key"],
            )
            self.assertTrue(
                main["source"].startswith(
                    f"{(root / 'cargo').as_posix()}/registry/src/*/monty-runtime-"
                )
            )

    def test_alpine_cache_contains_only_downloads_with_compatible_identity(self):
        _, release = self.workflow("release")
        linux = release["build-linux"]
        restore = linux.split("uses: actions/cache/restore@v5", 1)[1]
        restore = restore.split("\n      - ", 1)[0]
        save = linux.split("uses: actions/cache/save@v5", 1)[1]
        save = save.split("\n      - ", 1)[0]
        for operation in (restore, save):
            self.assertEqual(
                re.findall(r"(?m)^            (.+)$", operation.split("path: |", 1)[1]),
                list(ALPINE_CACHE_PATHS),
            )
        self.assertIn("alpine-cargo-v1-3.21-${{ matrix.target }}-", restore)
        self.assertIn("hashFiles('rust-toolchain.toml', '.cargo/config.toml')", restore)
        self.assertIn(
            "hashFiles('Cargo.lock', 'scripts/build-code-worker.py')", restore
        )
        self.assertNotIn("github.ref", restore)
        self.assertIn("!cancelled()", save)
        self.assertIn("steps.cargo-cache.outputs.cache-hit != 'true'", save)
        self.assertIn('"$RUNNER_TEMP/alpine-cargo:/cargo"', linux)
        self.assertIn("-e CARGO_HOME=/cargo", linux)
        self.assertIn('. "$CARGO_HOME/env"', linux)
        cleanup = linux.split("- name: Restore ownership after Alpine", 1)[1]
        cleanup = cleanup.split("\n      - ", 1)[0]
        self.assertIn("if: always()", cleanup)
        self.assertIn('sudo chown -R "$(id -u):$(id -g)"', cleanup)
        self.assertIn(
            'for directory in target licenses "$RUNNER_TEMP/alpine-cargo"', cleanup
        )
        self.assertLess(
            linux.index("Restore ownership after Alpine"),
            linux.index("uses: actions/cache/save@v5"),
        )

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "Unix and Bash required"
    )
    def test_alpine_preparation_leaves_attribution_output_fresh_and_cleanup_handles_failures(
        self,
    ):
        _, release = self.workflow("release")
        linux = release["build-linux"]
        prepare = linux.split("- name: Prepare Alpine cache directories", 1)[1]
        prepare = prepare.split("\n      - ", 1)[0].split("run: ", 1)[1].strip()
        cleanup = linux.split("- name: Restore ownership after Alpine", 1)[1]
        cleanup = cleanup.split("\n      - ", 1)[0].split("run: |\n", 1)[1]
        cleanup = textwrap.dedent(cleanup)
        for phase in ("before_setup", "before_attribution", "after_attribution"):
            with (
                self.subTest(phase=phase),
                tempfile.TemporaryDirectory(prefix="alpine cache ") as temporary,
            ):
                root = Path(temporary).resolve()
                cache = root / "runner temp" / "alpine-cargo"
                licenses = root / "licenses"
                record = root / "ownership"
                environment = os.environ | {
                    "RUNNER_TEMP": str(cache.parent),
                    "RECORD": str(record),
                }
                expected = []
                if phase != "before_setup":
                    subprocess.run(
                        ["bash", "-euo", "pipefail", "-c", prepare],
                        cwd=root,
                        env=environment,
                        check=True,
                    )
                    self.assertTrue((root / "target").is_dir())
                    self.assertTrue(cache.is_dir())
                    self.assertFalse(licenses.exists())
                    expected = ["target", str(cache)]
                if phase == "after_attribution":
                    licenses.mkdir()
                    (licenses / "manifest.json").write_text("verified bundle")
                    expected.insert(1, "licenses")
                subprocess.run(
                    [
                        "bash",
                        "-euo",
                        "pipefail",
                        "-c",
                        'sudo() { printf "%s\\n" "$4" >> "$RECORD"; }\n' + cleanup,
                    ],
                    cwd=root,
                    env=environment,
                    check=True,
                )
                self.assertEqual(
                    record.read_text().splitlines() if record.exists() else [], expected
                )
                if phase == "after_attribution":
                    self.assertEqual(
                        (licenses / "manifest.json").read_text(), "verified bundle"
                    )
                else:
                    self.assertFalse(licenses.exists())


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
