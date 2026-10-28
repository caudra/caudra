import os
import runpy
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("sweep-incremental.py")
SWEEP = runpy.run_path(str(SCRIPT))
HOUR = 60 * 60
DAY = 24 * HOUR
KIB = 1 << 10


class SweepTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="sweep ")
        self.addCleanup(temporary.cleanup)
        self.target = Path(temporary.name)
        self.now = time.time()

    def cache(self, relative: str, age: float, size: int = KIB) -> Path:
        path = self.target / relative
        session = path / "s-session"
        session.mkdir(parents=True)
        (session / "query-cache.bin").write_bytes(bytes(size))
        os.utime(path, (self.now - age, self.now - age))
        return path

    def sweep(self, limit: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--target-dir",
                str(self.target),
                "--limit",
                limit,
            ],
            check=True,
            capture_output=True,
            text=True,
        )

    def test_idle_caches_go_even_under_the_limit(self):
        idle = self.cache("debug/incremental/caudra_agent-old", 3 * DAY)
        recent = self.cache("debug/incremental/caudra_agent-new", 2 * HOUR)
        self.sweep("1G")
        self.assertFalse(idle.exists())
        self.assertTrue(recent.exists())

    def test_least_recently_compiled_go_until_the_total_fits(self):
        oldest = self.cache("debug/incremental/a-1", 5 * HOUR, 4 * KIB)
        middle = self.cache(
            "x86_64-unknown-linux-musl/debug/incremental/b-2", 4 * HOUR, 4 * KIB
        )
        newest = self.cache("release-fast/incremental/c-3", 3 * HOUR, 4 * KIB)
        self.sweep("5K")
        self.assertFalse(oldest.exists())
        self.assertFalse(middle.exists())
        self.assertTrue(newest.exists())

    def test_caches_compiled_within_the_active_window_survive_any_limit(self):
        active = self.cache("debug/incremental/caudra_ui-live", 10 * 60, 8 * KIB)
        result = self.sweep("1")
        self.assertTrue(active.exists())
        self.assertEqual(result.stdout, "")

    def test_missing_target_directory_is_not_an_error(self):
        self.target = self.target / "absent"
        self.sweep("1G")

    def test_sizes_accept_byte_counts_and_suffixes(self):
        parse = SWEEP["parse_size"]
        for text, expected in (("512", 512), ("2k", 2 * KIB), ("1.5G", 3 << 29)):
            with self.subTest(text=text):
                self.assertEqual(parse(text), expected)


if __name__ == "__main__":
    unittest.main()
