#!/usr/bin/env python3

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
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
VERSION_INHERITANCE_ERROR = "package.version must inherit workspace.package.version"
NOTES_LIMIT = 64 * 1024
MANIFEST_LIMIT = 8 * 1024 * 1024
ATTRIBUTION_LIMIT = 128 * 1024 * 1024
ARCHIVE_ENTRY_LIMIT = 20000
COMPACT_FILES = {
    "LICENSE",
    "NOTICE",
    "THIRD_PARTY_NOTICES.txt",
    "ATTRIBUTION.txt",
    "manifest.json",
    "attribution.tar.gz",
}
POLICY_PREFIX = "<!-- caudra-attribution-layout: "
COMPACT_MARKER = b"CAUDRA-ATTRIBUTION compact-v2\n"
GRAPH_IDENTITY = ("name", "package", "version", "default_features")


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


def attribution_layout(value: str) -> str:
    normalized = value.strip().lower()
    require(
        normalized in ("", "false", "0", "true", "1"),
        "CAUDRA_EXPANDED_ATTRIBUTION must be false/0 or true/1",
    )
    return "expanded" if normalized in ("true", "1") else "compact"


def release_notes(tag: str, layout: str = "compact") -> str:
    version(tag)
    require(layout in ("compact", "expanded"), "Unknown attribution layout")
    directory = ROOT / "release-notes"
    path = directory / f"{tag[1:]}.md"
    require(
        not directory.is_symlink() and not path.is_symlink() and path.is_file(),
        f"Missing version-specific release notes: {path}",
    )
    require(path.stat().st_size <= NOTES_LIMIT, "Release notes exceed the size limit")
    content = path.read_text(encoding="utf-8").strip()
    require(bool(content), "Release notes must not be empty")
    require(
        re.search(
            r"\b(?:TODO|TBD|FIXME)\b|\{\{.*?\}\}|<placeholder>", content, re.IGNORECASE
        )
        is None
        and POLICY_PREFIX not in content,
        "Release notes contain an unresolved placeholder or reserved policy marker",
    )
    status = (
        "Preview / prerelease. Interfaces, configuration, "
        "and stored-data formats may change."
        if version(tag)[1] is not None
        else "Stable release."
    )
    return f"""**Status:** {status}

{content}

{POLICY_PREFIX}{layout} -->
"""


def assert_release_policy(release: dict, layout: str) -> None:
    markers = re.findall(
        re.escape(POLICY_PREFIX) + r"(compact|expanded) -->", release.get("body", "")
    )
    require(
        markers == [layout],
        "Draft attribution layout is missing or conflicts with this release attempt",
    )


def validate_workspace_versions(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as manifest:
        workspace = tomllib.load(manifest)["workspace"]
    for member in (".", *workspace["members"]):
        path = Path(member) / "Cargo.toml"
        with (root / path).open("rb") as manifest:
            package = tomllib.load(manifest)["package"]
        require(
            package.get("version") == {"workspace": True},
            f"{path.as_posix()}: {VERSION_INHERITANCE_ERROR}",
        )
    return workspace["package"]["version"]


def validate_source(repository: str, ref: str, sha: str, root: Path) -> str:
    require(repository == REPOSITORY, "Releases are restricted to caudra/caudra")
    require(ref.startswith("refs/tags/"), "Release source must be a tag")
    tag = ref.removeprefix("refs/tags/")
    version(tag)
    require(
        SHA.fullmatch(sha) is not None, "Release source must be an exact commit SHA"
    )
    workspace_version = validate_workspace_versions(root)
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


def prepare(tag: str, sha: str, layout: str = "compact") -> None:
    notes = release_notes(tag, layout)
    release = existing_draft(tag, sha)
    if release is None:
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
            notes,
        )
        release = existing_draft(tag, sha)
    if release is None:
        raise ValueError("Draft creation did not produce a release")
    assert_release_policy(release, layout)


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


def archive_path(name: str) -> str:
    name = name.removeprefix("./").rstrip("/")
    require(
        bool(name)
        and not name.startswith("/")
        and "\\" not in name
        and ":" not in name
        and ".." not in PurePosixPath(name).parts
        and str(PurePosixPath(name)) == name,
        f"Unsafe archive path: {name!r}",
    )
    return name


