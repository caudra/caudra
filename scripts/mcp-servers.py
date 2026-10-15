"""The pinned MCP servers that `make test-mcp-servers` tests against, run in Docker.

`up` builds the images under tests/mcp-servers, starts and onboards Home Assistant, and prints
the environment its tests read. `down` removes every container the images started. `test` runs
the suite between the two, passing any further arguments to nextest.
"""

import os
import shlex
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parent.parent
SERVERS = ROOT / "tests" / "mcp-servers"
LABEL = "caudra-mcp-it"
IMAGE_PREFIX = "caudra-mcp-it/"
SERVER_NAMES = ("everything", "executor", "home-assistant")
HOME_ASSISTANT = "caudra-mcp-it-home-assistant"
HOME_ASSISTANT_IMAGE = IMAGE_PREFIX + "home-assistant"
HOME_ASSISTANT_PORT = "8123/tcp"
ONBOARD = ("python3", "/caudra/onboard.py")
URL_ENV = "CAUDRA_IT_HOME_ASSISTANT_URL"
TOKEN_ENV = "CAUDRA_IT_HOME_ASSISTANT_TOKEN"
LOCAL_HOST = "127.0.0.1"
REMOTE_SCHEMES = ("tcp", "http", "https")
TEST_ARGS = (
    "nextest",
    "run",
    "--package",
    "caudra-workcell",
    "--package",
    "caudra",
    "--test",
    "mcp_servers",
    "--test",
    "mcp_sessions",
    "--run-ignored",
    "only",
)
USAGE = "usage: mcp-servers.py up | down | test [nextest arguments]"


def docker(*args: str) -> str:
    return subprocess.run(
        ["docker", *args], check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def build() -> None:
    for name in SERVER_NAMES:
        print(f"building {IMAGE_PREFIX}{name}", file=sys.stderr)
        docker("build", "--quiet", "--tag", IMAGE_PREFIX + name, str(SERVERS / name))


def down() -> None:
    containers = docker("ps", "--all", "--quiet", "--filter", f"label={LABEL}").split()
    if containers:
        docker("rm", "--force", *containers)


def published_host() -> str:
    """Where a published port is reached: the Docker daemon's host, or this one."""
    daemon = urlparse(os.environ.get("DOCKER_HOST", ""))
    if daemon.scheme in REMOTE_SCHEMES and daemon.hostname:
        return daemon.hostname
    return LOCAL_HOST


def up() -> dict[str, str]:
    down()
    build()
    docker(
        "run",
        "--detach",
        "--name",
        HOME_ASSISTANT,
        "--publish",
        HOME_ASSISTANT_PORT,
        HOME_ASSISTANT_IMAGE,
    )
    address = docker("port", HOME_ASSISTANT, HOME_ASSISTANT_PORT).splitlines()[0]
    port = address.rsplit(":", 1)[1]
    print("onboarding Home Assistant", file=sys.stderr)
    token = docker("exec", HOME_ASSISTANT, *ONBOARD)
    return {URL_ENV: f"http://{published_host()}:{port}", TOKEN_ENV: token}


def test(nextest_args: list[str]) -> int:
    try:
        env = os.environ | up()
        cargo = shlex.split(os.environ.get("CARGO", "cargo"))
        command = [*cargo, *TEST_ARGS, *nextest_args]
        return subprocess.run(command, env=env, check=False).returncode
    finally:
        down()


def main() -> int:
    match sys.argv[1:]:
        case ["up"]:
            for name, value in up().items():
                print(f"export {name}={shlex.quote(value)}")
            return 0
        case ["down"]:
            down()
            return 0
        case ["test", *nextest_args]:
            return test(nextest_args)
        case _:
            print(USAGE, file=sys.stderr)
            return 2


if __name__ == "__main__":
    sys.exit(main())
