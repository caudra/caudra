"""Time a release phase without dumping command arguments or the environment."""

import argparse
import os
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

LABEL = re.compile(r"[a-zA-Z0-9_-]{1,64}\Z")
PROC_READ_LIMIT = 64 * 1024
CPU_MODEL_LIMIT = 160


def label(value: str) -> str:
    return value if LABEL.fullmatch(value) else "unknown"


def proc_fields(name: str) -> dict[str, str]:
    try:
        with (Path("/proc") / name).open("rb") as source:
            text = source.read(PROC_READ_LIMIT).decode("utf-8", errors="replace")
    except OSError:
        return {}
    fields = {}
    for line in text.splitlines():
        key, separator, value = line.partition(":")
        if separator:
            fields.setdefault(key.strip(), value.strip())
    return fields


def hardware() -> str:
    cpu = proc_fields("cpuinfo") if sys.platform == "linux" else {}
    memory = proc_fields("meminfo") if sys.platform == "linux" else {}
    model = cpu.get("model name") or cpu.get("Hardware") or cpu.get("Processor")
    if not model:
        model = " ".join(
            f"{key} {cpu[key]}" for key in ("CPU implementer", "CPU part") if key in cpu
        )
    model = re.sub(r"[^a-zA-Z0-9 ._()+@/-]", "_", model)[:CPU_MODEL_LIMIT] or "unknown"
    total = re.fullmatch(r"([0-9]{1,20})\s+kB", memory.get("MemTotal", ""))
    return f'cpu_model="{model}" host_memory_kib={total[1] if total else "unknown"}'


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", type=label)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    if not arguments.command:
        parser.error("a command is required")
    profile = label(os.environ.get("CAUDRA_RUNNER_PROFILE", "github"))
    print(
        f"::group::{arguments.phase} profile={profile} "
        f"arch={label(platform.machine())} cpus={os.cpu_count() or 1} {hardware()}",
        file=sys.stderr,
        flush=True,
    )
    start = time.monotonic()
    status = 127
    try:
        status = subprocess.run(arguments.command, check=False).returncode
        if status < 0:
            status = 128 - status
    except OSError:
        print("Unable to start release phase command", file=sys.stderr)
    finally:
        print(
            f"phase={arguments.phase} elapsed_seconds={time.monotonic() - start:.3f} "
            f"status={status}\n::endgroup::",
            file=sys.stderr,
            flush=True,
        )
    return status


if __name__ == "__main__":
    sys.exit(main())
