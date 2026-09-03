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
    # Progress reduction is command-independent, so these cases deliberately run
    # commands no rule names. `python3 script.py` is the shape that matters: the
    # programs that emit bars are overwhelmingly ones a corpus cannot anticipate.
    ("/work/progressdemo", "python3 tqdm_stderr.py 600", None),
    ("/work/progressdemo", "python3 tqdm_stdout.py 400", None),
    ("/work/progressdemo", "python3 tqdm_nested.py", None),
    ("/work/progressdemo", "python3 bare_cr.py", None),
    ("/work/progressdemo", "python3 newline_frames.py", None),
    # `schollz/progressbar` is the hardest observed case: it redraws into a pipe,
    # blanks each frame with spaces, and emits no newline at all.
    ("/work/progressdemo", "./go_progressbar/generator", None),
    # Negative cases. A reduction that shrinks these is destroying output, so
    # they are measured alongside the rest rather than trusted to unit tests.
    ("/work/progressdemo", "python3 numeric_table.py", None),
    ("/work/progressdemo", "python3 repeated_diagnostic.py", None),
    # Libraries that consult isatty. `rich` and `ora` print a final line and
    # never redraw; indicatif, mpb, and cli-progress print nothing. Running them
    # keeps that a measurement the harness re-checks rather than a claim.
    ("/work/progressdemo", "python3 rich_progress.py", None),
    ("/work/progressdemo", "node node_ora.mjs", None),
    ("/work/progressdemo", "node node_cli_progress.js", None),
    ("/work/progressdemo", "./rust_indicatif/target/release/indicatif-generator", None),
    ("/work/progressdemo", "./go_mpb/generator", None),
    ("/work", "curl -o /dev/null https://registry.npmjs.org/express", None),
]

# Cases whose output must not shrink. Filtering them would mean the gates that
# separate a progress readout from data have stopped working.
MUST_NOT_SHRINK = {
    "python3 numeric_table.py",
    "python3 repeated_diagnostic.py",
    "python3 rich_progress.py",
    "node node_ora.mjs",
}

# `wrote` is the bytes the process emitted, taken from the structured counters
# rather than from a run with filtering off. Terminal rendering is decoding, not
# filtering, so it applies to both runs and a disabled-filter baseline would hide
# the reduction it accounts for. The two reductions are therefore reported
# separately: `wrote -> raw` is rendering, `raw -> filt` is the rule corpus and
# the line collapse.
print(f"{'command':<54}{'wrote':>8}{'raw':>7}{'filt':>7}{'redraw':>8}{'saved':>8}  exit  stages")
print("-" * 118)
tot_w = tot_r = tot_f = 0
worse = 0
shrank_but_must_not = []
for root, cmd, setup in CASES:
    if setup:
        run(root, setup, False)
    a = run(root, cmd, False)["result"]
    if setup:
        run(root, setup, False)
    b = run(root, cmd, True)["result"]
    ta = a["content"][0]["text"]
    tb = b["content"][0]["text"]
    structured = b.get("structuredContent", {})
    ex = structured.get("exitCode")
    wrote = structured.get("stdoutUtf8Bytes", 0) + structured.get("stderrUtf8Bytes", 0)
    redraws = structured.get("stdoutRedrawsCollapsed", 0) + structured.get(
        "stderrRedrawsCollapsed", 0
    )
    tot_w += wrote
    tot_r += len(ta)
    tot_f += len(tb)
    if len(tb) > len(ta):
        worse += 1
    if cmd in MUST_NOT_SHRINK and (len(tb) < len(ta) or redraws):
        shrank_but_must_not.append(cmd)
    render_pct = (1 - len(ta) / wrote) * 100 if wrote else 0.0
    pct = (1 - len(tb) / len(ta)) * 100 if ta else 0.0
    stages = tb.rsplit("[filtered:", 1)[1].rstrip("]\n").strip() if "[filtered:" in tb else "-"
    print(
        f"{cmd:<54}{wrote:>8}{len(ta):>7}{len(tb):>7}{render_pct:>7.1f}%{pct:>7.1f}%  {str(ex):>4}  {stages}"
    )
print("-" * 118)
render_total = (1 - tot_r / tot_w) * 100 if tot_w else 0.0
print(
    f"{'TOTAL':<54}{tot_w:>8}{tot_r:>7}{tot_f:>7}{render_total:>7.1f}%{(1 - tot_f / tot_r) * 100:>7.1f}%"
)
print(f"\ncommands where filtering enlarged output: {worse}")
print(f"commands that must not shrink but did: {shrank_but_must_not or 'none'}")
