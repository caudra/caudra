#!/usr/bin/env python3

import json
import os
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
OLD_MANIFEST = b"previous manifest\n"
OLD_ATTRIBUTION = b"previous attribution\n"
BUNDLE_FILES = {
    "manifest.json": b'{"version":1}\n',
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
        self.scenario = {"explicit": release("v0.1.0"), "pages": [[release("v0.1.0")]]}
        self.env: dict[str, str] = {
            **os.environ,
            "PATH": f"{self.commands}{os.pathsep}{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "TMPDIR": str(self.root),
            "CAUDRA_INSTALL_DIR": str(self.install_dir),
            "TEST_ARCHIVE": str(self.archive),
            "TEST_SUDO_LOG": str(self.root / "sudo.log"),
            "TEST_SCENARIO": str(self.root / "scenario.json"),
            "TEST_REQUEST_LOG": str(self.root / "requests.jsonl"),
        }
        for key in ("GITHUB_TOKEN", "GH_TOKEN"):
            self.env.pop(key, None)
        if self.binary_name == "caudra.exe":
            return
        self.command("curl", f'exec "{sys.executable}" "{HTTP_HELPER}" "$@"')
        self.command(
            "tar",
            f'touch "{self.root / "extracted"}"\nexec "{shutil.which("tar")}" "$@"',
        )
        self.command("uname", 'case "$1" in -s) echo Linux;; -m) echo x86_64;; esac')
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
    def test_minimal_path_without_python_or_jq_and_available_hashers(self):
        for hasher in ("sha256sum", "shasum", "openssl"):
            executable = shutil.which(hasher)
            if executable is None:
                continue
            with self.subTest(hasher=hasher):
                commands = self.root / hasher
                commands.mkdir()
                for name in ("curl", "uname", "cp", "mv", "mkdir", "tar"):
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
