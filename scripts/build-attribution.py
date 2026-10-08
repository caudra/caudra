"""Build a fail-closed, offline-readable attribution bundle using Cargo and stdlib.

The artifact uses default features; the separately locked worker uses no default
features, matching build-code-worker.py. This inventories distributed dependency
source and Rust standard-library/runtime notices, not system libraries or compiler
build tools, and is not legal clearance.
"""

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
from html.parser import HTMLParser
from pathlib import Path, PurePosixPath
from urllib.parse import unquote, urlsplit

import tomllib

ROOT = Path(__file__).resolve().parent.parent
POLICY = Path(__file__).with_name("attribution-policy.json")
EXCLUDED_DIRS = {".git", "target", "__pycache__", ".venv", "node_modules"}
WORKSPACE_BUILD_ENTRIES = {".git", ".cargo", ".cargo-ok", "target"}
BINARY_DISTRIBUTION = "binary-artifact"
RUST_RUNTIME_REPORT = "COPYRIGHT-library.html"
RUST_RUNTIME_SCOPE = "Upstream Rust library-wide notices, preserved as supplied: conservative across targets, not a per-artifact link map. Canonical license texts do not by their presence alone imply applicability."
LICENSE_NAME = re.compile(
    r"licen[cs]e|copying|copyright|notice|authors|patents|credits|contributors|thanks",
    re.IGNORECASE,
)
REVIEW_REQUIRED = {"aws-lc-sys", "ring", "rmcp", "two-face", "monty-typeshed"}
NATIVE_SUFFIXES = {".c", ".h", ".cc", ".cpp", ".hpp", ".s", ".asm"}
NATIVE_COMMENT = re.compile(
    rb"/\*.*?\*/|(?m:^\s*//[^\n]*(?:\n\s*//[^\n]*)*)", re.DOTALL
)
ASSEMBLY_COMMENT = re.compile(rb"(?m)^\s*[#;][^\n]*(?:\n\s*[#;][^\n]*)*")
LEGAL_TEXT = re.compile(
    rb"copyright|licen[cs]e|public domain|permission (?:is hereby granted|to use)",
    re.IGNORECASE,
)
EVIDENCE = {
    "MIT": ("permission is hereby granted, free of charge", "without warranty"),
    "MIT-0": ("permission is hereby granted, free of charge", "without warranty"),
    "Apache-2.0": ("apache license", "version 2.0", "end of terms and conditions"),
    "Apache-2.0 WITH LLVM-exception": ("apache license", "llvm exceptions"),
    "ISC": (
        "permission to use, copy, modify",
        "for any purpose",
        "the software is provided",
    ),
    "BSD-2-Clause": (
        "redistribution and use in source and binary forms",
        "this software is provided",
    ),
    "BSD-3-Clause": (
        "redistribution and use in source and binary forms",
        "neither the name",
    ),
    "0BSD": (
        "permission to use, copy, modify",
        "for any purpose",
        "the software is provided",
    ),
    "Zlib": (
        "this software is provided",
        "the origin of this software must not be misrepresented",
    ),
    "BSL-1.0": (
        "boost software license",
        "version 1.0",
        "permission is hereby granted",
    ),
    "Unlicense": (
        "this is free and unencumbered software released into the public domain",
    ),
    "CC0-1.0": ("creative commons", "cc0", "waiver"),
    "MPL-2.0": (
        "mozilla public license",
        "2.0",
        "3.1. distribution of source form",
        "10. versions of the license",
    ),
    "Unicode-3.0": ("unicode", "permission is hereby granted", "data files"),
    "Unicode-DFS-2016": ("unicode", "permission is hereby granted", "data files"),
    "BlueOak-1.0.0": ("blue oak model license", "1.0.0"),
    "CDLA-Permissive-2.0": ("community data license agreement", "permissive", "2.0"),
    "WTFPL": ("do what the fuck you want to public license", "version 2"),
    "curl": (
        "permission to use, copy, modify, and distribute this software",
        "except as contained in this notice",
        "prior written authorization of the copyright holder",
    ),
}


class AttributionError(Exception):
    pass


class RuntimeLinks(HTMLParser):
    def __init__(self):
        super().__init__()
        self.licenses = set()

    def handle_starttag(self, tag, attrs):
        if tag == "base":
            raise AttributionError(
                "Rust runtime report must not override its offline base path"
            )
        for name, value in attrs:
            if name not in {"href", "src"} or not value:
                continue
            link = urlsplit(value)
            if tag == "a" and name == "href" and link.scheme in {"http", "https"}:
                continue
            if link.scheme or link.netloc:
                raise AttributionError(
                    f"Rust runtime report has unsupported resource link: {value}"
                )
            if not link.path:
                continue
            path = safe_relative(unquote(link.path))
            if path.as_posix() == RUST_RUNTIME_REPORT:
                continue
            if (
                len(path.parts) != 2
                or path.parts[0] != "licenses"
                or path.suffix != ".txt"
            ):
                raise AttributionError(
                    f"Rust runtime report has unexpected local link: {value}"
                )
            self.licenses.add(path.as_posix())


