#!/usr/bin/env python3

import hashlib
import io
import json
import os
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
HTTP_HELPER = Path(__file__).with_name("test-install-http.py")
POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")
OLD_BINARY = b"previous executable"
NEW_BINARY = b"replacement executable"
OLD_MANIFEST = b'{"schema_version":1,"previous":true}\n'
OLD_ATTRIBUTION = b"previous attribution\n"
RUN_NOW = "Run Caudra now (no PATH change needed):"
CHILD_PATH_NOTICE = "cannot change PATH in your current terminal"
NO_SHELL_EDITS = "No shell startup files were changed."
ROOT_WARNING = "warning: running as root"
BUNDLE_FILES = {
    "manifest.json": b'{"schema_version":1}\n',
    "ATTRIBUTION.txt": b"Dependency attribution\n",
    "LICENSE": b"Application license\n",
    "NOTICE": b"Application notice\n",
    "dependencies/example/LICENSE": b"Dependency license\n",
    "sources/example.tar.gz": b"Covered dependency source\n",
}


def release(tag, **overrides):
    names = [
        f"caudra-{tag}-x86_64-unknown-linux-musl.tar.gz",
        f"caudra-{tag}-x86_64-pc-windows-msvc.zip",
        "sha256sums.txt",
        "install.sh",
        "install.ps1",
    ]
    return {
        "tag_name": tag,
        "draft": False,
        "prerelease": "-" in tag.split("+")[0],
        "published_at": "2026-10-01T12:00:00Z",
        "assets": [
            {
                "name": name,
                "state": "uploaded",
                "size": 100,
                "browser_download_url": f"https://github.com/caudra/caudra/releases/download/{tag}/{name}",
            }
            for name in names
        ],
        **overrides,
    }


