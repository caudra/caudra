#!/usr/bin/env python3
"""Pin Workcell at a pushed commit and reconcile the flake's git dependency hashes.

The flake feeds crane a fixed-output hash per git dependency, keyed by the exact
Cargo.lock source string. A key embeds its own commit, so a bump changes the key
itself and the old one has to go. Doing that by hand means editing three files
and recovering the hash from a deliberate build failure.

The hash is the NAR hash of the fetched checkout, which `nix hash path` computes
without the daemon, so this works on a host that cannot build. That equivalence
holds only for a repository without submodules, which is asserted rather than
assumed: crane fetches submodules and a bare checkout would not, so the hashes
would silently disagree.
"""

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn

import tomllib

ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
FLAKE = ROOT / "flake.nix"

DEPENDENCY = "workcell"
GIT_PREFIX = "git+"
BLOCK_OPEN = "      gitDepHashes = {"
BLOCK_CLOSE = "      };"
ENTRY = re.compile(r'^\s*"(git\+[^"]+)" =$')
# `git+URL?rev=REV#COMMIT`, or `?tag=v1#COMMIT`. The fragment is the locked
# commit and is what to fetch; the query is only how the dependency was written.
SOURCE = re.compile(r"^git\+(?P<url>[^?#]+)(?:\?[^#]*)?#(?P<commit>[0-9a-f]{40})$")
NIX_HASH = ["nix", "--extra-experimental-features", "nix-command", "hash", "path"]
TRACKED = ("Cargo.toml", "Cargo.lock", "flake.nix")


def run(command, **kwargs):
    """Runs a command, capturing stdout and raising on a non-zero exit."""
    return subprocess.run(
        command, check=True, capture_output=True, text=True, **kwargs
    ).stdout


def fail(message) -> NoReturn:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def pinned_dependency():
    """The declared git URL and revision of the dependency being bumped."""
    manifest = tomllib.loads(CARGO_TOML.read_text())
    entry = manifest["workspace"]["dependencies"][DEPENDENCY]
    return entry["git"], entry["rev"]


def locked_sources():
    """Every git source string in Cargo.lock, in first-seen order."""
    packages = tomllib.loads(CARGO_LOCK.read_text())["package"]
    sources = (package.get("source", "") for package in packages)
    return list(dict.fromkeys(s for s in sources if s.startswith(GIT_PREFIX)))


def nar_hash(url, commit):
    """The hash crane expects for a git dependency: its checkout, without `.git`."""
    with tempfile.TemporaryDirectory() as directory:
        run(["git", "init", "--quiet", directory])
        git = ["git", "-C", directory]
        run(git + ["remote", "add", "origin", url])
        try:
            run(git + ["fetch", "--quiet", "--depth", "1", "origin", commit])
        except subprocess.CalledProcessError:
            run(git + ["fetch", "--quiet", "origin"])
        run(git + ["checkout", "--quiet", commit])
        checkout = Path(directory)
        if (checkout / ".gitmodules").exists():
            fail(f"{url} has submodules; its checkout hash would not match crane's")
        shutil.rmtree(checkout / ".git")
        return run(NIX_HASH + ["--type", "sha256", "--sri", directory]).strip()


def read_hashes():
    """The flake's recorded hashes, keyed by source string, in file order."""
    lines = FLAKE.read_text().splitlines()
    start = lines.index(BLOCK_OPEN)
    end = lines.index(BLOCK_CLOSE, start)
    hashes = {}
    for index in range(start + 1, end):
        key = ENTRY.match(lines[index])
        if key:
            hashes[key.group(1)] = lines[index + 1].strip().rstrip(";").strip('"')
    return hashes


def write_hashes(hashes):
    lines = FLAKE.read_text().splitlines(keepends=True)
    start = lines.index(BLOCK_OPEN + "\n")
    end = lines.index(BLOCK_CLOSE + "\n", start)
    body = [f'        "{key}" =\n          "{value}";\n' for key, value in hashes.items()]
    FLAKE.write_text("".join(lines[: start + 1] + body + lines[end:]))


def reconcile(sources):
    """Adds a hash for every source missing one and drops the keys nothing needs."""
    recorded = read_hashes()
    kept = {key: value for key, value in recorded.items() if key in sources}
    for source in sources:
        if source in kept:
            continue
        parsed = SOURCE.match(source)
        if not parsed:
            fail(f"cannot read a URL and commit out of {source}")
        print(f"  hashing {source}")
        kept[source] = nar_hash(parsed.group("url"), parsed.group("commit"))
    write_hashes(kept)
    for source in recorded.keys() - kept.keys():
        print(f"  dropped {source}")
    if read_hashes().keys() != set(sources):
        fail("the rewritten flake does not match Cargo.lock")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--rev", help="commit to pin (default: the remote's default branch head)"
    )
    arguments = parser.parse_args()

    url, current = pinned_dependency()
    target = arguments.rev or run(["git", "ls-remote", url, "HEAD"]).split()[0]
    if target == current and read_hashes().keys() == set(locked_sources()):
        print(f"{DEPENDENCY} is already at {current[:8]} and the flake agrees")
        return

    dirty = [path for path in TRACKED if run(["git", "-C", ROOT, "status", "--porcelain", path])]
    if dirty:
        fail(f"uncommitted changes in {', '.join(dirty)}; commit or stash them first")

    if target != current:
        print(f"{DEPENDENCY} {current[:8]} -> {target[:8]}")
        manifest = CARGO_TOML.read_text()
        line = next(l for l in manifest.splitlines() if l.startswith(f"{DEPENDENCY} = "))
        CARGO_TOML.write_text(manifest.replace(line, line.replace(current, target), 1))
        # Plain cargo: the dev wrapper patches the dependency to a local path,
        # which would leave the lockfile describing something other than the pin.
        run(["cargo", "update", "-p", DEPENDENCY], cwd=ROOT)

    reconcile(locked_sources())
    print("done. Nothing is committed; run the workspace tests against the new pin.")


if __name__ == "__main__":
    main()