def digest(data):
    return hashlib.sha256(data).hexdigest()


def safe_relative(value):
    path = PurePosixPath(value)
    if (
        not path.parts
        or path.is_absolute()
        or ".." in path.parts
        or "\\" in value
        or ":" in value
    ):
        raise AttributionError(f"Unsafe relative path: {value!r}")
    return path


def regular_file(path):
    if path.is_symlink():
        raise AttributionError(f"Refusing symlink: {path}")
    if not path.is_file():
        raise AttributionError(f"Missing regular file: {path}")
    return path.read_bytes()


def confined_file(root, relative):
    relative = safe_relative(relative)
    path = root
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            raise AttributionError(f"Refusing symlink: {path}")
    if not path.is_file():
        raise AttributionError(f"Missing regular file: {path}")
    return path


def files(root, excluded=EXCLUDED_DIRS):
    if root.is_symlink():
        raise AttributionError(f"Refusing symlink: {root}")
    if not root.is_dir():
        raise AttributionError(f"Missing source directory: {root}")

    def unreadable(error):
        raise AttributionError(f"Cannot inventory source directory: {error}") from error

    result = []
    for directory, dirs, names in os.walk(root, followlinks=False, onerror=unreadable):
        parent = Path(directory)
        for name in dirs + names:
            path = parent / name
            if path.is_symlink():
                raise AttributionError(f"Refusing symlink: {path}")
        dirs[:] = sorted(d for d in dirs if d not in excluded)
        for name in names:
            if name in excluded:
                continue
            path = parent / name
            if not stat.S_ISREG(path.stat().st_mode):
                raise AttributionError(f"Refusing non-regular file: {path}")
            safe_relative(path.relative_to(root).as_posix())
            result.append(path)
    return sorted(result)


def license_files(root, owned=False):
    if owned:
        result = []
        for path in sorted(root.iterdir()):
            if LICENSE_NAME.search(path.name) and path.name != "THIRD_PARTY_LICENSES":
                if path.is_symlink():
                    raise AttributionError(f"Refusing symlink: {path}")
                result.extend(files(path) if path.is_dir() else [path])
        return result
    return [
        p
        for p in files(root)
        if any(LICENSE_NAME.search(part) for part in p.relative_to(root).parts)
    ]


def choose_license(expression, text):
    if not expression:
        raise AttributionError("Missing SPDX license expression")
    expression = expression.replace("/", " OR ").replace("MPL-2.0+", "MPL-2.0")
    tokens = re.findall(r"\(|\)|[^\s()]+", expression)
    position = 0
    normalized = " ".join(re.sub(r"(?m)^\s*(?://|\*)\s?", "", text).lower().split())

    def atom():
        nonlocal position
        if position >= len(tokens):
            raise AttributionError(f"Incomplete SPDX expression: {expression}")
        token = tokens[position]
        position += 1
        if token == "(":
            options = alternatives()
            if position >= len(tokens) or tokens[position] != ")":
                raise AttributionError(f"Unclosed SPDX expression: {expression}")
            position += 1
            return options
        if position < len(tokens) and tokens[position] == "WITH":
            if position + 1 >= len(tokens):
                raise AttributionError(f"Missing SPDX exception: {expression}")
            token += " WITH " + tokens[position + 1]
            position += 2
        if token not in EVIDENCE:
            raise AttributionError(f"Unreviewed SPDX license: {token}")
        return [{token}]

    def conjunction():
        nonlocal position
        options = atom()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            right = atom()
            options = [left | r for left in options for r in right]
        return options

    def alternatives():
        nonlocal position
        options = conjunction()
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            options += conjunction()
        return options

    options = alternatives()
    if position != len(tokens):
        raise AttributionError(f"Invalid SPDX expression: {expression}")
    supported = [
        option
        for option in options
        if all(
            all(phrase in normalized for phrase in EVIDENCE[item]) for item in option
        )
    ]
    if not supported:
        raise AttributionError(f"Missing license text evidence for {expression}")
    return sorted(
        min(
            supported,
            key=lambda option: ("MPL-2.0" in option, len(option), sorted(option)),
        )
    )


def tree_ids(tree, packages):
    selected = set()
    for line in tree.splitlines():
        line = line.removesuffix(" (*)").replace(" (proc-macro)", "")
        match = re.fullmatch(r"(\S+) v(\S+)(?: \((.+)\))?", line)
        if not match:
            raise AttributionError(f"Unrecognized cargo tree row: {line!r}")
        name, version, location = match.groups()
        candidates = [
            p for p in packages if p["name"] == name and p["version"] == version
        ]
        if location is None:
            candidates = [
                p for p in candidates if (p["source"] or "").startswith("registry+")
            ]
        elif location.startswith("/") or re.match(r"^[A-Za-z]:", location):
            candidates = [
                p
                for p in candidates
                if Path(p["manifest_path"]).parent == Path(location)
            ]
        else:
            candidates = [
                p
                for p in candidates
                if (p["source"] or "").removeprefix("git+").startswith(location)
            ]
        if len(candidates) != 1:
            raise AttributionError(
                f"Cannot identify cargo tree package uniquely: {line}"
            )
        selected.add(candidates[0]["id"])
    return selected


