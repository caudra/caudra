import contextlib
import io
import os
import runpy
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("time-command.py")
TIMER = runpy.run_path(str(SCRIPT))
START_ERROR = "Unable to start release phase command"


class TimingTests(unittest.TestCase):
    def test_child_output_arguments_and_exit_status_are_preserved(self):
        for status in (0, 7):
            with self.subTest(status=status):
                result = subprocess.run(
                    [
                        sys.executable,
                        str(SCRIPT),
                        "smoke-execute",
                        sys.executable,
                        "-c",
                        "import sys; print(sys.argv[1]); sys.exit(int(sys.argv[2]))",
                        "literal ; $(not-a-shell)",
                        str(status),
                    ],
                    env=os.environ | {"CAUDRA_RUNNER_PROFILE": "github"},
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, status)
                self.assertEqual(result.stdout, "literal ; $(not-a-shell)\n")
                self.assertIn("::group::smoke-execute profile=github", result.stderr)
                self.assertRegex(result.stderr, r"arch=[\w-]+ cpus=[1-9]\d*")
                self.assertRegex(result.stderr, r"elapsed_seconds=\d+\.\d{3}")
                self.assertTrue(
                    result.stderr.endswith(f"status={status}\n::endgroup::\n")
                )
                self.assertNotIn("not-a-shell", result.stderr)

    def test_failures_close_the_group_and_use_a_monotonic_duration(self):
        for child_status, error, expected in ((-9, None, 137), (0, OSError(), 127)):
            with (
                self.subTest(status=expected),
                patch.object(sys, "argv", [str(SCRIPT), "worker", "private-command"]),
                patch("subprocess.run", side_effect=error) as run,
                patch("time.monotonic", side_effect=[10, 12.5]),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                run.return_value.returncode = child_status
                self.assertEqual(TIMER["main"](), expected)
                self.assertIn(
                    f"elapsed_seconds=2.500 status={expected}\n::endgroup::",
                    stderr.getvalue(),
                )
                self.assertEqual(START_ERROR in stderr.getvalue(), error is not None)
                self.assertNotIn("private-command", stderr.getvalue())

    def test_untrusted_labels_cannot_inject_workflow_commands(self):
        with (
            patch.object(sys, "argv", [str(SCRIPT), "bad\n::error::", "command"]),
            patch.dict(os.environ, {"CAUDRA_RUNNER_PROFILE": "bad\n::error::"}),
            patch("platform.machine", return_value="bad\n::error::"),
            patch("subprocess.run") as run,
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            run.return_value.returncode = 0
            self.assertEqual(TIMER["main"](), 0)
        self.assertIn(
            "::group::unknown profile=unknown arch=unknown", stderr.getvalue()
        )
        self.assertNotIn("::error::", stderr.getvalue())

    def test_linux_hardware_is_bounded_sanitized_and_not_inferred_from_core_count(self):
        cpuinfo = (
            b"model name : AMD EPYC 9B14 @ 2.6GHz\n"
            b"model name : ignored later socket\n"
            b"unrelated : private data\n"
        )
        with (
            patch.object(sys, "platform", "linux"),
            patch.object(
                Path,
                "open",
                side_effect=[
                    io.BytesIO(cpuinfo),
                    io.BytesIO(b"MemTotal:  123456 kB\n"),
                ],
            ),
        ):
            self.assertEqual(
                TIMER["hardware"](),
                'cpu_model="AMD EPYC 9B14 @ 2.6GHz" host_memory_kib=123456',
            )
        with patch.object(Path, "open") as source:
            source.return_value.__enter__.return_value.read.return_value = b""
            self.assertEqual(TIMER["proc_fields"]("cpuinfo"), {})
            source.return_value.__enter__.return_value.read.assert_called_once_with(
                TIMER["PROC_READ_LIMIT"]
            )
        for model in ('bad\x1b[31m"::error::', "x" * 200):
            with (
                self.subTest(model=model),
                patch.object(sys, "platform", "linux"),
                patch.object(
                    Path,
                    "open",
                    side_effect=[
                        io.BytesIO(f"model name: {model}\n".encode()),
                        io.BytesIO(b"MemTotal: invalid\n"),
                    ],
                ),
            ):
                result = TIMER["hardware"]()
                self.assertNotIn("::error::", result)
                self.assertNotIn("\x1b", result)
                self.assertLessEqual(
                    len(result.split('"')[1]), TIMER["CPU_MODEL_LIMIT"]
                )
                self.assertTrue(result.endswith("host_memory_kib=unknown"))

    def test_missing_proc_files_and_other_platforms_report_unknown_hardware(self):
        for system in ("linux", "darwin", "win32"):
            with (
                self.subTest(system=system),
                patch.object(sys, "platform", system),
                patch.object(Path, "open", side_effect=OSError) as source,
            ):
                self.assertEqual(
                    TIMER["hardware"](), 'cpu_model="unknown" host_memory_kib=unknown'
                )
                self.assertEqual(source.call_count, 2 if system == "linux" else 0)

    def test_arm_cpu_identifiers_are_reported_when_model_name_is_unavailable(self):
        with (
            patch.object(sys, "platform", "linux"),
            patch.object(
                Path,
                "open",
                side_effect=[
                    io.BytesIO(b"CPU implementer: 0x41\nCPU part: 0xd0c\n"),
                    io.BytesIO(b""),
                ],
            ),
        ):
            self.assertEqual(
                TIMER["hardware"](),
                'cpu_model="CPU implementer 0x41 CPU part 0xd0c" host_memory_kib=unknown',
            )


if __name__ == "__main__":
    unittest.main()
