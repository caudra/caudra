#!/usr/bin/env python3

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

SPEC = importlib.util.spec_from_file_location(
    "build_code_worker", Path(__file__).with_name("build-code-worker.py")
)
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load build-code-worker.py")
WORKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WORKER)

VERSION = "1.0.0"
NEXT_VERSION = "1.0.1"
REPORTED = f"monty-runtime {VERSION}"
TARGET = "x86_64-unknown-linux-gnu"
OTHER_TARGET = "aarch64-unknown-linux-gnu"
TOOLCHAIN = f"rustc 1.99.0\ncommit-hash: original\nhost: {TARGET}\n"
UNSTRIPPED = b"worker executable with diagnostic symbols"
STRIPPED = b"worker executable"
OLD_WORKER = b"previous same-version worker"
OLD_SYMBOLS = b"previous worker diagnostic symbols"
OLD_STAMP = "previous fingerprint"
PUBLICATION_ERROR = "cannot publish worker"
VERSION_ERROR = f"Staged worker must report {REPORTED}"
RELEASE_PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
    "CARGO_PROFILE_RELEASE_LTO": "thin",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_PANIC": "unwind",
}


class BuildCodeWorkerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="test-code-worker-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / "worker"
        self.target_dir = Path(temporary.name) / "target"
        self.worker = self.root / "bin" / "monty"
        self.symbols = self.root / "symbols" / "bin" / "monty"
        self.stamp = self.root / "build-fingerprint"
        self.toolchain = TOOLCHAIN
        self.installed_version = VERSION
        self.enterContext(patch.dict(os.environ, {}, clear=True))
        self.run_mock = self.enterContext(
            patch.object(WORKER.subprocess, "run", side_effect=self.run_tool)
        )
        self.check_output = self.enterContext(
            patch.object(
                WORKER.subprocess, "check_output", side_effect=self.tool_output
            )
        )

    def run_tool(self, command, **kwargs):
        self.assertTrue(kwargs["check"])
        if command[:2] == ["cargo", "install"]:
            requested = command[command.index("--version") + 1]
            self.installed_version = requested.removeprefix("=")
            installed = Path(command[command.index("--root") + 1]) / "bin" / "monty"
            installed.parent.mkdir(parents=True, exist_ok=True)
            installed.write_bytes(UNSTRIPPED)
        elif command[0] == "strip":
            staged = Path(command[1])
            self.assertEqual(staged.read_bytes(), UNSTRIPPED)
            staged.write_bytes(STRIPPED)
        else:
            self.fail(f"Unexpected tool invocation: {command}")
        return subprocess.CompletedProcess(command, 0)

    def tool_output(self, command, **kwargs):
        self.assertTrue(kwargs["text"])
        if command == ["rustc", "-vV"]:
            return self.toolchain
        self.assertEqual(command[1:], ["--version"])
        self.assertTrue(Path(command[0]).is_file())
        return f"monty-runtime {self.installed_version}\n"

    def build(self, version=VERSION, force=False):
        return WORKER.build(version, self.root, self.target_dir, force)

    def seed_existing_worker(self):
        self.worker.parent.mkdir(parents=True, exist_ok=True)
        self.symbols.parent.mkdir(parents=True, exist_ok=True)
        self.worker.write_bytes(OLD_WORKER)
        self.symbols.write_bytes(OLD_SYMBOLS)
        self.stamp.write_text(OLD_STAMP)

    def assert_existing_worker_preserved(self):
        self.assertEqual(self.worker.read_bytes(), OLD_WORKER)
        self.assertEqual(self.symbols.read_bytes(), OLD_SYMBOLS)
        self.assertEqual(self.stamp.read_text(), OLD_STAMP)
        self.assertEqual(
            sorted(path.name for path in self.root.iterdir()),
            ["bin", "build-fingerprint", "symbols"],
        )

    def test_a_legacy_same_version_worker_without_a_fingerprint_is_rebuilt(self):
        self.seed_existing_worker()
        self.stamp.unlink()
        self.assertEqual(WORKER.worker_version(self.worker), REPORTED)

        self.assertEqual(self.build(), self.worker)

        self.assertEqual(self.run_mock.call_count, 2)
        self.assertEqual(self.worker.read_bytes(), STRIPPED)
        self.assertEqual(self.stamp.read_text(), WORKER.fingerprint(TOOLCHAIN, TARGET))

    def test_a_matching_fingerprint_skips_install(self):
        self.build()
        stamp = self.stamp.read_bytes()
        self.run_mock.reset_mock()
        self.check_output.reset_mock()

        self.assertEqual(self.build(), self.worker)

        self.run_mock.assert_not_called()
        self.check_output.assert_has_calls(
            [
                call(["rustc", "-vV"], text=True),
                call(
                    [str(self.worker), "--version"],
                    text=True,
                    stderr=subprocess.STDOUT,
                    timeout=30,
                ),
            ]
        )
        self.assertEqual(self.stamp.read_bytes(), stamp)
        self.assertEqual(self.worker.read_bytes(), STRIPPED)
        self.assertEqual(self.symbols.read_bytes(), UNSTRIPPED)

    def test_a_version_bump_rebuilds_a_worker_whose_fingerprint_still_matches(self):
        self.build()
        expected = WORKER.fingerprint(TOOLCHAIN, TARGET)
        self.assertTrue(
            WORKER.cached(self.worker, self.symbols, self.stamp, expected, REPORTED)
        )
        self.run_mock.reset_mock()

        self.build(version=NEXT_VERSION)

        self.assertEqual(self.run_mock.call_count, 2)
        command = self.run_mock.call_args_list[0].args[0]
        self.assertEqual(command[command.index("--version") + 1], f"={NEXT_VERSION}")

    def test_a_changed_toolchain_invalidates_the_fingerprint(self):
        self.build()
        stamp = self.stamp.read_bytes()
        self.run_mock.reset_mock()
        self.toolchain = TOOLCHAIN.replace(
            "commit-hash: original", "commit-hash: changed"
        )

        self.build()

        self.assertEqual(self.run_mock.call_count, 2)
        self.assertNotEqual(self.stamp.read_bytes(), stamp)

    def test_changed_rustflags_invalidate_the_fingerprint(self):
        for key in (
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        ):
            with self.subTest(key=key), patch.dict(os.environ, {}, clear=True):
                self.build()
                stamp = self.stamp.read_bytes()
                self.run_mock.reset_mock()
                os.environ[key] = "-Cdebuginfo=2"

                self.build()

                self.assertEqual(self.run_mock.call_count, 2)
                self.assertEqual(
                    self.run_mock.call_args_list[0].kwargs["env"][key], "-Cdebuginfo=2"
                )
                self.assertNotEqual(self.stamp.read_bytes(), stamp)

    def test_a_changed_host_triple_rebuilds_for_the_new_target(self):
        self.build()
        stamp = self.stamp.read_bytes()
        self.run_mock.reset_mock()
        self.toolchain = TOOLCHAIN.replace(TARGET, OTHER_TARGET)

        self.build()

        self.assertEqual(self.run_mock.call_count, 2)
        command = self.run_mock.call_args_list[0].args[0]
        self.assertEqual(command[command.index("--target") + 1], OTHER_TARGET)
        self.assertNotEqual(self.stamp.read_bytes(), stamp)

    def test_stripped_staging_preserves_the_unstripped_diagnostic_copy(self):
        with patch.dict(os.environ, {"CARGO_PROFILE_RELEASE_STRIP": "symbols"}):
            self.build()

        cargo, strip = self.run_mock.call_args_list
        staged = Path(strip.args[0][1])
        self.assertEqual(
            cargo.args[0],
            [
                "cargo",
                "install",
                "monty-runtime",
                "--version",
                f"={VERSION}",
                "--locked",
                "--no-default-features",
                "--force",
                "--message-format=json-render-diagnostics",
                "--root",
                str(staged.parent / "symbols"),
                "--target-dir",
                str(self.target_dir),
                "--target",
                TARGET,
            ],
        )
        self.assertEqual(cargo.kwargs["env"], RELEASE_PROFILE)
        self.assertEqual(strip.args[0][0], "strip")
        self.assertTrue(staged.is_relative_to(self.root))
        self.assertNotIn(staged, (self.worker, self.symbols))
        self.assertFalse(staged.parent.exists())
        self.assertEqual(self.worker.read_bytes(), STRIPPED)
        self.assertEqual(self.symbols.read_bytes(), UNSTRIPPED)

    def test_a_missing_diagnostic_copy_rebuilds(self):
        self.build()
        self.symbols.unlink()
        self.run_mock.reset_mock()

        self.build()

        self.assertEqual(self.run_mock.call_count, 2)
        self.assertEqual(self.symbols.read_bytes(), UNSTRIPPED)

    def test_cargo_and_strip_failures_preserve_the_existing_worker_and_stamp(self):
        for tool in ("cargo", "strip"):
            with self.subTest(tool=tool):
                self.seed_existing_worker()

                def fail_tool(command, tool=tool, **kwargs):
                    result = self.run_tool(command, **kwargs)
                    if command[0] == tool:
                        raise subprocess.CalledProcessError(1, command)
                    return result

                self.run_mock.side_effect = fail_tool
                with self.assertRaises(subprocess.CalledProcessError):
                    self.build(force=True)

                self.assert_existing_worker_preserved()

    def test_version_failures_preserve_the_existing_worker_and_stamp(self):
        for failure in (
            "monty-runtime 0.0.0\n",
            OSError("cannot execute staged worker"),
            subprocess.CalledProcessError(1, ["monty", "--version"]),
            subprocess.TimeoutExpired(["monty", "--version"], 30),
        ):
            with self.subTest(failure=failure):
                self.seed_existing_worker()

                def fail_version(command, failure=failure, **kwargs):
                    result = self.tool_output(command, **kwargs)
                    if command[0] == "rustc":
                        return result
                    self.assertNotEqual(Path(command[0]), self.worker)
                    if isinstance(failure, Exception):
                        raise failure
                    return failure

                self.check_output.side_effect = fail_version
                with self.assertRaises(RuntimeError) as caught:
                    self.build(force=True)

                self.assertEqual(str(caught.exception), VERSION_ERROR)
                self.assert_existing_worker_preserved()

    def test_a_publication_failure_invalidates_the_previous_cache(self):
        self.seed_existing_worker()
        expected = WORKER.fingerprint(TOOLCHAIN, TARGET)
        self.stamp.write_text(expected)
        replace = Path.replace

        def fail_publication(source, destination):
            if destination == self.worker:
                raise OSError(PUBLICATION_ERROR)
            return replace(source, destination)

        with (
            patch.object(Path, "replace", fail_publication),
            self.assertRaisesRegex(OSError, PUBLICATION_ERROR),
        ):
            self.build(force=True)

        self.assertEqual(self.worker.read_bytes(), OLD_WORKER)
        self.assertFalse(self.stamp.exists())
        self.assertFalse(
            WORKER.cached(self.worker, self.symbols, self.stamp, expected, REPORTED)
        )

        self.build()

        self.assertEqual(self.worker.read_bytes(), STRIPPED)
        self.assertEqual(self.symbols.read_bytes(), UNSTRIPPED)
        self.assertEqual(self.stamp.read_text(), expected)


if __name__ == "__main__":
    unittest.main()
