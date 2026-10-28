"""Bound rustc's incremental caches under a Cargo target directory.

Every profile, feature set and toolchain gets its own `<crate>-<hash>` directory under
`<target>/<profile>/incremental`, and rustc never removes one it no longer uses. Directories
idle past `IDLE_SECONDS` go first, then the least recently compiled ones until the total fits
the limit. Anything compiled within `ACTIVE_SECONDS` stays, so a running build keeps its cache.
"""

import argparse
import os
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIMIT_VARIABLE = "INCREMENTAL_CACHE_SIZE"
DEFAULT_LIMIT = "10G"
SIZE_UNITS = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}
GIB = SIZE_UNITS["G"]
IDLE_SECONDS = 2 * 24 * 60 * 60
ACTIVE_SECONDS = 60 * 60
CACHE_PATTERNS = ("*/incremental", "*/*/incremental")


@dataclass(frozen=True)
class Cache:
    path: Path
    compiled_at: float
    size: int


def parse_size(text: str) -> int:
    text = text.strip().upper()
    unit = SIZE_UNITS.get(text[-1:], 1)
    number = text[:-1] if text[-1:] in SIZE_UNITS else text
    try:
        return int(float(number) * unit)
    except ValueError:
        raise argparse.ArgumentTypeError(
            f"invalid size {text!r}; expected bytes or a K, M, G or T suffix"
        ) from None


def tree_size(path: Path) -> int:
    total = 0
    for directory, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(directory, name)).st_size
            except FileNotFoundError:
                pass
    return total


def caches(target: Path) -> list[Cache]:
    found = []
    for pattern in CACHE_PATTERNS:
        for incremental in target.glob(pattern):
            for path in incremental.iterdir():
                try:
                    compiled_at = path.stat().st_mtime
                except FileNotFoundError:
                    continue
                if path.is_dir():
                    found.append(Cache(path, compiled_at, tree_size(path)))
    return found


def doomed(found: list[Cache], limit: int, now: float) -> list[Cache]:
    total = sum(cache.size for cache in found)
    chosen = []
    for cache in sorted(found, key=lambda cache: cache.compiled_at):
        age = now - cache.compiled_at
        if age < ACTIVE_SECONDS or (age < IDLE_SECONDS and total <= limit):
            break
        chosen.append(cache)
        total -= cache.size
    return chosen


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--target-dir",
        type=Path,
        default=Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target"),
    )
    parser.add_argument(
        "--limit",
        type=parse_size,
        default=os.environ.get(LIMIT_VARIABLE) or DEFAULT_LIMIT,
        help=f"total size to keep (default ${LIMIT_VARIABLE} or {DEFAULT_LIMIT})",
    )
    arguments = parser.parse_args()
    if not arguments.target_dir.is_dir():
        return
    removed = doomed(caches(arguments.target_dir), arguments.limit, time.time())
    for cache in removed:
        shutil.rmtree(cache.path, ignore_errors=True)
    if removed:
        freed = sum(cache.size for cache in removed) / GIB
        print(f"Removed {len(removed)} incremental caches ({freed:.1f} GiB)")


if __name__ == "__main__":
    main()
