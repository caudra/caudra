import argparse
import hashlib
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

PACKAGE = "monty-runtime"
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


def cached(
    worker: Path, symbols: Path, stamp: Path, expected: str, reported: str
) -> bool:
    try:
        return (
            stamp.read_bytes() == expected.encode()
            and symbols.is_file()
            and worker_version(worker) == reported
        )
    except OSError:
        return False


def build(version: str, root: Path, target_dir: Path, force: bool) -> Path:
    reported = f"{PACKAGE} {version}"
    toolchain = subprocess.check_output(["rustc", "-vV"], text=True)
    target = next(
        line.removeprefix("host: ")
        for line in toolchain.splitlines()
        if line.startswith("host: ")
    )
    worker = root / "bin" / "monty"
    symbols = root / "symbols" / "bin" / "monty"
    stamp = root / "build-fingerprint"
    expected = fingerprint(toolchain, target)
    if not force and cached(worker, symbols, stamp, expected, reported):
        return worker

    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="worker-", dir=root) as directory:
        staging = Path(directory)
        subprocess.run(
            [
                "cargo",
                "install",
                PACKAGE,
                "--version",
                f"={version}",
                "--locked",
                "--no-default-features",
                "--force",
                "--message-format=json-render-diagnostics",
                "--root",
                str(staging / "symbols"),
                "--target-dir",
                str(target_dir),
                "--target",
                target,
            ],
            env=os.environ | PROFILE,
            stdout=subprocess.DEVNULL,
            check=True,
        )
        staged_symbols = staging / "symbols" / "bin" / "monty"
        staged = staging / "monty"
        shutil.copy2(staged_symbols, staged)
        subprocess.run(["strip", str(staged)], check=True)
        if worker_version(staged) != reported:
            raise RuntimeError(f"Staged worker must report {reported}")
        staged_stamp = staging / stamp.name
        staged_stamp.write_text(expected)
        worker.parent.mkdir(parents=True, exist_ok=True)
        symbols.parent.mkdir(parents=True, exist_ok=True)
        stamp.unlink(missing_ok=True)
        staged_symbols.replace(symbols)
        staged.replace(worker)
        staged_stamp.replace(stamp)
    return worker


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--root", type=Path, default=ROOT / "target" / "code-worker")
    parser.add_argument(
        "--target-dir", type=Path, default=ROOT / "target" / "code-worker-build"
    )
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()
    worker = build(
        args.version, args.root.resolve(), args.target_dir.resolve(), args.force
    )
    print(f"{PACKAGE} {args.version}: {worker}")


if __name__ == "__main__":
    main()