def tree_edges(tree, packages):
    stack, edges, identifiers = [], set(), {}
    for line in tree.splitlines():
        match = re.fullmatch(r"(\d+)(.+)", line)
        if not match:
            raise AttributionError(f"Missing cargo tree depth: {line!r}")
        depth, label = int(match[1]), match[2]
        if depth > len(stack) or (depth == 0 and stack):
            raise AttributionError(f"Invalid cargo tree depth: {line!r}")
        if label not in identifiers:
            identifiers[label] = next(iter(tree_ids(label, packages)))
        package_id = identifiers[label]
        if depth:
            edges.add((stack[depth - 1], package_id))
        stack[depth:] = [package_id]
    return set(identifiers.values()), sorted(edges)


def rust_environment(repository):
    environment = os.environ.copy()
    environment["CARGO_TERM_COLOR"] = "never"
    channel = tomllib.loads(regular_file(repository / "rust-toolchain.toml").decode())[
        "toolchain"
    ]["channel"]
    environment.setdefault("RUSTUP_TOOLCHAIN", channel)
    return environment


def load_graph(manifest, package, target, worker=False):
    manifest = manifest.resolve()
    environment = rust_environment(ROOT)
    common = ["--locked", "--offline", "--manifest-path", str(manifest)]
    if worker:
        common += ["--no-default-features"]
    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--filter-platform",
                target,
                *common,
            ],
            text=True,
            encoding="utf-8",
            errors="strict",
            cwd=manifest.parent,
            env=environment,
        )
    )
    roots = [
        p for p in metadata["packages"] if p["name"] == package and p["source"] is None
    ]
    if len(roots) != 1:
        raise AttributionError(f"Expected exactly one local package named {package}")
    tree = subprocess.check_output(
        [
            "cargo",
            "tree",
            "--package",
            package,
            "--target",
            target,
            "--edges",
            "normal,build",
            "--prefix",
            "depth",
            "--format",
            "{p}",
            *common,
        ],
        text=True,
        encoding="utf-8",
        errors="strict",
        cwd=manifest.parent,
        env=environment,
    )
    selected, edges = tree_edges(tree, metadata["packages"])
    runtime_tree = subprocess.check_output(
        [
            "cargo",
            "tree",
            "--package",
            package,
            "--target",
            target,
            "--edges",
            "normal,build,no-proc-macro",
            "--prefix",
            "none",
            "--format",
            "{p}",
            *common,
        ],
        text=True,
        encoding="utf-8",
        errors="strict",
        cwd=manifest.parent,
        env=environment,
    )
    runtime_ids = tree_ids(runtime_tree, metadata["packages"])
    if not runtime_ids <= selected:
        raise AttributionError(
            "Runtime cargo tree contains packages absent from the complete tree"
        )
    metadata["dependency_edges"] = edges
    metadata["runtime_ids"] = sorted(runtime_ids)
    if roots[0]["id"] not in selected:
        raise AttributionError(f"Selected tree omits root {package}")
    metadata["selected"] = [p for p in metadata["packages"] if p["id"] in selected]
    for selected_package in metadata["selected"]:
        selected_package["workspace_root"] = metadata["workspace_root"]
    metadata["artifact_root"] = roots[0]
    metadata["manifest_path"] = str(manifest)
    return metadata


def package_checksum(package, locked):
    matching = [
        p
        for p in locked.get("package", [])
        if p["name"] == package["name"]
        and p["version"] == package["version"]
        and p.get("source") == package["source"]
    ]
    return matching[0].get("checksum") if len(matching) == 1 else None


