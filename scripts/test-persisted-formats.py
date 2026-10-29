#!/usr/bin/env python3

import importlib.util
import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "persisted_formats", Path(__file__).with_name("persisted-formats.py")
)
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load persisted-formats.py")
FORMATS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FORMATS)

BASE = "v0.2.0"
DATABASE = "storage/src/database.rs"
CONFIG = "config/src/lib.rs"
DATABASE_ENTRY = """
[[tracked]]
name = "database"
path = "storage/src/database.rs"
const = "SCHEMA_VERSION"
kind = "database"
"""
CONFIG_ENTRY = """
[[tracked]]
name = "config"
path = "config/src/lib.rs"
const = "CONFIG_VERSION"
kind = "config"
"""
CACHE_ENTRY = """
[[exempt]]
path = "storage/src/database.rs"
const = "CACHE_VERSION"
reason = "Regenerated cache"
"""
MISSING_TAG = "Base tag v0.2.0 is missing"
NOT_LITERAL = "must be an integer literal"
UNLISTED = "is neither tracked nor exempt"
STALE = "no longer exists"
CHANGED = "changed from 43 to 44"
ADDED = "config (config/src/lib.rs CONFIG_VERSION = 5) was added"
REMOVED = "config (config/src/lib.rs CONFIG_VERSION) was removed"
NO_REGISTRY = "has no scripts/persisted-formats.toml"
GIT_ENV = {
    "GIT_AUTHOR_NAME": "test",
    "GIT_AUTHOR_EMAIL": "test@example.com",
    "GIT_COMMITTER_NAME": "test",
    "GIT_COMMITTER_EMAIL": "test@example.com",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


def database(version: int) -> str:
    return textwrap.dedent(f"""\
        const SCHEMA_VERSION: i64 = {version};
        pub(crate) const CACHE_VERSION: u32 = 1;

        #[cfg(test)]
        mod tests {{
            const NEWER_SCHEMA_VERSION: i64 = SCHEMA_VERSION + 1;
        }}
        """)


class Repository:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.git("init", "--quiet", "--initial-branch=main")

    def git(self, *arguments: str) -> None:
        subprocess.run(
            ["git", *arguments],
            cwd=self.root,
            check=True,
            capture_output=True,
            env={**os.environ, **GIT_ENV},
        )

    def write(self, path: str, text: str) -> None:
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text, encoding="utf-8")

    def version(self, version: str) -> None:
        self.write("Cargo.toml", f'[workspace.package]\nversion = "{version}"\n')

    def registry(self, *entries: str) -> None:
        self.write(FORMATS.REGISTRY, "".join(entries))

    def commit(self, tag: str | None = None) -> None:
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "--message", "change")
        if tag:
            self.git("tag", tag)


class PersistedFormatTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repository = Repository(Path(directory.name))
        self.repository.version("0.2.0")
        self.repository.registry(DATABASE_ENTRY, CONFIG_ENTRY, CACHE_ENTRY)
        self.repository.write(DATABASE, database(43))
        self.repository.write(CONFIG, "pub const CONFIG_VERSION: u32 = 5;\n")
        self.repository.commit(BASE)

    def check(self, version: str) -> list[str]:
        self.repository.version(version)
        return FORMATS.check(self.repository.root)

    def test_an_unchanged_patch_passes(self) -> None:
        self.assertEqual(self.check("0.2.1"), [])

    def test_a_moved_format_keeps_its_name_and_passes(self) -> None:
        self.repository.registry(
            DATABASE_ENTRY.replace(DATABASE, "storage/src/schema.rs"),
            CONFIG_ENTRY,
            CACHE_ENTRY.replace(DATABASE, "storage/src/schema.rs"),
        )
        (self.repository.root / DATABASE).rename(
            self.repository.root / "storage/src/schema.rs"
        )
        self.assertEqual(self.check("0.2.1"), [])

    def test_a_changed_format_fails_a_patch_and_its_prereleases(self) -> None:
        self.repository.write(DATABASE, database(44))
        for version in ("0.2.1", "0.2.1-rc.1", "0.2.0"):
            with self.subTest(version=version):
                (problem,) = self.check(version)
                self.assertIn(CHANGED, problem)
                self.assertIn(DATABASE, problem)

    def test_an_added_format_fails_a_patch(self) -> None:
        self.repository.registry(DATABASE_ENTRY, CACHE_ENTRY)
        self.repository.commit()
        self.repository.git("tag", "--force", BASE)
        self.repository.registry(DATABASE_ENTRY, CONFIG_ENTRY, CACHE_ENTRY)
        (problem,) = self.check("0.2.1")
        self.assertIn(ADDED, problem)

    def test_a_removed_format_fails_a_patch(self) -> None:
        self.repository.registry(DATABASE_ENTRY, CACHE_ENTRY)
        (problem,) = self.check("0.2.1")
        self.assertIn(REMOVED, problem)

    def test_previews_of_the_next_minor_may_migrate(self) -> None:
        self.repository.write(DATABASE, database(44))
        self.assertEqual(self.check("0.3.0-preview.1"), [])

    def test_a_missing_base_tag_is_an_error(self) -> None:
        self.repository.git("tag", "--delete", BASE)
        with self.assertRaisesRegex(FORMATS.FormatError, MISSING_TAG):
            self.check("0.2.1")

    def test_a_base_without_a_registry_is_an_error(self) -> None:
        (self.repository.root / FORMATS.REGISTRY).unlink()
        self.repository.commit()
        self.repository.git("tag", "--force", BASE)
        self.repository.registry(DATABASE_ENTRY)
        with self.assertRaisesRegex(FORMATS.FormatError, NO_REGISTRY):
            self.check("0.2.1")

    def test_a_tracked_format_must_be_an_integer_literal(self) -> None:
        self.repository.write(CONFIG, "pub const CONFIG_VERSION: u32 = 4 + 1;\n")
        with self.assertRaisesRegex(FORMATS.FormatError, NOT_LITERAL):
            FORMATS.snapshot(self.repository.root)

    def test_lint_passes_a_fully_listed_tree(self) -> None:
        self.assertEqual(FORMATS.lint(self.repository.root), [])

    def test_lint_flags_an_unlisted_const_but_not_tests(self) -> None:
        self.repository.write(
            CONFIG,
            "pub const CONFIG_VERSION: u32 = 5;\nconst MCP_FORMAT_VERSION: u8 = 1;\n",
        )
        self.repository.write(
            "storage/tests/migrations.rs", "const OLD_SCHEMA_VERSION: i64 = 1;\n"
        )
        self.repository.write(
            "vendor/crossterm/src/lib.rs", "const WIRE_VERSION: u8 = 1;\n"
        )
        self.repository.git("add", "--all")
        (problem,) = FORMATS.lint(self.repository.root)
        self.assertIn(f"{CONFIG}: MCP_FORMAT_VERSION {UNLISTED}", problem)

    def test_lint_flags_a_stale_entry(self) -> None:
        self.repository.write(DATABASE, "const SCHEMA_VERSION: i64 = 43;\n")
        (problem,) = FORMATS.lint(self.repository.root)
        self.assertIn(f"{DATABASE} CACHE_VERSION {STALE}", problem)

    def test_lint_reports_an_unstaged_deletion_without_crashing(self) -> None:
        (self.repository.root / CONFIG).unlink()
        (problem,) = FORMATS.lint(self.repository.root)
        self.assertIn(f"{CONFIG} CONFIG_VERSION {STALE}", problem)
        self.repository.registry(DATABASE_ENTRY, CACHE_ENTRY)
        self.assertEqual(FORMATS.lint(self.repository.root), [])


if __name__ == "__main__":
    unittest.main()
