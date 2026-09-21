#!/usr/bin/env python3
"""Opt-in real KVM acceptance. Never uses the operator's catalog or Caudra state."""
import argparse
import contextlib
import fcntl
import hashlib
import http.client
import http.server
import json
import os
import pty
import re
import secrets
import select
import shutil
import signal
import socket
import sqlite3
import stat
import subprocess
import tempfile
import termios
import threading
import time
import xml.etree.ElementTree as ET
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PROTOCOL = "2026-07-28"
URI = "qemu:///session"
TTL = 900
LIMIT = 16 * 1024 * 1024
START = time.monotonic()
SECRETS = []


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def safe(value):
    text = str(value)
    for secret in SECRETS:
        text = text.replace(secret, "[REDACTED]")
    return re.sub(r"(?i)(Bearer\s+)[A-Za-z0-9._~+/=-]+", r"\1[REDACTED]", text)


def event(scenario, **fields):
    print(safe(json.dumps({"scenario": scenario, "elapsed_s": round(time.monotonic() - START, 3), **fields})), flush=True)


@contextlib.contextmanager
def child(args, **kwargs):
    process = subprocess.Popen(args, start_new_session=True, umask=0o077, **kwargs)
    try:
        yield process
    finally:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run(args, *, timeout=60, stdin="", check=True, **kwargs):
    with child(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs) as process:
        out, err = process.communicate(stdin.encode(), timeout=timeout)
        require(len(out) + len(err) <= LIMIT, "subprocess output exceeded acceptance bound")
        if check:
            require(process.returncode == 0, f"{Path(args[0]).name} failed ({process.returncode}): {safe((out + err).decode(errors='replace'))[-6000:]}")
        return process.returncode, out.decode(), err.decode()


def virsh(*args, **kwargs):
    return run(["virsh", "-c", URI, *args], timeout=30, **kwargs)[1]


def digest(path):
    with path.open("rb") as stream:
        return "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()


def request(port, method, path, body=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=100)
    try:
        connection.request(method, path, body, headers or {})
        response = connection.getresponse()
        result = response.read(LIMIT + 1)
        require(len(result) <= LIMIT, "HTTP response exceeded acceptance bound")
        return response.status, result
    finally:
        connection.close()


def ports(first, last, count):
    for start in range(first, last - count, count):
        with contextlib.ExitStack() as stack:
            try:
                for port in range(start, start + count):
                    sock = stack.enter_context(socket.socket())
                    sock.bind(("127.0.0.1", port))
                return start
            except OSError:
                pass
    raise RuntimeError("no free test port range")


class JsonLines:
    def __init__(self, process):
        self.process = process
        self.buffer = b""

    def send(self, value):
        self.process.stdin.write((json.dumps(value) + "\n").encode())
        self.process.stdin.flush()

    def read(self, timeout=100):
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            require(select.select([self.process.stdout], [], [], max(0, deadline - time.monotonic()))[0], "JSON line deadline")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                return None
            self.buffer += chunk
            require(len(self.buffer) <= LIMIT, "JSON line too large")
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)