def binary_exclusions(metadata, locked, policy, distribution):
    review = policy.get("binary_scope_review", {})
    excluded = [
        p
        for p in metadata["selected"]
        if p["name"] in review.get("excluded_packages", [])
    ]
    if not excluded:
        return {}
    if distribution != BINARY_DISTRIBUTION or review["scope"] != BINARY_DISTRIBUTION:
        raise AttributionError(
            "Binary-scope review does not apply to source or build-cache distributions"
        )
    profile = review["consumer_profiles"].get(metadata["artifact_root"]["name"])
    if profile is None:
        raise AttributionError("Unreviewed binary-scope consumer profile")
    checked = {}
    for name, pin in (review["chain"] | profile).items():
        candidates = [p for p in metadata["selected"] if p["name"] == name]
        if len(candidates) != 1:
            raise AttributionError(f"Binary-scope chain changed: {name}")
        package = candidates[0]
        if (
            package["version"] != pin["version"]
            or package["source"] != pin["source"]
            or package_checksum(package, locked) != pin["checksum"]
            or package["license"] != pin["declared_license"]
        ):
            raise AttributionError(
                f"Binary-scope package identity/checksum changed: {name}"
            )
        if (
            "proc_macro" in pin
            and any("proc-macro" in t["kind"] for t in package["targets"])
            != pin["proc_macro"]
        ):
            raise AttributionError(f"Binary-scope macro boundary changed: {name}")
        root = Path(package["manifest_path"]).parent
        for path, expected in pin.get("reviewed_files", {}).items():
            if digest(regular_file(confined_file(root, path))) != expected:
                raise AttributionError(
                    f"Binary-scope reviewed source changed: {name}/{path}"
                )
        checked[name] = package
    for name, pin in review["chain"].items():
        expected_names = (
            pin["incoming"] if pin["incoming"] is not None else list(profile)
        )
        expected = {checked[parent]["id"] for parent in expected_names}
        actual = {
            parent
            for parent, child in metadata["dependency_edges"]
            if child == checked[name]["id"]
        }
        if actual != expected:
            raise AttributionError(
                f"Binary-scope incoming consumer edges changed: {name}"
            )
    if any(p["id"] in metadata["runtime_ids"] for p in excluded):
        raise AttributionError("Binary-scope excluded package is runtime reachable")
    proof = {
        "review_id": review["id"],
        "root": metadata["artifact_root"]["name"],
        "scope": BINARY_DISTRIBUTION,
        "reason": review["reason"],
        "evidence": review["evidence"],
        "runtime_tree_edges": "normal,build,no-proc-macro",
        "checked_packages": [
            {
                "name": name,
                "version": p["version"],
                "source": p["source"],
                "checksum": package_checksum(p, locked),
            }
            for name, p in sorted(checked.items())
        ],
    }
    return {p["id"]: proof for p in excluded}


def load_policy(repository):
    policy = json.loads(regular_file(POLICY))
    for relative in policy.get("supplement_manifests", []):
        provenance = json.loads(regular_file(confined_file(repository, relative)))
        evidence = {item["path"]: item for item in provenance["files"]}
        for package in provenance["packages"]:
            if package["status"] != "collected":
                continue
            key = package["name"] + "@" + package["version"]
            entry = policy["packages"].setdefault(key, {})
            entry["provenance"] = {
                k: package[k]
                for k in ("source", "checksum", "declared_license", "cargo_vcs_info")
                if k in package
            }
            entry["embedded_components"] = package.get("embedded_components", [])
            entry.setdefault("files", []).extend(
                {"path": path, "sha256": evidence[path]["sha256"]}
                for path in package["license_files"] + package["notice_files"]
            )
    return policy


def workspace_digest(root):
    records = []

    def visit(directory):
        for path in sorted(directory.iterdir()):
            if directory == root and path.name in WORKSPACE_BUILD_ENTRIES:
                continue
            relative = path.relative_to(root).as_posix()
            if path.is_symlink():
                records.append([relative, "symlink", os.readlink(path)])
            elif path.is_dir():
                visit(path)
            else:
                records.append([relative, "file", digest(regular_file(path))])

    visit(root)
    return digest(json.dumps(sorted(records), separators=(",", ":")).encode())


def verify_workspace(root, policy):
    actual = workspace_digest(root)
    matches = [
        entry
        for entry in policy.get("workspace_sources", [])
        if entry["tree_sha256"] == actual
    ]
    if len(matches) != 1:
        raise AttributionError(f"Unreviewed worker workspace source tree: {actual}")
    return matches[0]


def notice_bytes(source):
    return source if isinstance(source, bytes) else regular_file(source)


def git_notices(package, repository, policy):
    source = package["source"]
    reviewed = policy.get("git_sources", {}).get(source)
    if reviewed is None:
        raise AttributionError(f"Unreviewed git source: {source}")
    root = Path(package["manifest_path"]).parent
    original = root / "Cargo.toml.orig"
    manifest = original if original.exists() else root / "Cargo.toml"
    expected = reviewed["packages"].get(package["name"] + "@" + package["version"], [])
    if digest(regular_file(manifest)) not in expected:
        raise AttributionError(
            f"Pinned git package manifest mismatch: {package['name']}"
        )
    result = []
    for notice in reviewed["notices"]:
        data = (
            notice["text"].encode()
            if "text" in notice
            else regular_file(confined_file(repository, notice["path"]))
        )
        if digest(data) != notice["sha256"]:
            raise AttributionError(f"Pinned git license text mismatch: {source}")
        safe_relative(notice["name"])
        result.append((data, notice["name"]))
    return result


def source_archive(root, destination):
    if destination.resolve().is_relative_to(root.resolve()):
        raise AttributionError(
            "Source archive destination must be outside the source directory"
        )
    inventory = []
    with (
        destination.open("wb") as output,
        gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed,
        tarfile.open(
            fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT
        ) as archive,
    ):
        for path in files(root, excluded={".git"}):
            relative = path.relative_to(root).as_posix()
            data = regular_file(path)
            entry = tarfile.TarInfo(relative)
            entry.size = len(data)
            entry.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
            archive.addfile(entry, io.BytesIO(data))
            inventory.append(
                {"path": relative, "sha256": digest(data), "size": len(data)}
            )
    return inventory


