#!/usr/bin/env python3
"""Copy the pstack subset Caudra embeds from a cursor/plugins checkout into caudra-pstack/upstream.

The copy is verbatim apart from one rewrite: a relative Markdown link to a pstack file the copy
leaves out points at that file on GitHub instead, the same rewrite upstream's own harness
generator applies. Ported scripts are left out and recorded by hash, so a sync that changes one
names the Rust port to revisit.
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


def previous_script_hashes() -> dict[str, str]:
    if not MANIFEST.exists():
        return {}
    return json.loads(MANIFEST.read_text())["ported_scripts"]


def sync(source: Path) -> int:
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

    before = previous_script_hashes()
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
            f"PORT {name} changed upstream; update its Rust port in caudra-pstack/src"
        )
    return 1 if changed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source", type=Path, required=True, help="a cursor/plugins checkout"
    )
    args = parser.parse_args()
    return sync(args.source.resolve())


if __name__ == "__main__":
    sys.exit(main())