class ReleaseCases(unittest.TestCase):
    def test_default_prefers_stable_over_newer_preview(self):
        self.scenario["pages"] = [[release("v2.0.0-rc.10"), release("v1.0.0")]]
        result = self.install([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0 installed", result.stdout)
        self.assertNotIn("Preview", result.stdout)

    def test_fallback_uses_numeric_semver_order_not_api_order(self):
        self.scenario["pages"] = [[release("v1.0.0-rc.2"), release("v1.0.0-rc.10")]]
        result = self.install([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0-rc.10 (Preview) installed", result.stdout)

    def test_prerelease_ordering(self):
        for smaller, larger in [
            ("v1.0.0-9", "v1.0.0-alpha"),
            ("v1.0.0-alpha", "v1.0.0-alpha.1"),
            ("v1.0.0-alpha.1", "v1.0.0-beta"),
            ("v1.0.0-rc.99999999999999999998", "v1.0.0-rc.99999999999999999999"),
            ("v1.9.0-rc.1", "v1.10.0-rc.1"),
            ("v1.0.0-rc.2+z", "v1.0.0-rc.10+a"),
        ]:
            with self.subTest(larger=larger):
                self.scenario["pages"] = [[release(smaller), release(larger)]]
                result = self.install(["--channel", "preview"])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"{larger} (Preview) installed", result.stdout)

    def test_stable_found_on_later_page_prevents_fallback(self):
        self.scenario["pages"] = [[release("v2.0.0-rc.1")], [release("v1.0.0")]]
        result = self.install([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0 installed", result.stdout)

    def test_explicit_preview_is_preserved(self):
        self.scenario["explicit"] = release("v0.1.0-rc.2")
        result = self.install(["v0.1.0-rc.2"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v0.1.0-rc.2 (Preview)", result.stdout)

    def test_stable_channel_excludes_prereleases(self):
        self.scenario["pages"] = [[release("v1.0.0-rc.1")]]
        result = self.install(["--channel", "stable"])
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.install_dir.exists())

    def test_preview_channel_accepts_stable_only(self):
        self.scenario["pages"] = [[release("v1.0.0")]]
        result = self.install(["--channel", "preview"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0 installed", result.stdout)
        self.assertNotIn("Preview", result.stdout)

    def test_preview_channel_graduation_and_future_preview_across_pages(self):
        for smaller, larger, label in [
            ("v1.0.0-rc.10", "v1.0.0", ""),
            ("v1.0.0", "v2.0.0-rc.1", " (Preview)"),
        ]:
            for tags in [(smaller, larger), (larger, smaller)]:
                with self.subTest(tags=tags):
                    self.scenario["pages"] = [[release(tag)] for tag in tags]
                    result = self.install(["--channel", "preview"])
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f"{larger}{label} installed", result.stdout)

    def test_invalid_arguments(self):
        for args in [
            ["--channel"],
            ["--channel", "nightly"],
            ["--wat"],
            ["v1.0.0", "--channel", "stable"],
            ["v1.0.0", "v2.0.0"],
            ["--channel", "stable", "--channel", "preview"],
            ["1.0.0"],
            ["v01.0.0"],
            ["v1.0.0-rc.01"],
            ["v1.0.0/evil"],
        ]:
            with self.subTest(args=args):
                self.assertNotEqual(self.install(args).returncode, 0)

    def test_discovery_errors_never_fallback(self):
        self.seed_installation()
        for failure in [
            {"http_status": status} for status in [0, 301, 403, 404, 429, 500]
        ] + [
            "not JSON",
            '[{"tag_name":',
            "{}",
            "[null]",
            '[{"draft":false}]',
            "[/* comment */]",
            '[{"draft":false,}]',
            '[{"draft":true,"draft":false}]',
            '[{"draft":true,"\\u0064raft":false}]',
            '[{"draft":false},]',
            "[] false",
        ]:
            with self.subTest(failure=failure):
                self.scenario["pages"] = [[release("v1.0.0-rc.1")], failure]
                result = self.install([])
                self.assertNotEqual(result.returncode, 0)
                self.assert_previous_installation()

    def test_explicit_tag_lookup_never_substitutes_another_release(self):
        self.seed_installation()
        for response in [{"http_status": 404}, release("v2.0.0"), "invalid JSON"]:
            with self.subTest(response=response):
                self.scenario["explicit"] = response
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assert_previous_installation()

    def test_json_strings_and_unicode_escapes_are_not_release_fields(self):
        item = release(
            "v1.0.0-rc.1", body='Notes: "tag_name":"v99.0.0", } ] \\ \n café'
        )
        self.scenario["pages"] = [
            json.dumps([item]).replace('"v1.0.0-rc.1"', '"\\u00761.0.0-rc.1"')
        ]
        result = self.install([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0-rc.1 (Preview)", result.stdout)

    def test_pagination_bound_fails_closed(self):
        self.scenario["pages"] = [[release(f"v1.0.0-rc.{i}")] for i in range(10)]
        result = self.install([])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("10 pages", result.stderr)
        self.assertFalse(self.install_dir.exists())

    def test_duplicate_page_fails_closed(self):
        self.scenario["pages"] = [[release("v1.0.0-rc.1")]] * 2
        result = self.install([])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate release", result.stderr)

    def test_invalid_or_unpublished_releases_are_not_selected(self):
        self.scenario["pages"] = [
            [
                release("v3.0.0", draft=True),
                release("v4.0.0", published_at=None),
                release("v5.0.0", prerelease=True),
                release("v6.0.0-rc.1", prerelease=False),
                release("v01.0.0"),
                release("v1.0.0-rc.01"),
                release("1.0.0"),
                release("v1.0.0-rc.1"),
            ]
        ]
        result = self.install([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v1.0.0-rc.1 (Preview)", result.stdout)

    def test_missing_stable_assets_do_not_trigger_preview_fallback(self):
        self.scenario["pages"] = [
            [release("v2.0.0-rc.1"), release("v1.0.0", assets=[])]
        ]
        result = self.install([])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required uploaded assets", result.stderr)

    def test_preview_graduation_missing_final_assets_fails_closed(self):
        self.scenario["pages"] = [
            [release("v1.0.0-rc.1")],
            [release("v1.0.0", assets=[])],
        ]
        result = self.install(["--channel", "preview"])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required uploaded assets", result.stderr)

    def test_required_assets_must_be_unique_uploaded_and_canonical(self):
        for mode in [
            "duplicate",
            "installer",
            "checksum",
            "archive",
            "origin",
            "state",
            "size",
        ]:
            with self.subTest(mode=mode):
                item = release("v0.1.0")
                assets = item["assets"]
                if mode == "duplicate":
                    assets.append(assets[2].copy())
                elif mode in {"installer", "checksum", "archive"}:
                    item["assets"] = [
                        a
                        for a in assets
                        if not (
                            (mode == "installer" and a["name"].startswith("install."))
                            or (mode == "checksum" and a["name"] == "sha256sums.txt")
                            or (mode == "archive" and a["name"].startswith("caudra-"))
                        )
                    ]
                elif mode == "origin":
                    assets[2]["browser_download_url"] = (
                        "https://example.invalid/sha256sums.txt"
                    )
                elif mode == "state":
                    assets[2]["state"] = "starter"
                else:
                    assets[2]["size"] = 0
                self.scenario["explicit"] = item
                self.assertNotEqual(self.install().returncode, 0)

    def test_checksum_failures_preserve_installation_before_extraction(self):
        self.seed_installation()
        for mode in [
            "corrupt",
            "duplicate",
            "missing",
            "malformed",
            "suffix",
            "traversal",
            "malformed_other",
        ]:
            with self.subTest(mode=mode):
                self.scenario["checksum"] = mode
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("checksum", result.stderr.lower())
                self.assert_previous_installation()
                self.assertFalse((self.root / "extracted").exists())

    def test_checksum_crlf_and_binary_marker(self):
        for mode in ["crlf", "binary"]:
            with self.subTest(mode=mode):
                self.scenario["checksum"] = mode
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_checksum_download_failure_preserves_installation(self):
        self.seed_installation()
        self.scenario["download_failure"] = "sha256sums.txt"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assert_previous_installation()

    binary_name = "caudra"

    def test_published_legacy_archive_root_notices_are_not_installed(self):
        extras = {
            "LICENSE": b"Legacy archive license\n",
            "NOTICE.md": b"Legacy archive notice\n",
            "THIRD_PARTY_LICENSES/nested/LICENSE": b"Legacy archive dependency\n",
        }
        for name, content in extras.items():
            path = self.payload / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.install_dir / self.binary_name).read_bytes(), NEW_BINARY)
        for name, content in BUNDLE_FILES.items():
            self.assertEqual((self.license_dir / name).read_bytes(), content)
        self.assertEqual(
            {path.name for path in self.install_dir.iterdir()}, {self.binary_name}
        )
        self.assertFalse((self.license_dir / "NOTICE.md").exists())
        self.assertFalse((self.license_dir / "THIRD_PARTY_LICENSES").exists())

    def test_unknown_archive_root_is_rejected_before_extraction(self):
        self.seed_installation()
        (self.payload / "unknown-root").write_bytes(NEW_BINARY)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe", result.stderr)
        self.assert_previous_installation()
        self.assertFalse((self.root / "extracted").exists())

    def compact_payload(self):
        bundle = self.payload / "licenses"
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w:gz") as archive:
            for name, data in BUNDLE_FILES.items():
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
        shutil.rmtree(bundle)
        bundle.mkdir()
        files = {
            "LICENSE": BUNDLE_FILES["LICENSE"],
            "NOTICE": BUNDLE_FILES["NOTICE"],
            "THIRD_PARTY_NOTICES.txt": b"Dependency notices\n",
            "ATTRIBUTION.txt": b"CAUDRA-ATTRIBUTION compact-v2\nEvidence: attribution.tar.gz\n",
            "attribution.tar.gz": stream.getvalue(),
        }
        manifest = {
            "schema_version": 2,
            "layout": "compact",
            "target": "x86_64-pc-windows-msvc"
            if self.binary_name.endswith(".exe")
            else "x86_64-unknown-linux-musl",
            "files": [
                {"path": name, "sha256": hashlib.sha256(data).hexdigest()}
                for name, data in files.items()
            ],
        }
        files["manifest.json"] = json.dumps(manifest).encode()
        for name, data in files.items():
            (bundle / name).write_bytes(data)
        return files

    def test_compact_install_and_layout_transitions(self):
        files = self.compact_payload()
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual({p.name for p in self.license_dir.iterdir()}, set(files))
            for name, data in files.items():
                self.assertEqual((self.license_dir / name).read_bytes(), data)
        shutil.rmtree(self.payload / "licenses")
        for name, data in BUNDLE_FILES.items():
            path = self.payload / "licenses" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.license_dir / "dependencies/example/LICENSE").is_file())
        self.compact_payload()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(list(self.license_dir.iterdir())), 6)

    def test_invalid_compact_preserves_previous_pair(self):
        self.seed_installation()
        for case in (
            "missing",
            "corrupt",
            "extra",
            "directory",
            "unsupported",
            "marker",
            "target",
            "duplicate",
            "path",
        ):
            with self.subTest(case=case):
                self.compact_payload()
                bundle = self.payload / "licenses"
                manifest = json.loads((bundle / "manifest.json").read_bytes())
                if case == "missing":
                    (bundle / "attribution.tar.gz").unlink()
                elif case == "corrupt":
                    (bundle / "attribution.tar.gz").write_bytes(b"corrupted companion")
                elif case == "extra":
                    (bundle / "extra").write_bytes(b"unexpected")
                elif case == "directory":
                    (bundle / "attribution.tar.gz").unlink()
                    (bundle / "attribution.tar.gz").mkdir()
                elif case == "marker":
                    (bundle / "ATTRIBUTION.txt").write_bytes(OLD_ATTRIBUTION)
                elif case == "unsupported":
                    manifest["schema_version"] = 3
                elif case == "target":
                    manifest["target"] = "unsupported-target"
                elif case == "duplicate":
                    manifest["files"][1] = manifest["files"][0]
                elif case == "path":
                    manifest["files"][0]["path"] = "../LICENSE"
                (bundle / "manifest.json").write_text(json.dumps(manifest))
                result = self.install()
                self.assertNotEqual(result.returncode, 0, case)
                self.assert_previous_installation()

    def test_managed_backups_are_bounded_and_ambiguous_backups_untouched(self):
        self.compact_payload()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        ambiguous = self.install_dir / ".caudra-backup.ambiguous"
        ambiguous.mkdir()
        (ambiguous / "previous").write_bytes(OLD_BINARY)
        for _ in range(3):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                len(list(self.install_dir.glob(".caudra-backup.*/managed-pair"))), 1
            )
            self.assertEqual(
                len(
                    list(self.license_dir.parent.glob(".caudra-backup.*/managed-pair"))
                ),
                1,
            )
            self.assertEqual((ambiguous / "previous").read_bytes(), OLD_BINARY)

    def test_matching_updater_snapshot_avoids_duplicate_backup(self):
        self.compact_payload()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        snapshot = self.root / "snapshot"
        snapshot.mkdir()
        shutil.copyfile(self.install_dir / self.binary_name, snapshot / "binary")
        shutil.copytree(self.license_dir, snapshot / "licenses")
        self.env["CAUDRA_UPDATE_SNAPSHOT"] = str(snapshot)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.install_dir.glob(".caudra-backup.*")), [])
        self.assertEqual(list(self.license_dir.parent.glob(".caudra-backup.*")), [])
        self.assertTrue((snapshot / "licenses/attribution.tar.gz").is_file())
        (snapshot / "binary").write_bytes(OLD_BINARY)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            len(list(self.install_dir.glob(".caudra-backup.*/previous"))), 1
        )

    def test_failed_compact_update_keeps_previous_managed_backup(self):
        self.compact_payload()
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
        backups = list(self.install_dir.glob(".caudra-backup.*/previous"))
        self.env["TEST_PUBLISH_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(
            list(self.install_dir.glob(".caudra-backup.*/previous")), backups
        )
        self.assertEqual((self.install_dir / self.binary_name).read_bytes(), NEW_BINARY)
        self.assertTrue((self.license_dir / "attribution.tar.gz").is_file())

    def test_unusable_matching_snapshot_keeps_installer_backup(self):
        self.compact_payload()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        snapshot = self.root / "snapshot"
        snapshot.mkdir()
        shutil.copyfile(self.install_dir / self.binary_name, snapshot / "binary")
        shutil.copytree(self.license_dir, snapshot / "licenses")
        self.env["CAUDRA_UPDATE_SNAPSHOT"] = str(snapshot)
        for extra in ("unexpected", "no-license-bundle"):
            with self.subTest(extra=extra):
                (snapshot / extra).write_bytes(OLD_BINARY)
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    len(list(self.install_dir.glob(".caudra-backup.*/previous"))), 1
                )
                self.assertEqual(
                    len(
                        list(self.license_dir.parent.glob(".caudra-backup.*/previous"))
                    ),
                    1,
                )
                (snapshot / extra).unlink()

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve(strict=True)
        self.commands = self.root / "commands"
        self.commands.mkdir()
        self.install_dir = self.root / "custom prefix" / "bin"
        self.license_dir = self.install_dir.parent / "share" / "licenses" / "caudra"
        self.payload = self.root / "payload"
        self.payload.mkdir()
        (self.payload / self.binary_name).write_bytes(NEW_BINARY)
        for name, content in BUNDLE_FILES.items():
            path = self.payload / "licenses" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.archive = self.root / "release.tar.gz"
        self.archive_entries = []
        self.scenario = {"explicit": release("v0.1.0"), "pages": [[release("v0.1.0")]]}
        self.env: dict[str, str] = {
            **os.environ,
            "PATH": f"{self.commands}{os.pathsep}{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "SHELL": "/bin/sh",
            "TMPDIR": str(self.root),
            "CAUDRA_INSTALL_DIR": str(self.install_dir),
            "TEST_ARCHIVE": str(self.archive),
            "TEST_SUDO_LOG": str(self.root / "sudo.log"),
            "TEST_SCENARIO": str(self.root / "scenario.json"),
            "TEST_REQUEST_LOG": str(self.root / "requests.jsonl"),
        }
        for key in ("GITHUB_TOKEN", "GH_TOKEN", "CAUDRA_UPDATE_SNAPSHOT", "ZDOTDIR"):
            self.env.pop(key, None)
        if self.binary_name == "caudra.exe":
            return
        self.command("curl", f'exec "{sys.executable}" "{HTTP_HELPER}" "$@"')
        self.command(
            "tar",
            f'case "$1" in x*) touch "{self.root / "extracted"}";; esac\n'
            f'exec "{shutil.which("tar")}" "$@"',
        )
        self.command("uname", 'case "$1" in -s) echo Linux;; -m) echo x86_64;; esac')
        self.command("id", "printf '1000\\n'")
        self.command(
            "sudo",
            'printf "%s\\n" "$*" >> "$TEST_SUDO_LOG"\n'
            'export TEST_ELEVATED=1\nexec "$@"',
        )
        self.command(
            "mkdir",
            'if [ "${TEST_REQUIRE_SUDO:-}" = 1 ] && '
            '[ "${TEST_ELEVATED:-}" != 1 ]; then exit 1; fi\n'
            f'exec "{shutil.which("mkdir")}" "$@"',
        )
        self.command(
            "cp",
            'if [ "${TEST_COPY_FAILURE:-}" = 1 ]; then exit 1; fi\n'
            'if [ "${TEST_PARTIAL_COPY_FAILURE:-}" = 1 ] && [ "$1" = -R ]; then\n'
            f'  "{shutil.which("mkdir")}" -p "$3"\n'
            '  printf partial > "$3/manifest.json"\n'
            "  exit 1\nfi\n"
            f'exec "{shutil.which("cp")}" "$@"',
        )
        self.command(
            "mv",
            'if [ "${TEST_PUBLISH_FAILURE:-}" = 1 ] && '
            '[ "$2" = "$CAUDRA_INSTALL_DIR/caudra" ]; then\n'
            '  case "$1" in */new) exit 1;; esac\nfi\n'
            'if [ "${TEST_BUNDLE_PUBLISH_FAILURE:-}" = 1 ]; then\n'
            '  case "$1:$2" in */new:*/share/licenses/caudra) exit 1;; esac\nfi\n'
            'if [ "${TEST_ROLLBACK_FAILURE:-}" = 1 ]; then\n'
            '  case "$1" in */previous) exit 1;; esac\nfi\n'
            f'exec "{shutil.which("mv")}" "$@"',
        )

    def command(self, name, body):
        path = self.commands / name
        path.write_text(f"#!/bin/sh\nset -eu\n{body}\n")
        path.chmod(0o755)

    def install(self, args=None):
        Path(self.env["TEST_SCENARIO"]).write_text(json.dumps(self.scenario))
        with tarfile.open(self.archive, "w:gz") as archive:
            for child in sorted(self.payload.iterdir()):
                archive.add(child, arcname=child.name)
            for name, kind in self.archive_entries:
                entry = tarfile.TarInfo(name)
                entry.type = kind
                entry.linkname = (
                    "licenses/NOTICE"
                    if kind in (tarfile.LNKTYPE, tarfile.SYMTYPE)
                    else ""
                )
                archive.addfile(entry)
        return subprocess.run(
            ["sh", str(INSTALLER), *(args if args is not None else ["v0.1.0"])],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def seed_binary(self):
        self.install_dir.mkdir(parents=True)
        (self.install_dir / self.binary_name).write_bytes(OLD_BINARY)

    def seed_installation(self):
        self.seed_binary()
        self.license_dir.mkdir(parents=True)
        (self.license_dir / "manifest.json").write_bytes(OLD_MANIFEST)
        (self.license_dir / "ATTRIBUTION.txt").write_bytes(OLD_ATTRIBUTION)
        (self.license_dir / "NOTICE").write_bytes(BUNDLE_FILES["NOTICE"])

    def assert_previous_installation(self):
        self.assertEqual((self.install_dir / self.binary_name).read_bytes(), OLD_BINARY)
        self.assertEqual(
            (self.license_dir / "manifest.json").read_bytes(), OLD_MANIFEST
        )
        self.assertEqual(
            (self.license_dir / "ATTRIBUTION.txt").read_bytes(), OLD_ATTRIBUTION
        )

    def assert_installed(self):
        self.assertEqual((self.install_dir / "caudra").read_bytes(), NEW_BINARY)
        self.assertTrue(os.access(self.install_dir / "caudra", os.X_OK))
        for name, content in BUNDLE_FILES.items():
            self.assertEqual((self.license_dir / name).read_bytes(), content)


@unittest.skipUnless(os.name == "posix", "Exercises the POSIX installer")
class InstallTests(ReleaseCases):
    def test_shell_specific_path_guidance_does_not_edit_startup_files(self):
        home = Path(self.env["HOME"])
        zdotdir = home / "user's zsh config"
        self.env["ZDOTDIR"] = str(zdotdir)
        configs = [
            home / ".profile",
            home / ".bashrc",
            home / ".bash_profile",
            home / ".zshrc",
            zdotdir / ".zshrc",
            home / ".config/fish/config.fish",
        ]
        for path in configs:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("# user configuration\n")
        before = {path: path.read_bytes() for path in home.rglob("*") if path.is_file()}
        for shell in ("zsh", "bash", "fish", "sh", "unknown", "", None):
            with self.subTest(shell=shell):
                if shell is None:
                    self.env.pop("SHELL", None)
                    probe = subprocess.run(
                        ["sh", "-c", 'printf "%s" "${SHELL-}"'],
                        cwd=self.root,
                        env=self.env,
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=True,
                    )
                    expected_shell = probe.stdout.rsplit("/", 1)[-1]
                else:
                    self.env["SHELL"] = f"/bin/{shell}" if shell else ""
                    expected_shell = shell
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(RUN_NOW, result.stdout)
                self.assertIn(CHILD_PATH_NOTICE, result.stdout)
                self.assertIn(NO_SHELL_EDITS, result.stdout)
                self.assertNotIn(ROOT_WARNING, result.stderr)
                if expected_shell == "fish":
                    self.assertIn("set -gx PATH ", result.stdout)
                    self.assertIn("fish_add_path --prepend -- ", result.stdout)
                    self.assertNotIn("export PATH=", result.stdout)
                else:
                    self.assertIn("export PATH=", result.stdout)
                    if expected_shell == "zsh":
                        self.assertIn(
                            shlex.quote(str(zdotdir / ".zshrc")), result.stdout
                        )
                        self.assertNotIn(str(home / ".zshrc"), result.stdout)
                    elif expected_shell == "bash":
                        self.assertIn(str(home / ".bashrc"), result.stdout)
                        self.assertIn("~/.bash_profile", result.stdout)
                    else:
                        self.assertIn("POSIX sh, ~/.profile", result.stdout)
                self.assertEqual(
                    {
                        path: path.read_bytes()
                        for path in home.rglob("*")
                        if path.is_file()
                    },
                    before,
                )

    def test_zsh_guidance_defaults_to_home_without_creating_config(self):
        self.env["SHELL"] = "/bin/zsh"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str(Path(self.env["HOME"]) / ".zshrc"), result.stdout)
        self.assertFalse(Path(self.env["HOME"]).exists())

    def test_printed_commands_handle_shell_metacharacters(self):
        self.install_dir = (
            self.root / "user's $HOME `false` $(false) \\\" prefix" / "bin"
        )
        self.env["CAUDRA_INSTALL_DIR"] = str(self.install_dir)
        marker = "installed caudra executed"
        (self.payload / self.binary_name).write_text(
            f"#!/bin/sh\nprintf '%s\\n' '{marker}'\n"
        )
        for shell in ("sh", "bash", "zsh", "fish"):
            with self.subTest(shell=shell):
                executable = shutil.which(shell)
                if executable is None:
                    self.skipTest(f"{shell} is not installed")
                self.env["SHELL"] = executable
                result = self.install()
                self.assertEqual(result.returncode, 0, result.stderr)
                lines = result.stdout.splitlines()
                direct = lines[lines.index(RUN_NOW) + 1].strip()
                path_command = next(
                    line.strip()
                    for line in lines
                    if line.startswith(("  export PATH=", "  set -gx PATH "))
                )
                commands = [direct, f"{path_command}\ncaudra"]
                if shell == "fish":
                    commands.append(
                        next(
                            line.strip()
                            for line in lines
                            if line.startswith("  fish_add_path ")
                        )
                        + "\ncaudra"
                    )
                    commands.append("caudra")
                for command in commands:
                    execution = subprocess.run(
                        [executable, "-c", command],
                        env={
                            **self.env,
                            "XDG_CONFIG_HOME": str(self.root / "fish-config"),
                        },
                        cwd=self.root,
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertEqual(execution.returncode, 0, execution.stderr)
                    self.assertEqual(execution.stdout.strip(), marker)

    def test_path_already_contains_install_directory(self):
        self.env["PATH"] += os.pathsep + str(self.install_dir)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(RUN_NOW, result.stdout)
        self.assertNotIn(CHILD_PATH_NOTICE, result.stdout)
        self.assertNotIn("export PATH=", result.stdout)

    def test_similar_path_entry_does_not_suppress_guidance(self):
        self.env["PATH"] += os.pathsep + str(self.install_dir) + "-other"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(CHILD_PATH_NOTICE, result.stdout)

    def test_shadowed_binary_still_reports_direct_invocation(self):
        self.command("caudra", "exit 1")
        self.env["PATH"] += os.pathsep + str(self.install_dir)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(RUN_NOW, result.stdout)
        self.assertIn(shlex.quote(str(self.install_dir / "caudra")), result.stdout)
        self.assertIn(f"which shadows {self.install_dir / 'caudra'}", result.stdout)

    def test_default_root_install_warns_about_user_scope(self):
        self.command("id", "printf '0\\n'")
        self.env.pop("CAUDRA_INSTALL_DIR")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(ROOT_WARNING, result.stderr)
        self.assertIn("for root, not other users", result.stderr)
        self.assertIn("without sudo", result.stderr)
        default_binary = Path(self.env["HOME"]) / ".local/bin/caudra"
        self.assertEqual(default_binary.read_bytes(), NEW_BINARY)
        self.assertIn(str(default_binary.parent), result.stderr)

    def test_explicit_root_install_does_not_claim_directory_is_root_only(self):
        self.command("id", "printf '0\\n'")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(ROOT_WARNING, result.stderr)
        self.assertIn("explicitly selected directory", result.stderr)
        self.assertIn(str(self.install_dir), result.stderr)
        self.assertNotIn("for root, not other users", result.stderr)
        self.assertIn("other users may need to configure their own PATH", result.stderr)
        self.assert_installed()

    def test_unsafe_archive_entries_are_rejected_before_extraction(self):
        self.seed_installation()
        for name, kind in (
            ("../escaped", tarfile.REGTYPE),
            (str(self.root / "escaped"), tarfile.REGTYPE),
            ("licenses/../../escaped", tarfile.REGTYPE),
            ("licenses/NOTICE", tarfile.REGTYPE),
            ("licenses/link", tarfile.SYMTYPE),
            ("licenses/link", tarfile.LNKTYPE),
            ("licenses/pipe", tarfile.FIFOTYPE),
        ):
            with self.subTest(name=name, kind=kind):
                self.archive_entries = [(name, kind)]
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assert_previous_installation()
                self.assertFalse((self.root / "extracted").exists())
                self.assertFalse((self.root / "escaped").exists())

    def test_retention_does_not_follow_backup_symlinks(self):
        self.compact_payload()
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
        stage = next(self.license_dir.parent.glob(".caudra-backup.*"))
        outside = self.root / "outside-backup"
        stage.rename(outside)
        stage.symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(stage.is_symlink())
        self.assertTrue((outside / "previous/attribution.tar.gz").is_file())

    def test_minimal_path_without_python_or_jq_and_available_hashers(self):
        expanded = self.root / "expanded"
        shutil.copytree(self.payload / "licenses", expanded)
        for hasher in ("sha256sum", "shasum", "openssl"):
            executable = shutil.which(hasher)
            if executable is None:
                continue
            with self.subTest(hasher=hasher):
                commands = self.root / hasher
                commands.mkdir()
                for name in ("curl", "uname", "id", "cp", "mv", "mkdir", "tar"):
                    (commands / name).symlink_to(self.commands / name)
                for name in (
                    "sh",
                    "awk",
                    "cat",
                    "wc",
                    "rm",
                    "mktemp",
                    "find",
                    "chmod",
                    "gzip",
                    "touch",
                ):
                    tool = shutil.which(name)
                    assert tool is not None
                    (commands / name).symlink_to(tool)
                (commands / hasher).symlink_to(executable)
                self.env["PATH"] = str(commands)
                result = self.install([])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_installed()
                compact = self.compact_payload()
                result = self.install([])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    {p.name: p.read_bytes() for p in self.license_dir.iterdir()},
                    compact,
                )
                shutil.rmtree(self.payload / "licenses")
                shutil.copytree(expanded, self.payload / "licenses")

    def test_credentials_are_api_only_without_redirects(self):
        self.env["GITHUB_TOKEN"] = "fixture-token"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        requests = [
            json.loads(line)
            for line in Path(self.env["TEST_REQUEST_LOG"]).read_text().splitlines()
        ]
        for args in requests:
            self.assertEqual(args[0], "-q")
            uri = next(arg for arg in args if arg.startswith("https://"))
            if uri.startswith("https://api.github.com/"):
                self.assertIn("Authorization: Bearer fixture-token", args)
                self.assertNotIn("-fsSL", args)
                self.assertIn("--max-time", args)
            else:
                self.assertFalse(any("Authorization" in arg for arg in args))
                self.assertIn("--proto-redir", args)

    def test_install_retains_complete_bundle_with_space_paths(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()

    def test_install_with_symlinked_temporary_parent(self):
        alias = self.root / "temporary alias"
        alias.symlink_to(self.root, target_is_directory=True)
        result = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "InstallTests.test_install_retains_complete_bundle_with_space_paths",
            ],
            env={**os.environ, "TMPDIR": str(alias)},
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_reinstall_preserves_unrelated_files(self):
        self.seed_binary()
        self.license_dir.mkdir(parents=True)
        unrelated = self.license_dir / "user-file"
        unrelated.write_bytes(OLD_BINARY)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        self.assertFalse(unrelated.exists())
        backups = list(self.license_dir.parent.glob(".caudra-backup.*/previous"))
        self.assertEqual(len(backups), 1)
        self.assertEqual((backups[0] / "user-file").read_bytes(), OLD_BINARY)
        binary_backups = list(self.install_dir.glob(".caudra-backup.*/previous"))
        self.assertEqual(len(binary_backups), 1)
        self.assertEqual(binary_backups[0].read_bytes(), OLD_BINARY)

    def test_relative_install_path(self):
        self.env["CAUDRA_INSTALL_DIR"] = "custom prefix/bin"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(shlex.quote(str(self.install_dir / "caudra")), result.stdout)
        self.assertIn(
            f"export PATH={shlex.quote(str(self.install_dir))}:", result.stdout
        )
        self.assert_installed()

    def test_missing_bundle_preserves_previous_binary(self):
        self.seed_binary()
        shutil.rmtree(self.payload / "licenses")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("licenses/manifest.json", result.stderr)
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)

    def test_empty_manifest_preserves_previous_binary(self):
        self.seed_binary()
        (self.payload / "licenses" / "manifest.json").write_bytes(b"")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)

    def test_missing_attribution_preserves_previous_binary(self):
        self.seed_binary()
        (self.payload / "licenses" / "ATTRIBUTION.txt").unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("licenses/ATTRIBUTION.txt", result.stderr)
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)

    def test_failed_license_copy_preserves_previous_binary(self):
        self.seed_binary()
        self.env["TEST_COPY_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)

    def test_elevated_install_copies_binary_and_bundle(self):
        self.env["TEST_REQUIRE_SUDO"] = "1"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        operations = (self.root / "sudo.log").read_text().splitlines()
        for command in ("mkdir", "cp", "mv", "chmod"):
            self.assertTrue(any(line.startswith(f"{command} ") for line in operations))

    def test_partial_license_copy_preserves_previous_bundle_and_binary(self):
        self.seed_installation()
        self.env["TEST_PARTIAL_COPY_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assert_previous_installation()
        self.assertEqual(list(self.license_dir.parent.glob(".caudra-backup.*")), [])

    def test_binary_publication_failure_restores_previous_bundle_and_binary(self):
        self.seed_installation()
        self.env["TEST_PUBLISH_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to publish binary", result.stderr)
        self.assert_previous_installation()
        self.assertEqual(list(self.license_dir.parent.glob(".caudra-backup.*")), [])
        self.assertEqual(list(self.install_dir.glob(".caudra-backup.*")), [])

    def test_failed_first_binary_publication_removes_new_bundle(self):
        self.env["TEST_PUBLISH_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.install_dir / "caudra").exists())
        self.assertFalse(self.license_dir.exists())

    def test_bundle_publication_failure_restores_previous_bundle(self):
        self.seed_installation()
        self.env["TEST_BUNDLE_PUBLISH_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assert_previous_installation()

    def test_failed_recovery_retains_previous_data_for_manual_recovery(self):
        self.seed_installation()
        self.env["TEST_PUBLISH_FAILURE"] = "1"
        self.env["TEST_ROLLBACK_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("recovery failed", result.stderr)
        bundle_backups = list(self.license_dir.parent.glob(".caudra-backup.*/previous"))
        binary_backups = list(self.install_dir.glob(".caudra-backup.*/previous"))
        self.assertEqual(len(bundle_backups), 1)
        self.assertEqual(len(binary_backups), 1)
        self.assertEqual(
            (bundle_backups[0] / "manifest.json").read_bytes(), OLD_MANIFEST
        )
        self.assertEqual(binary_backups[0].read_bytes(), OLD_BINARY)

    def test_elevated_reinstall_retains_previous_bundle(self):
        self.seed_installation()
        self.env["TEST_REQUIRE_SUDO"] = "1"
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()
        backups = list(self.license_dir.parent.glob(".caudra-backup.*/previous"))
        self.assertEqual(len(backups), 1)
        self.assertEqual((backups[0] / "manifest.json").read_bytes(), OLD_MANIFEST)

    def test_source_bundle_symlink_is_refused(self):
        self.seed_installation()
        outside = self.root / "outside"
        outside.write_bytes(OLD_BINARY)
        (self.payload / "licenses" / "link").symlink_to(outside)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing symlink", result.stderr)
        self.assert_previous_installation()
        self.assertEqual(outside.read_bytes(), OLD_BINARY)

    def test_source_bundle_special_file_is_refused(self):
        self.seed_installation()
        os.mkfifo(self.payload / "licenses" / "pipe")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("special file", result.stderr)
        self.assert_previous_installation()

    def test_destination_bundle_symlink_is_refused(self):
        self.seed_binary()
        outside = self.root / "outside"
        outside.mkdir()
        sentinel = outside / "manifest.json"
        sentinel.write_bytes(OLD_MANIFEST)
        self.license_dir.parent.mkdir(parents=True)
        self.license_dir.symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing symlink", result.stderr)
        self.assertEqual(sentinel.read_bytes(), OLD_MANIFEST)
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)

    def test_destination_bundle_nested_symlink_is_refused(self):
        self.seed_installation()
        outside = self.root / "outside"
        outside.write_bytes(OLD_BINARY)
        (self.license_dir / "link").symlink_to(outside)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing symlink", result.stderr)
        self.assert_previous_installation()
        self.assertEqual(outside.read_bytes(), OLD_BINARY)

    def test_binary_destination_symlink_is_refused(self):
        self.seed_installation()
        outside = self.root / "outside"
        outside.write_bytes(OLD_BINARY)
        binary = self.install_dir / "caudra"
        binary.unlink()
        binary.symlink_to(outside)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing symlink", result.stderr)
        self.assert_previous_installation()

    def test_destination_parent_symlink_is_refused_before_directory_creation(self):
        outside = self.root / "outside"
        outside.mkdir()
        self.install_dir.parent.symlink_to(outside, target_is_directory=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing symlink", result.stderr)
        self.assertEqual(list(outside.iterdir()), [])


@unittest.skipUnless(POWERSHELL, "PowerShell is not installed")
class PowerShellTests(ReleaseCases):
    binary_name = "caudra.exe"

    def setUp(self):
        super().setUp()
        self.archive = self.root / "release.zip"
        self.env.update(
            {
                "TEST_ARCHIVE": str(self.archive),
                "TEST_PYTHON": sys.executable,
                "TEST_ARGUMENTS": str(self.root / "arguments.json"),
                "PROCESSOR_ARCHITECTURE": "AMD64",
                "LOCALAPPDATA": str(self.root),
            }
        )

    def install(self, args=None):
        assert POWERSHELL is not None
        Path(self.env["TEST_SCENARIO"]).write_text(json.dumps(self.scenario))
        Path(self.env["TEST_ARGUMENTS"]).write_text(
            json.dumps(args if args is not None else ["v0.1.0"])
        )
        with zipfile.ZipFile(self.archive, "w") as archive:
            for child in sorted(self.payload.rglob("*")):
                if child.is_file():
                    archive.write(child, child.relative_to(self.payload))
        return subprocess.run(
            [
                POWERSHELL,
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(Path(__file__).with_suffix(".ps1")),
            ],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def test_binary_publication_failure_restores_previous_installation(self):
        self.seed_installation()
        self.env["TEST_PUBLISH_FAILURE"] = "1"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assert_previous_installation()

    def test_streaming_size_limits_reject_before_or_during_read(self):
        self.seed_installation()
        for kind, limit in [("api", 4 * 1024 * 1024), ("checksum", 1024 * 1024)]:
            for mode in ("advertised", "unknown", "underreported"):
                with self.subTest(kind=kind, mode=mode):
                    self.scenario.update(oversize_kind=kind, oversize_mode=mode)
                    result = self.install([])
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("response too large", result.stderr)
                    self.assert_previous_installation()
                    self.assertFalse((self.root / "extracted").exists())
                    requests = [
                        json.loads(line)
                        for line in Path(self.env["TEST_REQUEST_LOG"])
                        .read_text()
                        .splitlines()
                    ]
                    record = requests[-1]
                    self.assertEqual(record["opened"], mode != "advertised")
                    self.assertEqual(
                        record["bytes_read"], 0 if mode == "advertised" else limit + 1
                    )

    def test_archive_advertised_size_limit(self):
        self.scenario.update(oversize_kind="archive", oversize_mode="advertised")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("response too large", result.stderr)

    def test_truncated_responses_fail_closed(self):
        self.seed_installation()
        for kind in ("api", "checksum", "archive"):
            with self.subTest(kind=kind):
                self.scenario["truncated_kind"] = kind
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("truncated release response", result.stderr)
                self.assert_previous_installation()

    def test_api_redirects_and_non_https_asset_redirects_are_refused(self):
        for kind, location in [
            ("api", "https://example.invalid/releases"),
            ("archive", "http://example.invalid/archive"),
        ]:
            with self.subTest(kind=kind):
                self.scenario.update(redirect_kind=kind, redirect_location=location)
                self.env["GITHUB_TOKEN"] = "fixture-token"
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "redirect limit" if kind == "api" else "non-HTTPS", result.stderr
                )
                requests = [
                    json.loads(line)
                    for line in Path(self.env["TEST_REQUEST_LOG"])
                    .read_text()
                    .splitlines()
                ]
                for request in requests:
                    self.assertNotIn("example.invalid", request["uri"])
                    self.assertEqual(
                        request["authenticated"],
                        request["uri"].startswith("https://api.github.com/"),
                    )

    def test_install_retains_complete_bundle(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.install_dir / self.binary_name).read_bytes(), NEW_BINARY)
        for name, content in BUNDLE_FILES.items():
            self.assertEqual((self.license_dir / name).read_bytes(), content)


def load_tests(loader, tests, pattern):
    return unittest.TestSuite(
        loader.loadTestsFromTestCase(case) for case in (InstallTests, PowerShellTests)
    )


if __name__ == "__main__":
    unittest.main()