def native_notices(root):
    for path in files(root):
        if path.suffix.lower() not in NATIVE_SUFFIXES:
            continue
        data = regular_file(path)
        patterns = [NATIVE_COMMENT]
        if path.suffix.lower() in {".s", ".asm"}:
            patterns.append(ASSEMBLY_COMMENT)
        comments = [
            match.group()
            for pattern in patterns
            for match in pattern.finditer(data)
            if LEGAL_TEXT.search(match.group())
        ]
        if comments:
            yield path.relative_to(root).as_posix(), b"\n\n".join(comments) + b"\n"


def validate_native_licenses(text, evidence):
    decoded = text.decode("utf-8", errors="replace")
    selected = set()
    for expression in re.findall(r"SPDX-License-Identifier:\s*([^\r\n]+)", decoded):
        selected.update(
            choose_license(
                expression.removesuffix("*/").strip(), evidence + "\n" + decoded
            )
        )
    if not selected and re.search(
        r"Mozilla Public License|mozilla\.org/MPL", decoded, re.IGNORECASE
    ):
        selected.update(choose_license("MPL-2.0", evidence + "\n" + decoded))
    return sorted(selected)


def package_licenses(package, repository, policy):
    root = Path(package["manifest_path"]).parent
    local = package["source"] is None and root.is_relative_to(repository)
    found = [
        (p, p.relative_to(root).as_posix())
        for p in license_files(root, owned=package["source"] is None)
    ]
    license_file = package.get("license_file")
    if license_file:
        relative = safe_relative(license_file)
        path = confined_file(root, license_file)
        if path not in [p for p, _ in found]:
            found.append((path, relative.as_posix()))
    if local:
        found += [(repository / "LICENSE", "repository/LICENSE")]
    elif package["source"] is None and package.get("workspace_root"):
        workspace = Path(package["workspace_root"])
        if not root.is_relative_to(workspace):
            raise AttributionError(
                f"Local package outside its declared workspace: {root}"
            )
        ancestor = root
        while ancestor != workspace:
            ancestor = ancestor.parent
            found += [
                (p, "workspace/" + p.relative_to(workspace).as_posix())
                for p in sorted(ancestor.iterdir())
                if p.is_file() and LICENSE_NAME.search(p.name)
            ]
    if (package["source"] or "").startswith("git+"):
        found += git_notices(package, repository, policy)
    key = package["name"] + "@" + package["version"]
    supplement = policy.get("packages", {}).get(key, {})
    provenance = supplement.get("provenance")
    if provenance:
        if package["license"] != provenance["declared_license"]:
            raise AttributionError(f"Supplement declared-license mismatch: {key}")
        if package["source"] == provenance["source"] or (
            package["source"] is None and (root / ".cargo_vcs_info.json").exists()
        ):
            if (
                package.get("checksum")
                and package["checksum"] != provenance["checksum"]
            ):
                raise AttributionError(f"Supplement package checksum mismatch: {key}")
            actual_vcs = json.loads(regular_file(root / ".cargo_vcs_info.json"))
            if actual_vcs != provenance["cargo_vcs_info"]:
                raise AttributionError(f"Supplement source revision mismatch: {key}")
        elif package["source"] is None and package.get("verified_workspace"):
            workspace = Path(package["workspace_root"])
            expected_vcs = provenance["cargo_vcs_info"]
            if (
                package["verified_workspace"]["revision"] != expected_vcs["git"]["sha1"]
                or root.relative_to(workspace).as_posix() != expected_vcs["path_in_vcs"]
            ):
                raise AttributionError(
                    f"Supplement workspace revision/path mismatch: {key}"
                )
        else:
            supplement = {}
    if package["name"] in REVIEW_REQUIRED and not supplement:
        raise AttributionError(f"Unreviewed native/asset license inventory: {key}")
    for component in supplement.get("embedded_components", []):
        evidence = component.get("revision_evidence")
        if (
            evidence
            and digest(regular_file(confined_file(root, evidence["package_path"])))
            != evidence["sha256"]
        ):
            raise AttributionError(
                f"Embedded component revision mismatch: {key}: {component['name']}"
            )
    for entry in supplement.get("files", []):
        path = confined_file(repository, entry["path"])
        if digest(regular_file(path)) != entry["sha256"]:
            raise AttributionError(f"Supplement hash mismatch: {path}")
        found.append((path, "supplements/" + entry["path"]))
    expression = supplement.get("expression", package["license"])
    if not found:
        raise AttributionError(f"Missing license texts for {key}")
    text = "\n".join(
        notice_bytes(p).decode("utf-8", errors="replace") for p, _ in found
    )
    chosen = choose_license(expression, text)
    required = supplement.get("required_files", [])
    names = {name for _, name in found}
    for name in required:
        if name not in names:
            raise AttributionError(
                f"Required subcomponent license missing for {key}: {name}"
            )
    return sorted(set(found), key=lambda item: item[1]), expression, chosen


def emit_file(output, relative, data):
    path = output / safe_relative(relative)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return {"path": relative, "sha256": digest(data), "size": len(data)}


