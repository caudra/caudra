"""Resolve the allowlisted CI runner profile to GitHub Actions outputs."""

import json
import os
import sys

GITHUB = {
    "linux_x64": "ubuntu-24.04",
    "linux_x64_heavy": "ubuntu-24.04",
    "linux_arm64": "ubuntu-24.04-arm",
    "macos_arm64": "macos-15",
    "macos_x64": "macos-15-intel",
    "windows_x64": "windows-2025",
}
BLACKSMITH = {
    **GITHUB,
    "linux_x64": "blacksmith-8vcpu-ubuntu-2404",
    "linux_x64_heavy": "blacksmith-16vcpu-ubuntu-2404",
    "linux_arm64": "blacksmith-16vcpu-ubuntu-2404-arm",
    "macos_arm64": "blacksmith-6vcpu-macos-15",
    "windows_x64": "blacksmith-16vcpu-windows-2025",
}
PROFILES = {
    "github": GITHUB,
    "blacksmith": BLACKSMITH,
    "ubicloud-blacksmith": {
        **BLACKSMITH,
        "linux_x64": "ubicloud-standard-8-ubuntu-2404",
        "linux_x64_heavy": "ubicloud-standard-16-ubuntu-2404",
        "linux_arm64": "ubicloud-standard-16-arm-ubuntu-2404",
    },
}


def main() -> None:
    profile = os.environ.get("CAUDRA_RUNNER_PROFILE") or "github"
    if profile not in PROFILES:
        sys.exit("CAUDRA_RUNNER_PROFILE must be one of: " + ", ".join(PROFILES))
    print(f"profile={profile}")
    print("runners=" + json.dumps(PROFILES[profile], separators=(",", ":")))


if __name__ == "__main__":
    main()
