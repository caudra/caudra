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
CACHE_VARIABLES = (
    "RUSTC_WRAPPER",
    "SCCACHE_CACHE_SIZE",
    "SCCACHE_CLIENT_SIDE",
    "SCCACHE_DIRECT",
    "CARGO_TARGET_DIR",
)
WRAPPER_VARIABLES = (
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
)
SCCACHE_DEFAULTS = {
    "RUSTC_WRAPPER": "sccache",
    "SCCACHE_CACHE_SIZE": "10G",
    "SCCACHE_CLIENT_SIDE": "1",
    "SCCACHE_DIRECT": "false",
}
UNCACHED = dict.fromkeys(SCCACHE_DEFAULTS)
SCCACHE_RAN = "sccache-ran"
PROBE = f"""import json, os, sys, tempfile
from pathlib import Path
kind, *args = sys.argv[1:]
worker = Path(os.environ['WORKCELL_BUNDLED_MONTY_WORKER'])
is_worker = kind == 'python' and args == ['scripts/build-code-worker.py']
with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8',
        dir=os.environ['TEST_RECORDS'], suffix='.json', delete=False) as record:
    json.dump({{'kind': 'worker' if is_worker else kind,
        'args': args, 'worker': str(worker),
        'required': os.environ.get('WORKCELL_REQUIRE_CODE_WORKER'),
        'env': {{name: os.environ.get(name) for name in {CACHE_VARIABLES!r}}}}}, record)
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
        (self.root / "scripts").mkdir()
        shutil.copyfile(
            ROOT / "scripts/cargo-cache.mk", self.root / "scripts/cargo-cache.mk"
        )
        self.probe = self.root / "probe.py"
        self.probe.write_text(PROBE)
        self.env: dict[str, str] = dict(os.environ)
        for key in (
            "MAKEFLAGS",
            "MFLAGS",
            "MAKELEVEL",
            "MAKEOVERRIDES",
            "SCCACHE",
            *CACHE_VARIABLES,
            *WRAPPER_VARIABLES,
        ):
            self.env.pop(key, None)
        (self.root / "workcell").mkdir()
        (self.root / "workcell/Makefile").write_text(
            "SHELL := bash\n.PHONY: check-native\n"
            "check-native:\n\t$(CARGO) native-feature-probe\n"
        )
        fake_tools = self.root / "fake tools"
        fake_tools.mkdir()
        fake_sccache = fake_tools / "sccache"
        fake_sccache.write_text(
            f'#!/bin/sh\ntouch "$TEST_RECORDS/{SCCACHE_RAN}"\nexit 1\n'
        )
        fake_sccache.chmod(0o755)
        self.path_with_sccache = os.pathsep.join(
            (str(fake_tools), self.env.get("PATH", ""))
        )

    def run_make(self, *arguments: str):
        self.record_dir = Path(tempfile.mkdtemp(prefix="records ", dir=self.root))
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
            env={**self.env, "TEST_RECORDS": str(self.record_dir)},
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def records(self):
        return [
            json.loads(path.read_text(encoding="utf-8"))
            for path in self.record_dir.iterdir()
        ]

    def test_default_help_does_not_build(self):
        result = self.run_make()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("make build", result.stdout)
        self.assertEqual(self.records(), [])

    def test_all_compiling_targets_prepare_worker_even_with_named_files(self):
        for target in COMPILING_TARGETS:
            with self.subTest(target=target):
                if (self.root / "target").exists():
                    shutil.rmtree(self.root / "target")
                (self.root / target).touch()
                result = self.run_make(target)
                self.assertEqual(result.returncode, 0, result.stderr)
                records = self.records()
                self.assertCountEqual([r["kind"] for r in records], ["worker", "cargo"])
                expected = "monty.exe" if os.name == "nt" else "monty"
                for record in records:
                    self.assertEqual(
                        Path(record["worker"]),
                        self.root / "target/code-worker/bin" / expected,
                    )
                if target == "test":
                    [cargo] = [r for r in records if r["kind"] == "cargo"]
                    self.assertEqual(cargo["required"], "1")

    def test_parallel_targets_share_one_completed_worker_preparation(self):
        result = self.run_make("-j4", "build", "check", "test", "workcell-build")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.records()
        self.assertCountEqual(
            [r["kind"] for r in records], ["worker", "cargo", "cargo", "cargo", "cargo"]
        )
        self.assertCountEqual(
            [r["args"] for r in records if r["kind"] == "cargo"],
            [
                ["build"],
                ["check", "--workspace", "--tests"],
                ["nextest", "run", "--workspace"],
                ["build", "--locked", "--package", "workcell-mcp"],
            ],
        )

    def test_parallel_probe_records_preserve_unique_payloads_across_invocations(self):
        targets = [f"probe-{index}" for index in range(16)]
        (self.root / "probes.mk").write_text(
            f".PHONY: {' '.join(targets)}\n"
            + "".join(
                f"{target}: code-worker\n\t$(CARGO) {target} '$(ARGS)'\n"
                for target in targets
            )
        )
        for invocation in range(2):
            with self.subTest(invocation=invocation):
                payload = f"invocation {invocation}: " + "x" * 4096
                result = self.run_make(
                    "-j8",
                    "-f",
                    "Makefile",
                    "-f",
                    "probes.mk",
                    f"ARGS={payload}",
                    *targets,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                records = self.records()
                self.assertCountEqual(
                    [r["kind"] for r in records], ["worker"] + ["cargo"] * len(targets)
                )
                self.assertCountEqual(
                    [r["args"] for r in records if r["kind"] == "cargo"],
                    [[target, payload] for target in targets],
                )

    def test_records_reject_empty_malformed_or_combined_documents(self):
        result = self.run_make()
        self.assertEqual(result.returncode, 0, result.stderr)
        for content in ("", "{", "{}\n{}\n"):
            with self.subTest(content=content):
                (self.record_dir / "invalid.json").write_text(content, encoding="utf-8")
                with self.assertRaises(json.JSONDecodeError):
                    self.records()

    def test_worker_failure_stops_cargo(self):
        self.env["TEST_FAIL_WORKER"] = "1"
        result = self.run_make("-j4", "build", "check", "test")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([r["kind"] for r in self.records()], ["worker"])

    def test_run_preserves_cargo_separator_and_quoted_arguments(self):
        result = self.run_make("run", "ARGS=-- auth login --label 'two words'")
        self.assertEqual(result.returncode, 0, result.stderr)
        [cargo] = [r for r in self.records() if r["kind"] == "cargo"]
        self.assertEqual(
            cargo["args"],
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

    def test_workcell_build_locks_once_and_preserves_cargo_arguments(self):
        for arguments in ("", "--locked", "--locked --target-dir 'two  spaces'"):
            with self.subTest(arguments=arguments):
                result = self.run_make("workcell-build", f"ARGS={arguments}")
                self.assertEqual(result.returncode, 0, result.stderr)
                [cargo] = [r for r in self.records() if r["kind"] == "cargo"]
                args = cargo["args"]
                self.assertEqual(args.count("--locked"), 1)
                self.assertEqual(args[0], "build")
                self.assertEqual(args[args.index("--package") + 1], "workcell-mcp")
                if "--target-dir" in arguments:
                    self.assertEqual(args[-2:], ["--target-dir", "two  spaces"])

    def test_ci_build_targets_share_locked_arguments(self):
        result = self.run_make("check", "build", "workcell-build", "ARGS=--locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.records()
        self.assertCountEqual(
            [record["kind"] for record in records],
            ["worker", "cargo", "cargo", "cargo"],
        )
        for record in records:
            if record["kind"] == "cargo":
                self.assertEqual(record["args"].count("--locked"), 1)

    def test_windows_worker_suffix_is_exported(self):
        result = self.run_make("OS=Windows_NT", "build")
        self.assertEqual(result.returncode, 0, result.stderr)
        [cargo] = [r for r in self.records() if r["kind"] == "cargo"]
        self.assertEqual(Path(cargo["worker"]).name, "monty.exe")

    def test_dependency_audit_does_not_build_worker(self):
        result = self.run_make("machete")
        self.assertEqual(result.returncode, 0, result.stderr)
        [cargo] = self.records()
        self.assertEqual(cargo["kind"], "cargo")
        self.assertEqual(cargo["args"], ["machete"])

    def cache_environments(self, *arguments: str):
        result = self.run_make(*arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.record_dir / SCCACHE_RAN).exists())
        return [
            {name: record["env"][name] for name in SCCACHE_DEFAULTS}
            for record in self.records()
        ]

    def test_installed_sccache_wraps_worker_and_cargo_without_running_it(self):
        self.env["PATH"] = self.path_with_sccache
        self.assertEqual(self.cache_environments(), [])
        self.assertEqual(
            self.cache_environments("build"), [SCCACHE_DEFAULTS, SCCACHE_DEFAULTS]
        )

    def test_missing_sccache_leaves_cargo_unwrapped(self):
        names = ("sccache", "sccache.exe")
        self.env["PATH"] = os.pathsep.join(
            directory
            for directory in self.env.get("PATH", "").split(os.pathsep)
            if not any((Path(directory) / name).is_file() for name in names)
        )
        if not shutil.which("bash", path=self.env["PATH"]):
            self.skipTest("sccache shares a directory with bash")
        self.assertEqual(self.cache_environments("build"), [UNCACHED, UNCACHED])

    def test_sccache_selection_and_settings_can_be_overridden(self):
        self.env["PATH"] = self.path_with_sccache
        custom = "cache tools/sccache"
        for arguments, environment, expected in (
            (["SCCACHE="], {}, UNCACHED),
            ([], {"SCCACHE": ""}, UNCACHED),
            ([f"SCCACHE={custom}"], {}, SCCACHE_DEFAULTS | {"RUSTC_WRAPPER": custom}),
            (
                ["SCCACHE_CACHE_SIZE=20G"],
                {"SCCACHE_DIRECT": "true"},
                SCCACHE_DEFAULTS
                | {"SCCACHE_CACHE_SIZE": "20G", "SCCACHE_DIRECT": "true"},
            ),
        ):
            with self.subTest(arguments=arguments, environment=environment):
                self.env.update(environment)
                self.assertEqual(
                    self.cache_environments("build", *arguments), [expected] * 2
                )
                for name in environment:
                    del self.env[name]

    def test_explicit_cargo_wrappers_win_over_detection(self):
        self.env["PATH"] = self.path_with_sccache
        for variable in WRAPPER_VARIABLES:
            for value in ("", "custom-wrapper"):
                with self.subTest(variable=variable, value=value):
                    self.env[variable] = value
                    wrapper = value if variable == "RUSTC_WRAPPER" else None
                    self.assertEqual(
                        self.cache_environments("build"),
                        [UNCACHED | {"RUSTC_WRAPPER": wrapper}] * 2,
                    )
                    del self.env[variable]

    def test_recursive_make_inherits_cache_settings(self):
        self.env["PATH"] = self.path_with_sccache
        self.assertEqual(
            self.cache_environments("workcell-check-native"), [SCCACHE_DEFAULTS] * 2
        )

    def test_workcell_leaves_target_to_cargo_unless_overridden(self):
        shutil.copyfile(ROOT / "workcell/Makefile", self.root / "workcell/Makefile")
        worker = self.root / "worker"
        worker.touch()
        self.env |= {
            "PATH": self.path_with_sccache,
            "WORKCELL_BUNDLED_MONTY_WORKER": str(worker),
        }
        for arguments, target in (
            ([], None),
            (["CARGO_TARGET_DIR=custom target"], "custom target"),
        ):
            with self.subTest(target=target):
                result = self.run_make("-C", "workcell", "build", *arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                [cargo] = self.records()
                self.assertEqual(cargo["args"][0], "build")
                self.assertEqual(
                    cargo["env"], SCCACHE_DEFAULTS | {"CARGO_TARGET_DIR": target}
                )


if __name__ == "__main__":
    unittest.main()