def runtime_legal_file(path):
    # Nix assembles toolchains with legal files symlinked into other store paths.
    # Only compiler-supplied fixed report/license paths use this resolution rule.
    try:
        data = regular_file(path.resolve(strict=True))
    except (OSError, RuntimeError, AttributionError) as error:
        raise AttributionError(
            f"Missing Rust runtime legal evidence: {path}; install the pinned toolchain's library copyright report and license texts"
        ) from error
    if not data.strip():
        raise AttributionError(f"Empty Rust runtime legal evidence: {path}")
    return data


def discover_rust_runtime(manifest, repository, policy):
    manifest = manifest.resolve()
    environment = rust_environment(repository)
    expected = policy.get("rust_runtime", {})
    pinned_release = tomllib.loads(
        regular_file(repository / "rust-toolchain.toml").decode()
    )["toolchain"]["channel"]
    if pinned_release != expected.get("release"):
        raise AttributionError(
            "Rust runtime policy does not match rust-toolchain.toml; review the release/commit pin"
        )
    try:
        version = subprocess.check_output(
            ["rustc", "-vV"],
            text=True,
            encoding="utf-8",
            errors="strict",
            cwd=manifest.parent,
            env=environment,
        )
        fields = dict(
            line.split(": ", 1) for line in version.splitlines() if ": " in line
        )
        if (
            fields.get("release") != pinned_release
            or fields.get("commit-hash") != expected.get("commit_hash")
            or not re.fullmatch(r"[A-Za-z0-9_.-]+", fields.get("host", ""))
        ):
            raise AttributionError(
                f"Rust runtime toolchain mismatch: expected {pinned_release} ({expected.get('commit_hash')}), "
                f"got {fields.get('release', 'missing release')} ({fields.get('commit-hash', 'missing commit')}); "
                "rustc -vV must also identify its host. Propagate the build's pinned compiler across manifest directories "
                "(with rustup, set RUSTUP_TOOLCHAIN); do not substitute unrelated legal documents."
            )
        reported_root = subprocess.check_output(
            ["rustc", "--print", "sysroot"],
            text=True,
            encoding="utf-8",
            errors="strict",
            cwd=manifest.parent,
            env=environment,
        ).strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise AttributionError(
            "Rust runtime discovery failed; install the pinned Rust toolchain and its legal documentation"
        ) from error
    if (
        not reported_root
        or len(reported_root.splitlines()) != 1
        or not Path(reported_root).is_absolute()
    ):
        raise AttributionError("Rust runtime discovery returned an invalid sysroot")
    docs = Path(reported_root) / "share/doc/rust"
    report = runtime_legal_file(docs / RUST_RUNTIME_REPORT)
    if b"Copyright notices for The Rust Standard Library" not in report:
        raise AttributionError(
            "Rust runtime copyright report is not the standard-library report"
        )
    links = RuntimeLinks()
    links.feed(report.decode("utf-8"))
    links.close()
    collected = {RUST_RUNTIME_REPORT: report}
    for path in sorted((docs / "licenses").glob("*.txt")):
        relative = "licenses/" + path.name
        safe_relative(relative)
        collected[relative] = runtime_legal_file(path)
    required = links.licenses | {"licenses/MIT.txt", "licenses/Apache-2.0.txt"}
    if missing := required - collected.keys():
        raise AttributionError(
            "Rust runtime license text missing: " + ", ".join(sorted(missing))
        )
    return {
        "release": fields["release"],
        "commit_hash": fields["commit-hash"],
        "host": fields["host"],
        "files": collected,
    }


def include_rust_runtime(output, repository, graphs, policy):
    records, emitted, identities = [], [], {}
    for label, metadata in graphs:
        manifest = Path(
            metadata["manifest_path"]
            if "manifest_path" in metadata
            else metadata["artifact_root"]["manifest_path"]
        )
        runtime = discover_rust_runtime(manifest, repository, policy)
        identity = digest(
            json.dumps(
                {
                    "release": runtime["release"],
                    "commit_hash": runtime["commit_hash"],
                    "host": runtime["host"],
                    "files": {
                        path: digest(data) for path, data in runtime["files"].items()
                    },
                },
                sort_keys=True,
            ).encode()
        )
        if identity in identities:
            identities[identity]["graphs"].append(label)
            continue
        directory = (
            "rust-runtime" if not records else "rust-runtime/variants/" + identity
        )
        record = {
            "release": runtime["release"],
            "commit_hash": runtime["commit_hash"],
            "host": runtime["host"],
            "graphs": [label],
            "scope": RUST_RUNTIME_SCOPE,
            "report": directory + "/" + RUST_RUNTIME_REPORT,
            "files": [],
        }
        for relative, data in sorted(runtime["files"].items()):
            file = emit_file(output, directory + "/" + relative, data)
            record["files"].append(file)
            emitted.append(file)
        records.append(record)
        identities[identity] = record
    return records, emitted


