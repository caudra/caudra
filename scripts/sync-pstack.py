#!/usr/bin/env python3
"""Copy the pstack subset Caudra embeds from a cursor/plugins checkout into caudra-pstack/upstream.

The copy is verbatim apart from one rewrite: a relative Markdown link to a pstack file the copy
leaves out points at that file on GitHub instead, the same rewrite upstream's own harness
generator applies. Ported scripts are left out and recorded by hash, so a sync that changes one
names the Rust port to revisit, and an added or changed file without a translation is named for a
look.

caudra-pstack/translated/ holds the Caudra translation of every file that names Cursor, each
recorded with the hash of the upstream file it was translated from. A sync names every
translation whose upstream file changed or went away. `--mark` records a finished translation.
The `.caudra/skills/pstack-translate` skill says how to translate.
"""

import argparse
import hashlib
import json
import posixpath
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "caudra-pstack" / "upstream"
MANIFEST = DEST / "upstream.json"
TRANSLATED = ROOT / "caudra-pstack" / "translated"
TRANSLATIONS = TRANSLATED / "translations.json"
TERM_CHECK = "cargo test -p caudra-pstack --lib -- library::"
GITHUB = "https://github.com/cursor/plugins"
PSTACK = "pstack"
TEAM_KIT = "cursor-team-kit"
TEAM_KIT_SKILLS = ("deslop", "control-cli", "control-ui")
PSTACK_FILES = ("README.md", "LICENSE", "plugin.json")
PSTACK_TREES = ("skills", "agents", "docs/guide", "automations/benny")
SKIPPED_PARTS = {"scripts", "images", ".DS_Store", "node_modules"}
TEXT_SUFFIXES = {".md", ".yaml", ".yml", ".json", ".tsv", ""}
PORTED_SCRIPTS = (
    "skills/poteto-mode/scripts/check-plan.mjs",
    "skills/poteto-mode/scripts/worktree-audit.sh",
    "skills/poteto-mode/scripts/watch-pr/cli.ts",
    "skills/poteto-mode/scripts/watch-pr/github.ts",
    "skills/poteto-mode/scripts/watch-pr/policy.ts",
    "skills/poteto-mode/scripts/watch-pr/render.ts",
    "skills/poteto-mode/scripts/watch-pr/types.ts",
    "skills/poteto-mode/scripts/orch/orch.ts",
    "skills/poteto-mode/scripts/orch/store.ts",
    "skills/show-me-your-work/scripts/log.sh",
)
LINK = re.compile(r"\]\(([^)\s]+)\)")
URL_SCHEME = re.compile(r"^[a-z][a-z0-9+.-]*:", re.IGNORECASE)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def wanted(relative: Path) -> bool:
    return (
        not SKIPPED_PARTS.intersection(relative.parts)
        and relative.suffix in TEXT_SUFFIXES
    )


def pstack_files(source: Path) -> list[str]:
    files = list(PSTACK_FILES)
    for tree in PSTACK_TREES:
        for path in sorted((source / PSTACK / tree).rglob("*")):
            relative = path.relative_to(source / PSTACK)
            if path.is_file() and wanted(relative):
                files.append(relative.as_posix())
    return files


def rewrite_links(text: str, path: str, kept: set[str], source: Path) -> str:
    def replace(match: re.Match[str]) -> str:
        href = match.group(1)
        target = re.sub(r"[#?].*", "", href)
        if not target or URL_SCHEME.match(target):
            return match.group(0)
        resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), target))
        if resolved.startswith("..") or not (source / PSTACK / resolved).exists():
            return match.group(0)
        if resolved in kept or any(name.startswith(f"{resolved}/") for name in kept):
            return match.group(0)
        kind = "tree" if (source / PSTACK / resolved).is_dir() else "blob"
        suffix = href[len(target) :]
        return f"]({GITHUB}/{kind}/main/{PSTACK}/{resolved}{suffix})"

    return LINK.sub(replace, text)


