#!/usr/bin/env python3
"""Regenerates the committed progress fixtures from real tools.

Reductions are written against captured output, never a remembered format, so
the fixtures under `crates/output-filter/tests/fixtures/progress` are produced by
running the generators in `progress/` and recording the exact bytes each stream
emitted. Run this to refresh them:

    python3 crates/output-filter/evals/capture-progress.py

The report is the measurement. Most progress libraries consult `isatty` and draw
nothing into a pipe, so they cannot produce redraw noise and need no support;
some do not, and those are the cases that matter. Assuming either way is how a
real case gets missed, so every library here is built and run rather than
described.

A generator whose library, toolchain, or network is unavailable is reported and
skipped. Its fixture is left alone, because a partial refresh that mixes stale
and fresh captures is worse than no refresh.
"""

import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
GEN = os.path.join(HERE, "progress")
OUT = os.path.abspath(os.path.join(HERE, "..", "tests", "fixtures", "progress"))

# Generators in a compiled language, built on demand. Each takes a real
# dependency on the library it measures, which is why `progress/` is excluded
# from the cargo workspace.
BUILDS = {
    "rust-indicatif": (
        ["cargo", "build", "--quiet", "--release"],
        "rust_indicatif",
        "rust_indicatif/target/release/indicatif-generator",
    ),
    "go-progressbar": (
        ["go", "build", "-o", "generator", "."],
        "go_progressbar",
        "go_progressbar/generator",
    ),
    "go-mpb": (["go", "build", "-o", "generator", "."], "go_mpb", "go_mpb/generator"),
    # Node resolves ESM imports from the importing file's directory and ignores
    # NODE_PATH, so the libraries are installed beside the generators rather than
    # relied on from a global prefix.
    "node": (["npm", "install", "--no-fund", "--no-audit", "--silent"], ".", None),
}

# name -> (argv or build key, stream). The stream names the pipe the progress
# lands on, which is the one captured; the other is discarded so a fixture stays
# single-purpose.
GENERATORS = [
    ("tqdm-stderr", [sys.executable, f"{GEN}/tqdm_stderr.py", "400"], "stderr"),
    ("tqdm-stdout", [sys.executable, f"{GEN}/tqdm_stdout.py", "300"], "stdout"),
    ("tqdm-nested", [sys.executable, f"{GEN}/tqdm_nested.py"], "stderr"),
    ("rich-progress", [sys.executable, f"{GEN}/rich_progress.py"], "stdout"),
    ("bare-cr", [sys.executable, f"{GEN}/bare_cr.py"], "stdout"),
    ("bare-cr-erase", [sys.executable, f"{GEN}/bare_cr_erase.py"], "stderr"),
    ("newline-frames", [sys.executable, f"{GEN}/newline_frames.py"], "stdout"),
    ("numeric-table", [sys.executable, f"{GEN}/numeric_table.py"], "stdout"),
    ("repeated-diagnostic", [sys.executable, f"{GEN}/repeated_diagnostic.py"], "stderr"),
    ("cr-only-records", [sys.executable, f"{GEN}/cr_only_records.py"], "stdout"),
    (
        "curl-download",
        ["curl", "-o", os.devnull, "https://registry.npmjs.org/express"],
        "stderr",
    ),
    ("rust-indicatif", "rust-indicatif", "stderr"),
    ("go-progressbar", "go-progressbar", "stdout"),
    ("go-mpb", "go-mpb", "stdout"),
    ("node-cli-progress", ("node", ["node", f"{GEN}/node_cli_progress.js"]), "stdout"),
    ("node-ora", ("node", ["node", f"{GEN}/node_ora.mjs"]), "stderr"),
]


class Unavailable(Exception):
    """The generator could not be run, as opposed to running and printing nothing."""


def reason(completed):
    """The most useful line of a failure, for the skip report."""
    detail = completed.stderr.decode("utf-8", "replace").strip().splitlines()
    interesting = [
        line
        for line in detail
        if line.strip() and not line.startswith((" ", "\t", "^", "Did you mean"))
    ]
    return (interesting[-1] if interesting else "unknown")[:70]


_BUILT = set()


def build(key):
    """Builds or installs a generator's dependencies once per run."""
    command, directory, binary = BUILDS[key]
    if key not in _BUILT:
        if shutil.which(command[0]) is None:
            raise Unavailable(f"{command[0]} not installed")
        done = subprocess.run(
            command,
            cwd=os.path.join(GEN, directory),
            capture_output=True,
            timeout=1800,
            check=False,
        )
        if done.returncode != 0:
            raise Unavailable(f"build failed: {reason(done)}")
        _BUILT.add(key)
    return [os.path.join(GEN, binary)] if binary else None


def capture(argv, stream):
    """Runs a generator with both streams as pipes and returns the chosen one."""
    if isinstance(argv, str):
        argv = build(argv)
    elif isinstance(argv, tuple):
        key, argv = argv
        build(key)
    if shutil.which(argv[0]) is None and not os.path.exists(argv[0]):
        raise Unavailable(f"{argv[0]} not installed")
    try:
        done = subprocess.run(argv, capture_output=True, timeout=900, check=False)
    except OSError as error:
        raise Unavailable(str(error)) from error
    # A generator that fails did not measure anything. Writing its diagnostic as
    # a fixture would be worse than writing nothing, because it would look like
    # a capture of the library.
    if done.returncode != 0:
        raise Unavailable(f"exit {done.returncode}: {reason(done)}")
    return done.stderr if stream == "stderr" else done.stdout


def main():
    os.makedirs(OUT, exist_ok=True)
    print(f"{'fixture':<22}{'bytes':>8}{'CR':>7}{'LF':>7}{'ESC':>6}  form")
    print("-" * 68)
    written = 0
    silent = []
    skipped = []
    for name, argv, stream in GENERATORS:
        try:
            data = capture(argv, stream)
        except Unavailable as reason:
            skipped.append(f"{name} ({reason})")
            print(f"{name:<22}{'-':>8}{'-':>7}{'-':>7}{'-':>6}  skipped")
            continue
        if not data:
            # A library that consults isatty and stays quiet on a pipe cannot
            # produce redraw noise, so it needs no support and no fixture.
            silent.append(name)
            print(f"{name:<22}{0:>8}{'-':>7}{'-':>7}{'-':>6}  silent when piped")
            continue
        carriage = data.count(b"\r")
        feeds = data.count(b"\n")
        escapes = data.count(b"\x1b")
        form = "redraw" if carriage else ("lines" if feeds else "single row")
        with open(os.path.join(OUT, f"{name}.raw"), "wb") as handle:
            handle.write(data)
        written += 1
        print(f"{name:<22}{len(data):>8}{carriage:>7}{feeds:>7}{escapes:>6}  {form}")
    print("-" * 68)
    print(f"{written} fixtures written to {OUT}")
    if silent:
        print(f"\nsilent on a pipe, no reduction needed: {', '.join(silent)}")
    if skipped:
        print(f"\nnot regenerated: {', '.join(skipped)}")
        print("their fixtures are unchanged; install the dependencies to refresh them")


if __name__ == "__main__":
    main()
