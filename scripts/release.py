#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import quote

import tomllib

REPOSITORY = "caudra/caudra"
ROOT = Path(__file__).resolve().parent.parent
TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
INSTALLERS = ("install.sh", "install.ps1")
CHECKSUMS = "sha256sums.txt"
SEMVER = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)
SHA = re.compile(r"[0-9a-f]{40}")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def version(tag: str) -> tuple[tuple[int, int, int], str | None]:
    match = SEMVER.fullmatch(tag)
    if match is None:
        raise ValueError(f"Not a strict v-prefixed SemVer tag: {tag!r}")
    prerelease = match[4]
    if prerelease:
        require(
            all(
                not part.isdigit() or part == "0" or not part.startswith("0")
                for part in prerelease.split(".")
            ),
            "Numeric prerelease identifiers must not have leading zeroes",
        )
    return (int(match[1]), int(match[2]), int(match[3])), prerelease


def release_title(tag: str) -> str:
    (major, minor, patch), prerelease = version(tag)
    if prerelease and re.fullmatch(r"preview\.[1-9][0-9]*", prerelease):
        base = f"{major}.{minor}" if patch == 0 else f"{major}.{minor}.{patch}"
        return f"Caudra {base} Preview {prerelease.split('.')[1]}"
    return f"Caudra {tag[1:]}"


def release_notes(tag: str, sha: str) -> str:
    status = (
        "Preview / prerelease. Interfaces, configuration, "
        "and stored-data formats may change."
        if version(tag)[1] is not None
        else "Stable release."
    )
    return f"""## {release_title(tag)}

**Status:** {status}

### Experimental features and platform limits

- Experimental features remain subject to change. Consult the documentation before use.
- Windows installation and updates require manually running the external PowerShell installer.
- Rollback is unsupported on Windows pending native verification.
- Published artifacts do not imply complete native-platform or end-to-end verification.
  Consult the release workflow results for the checks actually executed.

### Migration and rollback

Back up your data before upgrading. Rollback restores the executable and attribution
bundle only: database and other data migrations are not reversed, and prior data is
not restored. An older executable may be incompatible with migrated data.

### Source and documentation

- Tag: `{tag}`
- Exact source: [{sha}](https://github.com/{REPOSITORY}/commit/{sha})
- [Canonical documentation](https://caudra.ai/docs/)
"""


def validate_source(repository: str, ref: str, sha: str, root: Path) -> str:
    require(repository == REPOSITORY, "Releases are restricted to caudra/caudra")
    require(ref.startswith("refs/tags/"), "Release source must be a tag")
    tag = ref.removeprefix("refs/tags/")
    version(tag)
    require(
        SHA.fullmatch(sha) is not None, "Release source must be an exact commit SHA"
    )
    with (root / "Cargo.toml").open("rb") as manifest:
        workspace_version = tomllib.load(manifest)["workspace"]["package"]["version"]
    require(tag[1:] == workspace_version, "Tag does not match root workspace version")
    head = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=root, text=True
    ).strip()
    require(head == sha, "Checkout does not match the release commit")
    return tag


def gh(*arguments: str) -> str:
    return subprocess.check_output(["gh", *arguments], text=True)


