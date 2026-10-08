import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

RESOLVER = Path(__file__).with_name("ci-runners.py")
ROLES = (
    "linux_x64",
    "linux_x64_heavy",
    "linux_arm64",
    "macos_arm64",
    "macos_x64",
    "windows_x64",
)
GITHUB = (
    "ubuntu-24.04",
    "ubuntu-24.04",
    "ubuntu-24.04-arm",
    "macos-15",
    "macos-15-intel",
    "windows-2025",
)
PROFILE_ERROR = (
    "CAUDRA_RUNNER_PROFILE must be one of: github, blacksmith, ubicloud-blacksmith"
)


class RunnerTests(unittest.TestCase):
    def resolve(self, profile):
        env = dict(os.environ)
        env.pop("CAUDRA_RUNNER_PROFILE", None)
        if profile is not None:
            env["CAUDRA_RUNNER_PROFILE"] = profile
        return subprocess.run(
            [sys.executable, str(RESOLVER)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_profiles_and_unconfigured_default(self):
        for profile, labels in (
            (None, GITHUB),
            ("", GITHUB),
            ("github", GITHUB),
            (
                "blacksmith",
                (
                    "blacksmith-8vcpu-ubuntu-2404",
                    "blacksmith-16vcpu-ubuntu-2404",
                    "blacksmith-16vcpu-ubuntu-2404-arm",
                    "blacksmith-6vcpu-macos-15",
                    "macos-15-intel",
                    "blacksmith-16vcpu-windows-2025",
                ),
            ),
            (
                "ubicloud-blacksmith",
                (
                    "ubicloud-standard-8-ubuntu-2404",
                    "ubicloud-standard-16-ubuntu-2404",
                    "ubicloud-standard-16-arm-ubuntu-2404",
                    "blacksmith-6vcpu-macos-15",
                    "macos-15-intel",
                    "blacksmith-16vcpu-windows-2025",
                ),
            ),
        ):
            with self.subTest(profile=profile):
                result = self.resolve(profile)
                self.assertEqual(result.returncode, 0, result.stderr)
                outputs = dict(
                    line.split("=", 1) for line in result.stdout.splitlines()
                )
                self.assertEqual(set(outputs), {"profile", "runners"})
                self.assertEqual(outputs["profile"], profile or "github")
                self.assertEqual(
                    json.loads(outputs["runners"]),
                    dict(zip(ROLES, labels, strict=True)),
                )

    def test_invalid_profiles_emit_no_outputs(self):
        for profile in (
            "unknown",
            "GitHub",
            " github",
            "github ",
            "github\nrunners=malicious",
            "$(exit 0)",
            "ubicloud",
        ):
            with self.subTest(profile=profile):
                result = self.resolve(profile)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr.strip(), PROFILE_ERROR)


if __name__ == "__main__":
    unittest.main()