def read_attribution(archive, prefix: str, payloads=None) -> dict[str, bytes]:
    result = {}
    seen = set()
    total = 0
    zipped = isinstance(archive, zipfile.ZipFile)
    members = archive.infolist() if zipped else archive
    for member in members:
        raw = member.filename if zipped else member.name
        directory = member.is_dir() if zipped else member.isdir()
        if raw in (".", "./") and directory:
            continue
        name = archive_path(raw)
        require(name not in seen, f"Duplicate archive member: {name}")
        seen.add(name)
        require(len(seen) <= ARCHIVE_ENTRY_LIMIT, "Too many archive entries")
        if zipped:
            kind = stat.S_IFMT(member.external_attr >> 16)
            require(
                kind in (0, stat.S_IFDIR if directory else stat.S_IFREG),
                f"Unsafe archive type: {name}",
            )
        else:
            require(member.isfile() or directory, f"Unsafe archive type: {name}")
        if directory:
            if payloads is not None:
                require(
                    name == prefix.rstrip("/") or name.startswith(prefix),
                    f"Unexpected archive directory: {name}",
                )
            continue
        size = member.file_size if zipped else member.size
        require(size >= 0, f"Invalid archive member size: {name}")
        if not name.startswith(prefix):
            if payloads is not None:
                payloads[name] = size
            continue
        relative = name.removeprefix(prefix)
        total += size
        require(total <= ATTRIBUTION_LIMIT, "Attribution exceeds the size limit")
        with archive.open(member) if zipped else archive.extractfile(member) as stream:
            data = stream.read(size + 1)
        require(len(data) == size, f"Invalid archive member size: {name}")
        result[relative] = data
    return result


def validate_attribution(files: dict[str, bytes], target: str, layout: str) -> dict:
    data = files.get("manifest.json", b"")
    require(
        0 < len(data) <= MANIFEST_LIMIT, "Missing or oversized attribution manifest"
    )
    manifest = json.loads(data)
    require(
        manifest.get("target") == target, "Attribution target does not match artifact"
    )
    compact = layout == "compact"
    require(
        manifest.get("schema_version") == (2 if compact else 1)
        and manifest.get("layout", "expanded") == layout,
        "Artifact attribution layout does not match release policy",
    )
    records = manifest.get("files", [])
    names = [record["path"] for record in records]
    require(
        len(names) == len(set(names)) and set(names) == set(files) - {"manifest.json"},
        "Attribution manifest inventory does not match archive",
    )
    for record in records:
        content = files[record["path"]]
        require(
            record["size"] == len(content)
            and record["sha256"] == hashlib.sha256(content).hexdigest(),
            f"Attribution hash or size mismatch: {record['path']}",
        )
    if compact:
        require(
            set(files) == COMPACT_FILES,
            "Compact attribution must contain exactly six files",
        )
        require(
            files["ATTRIBUTION.txt"].startswith(COMPACT_MARKER),
            "Missing compact attribution marker",
        )
        with tarfile.open(
            fileobj=io.BytesIO(files["attribution.tar.gz"]), mode="r:gz"
        ) as archive:
            evidence = read_attribution(archive, "")
        original = validate_attribution(evidence, target, "expanded")
        require(
            manifest.get("graphs")
            == [
                {key: graph[key] for key in GRAPH_IDENTITY}
                for graph in original["graphs"]
            ],
            "Compact graph identities do not match original evidence",
        )
        reference = manifest.get("evidence_manifest", {})
        require(
            reference.get("archive") == "attribution.tar.gz"
            and reference.get("member") == "manifest.json"
            and reference.get("sha256")
            == hashlib.sha256(evidence["manifest.json"]).hexdigest()
            and reference.get("size") == len(evidence["manifest.json"]),
            "Compact evidence manifest identity does not match companion",
        )
    else:
        validate_evidence(files, manifest)
    return manifest


