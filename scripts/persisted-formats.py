"""Keep patch releases from changing what Caudra persists.

Every integer `const` whose name holds VERSION or FORMAT is listed in
`persisted-formats.toml`, either tracked as a persisted format or exempt with a
reason. A patch release must carry the tracked values of its `vX.Y.0` tag, so
a binary from anywhere in the minor line can read what any other one wrote.
"""

import argparse
import importlib.util
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = "scripts/persisted-formats.toml"
SKIPPED_PREFIXES = ("vendor/",)
TEST_FILES = re.compile(r"(^|/)(tests|benches)(/|\.rs$)")
TEST_MODULE = re.compile(r"^#\[cfg\(test\)\]\s*(?:#\[.*\]\s*)*mod\s", re.MULTILINE)
DECLARATION = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*"
    r"(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize)\s*=\s*([^;]+);",
    re.MULTILINE,
)
VERSIONED_NAME = re.compile(r"VERSION|FORMAT")
INTEGER = re.compile(r"[0-9][0-9_]*")
KINDS = {"database", "config", "record", "journal", "protocol"}

SPEC = importlib.util.spec_from_file_location(
    "release", Path(__file__).with_name("release.py")
)
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)


class FormatError(ValueError):
    pass


def git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=root, check=True, capture_output=True, text=True
    ).stdout


def read(root: Path, rev: str | None, path: str) -> str | None:
    if rev is None:
        file = root / path
        return file.read_text(encoding="utf-8") if file.is_file() else None
    try:
        return git(root, "show", f"{rev}:{path}")
    except subprocess.CalledProcessError:
        return None


def declarations(source: str) -> list[tuple[str, str]]:
    """Versioned integer consts outside the file's top-level test module."""
    test_module = TEST_MODULE.search(source)
    production = source[: test_module.start()] if test_module else source
    return [
        (name, value.strip())
        for name, value in DECLARATION.findall(production)
        if VERSIONED_NAME.search(name)
    ]


def registry(root: Path, rev: str | None = None) -> dict:
    text = read(root, rev, REGISTRY)
    if text is None:
        raise FormatError(f"{rev or 'The working tree'} has no {REGISTRY}")
    document = tomllib.loads(text)
    tracked = document.get("tracked", [])
    exempt = document.get("exempt", [])
    names = Counter(entry["name"] for entry in tracked)
    duplicates = sorted(name for name, count in names.items() if count > 1)
    if duplicates:
        raise FormatError(f"{REGISTRY}: duplicate names {', '.join(duplicates)}")
    for entry in tracked:
        if entry["kind"] not in KINDS:
            raise FormatError(f"{REGISTRY}: {entry['name']} has kind {entry['kind']!r}")
    for entry in exempt:
        if not entry.get("reason", "").strip():
            raise FormatError(
                f"{REGISTRY}: {entry['path']} {entry['const']} needs a reason"
            )
    keys = Counter((entry["path"], entry["const"]) for entry in (*tracked, *exempt))
    repeated = sorted(key for key, count in keys.items() if count > 1)
    if repeated:
        raise FormatError(f"{REGISTRY}: listed more than once: {repeated}")
    return {"tracked": tracked, "exempt": exempt}


def snapshot(root: Path, rev: str | None = None) -> dict[str, tuple[str, str, int]]:
    """Each tracked format's location and value, at `rev` or in the working tree."""
    values = {}
    for entry in registry(root, rev)["tracked"]:
        path, const = entry["path"], entry["const"]
        source = read(root, rev, path)
        found = [value for name, value in declarations(source or "") if name == const]
        if len(found) != 1:
            raise FormatError(
                f"{path}: expected one {const} outside tests, found {len(found)}; "
                f"update {REGISTRY} if it moved"
            )
        if not INTEGER.fullmatch(found[0]):
            raise FormatError(f"{path}: {const} must be an integer literal")
        values[entry["name"]] = (path, const, int(found[0].replace("_", "")))
    return values


def scanned_files(root: Path) -> list[str]:
    return [
        path
        for path in git(root, "ls-files", "*.rs").splitlines()
        if not path.startswith(SKIPPED_PREFIXES) and not TEST_FILES.search(path)
    ]


def lint(root: Path) -> list[str]:
    """Problems with the registry: unlisted versioned consts and stale entries."""
    listed = registry(root)
    known = {
        (entry["path"], entry["const"])
        for entry in (*listed["tracked"], *listed["exempt"])
    }
    found = {
        (path, name)
        for path in scanned_files(root)
        for name, _ in declarations(read(root, None, path) or "")
    }
    problems = [
        f"{path}: {name} is neither tracked nor exempt in {REGISTRY}"
        for path, name in sorted(found - known)
    ]
    problems += [
        f"{REGISTRY}: {path} {name} no longer exists"
        for path, name in sorted(known - found)
    ]
    return problems


def workspace_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["workspace"]["package"]["version"]


def base_tag(version: str) -> str | None:
    """The `vX.Y.0` a version must match, or None for a prerelease of `X.Y.0`."""
    (major, minor, patch), prerelease = RELEASE.version(f"v{version}")
    if patch == 0 and prerelease is not None:
        return None
    return f"v{major}.{minor}.0"


def tag_ref(root: Path, tag: str) -> str:
    """The ref of `tag`, fetched from origin when a shallow checkout lacks it."""
    ref = f"refs/tags/{tag}"
    commit = ("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
    try:
        git(root, *commit)
    except subprocess.CalledProcessError:
        try:
            git(root, "fetch", "--depth=1", "--no-tags", "origin", f"+{ref}:{ref}")
            git(root, *commit)
        except subprocess.CalledProcessError:
            raise FormatError(
                f"Base tag {tag} is missing locally and on origin"
            ) from None
    return ref


def differences(
    base: dict[str, tuple[str, str, int]], head: dict[str, tuple[str, str, int]]
) -> list[str]:
    problems = []
    for name in sorted(base.keys() | head.keys()):
        if name not in head:
            path, const, _ = base[name]
            problems.append(f"{name} ({path} {const}) was removed")
        elif name not in base:
            path, const, value = head[name]
            problems.append(f"{name} ({path} {const} = {value}) was added")
        elif base[name][2] != head[name][2]:
            path, const, value = head[name]
            problems.append(
                f"{name} ({path} {const}) changed from {base[name][2]} to {value}"
            )
    return problems


def check(root: Path, base: str | None = None) -> list[str]:
    """Persisted-format changes since `base` (default: this minor's `vX.Y.0`)."""
    version = workspace_version(root)
    tag = base or base_tag(version)
    if tag is None:
        return []
    problems = differences(snapshot(root, tag_ref(root, tag)), snapshot(root))
    return [
        f"{problem} since {tag}; {version} must keep its persisted formats"
        for problem in problems
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("command", choices=("lint", "check"))
    parser.add_argument("--base", help="Tag to compare against (default: vX.Y.0)")
    arguments = parser.parse_args()
    try:
        if arguments.command == "lint":
            problems = lint(ROOT)
        else:
            problems = check(ROOT, arguments.base)
    except FormatError as error:
        sys.exit(str(error))
    if problems:
        sys.exit("\n".join(problems))


if __name__ == "__main__":
    main()
