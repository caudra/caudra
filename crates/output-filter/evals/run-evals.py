#!/usr/bin/env python3
"""Replays real command output through the server with and without filtering.

Rules must be judged on real tool output, so this drives the actual MCP server
against the sample projects rather than replaying recorded fixtures.
"""
import json, subprocess, sys

BIN = "/usr/local/bin/workcell-mcp"


def session(root):
    p = subprocess.Popen(
        [BIN, "--tool-group", "shell", "--yolo", root],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1)
    send = lambda o: (p.stdin.write(json.dumps(o) + "\n"), p.stdin.flush())
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "eval", "version": "0"}}})
    if not p.stdout.readline():
        sys.exit("init failed: " + p.stderr.read()[-500:])
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    return p, send


def run(root, command, filtered, timeout=300000):
    args = [] if filtered else ["--no-shell-output-filter"]
    p = subprocess.Popen(
        [BIN, "--tool-group", "shell", "--yolo", *args, root],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1)
    send = lambda o: (p.stdin.write(json.dumps(o) + "\n"), p.stdin.flush())
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "eval", "version": "0"}}})
    if not p.stdout.readline():
        sys.exit("init failed: " + p.stderr.read()[-500:])
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "shell", "arguments": {"command": command, "timeout": timeout}}})
    while True:
        line = p.stdout.readline()
        if not line:
            sys.exit("EOF: " + p.stderr.read()[-500:])
        m = json.loads(line)
        if m.get("id") == 2:
            p.stdin.close(); p.terminate()
            return m


# Each case may carry a setup command. Setup runs unfiltered and unmeasured, so
# a measured command stays a single scope: a chain such as `clean && build` is
# several scopes and is deliberately never filtered.
#
# `cargo clippy` is absent from the eval image and shares the `cargo check`
# output format, so rule tests cover it instead of a case here. The docker cases
# rely on a context configured by the harness rather than an inline
# `DOCKER_HOST=` assignment, because an assignment makes a command opaque and an
# opaque command is never filtered.
CASES = [
    ("/work/rustdemo", "cargo build", "cargo clean"),
    ("/work/rustdemo", "cargo build", None),
    ("/work/rustdemo", "cargo test", "cargo clean"),
    ("/work/godemo", "go build ./...", None),
    ("/work/godemo", "go vet ./...", None),
    ("/work/pydemo", "pytest", None),
    ("/work/pydemo", "pytest -q", None),
    ("/work/nodedemo", "npm install --no-fund express", "rm -rf node_modules package-lock.json"),
    ("/work/nodedemo", "eslint src/app.js", None),
    ("/work/nodedemo", "tsc --noEmit -p tsconfig.json", None),
    ("/work/gitdemo", "git status", None),
    ("/work/gitdemo", "git log --oneline", None),
    ("/work", "git clone /work/gitdemo /tmp/clone1", "rm -rf /tmp/clone1"),
    ("/work/dockerdemo", "docker build --no-cache -t wc-eval-demo:t1 .", None),
    ("/work/dockerdemo", "docker build -t wc-eval-demo:t1 .", None),
    ("/work/gomulti", "go test ./...", "go clean -testcache"),
    ("/work/mvndemo", "mvn package", "mvn clean"),
    ("/work/jestdemo", "jest", None),
    ("/work/vitestdemo", "vitest run", None),
    ("/work", "tar -cvf /tmp/eval.tar tardemo", "rm -f /tmp/eval.tar"),
    ("/work", "pip3 install --no-cache-dir --target /tmp/pt requests", "rm -rf /tmp/pt"),
    ("/work", "apt-get update", None),
    ("/work", "apt-get install -y --reinstall --no-install-recommends xz-utils", None),
    ("/work", "docker pull busybox:1.36", "docker rmi busybox:1.36"),
    ("/work/composedemo", "docker compose up --abort-on-container-exit", "docker compose down -v"),
    ("/work/composedemo", "docker compose down", "docker compose up -d"),
    ("/work", "wget https://registry.npmjs.org/express -O /tmp/wget-eval.json", "rm -f /tmp/wget-eval.json"),
]

print(f"{'command':<62}{'raw':>7}{'filt':>7}{'saved':>8}  exit  rule")
print("-" * 104)
tot_r = tot_f = 0
worse = 0
for root, cmd, setup in CASES:
    if setup:
        run(root, setup, False)
    a = run(root, cmd, False)["result"]
    if setup:
        run(root, setup, False)
    b = run(root, cmd, True)["result"]
    ta = a["content"][0]["text"]
    tb = b["content"][0]["text"]
    ex = b.get("structuredContent", {}).get("exitCode")
    tot_r += len(ta); tot_f += len(tb)
    if len(tb) > len(ta):
        worse += 1
    pct = (1 - len(tb) / len(ta)) * 100 if ta else 0.0
    mark = "yes" if "[filtered:" in tb else "-"
    label = cmd
    print(f"{label:<62}{len(ta):>7}{len(tb):>7}{pct:>7.1f}%  {str(ex):>4}  {mark}")
print("-" * 104)
print(f"{'TOTAL':<62}{tot_r:>7}{tot_f:>7}{(1 - tot_f / tot_r) * 100:>7.1f}%")
print(f"\ncommands where filtering enlarged output: {worse}")