def validate_evidence(files: dict[str, bytes], manifest: dict) -> None:
    required = {"LICENSE", "NOTICE", "ATTRIBUTION.txt", "policy.json"}
    require(
        all(files.get(name) for name in required),
        "Missing required attribution evidence",
    )
    graphs = manifest.get("graphs", [])
    require(
        len(graphs) == 2
        and {graph["name"] for graph in graphs} == {"artifact", "worker"},
        "Attribution must identify artifact and worker graphs",
    )
    packages = manifest.get("packages", [])
    identities = {package["id"] for package in packages}
    require(
        bool(packages) and len(identities) == len(packages),
        "Missing or duplicate attribution packages",
    )
    require(
        identities == {identity for graph in graphs for identity in graph["packages"]},
        "Attribution graph package references do not match inventory",
    )
    for graph in graphs:
        require(
            bool(files.get(graph.get("lockfile"))), "Missing attribution graph lockfile"
        )
    for package in packages:
        if package.get("distribution_status") == "binary-scope-excluded":
            require(
                bool(package.get("binary_scope_checks"))
                and all(
                    package["id"] in graph.get("binary_scope_exclusions", [])
                    for graph in graphs
                    if package["id"] in graph["packages"]
                ),
                "Missing binary-scope exclusion evidence",
            )
            continue
        notices = package.get("notices", [])
        require(
            bool(notices)
            and all(path in files for path in notices)
            and any(files[path] for path in notices),
            f"Missing package legal notices: {package['id']}",
        )
        selected = package.get("selected_license", [])
        require(bool(selected), "Missing selected package licenses")
        if "MPL-2.0" in selected or any(
            "MPL-2.0" in licenses
            for licenses in package.get("native_selected_licenses", {}).values()
        ):
            require(
                bool(files.get(package.get("source_archive")))
                and bool(package.get("source_files")),
                "Missing corresponding-source evidence",
            )
    runtime = manifest.get("rust_runtime", [])
    require(
        bool(runtime)
        and {name for entry in runtime for name in entry["graphs"]}
        == {"artifact", "worker"},
        "Missing Rust runtime attribution",
    )
    for entry in runtime:
        require(
            bool(files.get(entry.get("report")))
            and bool(entry.get("files"))
            and all(files.get(record["path"]) for record in entry["files"]),
            "Missing Rust runtime legal evidence",
        )


def verify_archives(directory: Path, tag: str, layout: str) -> None:
    for target in TARGETS:
        base = f"caudra-{tag}-{target}"
        extension = "zip" if target.endswith("windows-msvc") else "tar.gz"
        for name in (f"{base}.{extension}", f"{base}-symbols.tar.gz"):
            path = directory / name
            payloads = {}
            with (
                zipfile.ZipFile(path)
                if name.endswith(".zip")
                else tarfile.open(path, "r:gz")
            ) as archive:
                files = read_attribution(archive, "licenses/", payloads)
            windows = target.endswith("windows-msvc")
            symbols = name.endswith("-symbols.tar.gz")
            required = {"caudra.exe" if windows else "caudra"}
            if symbols:
                required.add("monty.exe" if windows else "monty")
            allowed = required | (
                {"caudra.pdb", "monty.pdb"} if symbols and windows else set()
            )
            require(
                required <= set(payloads) <= allowed
                and all(size > 0 for size in payloads.values()),
                f"Archive binary/symbol inventory mismatch: {name}",
            )
            manifest = validate_attribution(files, target, layout)
            require(
                any(
                    graph.get("name") == "artifact"
                    and graph.get("package") == "caudra"
                    and graph.get("version") == tag[1:]
                    for graph in manifest.get("graphs", [])
                ),
                "Attribution artifact version does not match release tag",
            )


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


def publish(tag: str, sha: str, directory: Path, layout: str = "compact") -> None:
    notes = release_notes(tag, layout)
    release = existing_draft(tag, sha)
    if release is None:
        raise ValueError("Expected an existing verified draft")
    release_id = release["id"]
    assert_release_policy(release, layout)
    archives = [name for name in asset_names(tag) if name not in INSTALLERS]
    assert_inventory(directory, archives)
    verify_archives(directory, tag, layout)
    for name in INSTALLERS:
        shutil.copyfile(ROOT / name, directory / name)
    assert_inventory(directory, asset_names(tag))
    (directory / CHECKSUMS).write_bytes(checksums(directory, tag))
    expected = sorted([*asset_names(tag), CHECKSUMS])
    assert_inventory(directory, expected)
    for name in expected:
        current = guard(release_id, tag, sha)
        assert_release_policy(current, layout)
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
    assert_release_policy(guard(release_id, tag, sha), layout)
    api(
        f"releases/{release_id}",
        {
            "name": release_title(tag),
            "body": notes,
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
    parser.add_argument("--layout", choices=("compact", "expanded"))
    arguments = parser.parse_args()
    sha = os.environ["GITHUB_SHA"]
    tag = validate_source(
        os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_REF"], sha, ROOT
    )
    layout = arguments.layout or attribution_layout(
        os.environ.get("CAUDRA_EXPANDED_ATTRIBUTION", "")
    )
    release_notes(tag, layout)
    if arguments.command == "validate":
        release = existing_draft(tag, sha)
        if release is not None:
            assert_release_policy(release, layout)
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as stream:
                stream.write(f"attribution-layout={layout}\n")
    elif arguments.command == "prepare":
        prepare(tag, sha, layout)
    else:
        publish(tag, sha, arguments.artifacts, layout)


if __name__ == "__main__":
    main()
