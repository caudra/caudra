#!/usr/bin/env python3
"""Copy the subset of a skill pack Caudra embeds from an upstream checkout into <crate>/upstream.

`<crate>/pack.json` says what to copy: each root's files and trees, where they land, and which
directories and suffixes to leave out. The copy is verbatim apart from one rewrite: a relative
Markdown link to a file the copy moved points at its new place, and one to a file the copy leaves
out points at that file on GitHub instead. The manifest hashes every stored file, and under
`sources` the raw upstream bytes of each file the rewrite changed. Ported scripts are left out and
recorded by hash, so a sync that changes one names the Rust port to revisit, and an added or
changed file without a translation is named for a look.

<crate>/translated/ holds the Caudra translation of every file that names another harness's
vocabulary, each recorded with the hash of the upstream file it was translated from. A sync names
every translation whose upstream file changed or went away. `--mark` records a finished
translation. The `.caudra/skills/skillpack-translate` skill says how to translate.
"""

import argparse
import hashlib
import json
import posixpath
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKS = {
    "pstack": "caudra-pstack",
    "mattpocock": "caudra-mattpocock",
    "emil": "caudra-emil",
}
PACK_FILE = "pack.json"
MANIFEST_FILE = "upstream.json"
TRANSLATIONS_FILE = "translations.json"
LINK = re.compile(r"\]\(([^)\s]+)\)")
URL_SCHEME = re.compile(r"^[a-z][a-z0-9+.-]*:", re.IGNORECASE)


@dataclass
class Pack:
    name: str
    crate: str
    config: dict

    @property
    def dest(self) -> Path:
        return ROOT / self.crate / "upstream"

    @property
    def manifest(self) -> Path:
        return self.dest / MANIFEST_FILE

    @property
    def translated(self) -> Path:
        return ROOT / self.crate / "translated"

    @property
    def translations(self) -> Path:
        return self.translated / TRANSLATIONS_FILE


def load_pack(name: str) -> Pack:
    crate = PACKS[name]
    return Pack(name, crate, json.loads((ROOT / crate / PACK_FILE).read_text()))


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def join(*parts: str) -> str:
    return posixpath.normpath(posixpath.join(*parts)) if any(parts) else ""


def wanted(pack: Pack, relative: Path) -> bool:
    return not set(pack.config["skipped_parts"]).intersection(
        relative.parts
    ) and relative.suffix in set(pack.config["text_suffixes"])


def copied_files(pack: Pack, source: Path) -> dict[str, str]:
    """Every file to copy, from its path in the checkout to its path under upstream/."""
    files: dict[str, str] = {}
    for root in pack.config["roots"]:
        origin, target = root["from"], root["to"]
        for name in root.get("files", []):
            files[join(origin, name)] = join(target, name)
        for tree in root.get("trees", []):
            base = source / origin
            for path in sorted((base / tree).rglob("*")):
                relative = path.relative_to(base)
                if path.is_file() and wanted(pack, relative):
                    files[join(origin, relative.as_posix())] = join(
                        target, relative.as_posix()
                    )
    return files


def rewrite_links(
    pack: Pack, text: str, path: str, files: dict[str, str], source: Path
) -> str:
    scope = pack.config.get("link_scope", "")
    github = pack.config["repository"]

    def replace(match: re.Match[str]) -> str:
        href = match.group(1)
        target = re.sub(r"[#?].*", "", href)
        if not target or URL_SCHEME.match(target):
            return match.group(0)
        resolved = join(posixpath.dirname(path), target)
        inside = posixpath.relpath(resolved, scope or ".")
        if inside.startswith("..") or not (source / resolved).exists():
            return match.group(0)
        suffix = href[len(target) :]
        moved = files.get(resolved) or next(
            (
                dest[: -len(origin) + len(resolved)]
                for origin, dest in files.items()
                if origin.startswith(f"{resolved}/")
                and dest.endswith(origin[len(resolved) :])
            ),
            None,
        )
        if moved is None:
            kind = "tree" if (source / resolved).is_dir() else "blob"
            return f"]({github}/{kind}/main/{resolved}{suffix})"
        here = posixpath.dirname(files[path])
        if join(here, target) == moved:
            return match.group(0)
        return f"]({posixpath.relpath(moved, here or '.')}{suffix})"

    return LINK.sub(replace, text)


