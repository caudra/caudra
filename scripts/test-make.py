import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAKE = shutil.which("make")
COMPILING_TARGETS = (
    "build",
    "check",
    "run",
    "test",
    "lint",
    "lint-fix",
    "install",
    "install-fast",
    "gen-docs",
    "gen-docs-check",
    "workcell-build",
    "workcell-run",
    "workcell-release",
    "workcell-doc-test",
    "workcell-check-native",
)
PROBE = """import json, os, sys
from pathlib import Path
kind, *args = sys.argv[1:]
worker = Path(os.environ['WORKCELL_BUNDLED_MONTY_WORKER'])
is_worker = kind == 'python' and args == ['scripts/build-code-worker.py']
with Path(os.environ['TEST_LOG']).open('a') as log:
    log.write(json.dumps({'kind': 'worker' if is_worker else kind,
        'args': args, 'worker': str(worker),
        'required': os.environ.get('WORKCELL_REQUIRE_CODE_WORKER')}) + '\\n')
if is_worker:
    if os.environ.get('TEST_FAIL_WORKER'):
        sys.exit(7)
    worker.parent.mkdir(parents=True, exist_ok=True)
    worker.write_text('fixture worker')
elif kind == 'cargo' and args[0] not in ('machete', 'fmt'):
    if not worker.is_file():
        sys.exit('Cargo ran before the worker was ready')
"""


@unittest.skipUnless(MAKE and shutil.which("bash"), "GNU Make and Bash required")
class MakeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="make test ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        shutil.copyfile(ROOT / "Makefile", self.root / "Makefile")
        self.probe = self.root / "probe.py"
        self.probe.write_text(PROBE)
        self.log = self.root / "commands.jsonl"
        self.env: dict[str, str] = {**os.environ, "TEST_LOG": str(self.log)}
        for key in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL", "MAKEOVERRIDES"):
            self.env.pop(key, None)
        (self.root / "workcell").mkdir()
        (self.root / "workcell/Makefile").write_text(
            "SHELL := bash\n.PHONY: check-native\n"
            "check-native:\n\t$(CARGO) native-feature-probe\n"
        )

    def run_make(self, *arguments: str):
        python = shlex.quote(Path(sys.executable).as_posix())
        probe = shlex.quote(self.probe.as_posix())
        return subprocess.run(
            [
                str(MAKE),
                "--no-print-directory",
                f"CARGO={python} {probe} cargo",
                f"PYTHON={python} {probe} python",
                *arguments,
            ],
            cwd=self.root,
            env=self.env,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_default_help_does_not_build(self):
        result = self.run_make()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("make build", result.stdout)
        self.assertFalse(self.log.exists())

    def test_all_compiling_targets_prepare_worker_even_with_named_files(self):
        for target in COMPILING_TARGETS:
            with self.subTest(target=target):
                if self.log.exists():
                    self.log.unlink()
                if (self.root / "target").exists():
                    shutil.rmtree(self.root / "target")
                (self.root / target).touch()
                result = self.run_make(target)
                self.assertEqual(result.returncode, 0, result.stderr)
                records = self.records()
                self.assertEqual([r["kind"] for r in records], ["worker", "cargo"])
                expected = "monty.exe" if os.name == "nt" else "monty"
                for record in records:
                    self.assertEqual(
                        Path(record["worker"]),
                        self.root / "target/code-worker/bin" / expected,
                    )
                if target == "test":
                    self.assertEqual(records[-1]["required"], "1")

    def test_parallel_targets_share_one_completed_worker_preparation(self):
        result = self.run_make("-j4", "build", "check", "test", "workcell-build")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.records()
        self.assertEqual(records[0]["kind"], "worker")
        self.assertEqual(sum(r["kind"] == "worker" for r in records), 1)
        self.assertEqual(len(records), 5)

    def test_worker_failure_stops_cargo(self):
        self.env["TEST_FAIL_WORKER"] = "1"
        result = self.run_make("-j4", "build", "check", "test")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([r["kind"] for r in self.records()], ["worker"])

    def test_run_preserves_cargo_separator_and_quoted_arguments(self):
        result = self.run_make("run", "ARGS=-- auth login --label 'two words'")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.records()[-1]["args"],
            [
                "run",
                "--package",
                "caudra",
                "--",
                "auth",
                "login",
                "--label",
                "two words",
            ],
        )

    def test_windows_worker_suffix_is_exported(self):
        result = self.run_make("OS=Windows_NT", "build")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(self.records()[-1]["worker"]).name, "monty.exe")

    def test_dependency_audit_does_not_build_worker(self):
        result = self.run_make("machete")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([r["kind"] for r in self.records()], ["cargo"])
        self.assertEqual(self.records()[0]["args"], ["machete"])


if __name__ == "__main__":
    unittest.main()