def commit_of(source: Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(source), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def previous_manifest() -> dict:
    if not MANIFEST.exists():
        return {"files": {}, "ported_scripts": {}}
    return json.loads(MANIFEST.read_text())


def sync(source: Path) -> int:
    previous = previous_manifest()
    files = pstack_files(source)
    kept = set(files)
    if DEST.exists():
        shutil.rmtree(DEST)
    entries: dict[str, str] = {}
    for name in files:
        origin = source / PSTACK / name
        target = DEST / PSTACK / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if name.endswith(".md"):
            target.write_text(rewrite_links(origin.read_text(), name, kept, source))
        else:
            shutil.copyfile(origin, target)
        entries[f"{PSTACK}/{name}"] = sha256(target)
    for name in ["LICENSE", *(f"skills/{skill}/SKILL.md" for skill in TEAM_KIT_SKILLS)]:
        target = DEST / TEAM_KIT / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source / TEAM_KIT / name, target)
        entries[f"{TEAM_KIT}/{name}"] = sha256(target)

    before = previous["ported_scripts"]
    scripts = {name: sha256(source / PSTACK / name) for name in PORTED_SCRIPTS}
    plugin = json.loads((source / PSTACK / "plugin.json").read_text())
    manifest = {
        "repository": GITHUB,
        "commit": commit_of(source),
        "version": plugin["version"],
        "files": entries,
        "ported_scripts": scripts,
    }
    DEST.mkdir(parents=True, exist_ok=True)
    MANIFEST.write_text(json.dumps(manifest, indent=2) + "\n")
    changed = [
        name for name, digest in scripts.items() if before.get(name, digest) != digest
    ]
    print(f"pstack {plugin['version']}: {len(entries)} files")
    for name in changed:
        print(
            f"PORT {name} changed upstream; update its Rust port in caudra-pstack/src/scripts"
        )
    stale = stale_translations(entries) + untranslated_changes(
        previous["files"], entries
    )
    for line in stale:
        print(line)
    print(f"Then run `{TERM_CHECK}`: it lists every page that still names Cursor.")
    return 1 if changed or stale else 0


def recorded_translations() -> dict[str, str]:
    if not TRANSLATIONS.exists():
        return {}
    return json.loads(TRANSLATIONS.read_text())["files"]


def stale_translations(entries: dict[str, str]) -> list[str]:
    lines = []
    for name, digest in sorted(recorded_translations().items()):
        if name not in entries:
            lines.append(
                f"ORPHAN {name}: upstream removed it; delete or move the translation"
            )
        elif entries[name] != digest:
            lines.append(
                f"TRANSLATE {name}: upstream changed; see `git diff -- caudra-pstack/upstream/{name}`"
            )
    return lines


def untranslated_changes(before: dict[str, str], entries: dict[str, str]) -> list[str]:
    recorded = recorded_translations()
    return [
        f"NEW {name}: upstream added or changed it; translate it if the check names it"
        for name, digest in sorted(entries.items())
        if name not in recorded and before.get(name) != digest
    ]


def mark(names: list[str]) -> int:
    entries = json.loads(MANIFEST.read_text())["files"]
    recorded = recorded_translations()
    for name in names:
        if not (TRANSLATED / name).is_file():
            print(f"no translation at caudra-pstack/translated/{name}", file=sys.stderr)
            return 1
        if name not in entries:
            print(f"no upstream file {name} in {MANIFEST.name}", file=sys.stderr)
            return 1
        recorded[name] = entries[name]
    TRANSLATIONS.write_text(
        json.dumps({"files": dict(sorted(recorded.items()))}, indent=2) + "\n"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--source", type=Path, help="a cursor/plugins checkout")
    action.add_argument(
        "--mark",
        nargs="+",
        metavar="PATH",
        help="record translations as made from the current upstream files, such as pstack/skills/how/SKILL.md",
    )
    args = parser.parse_args()
    if args.mark:
        return mark(args.mark)
    return sync(args.source.resolve())


if __name__ == "__main__":
    sys.exit(main())
