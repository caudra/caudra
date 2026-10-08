import copy
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "build_attribution", Path(__file__).with_name("build-attribution.py")
)
assert SPEC is not None and SPEC.loader is not None
attribution = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(attribution)
MIT = """MIT License
Copyright (c) Fixture authors
Permission is hereby granted, free of charge, to any person obtaining a copy
THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND.
"""
MPL = "Mozilla Public License Version 2.0\n3.1. Distribution of Source Form\n10. Versions of the License\n"
RUNTIME_REPORT = b'<html><title>Copyright notices for The Rust Standard Library</title><a href="licenses/MIT.txt">MIT</a></html>\n'
RUNTIME_VERSION = "1.99.0"
RUNTIME_COMMIT = "b940084d7eb6a299eb4bfeb8e34901bc051e7ac4"
NON_ASCII_TEXT = "café-描"
INVALID_UTF8 = b"\xff"


def captured_output(data, **kwargs):
    return subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(bytes.fromhex(sys.argv[1]))",
            data.hex(),
        ],
        stdout=subprocess.PIPE,
        check=True,
        **kwargs,
    ).stdout


class AttributionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        runtime = patch.object(
            attribution,
            "discover_rust_runtime",
            return_value={
                "release": RUNTIME_VERSION,
                "commit_hash": RUNTIME_COMMIT,
                "host": "fixture-host",
                "files": {
                    "COPYRIGHT-library.html": RUNTIME_REPORT,
                    "licenses/MIT.txt": MIT.encode(),
                },
            },
        )
        runtime.start()
        self.addCleanup(runtime.stop)

    def package(self, name="dependency", license="MIT"):
        root = self.root / name
        root.mkdir()
        (root / "Cargo.toml").write_text('[package]\nname = "' + name + '"\n')
        return {
            "id": name,
            "name": name,
            "version": "1.0.0",
            "license": license,
            "license_file": None,
            "manifest_path": str(root / "Cargo.toml"),
            "source": "registry+https://github.com/rust-lang/crates.io-index",
        }

    def test_metadata_and_tree_are_locked_target_filtered_and_worker_has_no_defaults(
        self,
    ):
        manifest = self.root / "Cargo.toml"
        manifest.write_text("")
        metadata = {
            "packages": [
                {
                    "id": "worker",
                    "name": "monty-runtime",
                    "version": "1.0.0",
                    "manifest_path": str(manifest),
                    "source": None,
                }
            ],
            "resolve": {"nodes": [{"id": "worker", "deps": []}]},
            "workspace_root": str(self.root),
        }
        inherited = {
            "CARGO_TERM_COLOR": "always",
            "RUSTFLAGS": "--cfg attribution_fixture",
            "CARGO_PROFILE_RELEASE_STRIP": "none",
        }
        with (
            patch.dict(os.environ, inherited),
            patch.object(
                attribution.subprocess,
                "check_output",
                side_effect=[
                    json.dumps(metadata),
                    "0monty-runtime v1.0.0 (" + str(self.root) + ")\n",
                    "monty-runtime v1.0.0 (" + str(self.root) + ")\n",
                ],
            ) as run,
        ):
            attribution.load_graph(
                manifest, "monty-runtime", "aarch64-apple-darwin", True
            )
            for key, value in inherited.items():
                self.assertEqual(os.environ[key], value)
        commands = [c.args[0] for c in run.call_args_list]
        for command in commands:
            self.assertIn("--locked", command)
            self.assertIn("--offline", command)
            self.assertIn("--no-default-features", command)
            self.assertIn("aarch64-apple-darwin", command)
        self.assertIn("--filter-platform", commands[0])
        self.assertIn("normal,build", commands[1])
        self.assertIn("normal,build,no-proc-macro", commands[2])
        self.assertEqual(
            [call.kwargs["cwd"] for call in run.call_args_list], [manifest.parent] * 3
        )
        for call in run.call_args_list:
            for key, value in (inherited | {"CARGO_TERM_COLOR": "never"}).items():
                self.assertEqual(call.kwargs["env"][key], value)
            self.assertEqual(
                call.kwargs["env"]["RUSTUP_TOOLCHAIN"],
                os.environ.get("RUSTUP_TOOLCHAIN", RUNTIME_VERSION),
            )

    def test_cargo_streams_decode_strict_utf8_under_windows_locale(self):
        package = self.package("fixture")
        package.update(source=None, description=NON_ASCII_TEXT)
        manifest = self.root / NON_ASCII_TEXT / "Cargo.toml"
        Path(package["manifest_path"]).parent.rename(manifest.parent)
        package["manifest_path"] = str(manifest)
        metadata = {"packages": [package], "workspace_root": str(manifest.parent)}
        row = f"fixture v1.0.0 ({manifest.parent})\n"
        streams = [
            json.dumps(metadata, ensure_ascii=False).encode("utf-8"),
            ("0" + row).encode("utf-8"),
            row.encode("utf-8"),
        ]
        for invalid_stream in (None, 0, 1, 2):
            outputs = iter(
                INVALID_UTF8 if index == invalid_stream else data
                for index, data in enumerate(streams)
            )
            with (
                self.subTest(invalid_stream=invalid_stream),
                patch.object(subprocess, "_text_encoding", return_value="cp1252"),
                patch.object(
                    attribution.subprocess,
                    "check_output",
                    side_effect=lambda command, outputs=outputs, **kwargs: (
                        captured_output(next(outputs), **kwargs)
                    ),
                ),
            ):
                if invalid_stream is not None:
                    with self.assertRaises(UnicodeDecodeError):
                        attribution.load_graph(manifest, "fixture", "fixture-target")
                else:
                    result = attribution.load_graph(
                        manifest, "fixture", "fixture-target"
                    )
                    self.assertEqual(
                        result["selected"][0]["description"], NON_ASCII_TEXT
                    )
                    self.assertEqual(
                        result["selected"][0]["manifest_path"], str(manifest)
                    )
                    self.assertEqual(result["runtime_ids"], [package["id"]])

    def test_policy_requires_actual_license_text(self):
        with self.assertRaisesRegex(attribution.AttributionError, "evidence"):
            attribution.choose_license("MIT", "MIT")
        self.assertEqual(attribution.choose_license("MIT OR MPL-2.0", MIT), ["MIT"])

    def binary_scope_fixture(self):
        names = [
            "consumer",
            "get-size2",
            "get-size-derive2",
            "attribute-derive",
            "attribute-derive-macro",
            "quote-use",
            "quote-use-macros",
        ]
        packages = [self.package(name) for name in names]
        pins = {}
        for index, package in enumerate(packages):
            macro = package["name"] in {
                "get-size-derive2",
                "attribute-derive-macro",
                "quote-use-macros",
            }
            package["targets"] = [{"kind": ["proc-macro" if macro else "lib"]}]
            root = Path(package["manifest_path"]).parent
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text(
                "excluded-source-sentinel"
                if package["name"].startswith("quote-use")
                else "reviewed template"
            )
            if not package["name"].startswith("quote-use"):
                (root / "LICENSE").write_text(MIT)
            pins[package["name"]] = {
                "version": package["version"],
                "source": package["source"],
                "checksum": str(index) * 64,
                "declared_license": "MIT",
                "proc_macro": macro,
                "reviewed_files": {
                    "src/lib.rs": attribution.digest((root / "src/lib.rs").read_bytes())
                },
                "incoming": [names[index - 1]] if index else [],
            }
        review = {
            "id": "fixture-review",
            "scope": "binary-artifact",
            "reason": "Reviewed host parser only; legal text unresolved.",
            "evidence": ["fixture template review"],
            "excluded_packages": ["quote-use", "quote-use-macros"],
            "chain": {name: pins[name] for name in names[1:]},
            "consumer_profiles": {"consumer": {"consumer": pins["consumer"]}},
        }
        graph = {
            "workspace_root": str(self.root),
            "artifact_root": packages[0],
            "selected": packages,
            "dependency_edges": [
                [names[index - 1], name] for index, name in enumerate(names) if index
            ],
            "runtime_ids": names[:2],
        }
        locked = {
            "package": [
                {
                    "name": p["name"],
                    "version": p["version"],
                    "source": p["source"],
                    "checksum": pins[p["name"]]["checksum"],
                }
                for p in packages
            ]
        }
        return graph, locked, {"binary_scope_review": review}

    def test_binary_scope_exclusion_is_exact_and_retains_inventory(self):
        graph, locked, policy = self.binary_scope_fixture()
        exclusions = attribution.binary_exclusions(
            graph, locked, policy, "binary-artifact"
        )
        self.assertEqual(set(exclusions), {"quote-use", "quote-use-macros"})
        self.assertEqual(len(graph["selected"]), 7)
        for distribution in ("source", "build-cache"):
            with (
                self.subTest(distribution=distribution),
                self.assertRaises(attribution.AttributionError),
            ):
                attribution.binary_exclusions(graph, locked, policy, distribution)

    def test_binary_scope_exclusion_refuses_changed_identity_reach_or_consumer(self):
        graph, locked, policy = self.binary_scope_fixture()
        for change in (
            "version",
            "source",
            "checksum",
            "runtime",
            "consumer_checksum",
            "consumer_version",
            "consumer_source",
            "consumer_edge",
            "macro_consumer",
            "macro_boundary",
            "source_bytes",
        ):
            altered, lock = copy.deepcopy(graph), copy.deepcopy(locked)
            if change in {"version", "source"}:
                altered["selected"][-1][change] = "changed"
            elif change == "checksum":
                lock["package"][-1]["checksum"] = "changed"
            elif change == "runtime":
                altered["runtime_ids"].append("quote-use")
            elif change == "consumer_checksum":
                lock["package"][2]["checksum"] = "changed"
            elif change == "consumer_version":
                altered["selected"][2]["version"] = "changed"
            elif change == "consumer_source":
                altered["selected"][2]["source"] = "changed"
            elif change == "consumer_edge":
                altered["dependency_edges"].append(
                    ["consumer", "attribute-derive-macro"]
                )
            elif change == "macro_consumer":
                consumer = copy.deepcopy(altered["selected"][2])
                consumer.update(id="another-macro", name="another-macro")
                altered["selected"].append(consumer)
                altered["dependency_edges"].append(
                    ["another-macro", "attribute-derive-macro"]
                )
            elif change == "macro_boundary":
                altered["selected"][2]["targets"] = [{"kind": ["lib"]}]
            else:
                (self.root / "attribute-derive-macro/src/lib.rs").write_text("changed")
            with (
                self.subTest(change=change),
                self.assertRaises(attribution.AttributionError),
            ):
                attribution.binary_exclusions(altered, lock, policy, "binary-artifact")

    def test_binary_scope_exclusions_are_visible_without_shipping_excluded_files(self):
        graph, locked, policy = self.binary_scope_fixture()
        (self.root / "LICENSE").write_text(MIT)
        (self.root / "NOTICE").write_text("fixture root notice")
        (self.root / "THIRD_PARTY_LICENSES").mkdir()
        (self.root / "Cargo.lock").write_text(
            "version = 4\n"
            + "".join(
                "\n[[package]]\n"
                + "".join(f"{key} = {json.dumps(value)}\n" for key, value in p.items())
                for p in locked["package"]
            )
        )
        output = self.root / "bundle"
        output.mkdir()
        manifest = attribution.write_bundle(
            output, self.root, [("artifact", graph)], "fixture-target", policy
        )
        self.assertEqual(len(manifest["packages"]), 7)
        self.assertEqual(len(manifest["graphs"][0]["packages"]), 7)
        self.assertEqual(len(manifest["graphs"][0]["binary_scope_exclusions"]), 2)
        self.assertFalse((output / "sources").exists())
        excluded = [
            p
            for p in manifest["packages"]
            if p.get("distribution_status") == "binary-scope-excluded"
        ]
        self.assertEqual(
            {p["name"] for p in excluded}, {"quote-use", "quote-use-macros"}
        )
        for package in excluded:
            self.assertEqual(package["declared_license"], "MIT")
            self.assertEqual(package["license_text_status"], "unresolved")
            self.assertEqual(package["selected_license"], [])
            self.assertFalse((output / "packages" / package["id"]).exists())
            self.assertNotIn("source_archive", package)
        human = (output / "ATTRIBUTION.txt").read_text()
        self.assertIn("BINARY-ARTIFACT SCOPE EXCLUSION", human)
        self.assertIn("unresolved", human)
        self.assertIn("fixture template review", human)
        for path in output.rglob("*"):
            if path.is_file():
                self.assertNotIn(b"excluded-source-sentinel", path.read_bytes())

        second_graph = copy.deepcopy(graph)
        second_graph["runtime_ids"].append("quote-use")
        rejected = self.root / "rejected"
        rejected.mkdir()
        with self.assertRaisesRegex(attribution.AttributionError, "runtime reachable"):
            attribution.write_bundle(
                rejected,
                self.root,
                [("artifact", graph), ("second", second_graph)],
                "fixture-target",
                policy,
            )

    def test_unknown_spdx_is_not_hidden_by_or(self):
        for expression in (
            "MIT OR LicenseRef-Unknown",
            "GPL-3.0-only",
            "MIT garbage",
            "",
        ):
            with (
                self.subTest(expression=expression),
                self.assertRaises(attribution.AttributionError),
            ):
                attribution.choose_license(expression, MIT)

    def test_and_requires_both_licenses(self):
        with self.assertRaisesRegex(attribution.AttributionError, "evidence"):
            attribution.choose_license("MIT AND MPL-2.0", MIT)
        self.assertEqual(
            attribution.choose_license("MIT AND MPL-2.0", MIT + MPL), ["MIT", "MPL-2.0"]
        )

    def test_recursively_collects_subcomponent_notices(self):
        package = self.package()
        root = Path(package["manifest_path"]).parent
        (root / "LICENSE").write_text(MIT)
        (root / "native").mkdir()
        (root / "native" / "NOTICE.txt").write_text("Native attribution")
        self.assertEqual(
            [str(p.relative_to(root)) for p in attribution.license_files(root)],
            ["LICENSE", "native/NOTICE.txt"],
        )

    def test_missing_license_refuses(self):
        package = self.package()
        with self.assertRaisesRegex(attribution.AttributionError, "license"):
            attribution.package_licenses(package, self.root, {})

    def test_symlink_and_parent_escape_refused(self):
        outside = self.root / "secret"
        outside.write_text(MIT)
        package = self.package()
        root = Path(package["manifest_path"]).parent
        (root / "LICENSE").symlink_to(outside)
        with self.assertRaisesRegex(attribution.AttributionError, "symlink"):
            attribution.license_files(root)
        with self.assertRaises(attribution.AttributionError):
            attribution.safe_relative("../secret")
        with self.assertRaises(attribution.AttributionError):
            attribution.safe_relative("/secret")

    def test_source_archive_exact_contents_and_determinism(self):
        source = self.root / "source"
        source.mkdir()
        (source / "LICENSE").write_text(MPL)
        (source / "src").mkdir()
        (source / "src" / "lib.rs").write_bytes(b"// copyright\n\x00\xff")
        one, two = self.root / "one.tar.gz", self.root / "two.tar.gz"
        attribution.source_archive(source, one)
        (source / "src" / "lib.rs").touch()
        attribution.source_archive(source, two)
        self.assertEqual(one.read_bytes(), two.read_bytes())
        with tarfile.open(one) as archive:
            self.assertEqual(archive.getnames(), ["LICENSE", "src/lib.rs"])
            for member in archive:
                extracted = archive.extractfile(member)
                assert extracted is not None
                self.assertEqual(extracted.read(), (source / member.name).read_bytes())
                self.assertEqual(member.mtime, 0)
                self.assertEqual(member.uid, 0)

    def test_source_archive_refuses_symlink(self):
        source = self.root / "source"
        source.mkdir()
        (source / "escape").symlink_to(self.root)
        with self.assertRaisesRegex(attribution.AttributionError, "symlink"):
            attribution.source_archive(source, self.root / "source.tar.gz")

    def test_missing_source_directory_is_not_an_empty_archive(self):
        with self.assertRaisesRegex(attribution.AttributionError, "directory"):
            attribution.source_archive(
                self.root / "missing", self.root / "source.tar.gz"
            )

    def test_source_archive_refuses_own_output_and_preserves_target_named_source(self):
        source = self.root / "source"
        (source / "target").mkdir(parents=True)
        (source / "target" / "lib.rs").write_text("source, not a build cache")
        with self.assertRaises(attribution.AttributionError):
            attribution.source_archive(source, source / "output.tar.gz")
        output = self.root / "source.tar.gz"
        attribution.source_archive(source, output)
        with tarfile.open(output) as archive:
            self.assertEqual(archive.getnames(), ["target/lib.rs"])

    def test_native_comment_notices_preserved_without_code(self):
        source = self.root / "native.c"
        header = b"/* Copyright Native authors\nPermission is hereby granted... */"
        source.write_bytes(
            header + b"\nint value = 42;\n// Licensed under ISC\n// full notice\n"
        )
        notices = dict(attribution.native_notices(self.root))
        self.assertIn(header, notices["native.c"])
        self.assertIn(b"// full notice", notices["native.c"])
        self.assertNotIn(b"int value", notices["native.c"])

    def test_unknown_native_spdx_is_refused(self):
        with self.assertRaisesRegex(attribution.AttributionError, "Unreviewed"):
            attribution.validate_native_licenses(
                b"/* SPDX-License-Identifier: LicenseRef-Unknown */", MIT
            )
        attribution.validate_native_licenses(b"// SPDX-License-Identifier: MIT\n", MIT)
        self.assertEqual(
            attribution.validate_native_licenses(
                b"// SPDX-License-Identifier: MPL-2.0\n", MPL
            ),
            ["MPL-2.0"],
        )
        with self.assertRaisesRegex(attribution.AttributionError, "evidence"):
            attribution.choose_license(
                "MPL-2.0",
                "This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0",
            )

    def test_complete_bundle_deterministic_worker_mpl_and_manifest_hashes(self):
        (self.root / "LICENSE").write_text(MIT)
        (self.root / "NOTICE.md").write_text("Root attribution")
        (self.root / "THIRD_PARTY_LICENSES").mkdir()
        (self.root / "THIRD_PARTY_LICENSES" / "Inherited.txt").write_text(MIT)
        (self.root / "Cargo.lock").write_text("version = 4\n")
        app = self.package("app")
        worker = self.package("worker", "MPL-2.0")
        for package, license_text in ((app, MIT), (worker, MPL)):
            Path(package["manifest_path"]).with_name("LICENSE").write_text(license_text)
        graphs = [
            (
                label,
                {
                    "workspace_root": str(self.root),
                    "artifact_root": package,
                    "selected": [package],
                },
            )
            for label, package in (("artifact", app), ("worker", worker))
        ]
        bundles = [self.root / name for name in ("one", "two")]
        for output in bundles:
            output.mkdir()
            attribution.write_bundle(output, self.root, graphs, "fixture-target", {})
        self.assertEqual(
            {
                p.relative_to(bundles[0]): p.read_bytes()
                for p in bundles[0].rglob("*")
                if p.is_file()
            },
            {
                p.relative_to(bundles[1]): p.read_bytes()
                for p in bundles[1].rglob("*")
                if p.is_file()
            },
        )
        manifest = json.loads((bundles[0] / "manifest.json").read_text())
        self.assertNotIn(str(self.root), json.dumps(manifest))
        self.assertEqual(
            [g["default_features"] for g in manifest["graphs"]], [True, False]
        )
        self.assertEqual(len(list((bundles[0] / "sources").iterdir())), 1)
        for entry in manifest["files"]:
            self.assertEqual(
                entry["sha256"],
                attribution.digest((bundles[0] / entry["path"]).read_bytes()),
            )
        self.assertEqual((bundles[0] / "NOTICE").read_text(), "Root attribution")

    @unittest.skipUnless(
        shutil.which("git"), "Git needed for checkout integration fixture"
    )
    def test_autocrlf_checkout_preserves_pinned_license_bytes(self):
        repository = Path(__file__).resolve().parent.parent
        checkout = self.root / "checkout"
        checkout.mkdir()
        shutil.copyfile(repository / ".gitattributes", checkout / ".gitattributes")
        shutil.copytree(
            repository / "THIRD_PARTY_LICENSES", checkout / "THIRD_PARTY_LICENSES"
        )
        canonical_crlf = checkout / "THIRD_PARTY_LICENSES" / "canonical-crlf.txt"
        canonical_crlf.write_bytes(MIT.replace("\n", "\r\n").encode())
        control = checkout / "control.txt"
        control.write_bytes(MIT.encode())
        git = [
            "git",
            "-c",
            "core.autocrlf=true",
            "-c",
            "core.safecrlf=false",
            "-c",
            f"core.attributesFile={os.devnull}",
        ]
        env = {
            **os.environ,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        for args in (["init", "--quiet"], ["add", "."]):
            subprocess.run(
                git + args, cwd=checkout, env=env, check=True, capture_output=True
            )
        shutil.rmtree(checkout / "THIRD_PARTY_LICENSES")
        control.unlink()
        subprocess.run(
            git + ["checkout-index", "--all", "--force"],
            cwd=checkout,
            env=env,
            check=True,
            capture_output=True,
        )
        self.assertEqual(control.read_bytes(), MIT.replace("\n", "\r\n").encode())
        self.assertEqual(
            canonical_crlf.read_bytes(), MIT.replace("\n", "\r\n").encode()
        )
        policy = attribution.load_policy(checkout)
        entries = [
            entry
            for package in policy["packages"].values()
            for entry in package.get("files", [])
        ]
        entries += [
            notice
            for source in policy["git_sources"].values()
            for notice in source["notices"]
            if "path" in notice
        ]
        self.assertTrue(entries)
        for entry in entries:
            with self.subTest(path=entry["path"]):
                data = attribution.regular_file(
                    attribution.confined_file(checkout, entry["path"])
                )
                self.assertEqual(attribution.digest(data), entry["sha256"])

    def test_supplement_rejects_changed_line_endings(self):
        package = self.package()
        license_path = self.root / "supplement.txt"
        license_path.write_bytes(MIT.encode())
        policy = {
            "packages": {
                "dependency@1.0.0": {
                    "files": [
                        {
                            "path": "supplement.txt",
                            "sha256": attribution.digest(MIT.encode()),
                        }
                    ]
                }
            }
        }
        self.assertEqual(
            attribution.package_licenses(package, self.root, policy)[2], ["MIT"]
        )
        license_path.write_bytes(MIT.replace("\n", "\r\n").encode())
        with self.assertRaisesRegex(attribution.AttributionError, "hash mismatch"):
            attribution.package_licenses(package, self.root, policy)

    def test_supplement_requires_matching_hash(self):
        package = self.package()
        license_path = self.root / "supplement.txt"
        license_path.write_text(MIT)
        policy = {
            "packages": {
                "dependency@1.0.0": {
                    "files": [{"path": "supplement.txt", "sha256": "wrong"}]
                }
            }
        }
        with self.assertRaisesRegex(attribution.AttributionError, "hash mismatch"):
            attribution.package_licenses(package, self.root, policy)

    def test_supplement_parent_symlink_refused(self):
        package = self.package()
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "LICENSE").write_text(MIT)
        (self.root / "link").symlink_to(outside, target_is_directory=True)
        policy = {
            "packages": {
                "dependency@1.0.0": {
                    "files": [
                        {
                            "path": "link/LICENSE",
                            "sha256": attribution.digest(MIT.encode()),
                        }
                    ]
                }
            }
        }
        with self.assertRaisesRegex(attribution.AttributionError, "symlink"):
            attribution.package_licenses(package, self.root, policy)

    def test_external_worker_workspace_inherits_actual_license_not_repository_license(
        self,
    ):
        package = self.package("worker")
        package["source"] = None
        package["workspace_root"] = str(self.root)
        (self.root / "LICENSE").write_text(MIT)
        repository = self.root / "repository"
        repository.mkdir()
        (repository / "LICENSE").write_text("Different license")
        notices, expression, chosen = attribution.package_licenses(
            package, repository, {}
        )
        self.assertEqual(chosen, ["MIT"])
        self.assertEqual(expression, "MIT")
        self.assertIn((self.root / "LICENSE", "workspace/LICENSE"), notices)

    def test_vendored_git_uses_only_pinned_package_and_license_evidence(self):
        package = self.package()
        package["source"] = "git+https://example.invalid/source#" + "a" * 40
        manifest = Path(package["manifest_path"])
        manifest.write_text(
            '[package]\nname = "dependency"\nversion = { workspace = true }\n'
        )
        original = manifest.read_bytes()
        policy = {
            "git_sources": {
                package["source"]: {
                    "packages": {
                        "dependency@1.0.0": [
                            attribution.digest(manifest.read_bytes()),
                            attribution.digest(b"normalized vendored manifest"),
                        ]
                    },
                    "notices": [
                        {
                            "name": "upstream/LICENSE",
                            "text": MIT,
                            "sha256": attribution.digest(MIT.encode()),
                        }
                    ],
                }
            }
        }
        notices, _, selected = attribution.package_licenses(package, self.root, policy)
        self.assertEqual(selected, ["MIT"])
        self.assertIn((MIT.encode(), "upstream/LICENSE"), notices)
        manifest.write_bytes(b"normalized vendored manifest")
        self.assertEqual(
            attribution.package_licenses(package, self.root, policy)[2], ["MIT"]
        )
        manifest.write_text("unverified manifest")
        with self.assertRaisesRegex(attribution.AttributionError, "manifest"):
            attribution.package_licenses(package, self.root, policy)
        resolved = original.replace(b"{ workspace = true }", b'"1.0.0"')
        manifest.write_bytes(resolved)
        with self.assertRaisesRegex(attribution.AttributionError, "manifest"):
            attribution.package_licenses(package, self.root, policy)
        preserved = manifest.with_name("Cargo.toml.orig")
        preserved.write_bytes(original)
        self.assertEqual(
            attribution.package_licenses(package, self.root, policy)[2], ["MIT"]
        )
        self.assertEqual(manifest.read_bytes(), resolved)
        preserved.write_bytes(original + b"\ntampered")
        with self.assertRaisesRegex(attribution.AttributionError, "manifest"):
            attribution.package_licenses(package, self.root, policy)
        preserved.unlink()
        original_path = self.root / "original-manifest"
        original_path.write_bytes(original)
        preserved.symlink_to(original_path)
        with self.assertRaisesRegex(attribution.AttributionError, "symlink"):
            attribution.package_licenses(package, self.root, policy)

    def test_unreviewed_vendored_git_never_guesses_parent_license(self):
        package = self.package()
        package["source"] = "git+https://example.invalid/source#" + "b" * 40
        (self.root / "LICENSE").write_text(MIT)
        with self.assertRaisesRegex(attribution.AttributionError, "git source"):
            attribution.package_licenses(package, self.root, {})

    def test_pinned_workspace_without_git_retains_typeshed_supplement(self):
        package = self.package("monty-typeshed")
        package["source"] = None
        package["workspace_root"] = str(self.root)
        root = Path(package["manifest_path"]).parent
        (root / "source_commit.txt").write_text("typeshed revision")
        (self.root / "LICENSE").write_text(MIT)
        repository = self.root / "repository"
        repository.mkdir()
        (repository / "typeshed-license").write_text(MIT)
        pinned = {
            "revision": "a" * 40,
            "tree_sha256": attribution.workspace_digest(self.root),
        }
        policy = {
            "workspace_sources": [pinned],
            "packages": {
                "monty-typeshed@1.0.0": {
                    "provenance": {
                        "source": "registry+https://example.invalid",
                        "declared_license": "MIT",
                        "cargo_vcs_info": {
                            "git": {"sha1": "a" * 40},
                            "path_in_vcs": "monty-typeshed",
                        },
                    },
                    "files": [
                        {
                            "path": "typeshed-license",
                            "sha256": attribution.digest(MIT.encode()),
                        }
                    ],
                    "embedded_components": [
                        {
                            "name": "typeshed",
                            "revision_evidence": {
                                "package_path": "source_commit.txt",
                                "sha256": attribution.digest(b"typeshed revision"),
                            },
                        }
                    ],
                    "required_files": ["supplements/typeshed-license"],
                }
            },
        }
        package["verified_workspace"] = attribution.verify_workspace(self.root, policy)
        notices, _, _ = attribution.package_licenses(package, repository, policy)
        self.assertIn(
            (repository / "typeshed-license", "supplements/typeshed-license"), notices
        )
        (root / "source_commit.txt").write_text("changed")
        with self.assertRaisesRegex(attribution.AttributionError, "revision"):
            attribution.package_licenses(package, repository, policy)
        with self.assertRaisesRegex(attribution.AttributionError, "workspace"):
            attribution.verify_workspace(self.root, policy)

    def test_failed_cli_removes_partial_bundle_and_preserves_existing_output(self):
        output = self.root / "output"
        arguments = [
            "build-attribution.py",
            "--manifest-path",
            str(self.root / "Cargo.toml"),
            "--package",
            "caudra",
            "--target",
            "fixture-target",
            "--worker-manifest-path",
            str(self.root / "worker/Cargo.toml"),
            "--worker-package",
            "monty-runtime",
            "--output-dir",
            str(output),
        ]
        with (
            patch.object(attribution.sys, "argv", arguments),
            patch.object(attribution, "load_policy", return_value={}),
            patch.object(attribution, "load_graph", return_value={}),
            patch.object(
                attribution,
                "write_bundle",
                side_effect=attribution.AttributionError("missing license"),
            ),
            patch.object(attribution.sys, "stderr"),
        ):
            self.assertEqual(attribution.main(), 1)
        self.assertFalse(output.exists())
        self.assertEqual(list(self.root.iterdir()), [])
        output.mkdir()
        marker = output / "existing"
        marker.write_text("preserve")
        with (
            patch.object(attribution.sys, "argv", arguments),
            patch.object(attribution.sys, "stderr"),
        ):
            self.assertEqual(attribution.main(), 1)
        self.assertEqual(marker.read_text(), "preserve")

    @unittest.skipUnless(
        shutil.which("cargo"), "Cargo needed for graph integration fixture"
    )
    def test_worker_cargo_discovers_its_own_config_outside_current_directory(self):
        for name in ("worker", "config-only-dependency"):
            directory = self.root / name
            (directory / "src").mkdir(parents=True)
            (directory / "src/lib.rs").write_text("")
            (directory / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "1.0.0"\n'
            )
        worker = self.root / "worker"
        with (worker / "Cargo.toml").open("a") as manifest:
            manifest.write('[dependencies]\nconfig-only-dependency = "=1.0.0"\n')
        (worker / ".cargo").mkdir()
        (worker / ".cargo/config.toml").write_text(
            "[patch.crates-io]\nconfig-only-dependency = { path = "
            + json.dumps(str(self.root / "config-only-dependency"))
            + " }\n"
        )
        subprocess.run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=worker,
            check=True,
            capture_output=True,
        )
        graph = attribution.load_graph(
            worker / "Cargo.toml", "worker", "x86_64-unknown-linux-gnu", True
        )
        self.assertEqual(
            {p["name"] for p in graph["selected"]}, {"worker", "config-only-dependency"}
        )

    @unittest.skipUnless(
        shutil.which("cargo"), "Cargo needed for graph integration fixture"
    )
    def test_real_cargo_target_and_feature_selection(self):
        names = (
            "app",
            "normal",
            "builder",
            "development",
            "windows",
            "optional",
            "other",
            "macro-host",
            "host-helper",
            "shared",
            "leaf",
        )
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nresolver = "2"\nmembers = ' + json.dumps(names) + "\n"
        )
        for name in names:
            crate = self.root / name
            (crate / "src").mkdir(parents=True)
            (crate / "src/lib.rs").write_text("")
            (crate / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "1.0.0"\n'
            )
        with (self.root / "app/Cargo.toml").open("a") as file:
            file.write("""[dependencies]
normal = { path = "../normal" }
shared = { path = "../shared" }
macro-host = { path = "../macro-host" }
[build-dependencies]
builder = { path = "../builder" }
[dev-dependencies]
development = { path = "../development" }
normal = { path = "../normal", features = ["extra"] }
[target.'cfg(windows)'.dependencies]
windows = { path = "../windows" }
""")
        with (self.root / "normal/Cargo.toml").open("a") as file:
            file.write("""[dependencies]
shared = { path = "../shared" }
optional = { path = "../optional", optional = true }
[features]
extra = ["dep:optional"]
""")
        with (self.root / "other/Cargo.toml").open("a") as file:
            file.write(
                '[dependencies]\nnormal = { path = "../normal", features = ["extra"] }\n'
            )
        with (self.root / "macro-host/Cargo.toml").open("a") as file:
            file.write(
                '[lib]\nproc-macro = true\n[dependencies]\nhost-helper = { path = "../host-helper" }\n'
            )
        with (self.root / "shared/Cargo.toml").open("a") as file:
            file.write('[dependencies]\nleaf = { path = "../leaf" }\n')
        manifest = self.root / "Cargo.toml"
        subprocess.run(
            [
                "cargo",
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                str(manifest),
            ],
            check=True,
            capture_output=True,
        )
        outputs = []
        check_output = subprocess.check_output

        def query(*args, **kwargs):
            output = check_output(*args, **kwargs)
            outputs.append(output)
            return output

        for target, expected in (
            ("x86_64-unknown-linux-gnu", {"app", "normal", "builder"}),
            ("aarch64-apple-darwin", {"app", "normal", "builder"}),
            ("x86_64-pc-windows-msvc", {"app", "normal", "builder", "windows"}),
        ):
            with (
                self.subTest(target=target),
                patch.dict(os.environ, {"CARGO_TERM_COLOR": "always"}),
            ):
                outputs.clear()
                with patch.object(
                    attribution.subprocess, "check_output", side_effect=query
                ):
                    graph = attribution.load_graph(manifest, "app", target)
                self.assertEqual(len(outputs), 3)
                for tree in outputs[1:]:
                    self.assertIn(" (*)", tree)
                    self.assertNotIn("\x1b", tree)
                expected |= {"shared", "leaf"}
                self.assertEqual(
                    {p["name"] for p in graph["selected"]},
                    expected | {"macro-host", "host-helper"},
                )
                self.assertEqual(
                    {
                        p["name"]
                        for p in graph["selected"]
                        if p["id"] in graph["runtime_ids"]
                    },
                    expected,
                )


class RustRuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = self.root / "project/Cargo.toml"
        self.manifest.parent.mkdir()
        self.manifest.write_text("")
        (self.root / "rust-toolchain.toml").write_text(
            f'[toolchain]\nchannel = "{RUNTIME_VERSION}"\n'
        )
        self.sysroot = self.root / "sysroot"
        self.docs = self.sysroot / "share/doc/rust"
        (self.docs / "licenses").mkdir(parents=True)
        (self.docs / "COPYRIGHT-library.html").write_bytes(RUNTIME_REPORT)
        (self.docs / "COPYRIGHT.html").write_text("COMPILER ONLY - DO NOT COPY")
        (self.docs / "licenses/MIT.txt").write_text(MIT)
        (self.docs / "licenses/Apache-2.0.txt").write_text("Apache license fixture")
        self.policy = {
            "rust_runtime": {"release": RUNTIME_VERSION, "commit_hash": RUNTIME_COMMIT}
        }
        self.version = f"rustc {RUNTIME_VERSION}\nrelease: {RUNTIME_VERSION}\ncommit-hash: {RUNTIME_COMMIT}\nhost: fixture-host\n"

    def test_pinned_toolchain_is_preserved_across_manifest_directories(self):
        def compiler(command, *, text, cwd, env, encoding, errors):
            self.assertTrue(text)
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], RUNTIME_VERSION)
            self.assertIn(cwd, [self.manifest.parent, self.root / "worker"])
            return self.version if command == ["rustc", "-vV"] else str(self.sysroot)

        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(attribution.subprocess, "check_output", side_effect=compiler),
        ):
            for manifest in (self.manifest, self.root / "worker/Cargo.toml"):
                runtime = attribution.discover_rust_runtime(
                    manifest, self.root, self.policy
                )
                self.assertEqual(runtime["release"], RUNTIME_VERSION)
            self.assertNotIn("RUSTUP_TOOLCHAIN", os.environ)

    def test_explicit_conflicting_toolchain_is_not_silently_replaced(self):
        override = "1.98.0"

        def compiler(command, *, text, cwd, env, encoding, errors):
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], override)
            return self.version.replace(RUNTIME_VERSION, override)

        with (
            patch.dict(os.environ, {"RUSTUP_TOOLCHAIN": override}),
            patch.object(attribution.subprocess, "check_output", side_effect=compiler),
            self.assertRaisesRegex(attribution.AttributionError, "toolchain mismatch"),
        ):
            attribution.discover_rust_runtime(self.manifest, self.root, self.policy)

    def discover(self, version=None):
        with patch.object(
            attribution.subprocess,
            "check_output",
            side_effect=[
                self.version if version is None else version,
                str(self.sysroot) + "\n",
            ],
        ) as run:
            result = attribution.discover_rust_runtime(
                self.manifest, self.root, self.policy
            )
        self.assertEqual(
            [call.kwargs["cwd"] for call in run.call_args_list],
            [self.manifest.parent] * 2,
        )
        self.assertEqual(run.call_args_list[0].args[0], ["rustc", "-vV"])
        self.assertEqual(run.call_args_list[1].args[0], ["rustc", "--print", "sysroot"])
        return result

    def test_rustc_streams_decode_strict_utf8_under_windows_locale(self):
        sysroot = self.sysroot.with_name(NON_ASCII_TEXT)
        self.sysroot.rename(sysroot)
        streams = [
            (self.version + f"fixture: {NON_ASCII_TEXT}\n").encode("utf-8"),
            (str(sysroot) + "\n").encode("utf-8"),
        ]
        for invalid_stream in (None, 0, 1):
            outputs = iter(
                INVALID_UTF8 if index == invalid_stream else data
                for index, data in enumerate(streams)
            )
            with (
                self.subTest(invalid_stream=invalid_stream),
                patch.object(subprocess, "_text_encoding", return_value="cp1252"),
                patch.object(
                    attribution.subprocess,
                    "check_output",
                    side_effect=lambda command, outputs=outputs, **kwargs: (
                        captured_output(next(outputs), **kwargs)
                    ),
                ),
            ):
                if invalid_stream is not None:
                    with self.assertRaises(UnicodeDecodeError):
                        attribution.discover_rust_runtime(
                            self.manifest, self.root, self.policy
                        )
                else:
                    result = attribution.discover_rust_runtime(
                        self.manifest, self.root, self.policy
                    )
                    self.assertEqual(result["release"], RUNTIME_VERSION)
                    self.assertEqual(
                        result["files"]["COPYRIGHT-library.html"], RUNTIME_REPORT
                    )

    def test_runtime_missing_empty_or_wrong_report_refuses(self):
        report = self.docs / "COPYRIGHT-library.html"
        report.unlink()
        with self.assertRaisesRegex(attribution.AttributionError, "Rust runtime"):
            self.discover()
        for data in (b"", b" \n", b"compiler-only copyright"):
            report.write_bytes(data)
            with (
                self.subTest(data=data),
                self.assertRaisesRegex(attribution.AttributionError, "Rust runtime"),
            ):
                self.discover()

    def test_runtime_version_commit_and_repository_pin_must_match(self):
        for version in (
            "",
            self.version.replace(RUNTIME_VERSION, "1.98.0"),
            self.version.replace(RUNTIME_COMMIT, "a" * 40),
        ):
            with (
                self.subTest(version=version),
                self.assertRaisesRegex(attribution.AttributionError, "Rust runtime"),
            ):
                self.discover(version)
        (self.root / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.98.0"\n'
        )
        with self.assertRaisesRegex(attribution.AttributionError, "Rust runtime"):
            self.discover()

    def test_runtime_links_cannot_escape_and_required_license_text_cannot_be_missing(
        self,
    ):
        for link in ("../secret", "licenses/missing.txt"):
            (self.docs / "COPYRIGHT-library.html").write_bytes(
                RUNTIME_REPORT.replace(b"licenses/MIT.txt", link.encode())
            )
            with (
                self.subTest(link=link),
                self.assertRaises(attribution.AttributionError),
            ):
                self.discover()
        (self.docs / "COPYRIGHT-library.html").write_bytes(RUNTIME_REPORT)
        (self.docs / "licenses/MIT.txt").write_bytes(b"")
        with self.assertRaisesRegex(attribution.AttributionError, "Rust runtime"):
            self.discover()

    def test_runtime_nix_symlinks_preserve_exact_bytes_and_deterministic_hashes(self):
        store = self.root / "nix-store-docs"
        self.docs.rename(store)
        self.docs.symlink_to(store, target_is_directory=True)
        license_store = self.root / "nix-store-license"
        (store / "licenses/MIT.txt").rename(license_store)
        (store / "licenses/MIT.txt").symlink_to(license_store)
        data = self.discover()
        self.assertEqual(data["files"]["COPYRIGHT-library.html"], RUNTIME_REPORT)
        self.assertEqual(data["files"]["licenses/MIT.txt"], MIT.encode())
        self.assertNotIn("COPYRIGHT.html", data["files"])
        graphs = [
            (label, {"manifest_path": str(self.manifest)})
            for label in ("artifact", "worker")
        ]
        snapshots = []
        for name in ("one", "two"):
            output = self.root / name
            output.mkdir()
            with patch.object(attribution, "discover_rust_runtime", return_value=data):
                records, emitted = attribution.include_rust_runtime(
                    output, self.root, graphs, self.policy
                )
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["graphs"], ["artifact", "worker"])
            self.assertEqual(records[0]["commit_hash"], RUNTIME_COMMIT)
            self.assertNotIn(str(self.root), json.dumps(records))
            for file in emitted:
                self.assertEqual(
                    file["sha256"],
                    attribution.digest((output / file["path"]).read_bytes()),
                )
            snapshots.append(
                {
                    p.relative_to(output): p.read_bytes()
                    for p in output.rglob("*")
                    if p.is_file()
                }
            )
        self.assertEqual(snapshots[0], snapshots[1])


if __name__ == "__main__":
    unittest.main()
