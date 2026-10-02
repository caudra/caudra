import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

VERSION = "0.0.21"
EXPECTED_VERSION = f"monty-runtime {VERSION}"
ROOT = Path(__file__).resolve().parent.parent
PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
    "CARGO_PROFILE_RELEASE_LTO": "thin",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_PANIC": "unwind",
}


def worker_version(worker: Path) -> str | None:
    try:
        return subprocess.check_output(
            [str(worker), "--version"],
            text=True,
            stderr=subprocess.STDOUT,
            timeout=30,
        ).strip()
    except (OSError, subprocess.SubprocessError):
        return None


def fingerprint(toolchain: str, target: str) -> str:
    digest = hashlib.sha256(Path(__file__).read_bytes())
    for value in (
        toolchain,
        target,
        os.environ.get("RUSTFLAGS", ""),
        os.environ.get("CARGO_ENCODED_RUSTFLAGS", ""),
        os.environ.get(
            f"CARGO_TARGET_{target.upper().replace('-', '_')}_RUSTFLAGS", ""
        ),
    ):
        digest.update(b"\0")
        digest.update(value.encode())
    return digest.hexdigest()


def cached(worker: Path, symbols: Path, stamp: Path, expected: str) -> bool:
    try:
        return (
            json.loads(stamp.read_text())
            == {"fingerprint": expected, "pdb": symbols.with_suffix(".pdb").is_file()}
            and symbols.is_file()
            and worker_version(worker) == EXPECTED_VERSION
        )
    except (OSError, json.JSONDecodeError):
        return False


def build(target: str, root: Path, target_dir: Path, force: bool) -> Path:
    toolchain = subprocess.check_output(["rustc", "-vV"], text=True)
    if not target:
        target = next(
            line.removeprefix("host: ")
            for line in toolchain.splitlines()
            if line.startswith("host: ")
        )
    name = "monty.exe" if "windows" in target else "monty"
    worker = root / "bin" / name
    symbols = root / "symbols" / "bin" / name
    stamp = root / "build-fingerprint"
    expected = fingerprint(toolchain, target)
    if not force and cached(worker, symbols, stamp, expected):
        return worker

    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="worker-", dir=root) as directory:
        symbols_root = Path(directory) / "symbols"
        cargo = subprocess.run(
            [
                "cargo",
                "install",
                "monty-runtime",
                "--version",
                f"={VERSION}",
                "--locked",
                "--no-default-features",
                "--force",
                "--message-format=json-render-diagnostics",
                "--root",
                str(symbols_root),
                "--target-dir",
                str(target_dir),
                "--target",
                target,
            ],
            env=os.environ | PROFILE,
            stdout=subprocess.PIPE,
            text=True,
            check=True,
        )
        staged_symbols = symbols_root / "bin" / name
        staged_pdb = staged_symbols.with_suffix(".pdb")
        for line in cargo.stdout.splitlines():
            artifact = json.loads(line)
            if (
                artifact.get("reason") == "compiler-artifact"
                and artifact["target"]["name"] == "monty"
            ):
                for filename in artifact["filenames"]:
                    if Path(filename).suffix == ".pdb":
                        shutil.copy2(filename, staged_pdb)
        staged = Path(directory) / name
        shutil.copy2(staged_symbols, staged)
        if "windows" not in target:
            subprocess.run(["strip", str(staged)], check=True)
        if worker_version(staged) != EXPECTED_VERSION:
            raise RuntimeError(f"Bundled worker must report {EXPECTED_VERSION}")
        staged_stamp = Path(directory) / stamp.name
        staged_stamp.write_text(
            json.dumps({"fingerprint": expected, "pdb": staged_pdb.is_file()})
        )
        worker.parent.mkdir(parents=True, exist_ok=True)
        symbols.parent.mkdir(parents=True, exist_ok=True)
        stamp.unlink(missing_ok=True)
        staged_symbols.replace(symbols)
        pdb = symbols.with_suffix(".pdb")
        if staged_pdb.is_file():
            staged_pdb.replace(pdb)
        else:
            pdb.unlink(missing_ok=True)
        staged.replace(worker)
        staged_stamp.replace(stamp)
    return worker


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", default="")
    parser.add_argument("--root", type=Path, default=ROOT / "target" / "code-worker")
    parser.add_argument(
        "--target-dir", type=Path, default=ROOT / "target" / "code-worker-build"
    )
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()
    worker = build(
        args.target, args.root.resolve(), args.target_dir.resolve(), args.force
    )
    print(f"{EXPECTED_VERSION}: {worker}")


if __name__ == "__main__":
    main()
