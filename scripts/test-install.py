#!/usr/bin/env python3

import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
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


@unittest.skipUnless(os.name == "posix", "Exercises the POSIX installer")
class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.commands = self.root / "commands"
        self.commands.mkdir()
        self.install_dir = self.root / "custom prefix" / "bin"
        self.license_dir = self.install_dir.parent / "share" / "licenses" / "caudra"
        self.payload = self.root / "payload"
        self.payload.mkdir()
        (self.payload / "caudra").write_bytes(NEW_BINARY)
        for name, content in BUNDLE_FILES.items():
            path = self.payload / "licenses" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.archive = self.root / "release.tar.gz"
        self.env: dict[str, str] = {
            **os.environ,
            "PATH": f"{self.commands}{os.pathsep}{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "TMPDIR": str(self.root),
            "CAUDRA_INSTALL_DIR": str(self.install_dir),
            "TEST_ARCHIVE": str(self.archive),
            "TEST_SUDO_LOG": str(self.root / "sudo.log"),
        }
        for key in ("GITHUB_TOKEN", "GH_TOKEN"):
            self.env.pop(key, None)
        self.command("curl", 'cat "$TEST_ARCHIVE"')
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

    def install(self):
        with tarfile.open(self.archive, "w:gz") as archive:
            for child in sorted(self.payload.iterdir()):
                archive.add(child, arcname=child.name)
        return subprocess.run(
            ["sh", str(INSTALLER), "v0.1.0"],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def seed_binary(self):
        self.install_dir.mkdir(parents=True)
        (self.install_dir / "caudra").write_bytes(OLD_BINARY)

    def seed_installation(self):
        self.seed_binary()
        self.license_dir.mkdir(parents=True)
        (self.license_dir / "manifest.json").write_bytes(OLD_MANIFEST)
        (self.license_dir / "ATTRIBUTION.txt").write_bytes(OLD_ATTRIBUTION)

    def assert_previous_installation(self):
        self.assertEqual((self.install_dir / "caudra").read_bytes(), OLD_BINARY)
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

    def test_install_retains_complete_bundle_with_space_paths(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_installed()

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


if __name__ == "__main__":
    unittest.main()