class SyntheticProvider(http.server.BaseHTTPRequestHandler):
    requests = 0

    def log_message(self, format: str, *args: object) -> None:
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        require(length <= LIMIT, "provider request too large")
        self.rfile.read(length)
        type(self).requests += 1
        events = [
            {"type": "message_start", "message": {"id": "fixture", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "usage": {"input_tokens": 1, "output_tokens": 0}}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Synthetic local provider acceptance."}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}},
            {"type": "message_stop"},
        ]
        body = "".join(f"event: {item['type']}\ndata: {json.dumps(item)}\n\n" for item in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Acceptance:
    def __init__(self, args, temp):
        self.args, self.temp = args, temp
        self.key = secrets.token_hex(32)
        SECRETS.append(self.key)
        self.owner = hashlib.sha256(("e2b-libvirt-owner-v1\0" + self.key).encode()).hexdigest()
        self.pool = "caudra-accept-" + secrets.token_hex(6)
        self.pool_created = False
        self.owned = {}
        self.api = ports(18000, 19000, 2)
        self.proxy = self.api + 1
        self.forward = ports(23000, 29000, 4)
        self.egress = ports(62000, 64000, 4)
        self.token = ""
        self.sid = None
        self.rpc_id = 0
        self.db = temp / "state.db"
        self.image = temp / "derived.qcow2"
        self.local = temp / "client"
        self.seed = temp / "seed"
        for name in ("bin", "pool", "client", "seed", "home", "config", "state", "cache", "data", "guestfs-tmp", "guestfs-cache"):
            (temp / name).mkdir(mode=0o700)
        self.env = {"PATH": os.environ["PATH"], "HOME": str(temp / "home"), "LANG": "C.UTF-8", "NO_COLOR": "1", "NO_PROXY": "127.0.0.1,localhost",
                    "XDG_CONFIG_HOME": str(temp / "config"), "XDG_STATE_HOME": str(temp / "state"), "XDG_CACHE_HOME": str(temp / "cache"), "XDG_DATA_HOME": str(temp / "data"),
                    "ANTHROPIC_API_KEY": "synthetic-not-a-real-key", "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9", "ALL_PROXY": "http://127.0.0.1:9"}
        self.daemon_env = {key: value for key, value in os.environ.items() if key in ("PATH", "HOME", "USER", "LANG", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME")}
        self.daemon_env["E2B_LOCAL_API_KEY"] = self.key
        self.flags = ["--database", str(self.db), "--catalog-dir", str(temp / "catalog"), "--qemu-img", "/usr/bin/qemu-img", "--template", "acceptance",
                      "--api-addr", f"127.0.0.1:{self.api}", "--proxy-addr", f"127.0.0.1:{self.proxy}", "--libvirt-uri", URI, "--storage-pool", self.pool,
                      "--network-backend", "slirp", "--egress-mode", "enforced", "--iron-proxy-bin", args.iron_proxy,
                      "--egress-relay-bin", str(temp / "bin/e2b-egress-relay"), "--egress-jail-bin", str(temp / "bin/e2b-jail"),
                      "--egress-log-dir", str(temp / "egress"),
                      "--persist-max-age", "1h", "--startup-timeout", "120s", "--max-timeout", "30m",
                      "--max-cpus", "2", "--max-memory-mb", "1024", "--max-disk-mb", "8192", "--passt-port-start", str(self.forward), "--passt-port-end", str(self.forward + 3),
                      "--egress-port-start", str(self.egress), "--egress-port-end", str(self.egress + 3)]

    def cli(self, args, *, check=True, stdin="", timeout=160):
        return run([self.args.caudra, "--no-plugins", "--model", "anthropic/claude-sonnet-4-6", *args],
                   env=self.env, cwd=self.local, check=check, stdin=stdin, timeout=timeout)

    def lifecycle(self, method, path, value=None):
        status, body = request(self.api, method, "/daemon/v1/" + path, json.dumps(value) if value is not None else None,
                               {"X-API-Key": self.key, "Content-Type": "application/json"})
        require(200 <= status < 300, f"lifecycle {method} {path}: HTTP {status} {safe(body.decode())}")
        return json.loads(body) if body else None

    def admin(self, action, value, approve=False):
        _, output, _ = run([str(self.temp / "bin/e2b-locald"), "templates", action, *(["--approve"] if approve else []), *self.flags],
                           stdin=json.dumps(value), env=self.daemon_env, timeout=180)
        result = json.loads(output)
        require(result["ok"], "catalog admin refused")
        return result["result"]

    def build(self):
        before = digest(Path(self.args.source_image))
        for binary in ("e2b-locald", "e2b-egress-relay", "e2b-jail"):
            run(["go", "build", "-o", str(self.temp / "bin" / binary), "./cmd/" + binary], cwd=self.args.e2b, timeout=120)
        env = {key: value for key, value in os.environ.items() if key in ("PATH", "HOME", "USER", "LANG")}
        env.update(LIBGUESTFS_BACKEND="direct", LIBGUESTFS_TMPDIR=str(self.temp / "guestfs-tmp"), LIBGUESTFS_CACHEDIR=str(self.temp / "guestfs-cache"))
        started = time.monotonic()
        run(["bash", str(Path(self.args.e2b) / "scripts/build-caudra-image.sh"), self.args.source_image, str(self.image), self.args.workcell], env=env, timeout=300)
        require(digest(Path(self.args.source_image)) == before, "source image changed")
        recipe = self.admin("recipe", {"recipe": "caudra", "scriptsDir": str(Path(self.args.e2b) / "scripts")})
        resources = {"cpuCount": 2, "memoryMB": 1024, "diskSizeMB": 8192}
        manifest = {"schemaVersion": 1, "id": "acceptance", "architecture": "x86_64", "machine": "q35", "minimum": resources, "defaults": resources, "networkTopology": "slirp-enforced",
                    "workcell": {"version": "0.1.0", "sha256": digest(Path(self.args.workcell)), "protocolVersion": PROTOCOL, "transferProtocol": "workcell-reviewed-v1", "remoteWorkspace": True,
                                 "workspaceSnapshots": True, "reviewedTransfer": True, "workspaceRoot": "/workspace", "snapshotRoot": "/var/lib/workcell-mcp/snapshots", "transferRoot": "/var/lib/workcell-mcp/transfers"},
                    "build": {"recipe": "import", "recipeSHA256": recipe["recipeSHA256"], "sourceRevision": ""}}
        self.template = self.admin("import", {"sourcePath": str(self.image), "expectedSHA256": digest(self.image), "expectedRevision": "", "manifest": manifest}, True)
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            require(db.execute("PRAGMA user_version").fetchone() == (1,), "test database is not fresh schema 1")
        require(self.admin("inspect", {"id": "acceptance", "revision": self.template["revision"]}) == self.template, "catalog inspect drift")
        self.image.unlink()
        event("derived_image_and_real_catalog_import", seconds=round(time.monotonic() - started, 3), image=self.template["imageSHA256"], revision=self.template["revision"], workcell=manifest["workcell"]["sha256"], caudra=digest(Path(self.args.caudra)), daemon=digest(self.temp / "bin/e2b-locald"), source_unchanged=before)

    def configure(self):
        config = f'''version = 1
[sandbox.providers.fixture]
kind = "e2b-libvirt"
api_endpoint = "http://127.0.0.1:{self.api}"
proxy_endpoint = "http://127.0.0.1:{self.proxy}"
credential_ref = "sandbox-api:fixture"
[sandbox.networks.allowed]
enforcement = "required"
tls_mode = "sni-only"
domains = ["example.com"]
cidrs = []
[sandbox.transfers.reviewed]
respect_gitignore = true
initial_seed = "ask"
delete_extraneous = false
[sandbox.profiles.acceptance]
provider = "fixture"
template = "acceptance"
template_revision = "{self.template['revision']}"
cpus = 2
memory_mib = 1024
disk_gib = 8
cwd = "."
network = "allowed"
transfer = "reviewed"
persistent = true
running_ttl_seconds = {TTL}
on_exit = "detach"
'''
        for app in ("caudra", "caudra-debug"):
            directory = self.temp / "config" / app
            directory.mkdir(mode=0o700)
            (directory / "sandboxes.toml").write_text(config)
            (directory / "sandboxes.toml").chmod(0o600)
            (directory / "permissions.toml").write_text('default = "prompt"\n')
        self.cli(["auth", "sandbox", "set", "fixture", "--stdin"], stdin=self.key)

    def rpc(self, method, params=None):
        self.rpc_id += 1
        params = dict(params or {})
        params["_meta"] = {"io.modelcontextprotocol/protocolVersion": PROTOCOL, "io.modelcontextprotocol/clientCapabilities": {"extensions": {"ai.workcell/remote-host": {"versions": ["v1"]}}},
                           "io.modelcontextprotocol/clientInfo": {"name": "real-kvm-acceptance", "version": "1"}, "ai.workcell/remote-host": {"versions": ["v1"]}}
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": PROTOCOL, "Mcp-Method": method, "Authorization": "Bearer " + self.token}
        if "name" in params:
            headers["Mcp-Name"] = params["name"]
        status, body = request(self.proxy, "POST", f"/sandboxes/{self.sid}/mcp", json.dumps({"jsonrpc": "2.0", "id": self.rpc_id, "method": method, "params": params}), headers)
        require(status == 200, f"MCP {method} HTTP {status}: {safe(body.decode())[:2000]}")
        text = body.decode()
        if text.lstrip().startswith("{"):
            response = json.loads(text)
        else:
            values = [json.loads(line[5:]) for line in text.splitlines() if line.startswith("data:")]
            response = next(value for value in reversed(values) if value.get("id") == self.rpc_id)
        require("result" in response, f"MCP {method}: {safe(response)}")
        return response["result"]

    def tool(self, name, arguments):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments})
        require(not result.get("isError"), f"tool {name}: {safe(result)}")
        if name == "shell":
            output = result["structuredContent"]
            require(output["exitCode"] == 0 and not output["timedOut"], f"guest shell failed: {safe(output)}")
        return "\n".join(block.get("text", "") for block in result.get("content", []))

    def shell(self, command):
        return self.tool("shell", {"command": command, "timeout": 20000})

    def inventory(self):
        if not self.db.exists():
            return
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            for row in db.execute("SELECT id,owner_id,execution_id,domain_name,volume_name,state FROM sandboxes"):
                sid, owner, execution, domain, volume, state_value = row
                require(owner == self.owner and domain.startswith("e2b-") and volume.startswith("e2b-"), "foreign private DB row; refusing cleanup")
                self.owned[sid] = {"sandbox": sid, "execution": execution, "domain": domain, "volume": volume, "state": state_value}

    def transfer(self, mode, selected=(), *, success=True, permission="allow", change=None):
        args = [self.args.caudra, "--no-plugins", "sandbox", "transfer", mode, "managed", "--local-root", str(self.seed), "--remote-root", "sync", "--json-input"]
        for path in selected:
            args += ["--select", path]
        messages = []
        plan_reviewed = False
        with tempfile.TemporaryFile() as errors, child(args, cwd=self.local, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, bufsize=0) as process:
            stream = JsonLines(process)
            deadline = time.monotonic() + 120
            while True:
                require(time.monotonic() < deadline, "transfer exceeded deadline")
                value = stream.read(max(0, deadline - time.monotonic()))
                if value is None:
                    break
                messages.append(value)
                if value.get("event") == "plan":
                    if change:
                        change()
                    stream.send({"confirm_plan": value["plan_id"]})
                    plan_reviewed = True
                elif value.get("event") == "permission":
                    answer = permission if plan_reviewed else "allow"
                    stream.send({"request_id": value["request"]["id"] if answer != "stale" else "stale-request", "answer": answer if answer != "stale" else "allow"})
            code = process.wait(timeout=10)
            errors.seek(0)
            error = errors.read(LIMIT).decode()
            require((code == 0) == success, f"transfer {mode}: rc={code} {safe(error)} {safe(messages)[-4000:]}")
        return messages

    def data_checks(self):
        instance = self.lifecycle("GET", f"instances/{self.sid}")
        credentials = self.lifecycle("POST", f"instances/{self.sid}/credentials", {"expectedExecutionID": instance["executionID"], "expectedRevision": instance["revision"]})
        self.token = credentials["trafficAccessToken"]
        SECRETS.append(self.token)
        for auth in ({}, {"Authorization": "Bearer wrong-test-token"}):
            status, _ = request(self.proxy, "POST", f"/sandboxes/{self.sid}/mcp", "{}", {**auth, "Content-Type": "application/json"})
            require(status in (401, 403), "unauthenticated MCP accepted")
        self.descriptor = self.rpc("server/discover")["capabilities"]["extensions"]["ai.workcell/remote-host"]
        tools = self.rpc("tools/list")["tools"]
        require({"shell", "python_execution", "file_read", "file_write"} <= {tool["name"] for tool in tools}, "missing VM tools")
        status = self.cli(["--sandbox", "managed", "remote", "status"])[1]
        require("pending operations: 0" in status, "managed remote status failed")
        event("authenticated_exact_catalogue_and_managed_status", tools=len(tools), sandbox=self.sid, instance=self.descriptor["instanceId"], generation=self.descriptor["workspaceGeneration"])
        require("42" in self.tool("python_execution", {"code": "6 * 7"}), "bundled Python worker failed")
        self.tool("file_write", {"filePath": "acceptance.txt", "content": "before-snapshot\n"})
        require("before-snapshot" in self.tool("file_read", {"filePath": "acceptance.txt"}), "VM file roundtrip failed")
        require(digest(Path(self.args.workcell)).removeprefix("sha256:") in self.shell("sha256sum /usr/local/bin/workcell-mcp"), "guest binary digest mismatch")
        require("private-roots-ok" in self.shell("test \"$(stat -c '%a' /var/lib/workcell-mcp/transfers)\" = 700 && test \"$(stat -c '%a' /var/lib/workcell-mcp/snapshots)\" = 700 && printf private-roots-ok"), "guest private roots missing")
        event("guest_python_file_and_exact_binary_identity")
        self.shell("mkdir -p sync; git init -q")
        run(["git", "init", "-q", str(self.seed)], env=self.env)
        payload = bytes(range(256)) * 4 + b"\x00\xffacceptance"
        (self.seed / "nested").mkdir()
        (self.seed / "nested/seed.bin").write_bytes(payload)
        (self.seed / "plain.txt").write_text("seed-text\n")
        (self.seed / ".gitignore").write_text("ignored.txt\n")
        (self.seed / "ignored.txt").write_text("excluded")
        (self.seed / ".env").write_text("test-only-not-a-secret")
        comparison = self.transfer("compare")
        require(any(item.get("event") == "comparison" and item["complete"] for item in comparison), "incomplete compare")
        seeded = self.transfer("seed", ["nested/seed.bin", "plain.txt"])
        require(sum(item.get("event") == "permission" for item in seeded) >= 2, "both-end native authorization was not exercised")
        require(hashlib.sha256(payload).hexdigest() in self.shell("sha256sum sync/nested/seed.bin"), "seed binary differs")
        require("excluded-ok" in self.shell("test ! -e sync/.env && test ! -e sync/ignored.txt && printf excluded-ok"), "excluded files seeded")
        for excluded in (".env", "ignored.txt"):
            self.transfer("seed", [excluded], success=False)
        for answer in ("deny", "stale"):
            path = answer + ".txt"
            (self.seed / path).write_text("must not publish")
            denied = self.transfer("seed", [path], success=False, permission=answer)
            require(any(item.get("event") == "permission" for item in denied), "denial did not reach native authorization")
            require("absent" in self.shell(f"test ! -e sync/{path} && printf absent"), "denied file published")
        (self.seed / "conflict.txt").write_text("reviewed source")
        self.transfer("seed", ["conflict.txt"], success=False, change=lambda: self.tool("file_write", {"filePath": "sync/conflict.txt", "content": "external writer"}))
        require("external writer" in self.tool("file_read", {"filePath": "sync/conflict.txt"}), "stale plan replaced external writer")
        pulled = bytes(reversed(range(256))) * 3 + b"\x00pull"
        self.shell(f"mkdir -p sync/new/deep; python3 -c \"from pathlib import Path; Path('sync/new/deep/pull.bin').write_bytes(bytes.fromhex('{pulled.hex()}'))\"")
        self.transfer("pull", ["new/deep/pull.bin"])
        require((self.seed / "new/deep/pull.bin").read_bytes() == pulled, "binary nested Pull differs")
        event("seed_pull_exclusions_native_denial_stale_and_conflict", seed_bytes=len(payload), pull_bytes=len(pulled))
        host = {key: self.descriptor[key] for key in ("serverId", "instanceId", "workspaceId", "workspaceGeneration", "rootProjectId", "principalId")}
        host.update(cwdHandle=self.descriptor["cwd"]["handle"], catalogRevision=self.descriptor["revisions"]["catalog"], policyRevision=self.descriptor["revisions"]["policy"])
        binding = {"version": "v1", "host": host, "cwdHandle": host["cwdHandle"]}
        snapshot = self.rpc("ai.workcell/snapshot-capture", {**binding, "checkpointId": "kvm-acceptance"})["snapshot"]
        require(snapshot["state"] == "complete", "snapshot incomplete")
        self.tool("file_write", {"filePath": "acceptance.txt", "content": "changed-after-snapshot"})
        prepared = self.rpc("ai.workcell/snapshot-prepare-restore", {**binding, "snapshotId": snapshot["snapshotId"]})
        restored = self.rpc("ai.workcell/execute", {"version": "v1", "host": host, "preparationId": prepared["operation"]["preparationId"], "invocationId": "kvm-snapshot-restore"})
        require(restored["state"] == "completed", "snapshot restore failed")
        require("before-snapshot" in self.tool("file_read", {"filePath": "acceptance.txt"}), "restore did not restore bytes")
        event("snapshot_capture_prepared_restore", snapshot=snapshot["snapshotId"])

    def lifecycle_checks(self):
        self.inventory()
        before = self.owned[self.sid].copy()
        disk = self.temp / "pool" / before["volume"]
        inode = disk.stat().st_ino
        self.check_egress(True)
        policy = self.temp / "tight-policy.json"
        policy.write_text(json.dumps({"mode": "sni-only", "domains": [], "cidrs": []}))
        self.cli(["sandbox", "network", "managed", "--policy", str(policy), "--apply", "--yes"])
        tightened = self.lifecycle("GET", f"instances/{self.sid}")["egress"]
        require(tightened["revision"] == tightened["effectiveRevision"] and not tightened["policy"]["domains"], "tightened policy not effective")
        self.check_egress(False)
        started = time.monotonic()
        paused = json.loads(self.cli(["sandbox", "pause", "managed"])[1])["instance"]
        require(paused["state"] == "paused" and not virsh("list", "--all", "--name").strip(), "pause did not remove domain")
        require(disk.stat().st_ino == inode, "pause replaced disk")
        event("persistent_pause", seconds=round(time.monotonic() - started, 3), **{**before, "state": paused["state"]})
        started = time.monotonic()
        resumed = json.loads(self.cli(["sandbox", "resume", "managed", "--lease-seconds", str(TTL), "--yes"])[1])["instance"]
        require(resumed["executionID"] != before["execution"] and resumed["egress"]["revision"] == tightened["revision"] == resumed["egress"]["effectiveRevision"], "resume execution/policy mismatch")
        require(disk.stat().st_ino == inode, "resume replaced persistent disk")
        after = self.rpc("server/discover")["capabilities"]["extensions"]["ai.workcell/remote-host"]
        require(after["workspaceGeneration"] == self.descriptor["workspaceGeneration"] and after["instanceId"] != self.descriptor["instanceId"], "resume reused process or changed disk generation")
        require("before-snapshot" in self.tool("file_read", {"filePath": "acceptance.txt"}), "persistent bytes lost")
        event("persistent_resume_tight_policy", seconds=round(time.monotonic() - started, 3), previous_execution=before["execution"], execution=resumed["executionID"], volume=before["volume"], inode=inode, instance=after["instanceId"])
        self.check_egress(False)
        self.cli(["sandbox", "inspect", "managed"])
        current = self.lifecycle("GET", f"instances/{self.sid}")
        self.lifecycle("POST", f"instances/{self.sid}/renew", {"expectedExecutionID": current["executionID"], "expectedRevision": current["revision"], "leaseSeconds": TTL + 60})
        code, _, _ = self.cli(["sandbox", "pause", "managed"], check=False)
        require(code != 0, "stale lifecycle request unexpectedly succeeded")
        self.cli(["sandbox", "inspect", "managed"], check=False)
        records = json.loads(self.cli(["sandbox", "list"])[1])
        pending = next(row for row in records if row["name"] == "managed")
        require(pending["lifecycle"]["observed_revision"] is None and not pending["lifecycle"]["failure_acknowledged"], "failed intent not retained")
        ack = self.cli(["sandbox", "acknowledge-failure", "managed", "--yes"])[1]
        require("Acknowledge FAILURE, not success" in ack and '"failure_acknowledged": true' in ack, "failure review/ack UX missing")
        require(self.lifecycle("GET", f"instances/{self.sid}")["state"] == "running", "failed pause was replayed")
        self.cli(["--sandbox", "managed", "remote", "status"])
        event("real_stale_lifecycle_pending_inspect_and_ack_no_replay")

    def check_egress(self, allowed):
        result = self.rpc("tools/call", {"name": "shell", "arguments": {"command": "curl --fail --silent --show-error --max-time 15 --output /dev/null --write-out '%{http_code}' https://example.com", "timeout": 20000}})
        require(not result.get("isError"), "egress probe failed to execute")
        output = result["structuredContent"]
        require(not output["timedOut"], "egress probe exceeded shell deadline")
        if allowed:
            require(output["exitCode"] == 0 and output["stdout"] == "200", f"allowlisted HTTPS did not succeed: {safe(output)}")
        else:
            require(output["exitCode"] != 0 and "403" in output["stderr"], f"tight policy did not explicitly reject HTTPS CONNECT: {safe(output)}")
        direct = self.rpc("tools/call", {"name": "shell", "arguments": {"command": "curl --noproxy '*' --silent --show-error --connect-timeout 2 --max-time 3 http://1.1.1.1", "timeout": 5000}})
        require(not direct.get("isError") and direct["structuredContent"]["exitCode"] != 0 and not direct["structuredContent"]["timedOut"], "direct egress bypass succeeded or probe failed")
        event("live_egress_probe", allowlisted_https=allowed, explicit_proxy_denial=not allowed, direct_egress_blocked=True)

    def sessions(self):
        sdk = ["--print", "--input-format", "stream-json", "--output-format", "stream-json"]
        command = lambda content: json.dumps({"type": "user", "message": {"role": "user", "content": content}}) + "\n"
        output = self.cli([*sdk, "--sandbox", "managed"], stdin=command("!pwd"))[1]
        values = [json.loads(line) for line in output.splitlines() if line.startswith("{")]
        require(any(value.get("type") == "result" and not value.get("is_error") and "/workspace" in value.get("result", "") for value in values), "actual SDK !pwd failed")
        source = next(value["session_id"] for value in values if "session_id" in value)
        output = self.cli([*sdk, "--session", source], stdin=command("!pwd"))[1]
        require("/workspace" in output, "SDK saved source was not restored")
        output = self.cli([*sdk, "--session", source, "--max-turns", "1"], stdin=command("Respond locally."))[1]
        require("Synthetic local provider acceptance" in output and SyntheticProvider.requests > 0, "synthetic local provider was not used")
        with tempfile.TemporaryFile() as errors, child([self.args.caudra, "--no-plugins", "--model", "anthropic/claude-sonnet-4-6", "acp"], cwd=self.local, env=self.env,
                                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, bufsize=0) as process:
            stream = JsonLines(process)

            def rpc(number, method, params):
                stream.send({"jsonrpc": "2.0", "id": number, "method": method, "params": params})
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    value = stream.read(max(0, deadline - time.monotonic()))
                    require(value is not None, "ACP exited")
                    if value.get("id") == number:
                        require("error" not in value, f"ACP {method}: {safe(value)}")
                        return value["result"]
                raise TimeoutError("ACP response deadline")

            rpc(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "kvm-acceptance", "version": "1"}})
            rpc(2, "session/load", {"sessionId": source, "cwd": str(self.local), "mcpServers": []})
            code, _, error = self.cli(["sandbox", "detach", "managed"], check=False)
            require(code != 0 and "local runtime" in error, "ACP restore did not retain managed lease")
            rpc(3, "session/new", {"cwd": str(self.local), "mcpServers": []})
            self.cli(["sandbox", "detach", "managed"])
            process.stdin.close()
            require(process.wait(timeout=30) == 0, "ACP shutdown failed")
        self.cli(["sandbox", "attach", "managed"])
        event("sdk_bang_pwd_saved_restore_and_acp_restore_lease_release", session=source, paid_model_requests=0)
        master, slave = pty.openpty()
        termios.tcsetwinsize(slave, (48, 160))
        try:
            with child([self.args.caudra, "--no-plugins", "--model", "anthropic/claude-sonnet-4-6", "--sandbox", "managed"], cwd=self.local,
                       env={**self.env, "TERM": "xterm-256color", "COLUMNS": "160", "LINES": "48"}, stdin=slave, stdout=slave, stderr=slave) as process:
                def wait_text(expected):
                    transcript = ""
                    deadline = time.monotonic() + 60
                    while expected not in transcript:
                        require(select.select([master], [], [], max(0, deadline - time.monotonic()))[0], f"TUI waiting for {expected}: {safe(transcript[-2000:])}")
                        transcript += re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", os.read(master, 65536).decode(errors="replace"))
                        require(len(transcript) < LIMIT and time.monotonic() < deadline, "TUI deadline/output limit")
                wait_text("claude-sonnet-4-6")
                os.write(master, b"/sandbox\r")
                wait_text("managed")
                os.write(master, b"d\x1b[13;5us")
                wait_text("No reconnect or local fallback")
                require(process.wait(timeout=30) == 0, "TUI detach failed")
            require(json.loads(self.cli(["sandbox", "list"])[1])[0]["detached"], "TUI did not mark detached")
            require(self.lifecycle("GET", f"instances/{self.sid}")["state"] == "running", "TUI detach stopped VM")
            event("real_vm_pty_tui_exclusive_detach")
        finally:
            os.close(master)
            os.close(slave)

    def execute(self, lock_fd):
        self.build()
        self.configure()
        virsh("pool-create-as", self.pool, "dir", "--target", str(self.temp / "pool"))
        self.pool_created = True
        xml = ET.fromstring(virsh("pool-dumpxml", self.pool))
        require(xml.findtext("target/path") == str(self.temp / "pool"), "private pool mismatch")
        event("private_pool", name=self.pool, uuid=xml.findtext("uuid"), ttl_s=TTL, cpus=2, memory_mib=1024, disk_mib=8192, persist_max_age_s=3600)
        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SyntheticProvider)
        self.env["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{provider.server_port}"
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryFile() as log, child([str(self.temp / "bin/e2b-locald"), *self.flags], env=self.daemon_env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, pass_fds=(lock_fd,)) as daemon:
                ready = False
                try:
                    deadline = time.monotonic() + 30
                    while True:
                        require(daemon.poll() is None, "daemon exited at startup")
                        try:
                            if request(self.api, "GET", "/health")[0] == 204:
                                break
                        except OSError:
                            pass
                        require(time.monotonic() < deadline, "daemon readiness deadline")
                        time.sleep(0.1)
                    ready = True
                    doctor = json.loads(self.cli(["sandbox", "doctor", "--provider", "fixture"])[1])
                    require(doctor, "empty doctor result")
                    require(json.loads(self.cli(["sandbox", "list", "--provider", "fixture"])[1]) == [], "private provider not empty")
                    started = time.monotonic()
                    record = json.loads(self.cli(["sandbox", "create", "managed", "--profile", "acceptance"])[1])
                    self.sid = record["instance"]["sandboxID"]
                    self.inventory()
                    domain_xml = ET.fromstring(virsh("dumpxml", self.owned[self.sid]["domain"]))
                    require(any("restrict=on" in node.attrib.get("value", "") for node in domain_xml.iter()), "VM egress is not hypervisor enforced")
                    event("doctor_list_typed_create_inspect", seconds=round(time.monotonic() - started, 3), **self.owned[self.sid])
                    self.cli(["sandbox", "inspect", "managed"])
                    self.data_checks()
                    self.lifecycle_checks()
                    self.sessions()
                    self.cli(["sandbox", "delete", "managed", "--yes"])
                    require(not self.lifecycle("GET", "instances")["items"], "delete left instance")
                    event("explicit_delete", sandbox=self.sid)
                except BaseException:
                    log.seek(0)
                    event("sanitized_daemon_diagnostics", tail=safe(log.read(LIMIT).decode(errors="replace"))[-6000:])
                    raise
                finally:
                    self.inventory()
                    if ready and daemon.poll() is None:
                        for instance in self.lifecycle("GET", "instances")["items"]:
                            require(instance["ownerID"] == self.owner, "foreign lifecycle instance")
                            self.lifecycle("DELETE", f"instances/{instance['sandboxID']}", {"expectedExecutionID": instance["executionID"], "expectedRevision": instance["revision"]})
        finally:
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=5)

    def cleanup(self):
        self.inventory()
        for row in self.owned.values():
            names = virsh("list", "--all", "--name").split()
            if row["domain"] in names:
                xml = virsh("dumpxml", row["domain"])
                require(row["sandbox"] in xml and self.owner in xml and str(self.temp / "pool" / row["volume"]) in xml, "domain ownership not proven")
                virsh("destroy", row["domain"])
                if row["domain"] in virsh("list", "--all", "--name").split():
                    virsh("undefine", row["domain"], "--nvram")
            if (self.temp / "pool" / row["volume"]).exists():
                require(self.pool_created, "volume ownership pool missing")
                virsh("vol-delete", row["volume"], "--pool", self.pool)
        if self.pool_created:
            require(not list((self.temp / "pool").iterdir()), "unknown files in private pool; preserving")
            virsh("pool-destroy", self.pool)
        require(not virsh("list", "--all", "--name").strip(), "domains remain; no foreign cleanup attempted")
        event("owned_resources_cleaned", domains=0, pool_removed=self.pool_created)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-real-kvm", action="store_true", required=True)
    parser.add_argument("--source-image", required=True)
    parser.add_argument("--e2b", default=str(REPO.parent / "e2b-libvirt"))
    parser.add_argument("--caudra", default=str(REPO / "target/debug/caudra"))
    parser.add_argument("--workcell", default=str(REPO.parent / "workcell-mcp/target/release/workcell-mcp"))
    parser.add_argument("--iron-proxy", default=shutil.which("iron-proxy"))
    args = parser.parse_args()
    require(args.iron_proxy and os.access("/dev/kvm", os.R_OK | os.W_OK), "iron-proxy or KVM unavailable")
    require(shutil.disk_usage(tempfile.gettempdir()).free >= 20 * 1024**3, "need at least 20 GiB free for private image/import")
    os.umask(0o077)
    lock_path = Path(os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR") or "/tmp") / f"e2b-libvirt-{os.getuid()}-session.lock"
    fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid(), "unsafe canonical lock")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(run(["flock", "-n", str(lock_path), "true"], check=False)[0] == 1, "lock exclusion failed")
        for proc in Path("/proc").iterdir():
            if proc.name.isdigit():
                try:
                    require((proc / "comm").read_text().strip() != "e2b-locald", f"another e2b-locald exists (PID {proc.name}); refusing even when canonical lock is free")
                except (FileNotFoundError, PermissionError):
                    pass
        require(not virsh("list", "--all", "--name").strip(), "nonempty session namespace; refusing")
        temp = Path(tempfile.mkdtemp(prefix="caudra-real-kvm-"))
        suite = Acceptance(args, temp)
        event("canonical_lock_and_empty_namespace", lock=str(lock_path))
        try:
            suite.execute(fd)
        finally:
            try:
                suite.cleanup()
            except BaseException:
                event("cleanup_failed_preserving_owned_state", path=str(temp))
                raise
            shutil.rmtree(temp)
        event("PASS_real_KVM_acceptance")
    finally:
        os.close(fd)


if __name__ == "__main__":
    def deadline(_signum, _frame):
        raise TimeoutError("real KVM acceptance exceeded its 20-minute deadline")

    signal.signal(signal.SIGALRM, deadline)
    signal.signal(signal.SIGTERM, deadline)
    signal.signal(signal.SIGINT, deadline)
    signal.alarm(1200)
    try:
        main()
    except Exception as error:  # noqa: BLE001 -- Never expose unsanitized credential-bearing tracebacks.
        event("FAIL_real_KVM_acceptance", error=safe(error))
        raise SystemExit(1) from None