def api(path: str, payload: dict | None = None):
    command = ["gh", "api", f"repos/{REPOSITORY}/{path}"]
    if payload is not None:
        command += ["--method", "PATCH", "--input", "-"]
    result = subprocess.run(
        command,
        input=json.dumps(payload) if payload is not None else None,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(result.stdout)


def releases() -> list[dict]:
    pages = json.loads(
        gh("api", "--paginate", "--slurp", f"repos/{REPOSITORY}/releases?per_page=100")
    )
    return [release for page in pages for release in page]


def assert_tag_commit(tag: str, sha: str) -> None:
    target = api(f"git/ref/tags/{quote(tag, safe='')}")["object"]
    seen = set()
    while target["type"] == "tag":
        require(target["sha"] not in seen, "Cyclic annotated tag")
        seen.add(target["sha"])
        target = api(f"git/tags/{target['sha']}")["object"]
    require(
        target["type"] == "commit" and target["sha"] == sha,
        "Remote tag does not resolve to the verified source commit",
    )


def assert_draft(release: dict, tag: str, sha: str) -> None:
    require(release.get("draft") is True, "Published releases must never be modified")
    require(
        release.get("immutable") is False, "Release is immutable or status is unknown"
    )
    require(release.get("tag_name") == tag, "Release tag does not match")
    require(
        release.get("target_commitish") == sha, "Release source commit does not match"
    )
    require(
        release.get("prerelease") == (version(tag)[1] is not None),
        "Release prerelease status does not match the tag",
    )


def existing_draft(tag: str, sha: str) -> dict | None:
    assert_tag_commit(tag, sha)
    matching = [release for release in releases() if release["tag_name"] == tag]
    require(len(matching) <= 1, "Multiple releases claim the same tag")
    if matching:
        release = api(f"releases/{matching[0]['id']}")
        assert_draft(release, tag, sha)
        return release
    return None


def guard(release_id: int, tag: str, sha: str) -> dict:
    assert_tag_commit(tag, sha)
    release = api(f"releases/{release_id}")
    assert_draft(release, tag, sha)
    return release


def prepare(tag: str, sha: str) -> None:
    if existing_draft(tag, sha) is None:
        gh(
            "release",
            "create",
            tag,
            "--repo",
            REPOSITORY,
            "--verify-tag",
            "--target",
            sha,
            "--draft",
            f"--prerelease={str(version(tag)[1] is not None).lower()}",
            "--latest=false",
            "--title",
            release_title(tag),
            "--notes",
            release_notes(tag, sha),
        )
    require(
        existing_draft(tag, sha) is not None, "Draft creation did not produce a release"
    )


def asset_names(tag: str) -> list[str]:
    version(tag)
    names = list(INSTALLERS)
    for target in TARGETS:
        base = f"caudra-{tag}-{target}"
        extension = "zip" if target.endswith("windows-msvc") else "tar.gz"
        names += [f"{base}.{extension}", f"{base}-symbols.tar.gz"]
    return sorted(names)


def assert_inventory(directory: Path, expected: list[str]) -> None:
    actual = sorted(path.name for path in directory.iterdir())
    require(actual == sorted(expected), f"Asset inventory mismatch: {actual!r}")
    for name in expected:
        path = directory / name
        require(
            not path.is_symlink() and path.is_file() and path.stat().st_size > 0,
            f"Asset must be a nonempty regular file: {name}",
        )


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def checksums(directory: Path, tag: str) -> bytes:
    return "".join(
        f"{digest(directory / name)}  {name}\n" for name in asset_names(tag)
    ).encode("ascii")


def make_latest(tag: str, published: list[dict]) -> bool:
    candidate, prerelease = version(tag)
    if prerelease is not None:
        return False
    for release in published:
        if release["draft"] or release["prerelease"]:
            continue
        try:
            other, other_prerelease = version(release["tag_name"])
        except ValueError:
            continue
        if other_prerelease is None and other > candidate:
            return False
    return True


def publish(tag: str, sha: str, directory: Path) -> None:
    release = existing_draft(tag, sha)
    if release is None:
        raise ValueError("Expected an existing verified draft")
    release_id = release["id"]
    archives = [name for name in asset_names(tag) if name not in INSTALLERS]
    assert_inventory(directory, archives)
    for name in INSTALLERS:
        shutil.copyfile(ROOT / name, directory / name)
    assert_inventory(directory, asset_names(tag))
    (directory / CHECKSUMS).write_bytes(checksums(directory, tag))
    expected = sorted([*asset_names(tag), CHECKSUMS])
    assert_inventory(directory, expected)
    for name in expected:
        current = guard(release_id, tag, sha)
        require(
            all(asset["name"] in expected for asset in current["assets"]),
            "Draft contains unexpected assets; refusing to delete them",
        )
        gh(
            "release",
            "upload",
            tag,
            str(directory / name),
            "--repo",
            REPOSITORY,
            "--clobber",
        )
    current = guard(release_id, tag, sha)
    require(
        sorted(asset["name"] for asset in current["assets"]) == expected
        and all(
            asset["state"] == "uploaded"
            and asset["size"] == (directory / asset["name"]).stat().st_size
            for asset in current["assets"]
        ),
        "Uploaded release inventory is incomplete",
    )
    with tempfile.TemporaryDirectory(prefix="release-verify-") as temporary:
        downloaded = Path(temporary)
        gh("release", "download", tag, "--repo", REPOSITORY, "--dir", str(downloaded))
        assert_inventory(downloaded, expected)
        require(
            all(
                digest(downloaded / name) == digest(directory / name)
                for name in expected
            ),
            "Uploaded release checksums do not match verified local assets",
        )
    latest = make_latest(tag, releases())
    guard(release_id, tag, sha)
    api(
        f"releases/{release_id}",
        {
            "name": release_title(tag),
            "body": release_notes(tag, sha),
            "draft": False,
            "prerelease": version(tag)[1] is not None,
            "make_latest": str(latest).lower(),
        },
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Validate, stage, and publish a Caudra release"
    )
    parser.add_argument("command", choices=("validate", "prepare", "publish"))
    parser.add_argument("--artifacts", type=Path, default=Path("artifacts"))
    arguments = parser.parse_args()
    sha = os.environ["GITHUB_SHA"]
    tag = validate_source(
        os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_REF"], sha, ROOT
    )
    if arguments.command == "validate":
        existing_draft(tag, sha)
    elif arguments.command == "prepare":
        prepare(tag, sha)
    else:
        publish(tag, sha, arguments.artifacts)


if __name__ == "__main__":
    main()