def commit_of(source: Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(source), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def previous_manifest(pack: Pack) -> dict:
    if not pack.manifest.exists():
        return {"files": {}}
    return json.loads(pack.manifest.read_text())


def sync(pack: Pack, source: Path) -> int:
    previous = previous_manifest(pack)
    files = copied_files(pack, source)
    if pack.dest.exists():
        shutil.rmtree(pack.dest)
    entries: dict[str, str] = {}
    sources: dict[str, str] = {}
    for origin_path, dest_path in files.items():
        origin = source / origin_path
        target = pack.dest / dest_path
        target.parent.mkdir(parents=True, exist_ok=True)
        if origin_path.endswith(".md"):
            raw = origin.read_text()
            target.write_text(rewrite_links(pack, raw, origin_path, files, source))
            if target.read_bytes() != origin.read_bytes():
                sources[dest_path] = sha256(origin)
        else:
            shutil.copyfile(origin, target)
        entries[dest_path] = sha256(target)

    ported = pack.config.get("ported_scripts", {})
    scripts = {
        name: sha256(source / ported["root"] / name) for name in ported.get("files", [])
    }
    version_from = pack.config.get("version_from")
    version = (
        json.loads((source / version_from).read_text())["version"]
        if version_from
        else ""
    )
    manifest = {
        "repository": pack.config["repository"],
        "commit": commit_of(source),
        "version": version,
        "files": entries,
    }
    if sources:
        manifest["sources"] = sources
    if scripts:
        manifest["ported_scripts"] = scripts
    pack.dest.mkdir(parents=True, exist_ok=True)
    pack.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
    before = previous.get("ported_scripts", {})
    changed = [
        name for name, digest in scripts.items() if before.get(name, digest) != digest
    ]
    print(f"{pack.name} {version}: {len(entries)} files")
    for name in changed:
        print(
            f"PORT {name} changed upstream; update its Rust port in {pack.crate}/src/scripts"
        )
    stale = stale_translations(pack, entries) + untranslated_changes(
        pack, previous["files"], entries
    )
    for line in stale:
        print(line)
    print(
        f"Then run `cargo test -p {pack.crate} --lib`: it lists every page that still names "
        "another harness's vocabulary."
    )
    return 1 if changed or stale else 0


def recorded_translations(pack: Pack) -> dict[str, str]:
    if not pack.translations.exists():
        return {}
    return json.loads(pack.translations.read_text())["files"]


def stale_translations(pack: Pack, entries: dict[str, str]) -> list[str]:
    lines = []
    for name, digest in sorted(recorded_translations(pack).items()):
        if name not in entries:
            lines.append(
                f"ORPHAN {name}: upstream removed it; delete or move the translation"
            )
        elif entries[name] != digest:
            lines.append(
                f"TRANSLATE {name}: upstream changed; see `git diff -- {pack.crate}/upstream/{name}`"
            )
    return lines


def untranslated_changes(
    pack: Pack, before: dict[str, str], entries: dict[str, str]
) -> list[str]:
    recorded = recorded_translations(pack)
    return [
        f"NEW {name}: upstream added or changed it; translate it if the check names it"
        for name, digest in sorted(entries.items())
        if name not in recorded and before.get(name) != digest
    ]


def mark(pack: Pack, names: list[str]) -> int:
    entries = json.loads(pack.manifest.read_text())["files"]
    recorded = recorded_translations(pack)
    for name in names:
        if not (pack.translated / name).is_file():
            print(f"no translation at {pack.crate}/translated/{name}", file=sys.stderr)
            return 1
        if name not in entries:
            print(f"no upstream file {name} in {MANIFEST_FILE}", file=sys.stderr)
            return 1
        recorded[name] = entries[name]
    pack.translations.parent.mkdir(parents=True, exist_ok=True)
    pack.translations.write_text(
        json.dumps({"files": dict(sorted(recorded.items()))}, indent=2) + "\n"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pack", choices=sorted(PACKS), help="the pack to sync")
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument(
        "--source", type=Path, help="a checkout of the pack's repository"
    )
    action.add_argument(
        "--mark",
        nargs="+",
        metavar="PATH",
        help="record translations as made from the current upstream files, such as skills/tdd/SKILL.md",
    )
    args = parser.parse_args()
    pack = load_pack(args.pack)
    if args.mark:
        return mark(pack, args.mark)
    return sync(pack, args.source.resolve())


if __name__ == "__main__":
    sys.exit(main())