def write_bundle(
    output, repository, graphs, target, policy, distribution=BINARY_DISTRIBUTION
):
    policy_file = emit_file(
        output,
        "policy.json",
        (json.dumps(policy, indent=2, sort_keys=True) + "\n").encode(),
    )
    manifest = {
        "schema_version": 1,
        "target": target,
        "distribution": distribution,
        "scope": "Selected normal/build dependencies, separately locked bundled worker, and Rust standard-library/runtime notices; excludes system libraries and compiler build tools.",
        "policy": {
            "sha256": policy_file["sha256"],
            "path": policy_file["path"],
            "generator_sha256": digest(Path(__file__).read_bytes()),
            "spdx_identifiers": sorted(EVIDENCE),
        },
        "graphs": [],
        "packages": [],
        "files": [policy_file],
    }
    manifest["rust_runtime"], runtime_files = include_rust_runtime(
        output, repository, graphs, policy
    )
    manifest["files"].extend(runtime_files)
    for name in ("LICENSE", "NOTICE"):
        source = repository / name
        if name == "NOTICE" and not source.exists():
            source = repository / "NOTICE.md"
        manifest["files"].append(emit_file(output, name, regular_file(source)))
    supplement_root = repository / "THIRD_PARTY_LICENSES"
    if not supplement_root.is_dir():
        raise AttributionError("Missing THIRD_PARTY_LICENSES directory")
    for path in files(supplement_root):
        manifest["files"].append(
            emit_file(
                output,
                "THIRD_PARTY_LICENSES/" + path.relative_to(supplement_root).as_posix(),
                regular_file(path),
            )
        )
    errors = []
    for label, metadata in graphs:
        workspace = Path(metadata["workspace_root"])
        worker_workspace = None
        if (
            label == "worker"
            and not (
                Path(metadata["artifact_root"]["manifest_path"]).parent
                / ".cargo_vcs_info.json"
            ).exists()
            and any(p["source"] is None for p in metadata["selected"])
        ):
            worker_workspace = verify_workspace(workspace, policy)
        lock = emit_file(
            output, f"graphs/{label}/Cargo.lock", regular_file(workspace / "Cargo.lock")
        )
        manifest["files"].append(lock)
        locked = tomllib.loads((workspace / "Cargo.lock").read_text())
        exclusions = binary_exclusions(metadata, locked, policy, distribution)
        graph = {
            "name": label,
            "package": metadata["artifact_root"]["name"],
            "version": metadata["artifact_root"]["version"],
            "default_features": label != "worker",
            "lockfile": lock["path"],
            "packages": [],
            "binary_scope_exclusions": [],
        }
        manifest["graphs"].append(graph)
        for package in sorted(
            metadata["selected"], key=lambda p: (p["name"], p["version"], p["id"])
        ):
            if package["source"] is None and worker_workspace:
                package["verified_workspace"] = worker_workspace
            package["checksum"] = package_checksum(package, locked)
            root = Path(package["manifest_path"]).parent
            source = package["source"] or (
                "path:" + root.relative_to(repository).as_posix()
                if root.is_relative_to(repository)
                else "worker-root"
            )
            key = (
                f"{package['name']}-{package['version']}-{digest(source.encode())[:12]}"
            )
            safe_relative(key)
            graph["packages"].append(key)
            excluded = package["id"] in exclusions
            if excluded:
                graph["binary_scope_exclusions"].append(key)
            existing = next((p for p in manifest["packages"] if p["id"] == key), None)
            if existing is not None:
                if excluded:
                    existing["binary_scope_checks"].append(
                        {"graph": label, **exclusions[package["id"]]}
                    )
                continue
            try:
                if excluded:
                    manifest["packages"].append(
                        {
                            "id": key,
                            "name": package["name"],
                            "version": package["version"],
                            "source": source,
                            "declared_license": package["license"],
                            "checksum": package["checksum"],
                            "distribution_status": "binary-scope-excluded",
                            "license_text_status": "unresolved",
                            "selected_license": [],
                            "native_selected_licenses": {},
                            "notices": [],
                            "binary_scope_checks": [
                                {"graph": label, **exclusions[package["id"]]}
                            ],
                        }
                    )
                    continue
                notices, expression, chosen = package_licenses(
                    package, repository, policy
                )
                record = {
                    "id": key,
                    "name": package["name"],
                    "version": package["version"],
                    "source": source,
                    "declared_license": package["license"],
                    "checksum": package["checksum"],
                    "reviewed_expression": expression,
                    "selected_license": chosen,
                    "native_selected_licenses": {},
                    "notices": [],
                }
                for path, relative in notices:
                    saved = emit_file(
                        output, f"packages/{key}/{relative}", notice_bytes(path)
                    )
                    record["notices"].append(saved["path"])
                    manifest["files"].append(saved)
                if package["source"] is not None:
                    evidence = "\n".join(
                        notice_bytes(path).decode("utf-8", errors="replace")
                        for path, _ in notices
                    )
                    for relative, text in native_notices(root):
                        try:
                            native_selected = validate_native_licenses(text, evidence)
                        except AttributionError as error:
                            raise AttributionError(
                                f"Native subcomponent {relative}: {error}"
                            ) from error
                        saved = emit_file(
                            output,
                            f"packages/{key}/native-notices/{relative}.txt",
                            text,
                        )
                        if native_selected:
                            record["native_selected_licenses"][relative] = (
                                native_selected
                            )
                        record["notices"].append(saved["path"])
                        manifest["files"].append(saved)
                if "MPL-2.0" in chosen or any(
                    "MPL-2.0" in licenses
                    for licenses in record["native_selected_licenses"].values()
                ):
                    archive = f"sources/{key}.tar.gz"
                    destination = output / archive
                    destination.parent.mkdir(exist_ok=True)
                    record["source_files"] = source_archive(root, destination)
                    record["source_archive"] = archive
                    data = destination.read_bytes()
                    manifest["files"].append(
                        {"path": archive, "sha256": digest(data), "size": len(data)}
                    )
                manifest["packages"].append(record)
            except AttributionError as error:
                errors.append(
                    f"{label}: {package['name']}@{package['version']}: {error}"
                )
    if errors:
        raise AttributionError(
            "Unresolved attribution; bundle not published:\n"
            + "\n".join(sorted(set(errors)))
        )
    manifest["packages"].sort(key=lambda p: p["id"])
    lines = [
        "THIRD-PARTY ATTRIBUTION",
        "",
        f"Target: {target}",
        manifest["scope"],
        "This inventory is not legal clearance. Preserve LICENSE, NOTICE and THIRD_PARTY_LICENSES.",
        "Binary-scope exclusions, if listed below, do not apply to source or build-cache distributions.",
        "",
        "MPL source availability: complete corresponding package source is supplied in sources/.",
        "Archives preserve file bytes; manifest.json records every source file SHA-256.",
        "Extract each archive into its own directory. Cargo.lock files are in graphs/.",
        "",
    ]
    for graph in manifest["graphs"]:
        lines.append(
            f"{graph['name']}: {graph['package']} {graph['version']} ({len(graph['packages'])} packages)"
        )
    for runtime in manifest["rust_runtime"]:
        lines += [
            "",
            f"Rust standard-library/runtime {runtime['release']} (commit {runtime['commit_hash']}, compiler host {runtime['host']})",
            "  Graphs: " + ", ".join(runtime["graphs"]),
            "  Library copyright report: " + runtime["report"],
            "  " + runtime["scope"],
        ]
    for package in manifest["packages"]:
        lines += [
            "",
            f"{package['name']} {package['version']}",
            f"Source: {package['source']}",
            f"Declared: {package['declared_license']}; selected: {' AND '.join(package['selected_license'])}",
            *[f"  License/notice: {path}" for path in package["notices"]],
        ]
        if package.get("distribution_status") == "binary-scope-excluded":
            proof = package["binary_scope_checks"][0]
            lines += [
                "  BINARY-ARTIFACT SCOPE EXCLUSION: legal text unresolved; declared MIT is NOT verified.",
                "  No package source or build artifacts are emitted. Not applicable to source/build-cache distributions.",
                "  Registry checksum: " + package["checksum"],
                "  Review: " + proof["review_id"],
                "  Reason: " + proof["reason"],
                *["  Evidence: " + evidence for evidence in proof["evidence"]],
            ]
        if package.get("source_archive"):
            lines.append("  Corresponding source: " + package["source_archive"])
        if package["native_selected_licenses"]:
            licenses = sorted(
                {
                    license
                    for values in package["native_selected_licenses"].values()
                    for license in values
                }
            )
            lines.append(
                "  Native subcomponent licenses (see manifest for paths): "
                + ", ".join(licenses)
            )
    manifest["files"].append(
        emit_file(output, "ATTRIBUTION.txt", ("\n".join(lines) + "\n").encode())
    )
    manifest["files"].sort(key=lambda f: f["path"])
    emit_file(
        output,
        "manifest.json",
        (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode(),
    )
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=Path, required=True)
    parser.add_argument("--package", choices=("caudra", "workcell-mcp"), required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--worker-manifest-path", type=Path, required=True)
    parser.add_argument(
        "--worker-package", choices=("monty-runtime", "monty"), required=True
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.output_dir.exists() or args.output_dir.is_symlink():
            raise AttributionError(f"Output must not already exist: {args.output_dir}")
        policy = load_policy(ROOT)
        graphs = [
            (
                "artifact",
                load_graph(args.manifest_path.resolve(), args.package, args.target),
            ),
            (
                "worker",
                load_graph(
                    args.worker_manifest_path.resolve(),
                    args.worker_package,
                    args.target,
                    True,
                ),
            ),
        ]
        args.output_dir.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix="attribution-", dir=args.output_dir.parent
        ) as temporary:
            staged = Path(temporary) / "licenses"
            staged.mkdir()
            manifest = write_bundle(staged, ROOT, graphs, args.target, policy)
            staged.rename(args.output_dir)
        size = sum(p.stat().st_size for p in args.output_dir.rglob("*") if p.is_file())
        print(
            f"Wrote {len(manifest['packages'])} packages, {size} bytes to {args.output_dir}"
        )
    except (
        AttributionError,
        OSError,
        ValueError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"Attribution failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
