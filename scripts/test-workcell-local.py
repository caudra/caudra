#!/usr/bin/env python3
"""Run the real authenticated remote adapter suite, without external services."""

import contextlib
import http.client
import http.server
import json
import os
import pty
import re
import secrets
import select
import signal
import socket
import ssl
import subprocess
import tempfile
import termios
import threading
import time
from pathlib import Path
from typing import Any, ClassVar, cast


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
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def initialize_fixture_root(root, home):
    (root / "nested").mkdir()
    (root / "fixture.txt").write_text("original\n")
    (root / "nested" / "fixture.txt").write_text("nested original\n")
    (root / "AGENTS.md").write_text("Remote integration instruction sentinel.\n")
    # Workcell declares this basename; a Caudra that cannot account for it used
    # to lose the whole manifest, so the fixture carries one.
    (root / "AGENTS.local.md").write_text("Remote integration overlay sentinel.\n")
    (root / ".agents" / "skills" / "fixture").mkdir(parents=True)
    (root / ".agents" / "skills" / "fixture" / "SKILL.md").write_text(
        "---\nname: fixture\ndescription: Integration sentinel\n---\nFixture skill body.\n"
    )
    for args in (
        ["init", "-q"],
        ["add", "."],
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "commit",
            "-qm",
            "fixture",
        ],
    ):
        subprocess.run(
            ["git", *args],
            cwd=root,
            check=True,
            timeout=10,
            env={"PATH": os.environ["PATH"], "HOME": str(home)},
        )


def main():
    binary = Path(os.environ["WORKCELL_TEST_BINARY"]).resolve(strict=True)
    fault_mode = os.environ.get("WORKCELL_TEST_FAULT_MODE", "disconnect")
    sandbox_only = os.environ.get("CAUDRA_TEST_SANDBOX") == "only"
    assert fault_mode in ("disconnect", "http503"), fault_mode
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(
        prefix="caudra-workcell-integration-"
    ) as directory:
        temp = Path(directory)
        for name in ("root", "snapshots", "transfers", "state", "config", "home"):
            (temp / name).mkdir(mode=0o700)
        root = temp / "root"
        initialize_fixture_root(root, temp / "home")
        token = temp / "token"
        token.write_text(secrets.token_hex(32))
        token.chmod(0o600)
        ca, key = temp / "ca.pem", temp / "key.pem"
        subprocess.run(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(key),
                "-out",
                str(ca),
                "-days",
                "1",
                "-subj",
                "/CN=Caudra local integration",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
        )
        key.chmod(0o600)
        leaf_key, leaf = temp / "leaf-key.pem", temp / "leaf.pem"
        request, extensions = temp / "leaf.csr", temp / "extensions"
        extensions.write_text(
            "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
        )
        for command in (
            [
                "openssl",
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(leaf_key),
                "-out",
                str(request),
                "-subj",
                "/CN=localhost",
            ],
            [
                "openssl",
                "x509",
                "-req",
                "-in",
                str(request),
                "-CA",
                str(ca),
                "-CAkey",
                str(key),
                "-CAcreateserial",
                "-out",
                str(leaf),
                "-days",
                "1",
                "-extfile",
                str(extensions),
            ],
        ):
            subprocess.run(
                command,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=20,
            )
        leaf_key.chmod(0o600)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]

        class Proxy(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            execute_requests: ClassVar[list[tuple[object, object, object]]] = []
            provider_requests: ClassVar[list[dict[str, object]]] = []
            fault_lock = threading.Lock()
            lifecycle: ClassVar[dict] = {}
            lifecycle_calls: ClassVar[list[str]] = []
            mcp_methods: ClassVar[list[str]] = []
            rpc_active: ClassVar[dict] = {}
            operation_states: ClassVar[dict] = {}
            preparation_tools: ClassVar[dict[str, str]] = {}
            operation_limits: ClassVar[dict] = {}
            prepare_barrier: ClassVar[threading.Barrier | None] = None

            def log_message(self, format: str, *args: object) -> None:
                pass

            def forward(self):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=35)
                rpc_key = None
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if length > 8 * 1024 * 1024:
                        self.send_error(413)
                        return
                    headers = {
                        k: v
                        for k, v in self.headers.items()
                        if k.lower() not in ("host", "connection", "transfer-encoding")
                    }
                    request_body = self.rfile.read(length)
                    if self.path.startswith("/daemon/v1/") and self.lifecycle:
                        assert self.headers.get("X-API-Key") == self.lifecycle["key"]
                        assert not self.headers.get("Authorization")
                        self.lifecycle_calls.append(self.path)
                        route = self.path.removeprefix("/daemon/v1/")
                        if route == "discover":
                            value = self.lifecycle["discovery"]
                        elif route.startswith("templates/"):
                            value = self.lifecycle["template"]
                        elif route == "templates":
                            value = {
                                "items": [self.lifecycle["template"]],
                                "nextAfter": "",
                            }
                        elif route.endswith("/credentials"):
                            value = {
                                "instance": self.lifecycle["instance"],
                                "trafficAccessToken": token.read_text(),
                                "mcpPath": "/sandboxes/fixture/mcp",
                                "filesPath": "/files",
                                "credentialScope": "sandbox_lifetime",
                            }
                        elif route == "instances":
                            value = {
                                "items": [self.lifecycle["instance"]],
                                "nextAfter": "",
                            }
                        else:
                            assert route == "instances/fixture", route
                            value = self.lifecycle["instance"]
                        body = json.dumps(value).encode()
                        self.send_response(200)
                        self.send_header("Cache-Control", "no-store")
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    if self.path.startswith("/sandboxes/fixture/"):
                        assert (
                            self.headers.get("Authorization")
                            == "Bearer " + token.read_text()
                        )
                        assert not self.headers.get("X-API-Key")
                        self.path = self.path.removeprefix("/sandboxes/fixture")
                    if self.path.startswith("/files"):
                        assert self.path.startswith("/files?reviewed=v1&"), self.path
                    if self.path == "/v1/messages":
                        self.provider_requests.append(json.loads(request_body))
                        events = [
                            {
                                "type": "message_start",
                                "message": {
                                    "id": "fixture",
                                    "type": "message",
                                    "role": "assistant",
                                    "model": "claude-sonnet-4-6",
                                    "content": [],
                                    "usage": {"input_tokens": 1, "output_tokens": 0},
                                },
                            },
                            {
                                "type": "content_block_start",
                                "index": 0,
                                "content_block": {"type": "text", "text": ""},
                            },
                            {
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {
                                    "type": "text_delta",
                                    "text": "Synthetic provider sentinel.",
                                },
                            },
                            {"type": "content_block_stop", "index": 0},
                            {
                                "type": "message_delta",
                                "delta": {"stop_reason": "end_turn"},
                                "usage": {"output_tokens": 1},
                            },
                            {"type": "message_stop"},
                        ]
                        body = "".join(
                            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n"
                            for event in events
                        ).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    method = (
                        json.loads(request_body).get("method")
                        if request_body and self.path == "/mcp"
                        else None
                    )
                    if method:
                        self.mcp_methods.append(method)
                        request = json.loads(request_body)
                        rpc_key = f"{self.client_address[1]}:{request['id']}"
                        with self.fault_lock:
                            self.rpc_active[rpc_key] = {
                                "method": method,
                                "tool": request.get("params", {}).get("tool"),
                            }
                    if (
                        method == "ai.workcell/watch-open"
                        and (
                            temp / "drop-operation-response.watch-unavailable"
                        ).exists()
                    ):
                        assert (
                            self.headers.get("Authorization")
                            == "Bearer " + token.read_text()
                        )
                        body = json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "id": request["id"],
                                "error": {
                                    "code": -32000,
                                    "message": "injected watch setup refusal",
                                    "data": {
                                        "code": "watch_unavailable",
                                        "phase": "setup",
                                    },
                                },
                            }
                        ).encode()
                        self.send_response(400)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    batch_gate = temp / "batch-execute-gate"
                    if method == "ai.workcell/prepare":
                        with self.fault_lock:
                            arm = temp / "batch-prepare-barrier"
                            if arm.exists():
                                arm.unlink()
                                type(self).prepare_barrier = threading.Barrier(3)
                            barrier = self.prepare_barrier
                        if barrier is not None:
                            barrier.wait(timeout=15)
                            with self.fault_lock:
                                if self.prepare_barrier is barrier:
                                    type(self).prepare_barrier = None
                    if method in (
                        "ai.workcell/execute",
                        "ai.workcell/status",
                        "ai.workcell/cancel",
                    ):
                        with (
                            self.fault_lock,
                            (temp / "batch-rpc-trace").open("a") as trace,
                        ):
                            trace.write(
                                json.dumps(
                                    {
                                        "method": method,
                                        "params": json.loads(request_body)["params"],
                                    }
                                )
                                + "\n"
                            )
                    hold_execute = False
                    if method == "ai.workcell/execute":
                        with self.fault_lock:
                            preparation_id = json.loads(request_body)["params"][
                                "preparationId"
                            ]
                            if (
                                batch_gate.exists()
                                and self.preparation_tools.get(preparation_id)
                                == "shell"
                            ):
                                batch_gate.unlink()
                                hold_execute = True
                    if hold_execute:
                        batch_gate.with_suffix(".entered").touch()
                        deadline = time.monotonic() + 20
                        while not batch_gate.with_suffix(".release").exists():
                            if time.monotonic() >= deadline:
                                self.send_error(504, "fixture execute gate timed out")
                                return
                            time.sleep(0.01)
                    if (
                        method
                        in (
                            "ai.workcell/transfer/publicationStatus",
                            "ai.workcell/transfer/directoryStatus",
                        )
                        and (temp / "drop-operation-response.unknown").exists()
                    ):
                        request = json.loads(request_body)
                        body = json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "id": request["id"],
                                "result": {
                                    "version": "v1",
                                    "publicationId": request["params"]["publicationId"],
                                    "state": "unknown",
                                    "preparationId": None,
                                    "invocationId": None,
                                    "requestDigest": None,
                                    "directory"
                                    if method == "ai.workcell/transfer/directoryStatus"
                                    else "file": None,
                                },
                            }
                        ).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    restart = temp / "drop-operation-response.restart"
                    if method == "server/discover" and restart.exists():
                        restart_server()
                        restart.unlink()
                    preparation = temp / "drop-operation-response.preparation"
                    if (
                        method == "ai.workcell/execute"
                        and preparation.exists()
                        and json.loads(request_body)["params"]["preparationId"]
                        == preparation.read_text()
                    ):
                        request = json.loads(request_body)
                        with self.fault_lock:
                            self.execute_requests.append(
                                (
                                    request["id"],
                                    request["params"]["preparationId"],
                                    request["params"]["invocationId"],
                                )
                            )
                            count = temp / "drop-operation-response.count"
                            count.write_text(str(len(self.execute_requests)))
                    if (
                        temp / "drop-operation-response"
                    ).exists() and method == "ai.workcell/status":
                        self.send_error(503)
                        return
                    connection.request(self.command, self.path, request_body, headers)
                    response = connection.getresponse()
                    body = response.read(16 * 1024 * 1024 + 1)
                    if method:
                        if response.getheader("Content-Type", "").startswith(
                            "text/event-stream"
                        ):
                            messages = [
                                json.loads(line[5:])
                                for line in body.splitlines()
                                if line.startswith(b"data:")
                            ]
                            payload = next(
                                (message for message in messages if "id" in message), {}
                            )
                        else:
                            payload = json.loads(body)
                        result = payload.get("result", {})
                        params = json.loads(request_body).get("params", {})
                        with self.fault_lock:
                            preparation_id = result.get(
                                "preparationId", params.get("preparationId")
                            )
                            if preparation_id and result.get("state"):
                                self.operation_states[preparation_id] = result["state"]
                            if (
                                method == "ai.workcell/prepare"
                                and preparation_id
                                and "error" not in payload
                            ):
                                self.operation_states[preparation_id] = "prepared"
                                self.preparation_tools[preparation_id] = params["tool"]
                            if (
                                method == "ai.workcell/release"
                                and preparation_id
                                and result.get("released")
                            ):
                                self.operation_states.pop(preparation_id, None)
                                self.preparation_tools.pop(preparation_id, None)
                            if method == "server/discover":
                                pending = [result]
                                while pending:
                                    item = pending.pop()
                                    if isinstance(item, dict):
                                        if "maxLedgerBytes" in item:
                                            self.operation_limits.update(item)
                                        pending.extend(item.values())
                                    elif isinstance(item, list):
                                        pending.extend(item)
                            with (temp / "rpc-diagnostics").open("a") as trace:
                                trace.write(
                                    json.dumps(
                                        {
                                            "method": method,
                                            "tool": params.get("tool"),
                                            "preparationId": preparation_id,
                                            "result": result
                                            if method == "ai.workcell/release"
                                            else {"state": result.get("state")},
                                            "error": payload.get("error"),
                                        }
                                    )
                                    + "\n"
                                )
                            if (
                                payload.get("error", {}).get("data", {}).get("code")
                                == "quota_exceeded"
                            ):
                                counts = {
                                    state: list(self.operation_states.values()).count(
                                        state
                                    )
                                    for state in set(self.operation_states.values())
                                }
                                print(
                                    "Quota diagnostics:",
                                    json.dumps(
                                        {
                                            "method": method,
                                            "tool": params.get("tool"),
                                            "limits": self.operation_limits,
                                            "in_flight": self.rpc_active,
                                            "observed_operation_states": counts,
                                        }
                                    ),
                                    flush=True,
                                )
                            self.rpc_active.pop(rpc_key, None)
                    shorten = temp / "short-preparation-ttl"
                    if method == "ai.workcell/prepare" and shorten.exists():

                        def lapse_soon(payload):
                            result = payload.get("result", {})
                            if "expiresAtUnixMs" in result:
                                result["expiresAtUnixMs"] = int(
                                    time.time() * 1000
                                ) + int(shorten.read_text())
                                (temp / "short-preparation-expiry").write_text(
                                    str(result["expiresAtUnixMs"])
                                )
                                shorten.unlink()
                            return json.dumps(payload).encode()

                        with self.fault_lock:
                            if shorten.exists():
                                if response.getheader("Content-Type", "").startswith(
                                    "text/event-stream"
                                ):
                                    body = b"".join(
                                        b"data: "
                                        + lapse_soon(json.loads(line[5:]))
                                        + b"\n"
                                        if line.startswith(b"data:")
                                        else line
                                        for line in body.splitlines(keepends=True)
                                    )
                                else:
                                    body = lapse_soon(json.loads(body))
                    if (
                        method == "ai.workcell/execute"
                        and (temp / "drop-operation-response.progress").exists()
                    ):

                        def lose_progress(payload):
                            result = payload.get("result", {})
                            if result.get("state") == "completed":
                                result["progress"] = []
                                result["progressMetadata"]["firstRetainedSequence"] = (
                                    None
                                )
                                result["progressMetadata"]["gapBeforeFirst"] = True
                            return json.dumps(payload).encode()

                        if response.getheader("Content-Type", "").startswith(
                            "text/event-stream"
                        ):
                            body = b"".join(
                                b"data: " + lose_progress(json.loads(line[5:])) + b"\n"
                                if line.startswith(b"data:")
                                else line
                                for line in body.splitlines(keepends=True)
                            )
                        else:
                            body = lose_progress(json.loads(body))
                    if method in ("ai.workcell/cancel", "ai.workcell/status"):
                        if response.getheader("Content-Type", "").startswith(
                            "text/event-stream"
                        ):
                            messages = [
                                json.loads(line[5:])
                                for line in body.decode().splitlines()
                                if line.startswith("data:")
                            ]
                            payload = next(
                                (message for message in messages if "id" in message), {}
                            )
                        else:
                            payload = json.loads(body)
                        with self.fault_lock, (temp / "rpc-trace").open("a") as trace:
                            trace.write(
                                json.dumps(
                                    {"method": method, "result": payload.get("result")}
                                )
                                + "\n"
                            )
                    if (
                        self.path.startswith("/files?reviewed=v1&download=")
                        and body
                        and (temp / "drop-operation-response.corrupt").exists()
                    ):
                        body = bytes([body[0] ^ 1]) + body[1:]
                    if (
                        temp / "drop-operation-response"
                    ).exists() and method == "ai.workcell/execute":
                        if fault_mode == "http503":
                            self.send_error(503)
                        else:
                            self.close_connection = True
                            self.connection.shutdown(socket.SHUT_RDWR)
                        return
                    if response.status >= 400 and self.headers.get("Authorization"):
                        print(
                            "Authenticated upstream failure:",
                            method,
                            response.status,
                            body[:2000].decode(errors="replace"),
                            flush=True,
                        )
                    if len(body) > 16 * 1024 * 1024:
                        self.send_error(502)
                        return
                    self.send_response(response.status)
                    for k, v in response.getheaders():
                        if k.lower() not in (
                            "connection",
                            "transfer-encoding",
                            "content-length",
                        ):
                            self.send_header(k, v)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                finally:
                    if rpc_key is not None:
                        with self.fault_lock:
                            self.rpc_active.pop(rpc_key, None)
                    connection.close()

            do_POST = forward
            do_GET = forward

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(leaf, leaf_key)
        proxy.socket = context.wrap_socket(proxy.socket, server_side=True)
        thread = threading.Thread(target=proxy.serve_forever)
        thread.start()

        class DenyEgress(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_CONNECT(self):
                self.send_error(403, "fixture egress denied")

            do_GET = do_CONNECT

        egress = http.server.ThreadingHTTPServer(("127.0.0.1", 0), DenyEgress)
        egress_thread = threading.Thread(target=egress.serve_forever)
        egress_thread.start()
        try:
            args = [
                str(binary),
                str(root),
                "--transport",
                "http",
                "--port",
                str(port),
                "--http-token-file",
                str(token),
                "--allow-write",
                "--yolo",
                "--http-proxy",
                f"http://127.0.0.1:{egress.server_port}",
                "--snapshot-root",
                str(temp / "snapshots"),
            ]
            for group in ("files", "code_graph", "web", "shell", "python_execution"):
                args += ["--tool-group", group]
            for flag in (
                "server-id",
                "workspace-id",
                "workspace-generation",
                "root-project-id",
                "principal-id",
            ):
                args += ["--remote-" + flag, "integration-" + flag]
            server_env = {
                "PATH": os.environ["PATH"],
                "HOME": str(temp / "home"),
                "XDG_CONFIG_HOME": str(temp / "config"),
            }
            if worker := os.environ.get("WORKCELL_MCP_CODE_WORKER"):
                server_env["WORKCELL_MCP_CODE_WORKER"] = str(
                    Path(worker).resolve(strict=True)
                )
            with (
                (temp / "server.log").open("w+") as log,
                contextlib.ExitStack() as servers,
            ):

                def start_server():
                    process = servers.enter_context(
                        child(args, env=server_env, stdout=log, stderr=log)
                    )
                    deadline = time.monotonic() + 30
                    while True:
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(
                                "Workcell startup failed: " + log.read()[-4000:]
                            )
                        try:
                            with socket.create_connection(
                                ("127.0.0.1", port), timeout=0.2
                            ):
                                return process
                        except OSError:
                            if time.monotonic() >= deadline:
                                raise TimeoutError("Workcell startup")
                            time.sleep(0.05)

                server = start_server()

                def restart_server():
                    nonlocal server
                    os.killpg(server.pid, signal.SIGKILL)
                    server.wait(timeout=5)
                    Proxy.rpc_active.clear()
                    Proxy.operation_states.clear()
                    Proxy.preparation_tools.clear()
                    server = start_server()

                env = os.environ.copy()
                # The registry dispatch fixture nests large debug-build futures.
                env.setdefault("RUST_MIN_STACK", str(16 * 1024 * 1024))
                env.update(
                    WORKCELL_TEST_ENDPOINT=f"https://127.0.0.1:{proxy.server_port}/mcp",
                    WORKCELL_TEST_TOKEN_FILE=str(token),
                    WORKCELL_TEST_ROOT=str(root),
                    WORKCELL_TEST_STATE=str(temp / "state"),
                    SSL_CERT_FILE=str(ca),
                    WORKCELL_TEST_RPC_TRACE=str(temp / "rpc-trace"),
                    WORKCELL_TEST_FAULT=str(temp / "drop-operation-response"),
                )
                command = [
                    str(repo / "scripts" / "dev-cargo.sh"),
                    "test",
                    "-p",
                    "caudra-workcell",
                    "--test",
                    "authenticated_local",
                ]
                if not sandbox_only:
                    with child(
                        [*command, "unsupported_server", "--", "--nocapture"],
                        cwd=repo,
                        env={**env, "WORKCELL_TEST_UNSUPPORTED": "1"},
                    ) as tests:
                        assert tests.wait(timeout=600) == 0, (
                            "unsupported server was accepted"
                        )
                    assert Proxy.mcp_methods == ["server/discover"], Proxy.mcp_methods
                args += [
                    "--tool-group",
                    "transfer",
                    "--transfer-root",
                    str(temp / "transfers"),
                ]
                if not sandbox_only:
                    change_root = args.index("--snapshot-root")
                    change_args = args[change_root : change_root + 2]
                    del args[change_root : change_root + 2]
                    restart_server()
                    with child(
                        [*command, "unrecorded_server", "--", "--nocapture"],
                        cwd=repo,
                        env={**env, "WORKCELL_TEST_UNRECORDED": "1"},
                    ) as tests:
                        assert tests.wait(timeout=600) == 0, (
                            "a host without change records was refused"
                        )
                    args[change_root:change_root] = change_args
                restart_server()
                if not sandbox_only:
                    metadata_root = temp / "metadata-root"
                    metadata_root.mkdir(mode=0o700)
                    args[1] = str(metadata_root)
                    restart_server()
                    with child(
                        [
                            *command,
                            os.environ.get(
                                "WORKCELL_TEST_METADATA_FILTER", "metadata_"
                            ),
                            "--",
                            "--nocapture",
                            "--test-threads=1",
                        ],
                        cwd=repo,
                        env={**env, "WORKCELL_TEST_METADATA_ROOT": str(metadata_root)},
                    ) as tests:
                        metadata_status = tests.wait(timeout=600)
                    args[1] = str(root)
                    restart_server()
                    test_filter = os.environ.get("WORKCELL_TEST_FILTER")
                    selected = [test_filter] if test_filter else []
                    with child(
                        [
                            *command,
                            *selected,
                            "--",
                            "--nocapture",
                            "--test-threads=1",
                            "--skip",
                            "unsupported_server",
                            "--skip",
                            "unrecorded_server",
                            "--skip",
                            "metadata_",
                        ],
                        cwd=repo,
                        env=env,
                    ) as tests:
                        if tests.wait(timeout=600) != 0:
                            print(
                                "Fault execute requests:",
                                Proxy.execute_requests,
                                flush=True,
                            )
                            raise RuntimeError("authenticated local integration failed")
                    assert metadata_status == 0, "metadata/Workbench integration failed"
                if os.environ.get("CAUDRA_TEST_BINARY"):
                    root = temp / "entrypoint-root"
                    root.mkdir(mode=0o700)
                    initialize_fixture_root(root, temp / "home")
                    args[1] = str(root)
                    restart_server()
                    caudra = str(
                        Path(os.environ["CAUDRA_TEST_BINARY"]).resolve(strict=True)
                    )
                    local = temp / "client"
                    local.mkdir()
                    isolated = {
                        "PATH": os.environ["PATH"],
                        "HOME": str(temp / "home"),
                        "SSL_CERT_FILE": str(ca),
                        "NO_COLOR": "1",
                        "XDG_CONFIG_HOME": str(temp / "config"),
                        "XDG_STATE_HOME": str(temp / "client-state"),
                        "XDG_DATA_HOME": str(temp / "client-data"),
                        "XDG_CACHE_HOME": str(temp / "client-cache"),
                        "ANTHROPIC_API_KEY": "fixture-not-a-real-key",
                        "ANTHROPIC_BASE_URL": f"https://127.0.0.1:{proxy.server_port}",
                        "HTTP_PROXY": "http://127.0.0.1:9",
                        "HTTPS_PROXY": "http://127.0.0.1:9",
                        "ALL_PROXY": "http://127.0.0.1:9",
                        "NO_PROXY": "127.0.0.1,localhost",
                    }
                    for app in ("caudra", "caudra-debug"):
                        config = temp / "config" / app
                        config.mkdir(mode=0o700, exist_ok=True)
                        (config / "permissions.toml").write_text(
                            'default = "allow"\n[file_index]\nallow = ["*"]\n'
                        )
                        (config / "caudra.toml").write_text(
                            "[experimental]\nsandboxes = true\nremote_workcell = true\n"
                        )

                    def run_cli(arguments, stdin=""):
                        with child(
                            [caudra, *arguments],
                            cwd=local,
                            env=isolated,
                            stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                        ) as process:
                            stdout, stderr = process.communicate(
                                stdin.encode(), timeout=45
                            )
                            assert len(stdout) + len(stderr) < 4 * 1024 * 1024
                            return process.returncode, stdout.decode(), stderr.decode()

                    code, _, error = run_cli(
                        ["auth", "workcell", "set", "integration", "--stdin"],
                        token.read_text(),
                    )
                    assert code == 0, error
                    if sandbox_only:
                        sandbox_runtime_checks(
                            run_cli,
                            caudra,
                            isolated,
                            local,
                            temp,
                            proxy,
                            Proxy,
                            args,
                            restart_server,
                        )
                        return
                    remote = [
                        "--no-plugins",
                        "--model",
                        "anthropic/claude-sonnet-4-6",
                        "--workcell-endpoint",
                        env["WORKCELL_TEST_ENDPOINT"],
                        "--workcell-cwd",
                        ".",
                        "--workcell-credential-ref",
                        "credential:integration",
                    ]
                    checks = [
                        (
                            "remote status controller",
                            ["remote", "status"],
                            "",
                            "pending operations: 0",
                        ),
                        (
                            "remote reconcile controller",
                            ["remote", "reconcile"],
                            "",
                            "No mutation resent",
                        ),
                        (
                            "SDK remote status",
                            [
                                "--print",
                                "--input-format",
                                "stream-json",
                                "--output-format",
                                "stream-json",
                            ],
                            json.dumps(
                                {
                                    "type": "user",
                                    "message": {
                                        "role": "user",
                                        "content": "/remote status",
                                    },
                                }
                            )
                            + "\n",
                            "pending operations: 0",
                        ),
                        (
                            "SDK parent navigation",
                            [
                                "--print",
                                "--input-format",
                                "stream-json",
                                "--output-format",
                                "stream-json",
                            ],
                            "".join(
                                json.dumps(
                                    {
                                        "type": "user",
                                        "message": {"role": "user", "content": command},
                                    }
                                )
                                + "\n"
                                for command in [
                                    "/cd nested",
                                    "/cd ..",
                                    "/cd nested",
                                    "/cd ../nested",
                                ]
                            ),
                            '"cwd":"."',
                        ),
                        (
                            "startup prompt",
                            ["prompt"],
                            "",
                            "Remote integration instruction sentinel",
                        ),
                        ("skills discovery", ["skills", "--names"], "", "fixture"),
                        ("skill body", ["skills", "fixture"], "", "Fixture skill body"),
                        (
                            "root index dispatch",
                            ["--yolo", "--allowed-tools", "file_index", "index", "."],
                            "",
                            "fixture.txt",
                        ),
                        (
                            "print zero-turn startup",
                            [
                                "--print",
                                "--prompt",
                                "fixture",
                                "--max-turns",
                                "0",
                                "--output-format",
                                "json",
                            ],
                            "",
                            '"num_turns":0',
                        ),
                        (
                            "SDK direct command",
                            [
                                "--print",
                                "--yolo",
                                "--input-format",
                                "stream-json",
                                "--output-format",
                                "stream-json",
                            ],
                            json.dumps(
                                {
                                    "type": "user",
                                    "message": {"role": "user", "content": "!pwd"},
                                }
                            )
                            + "\n",
                            '"num_turns":0',
                        ),
                        (
                            "ACP initialize",
                            ["acp"],
                            json.dumps(
                                {
                                    "jsonrpc": "2.0",
                                    "id": 1,
                                    "method": "initialize",
                                    "params": {
                                        "protocolVersion": 1,
                                        "clientCapabilities": {},
                                        "clientInfo": {
                                            "name": "fixture",
                                            "version": "1",
                                        },
                                    },
                                }
                            )
                            + "\n",
                            '"protocolVersion":1',
                        ),
                    ]
                    failures = []
                    for label, arguments, stdin, expected in checks:
                        provider_count = len(Proxy.provider_requests)
                        try:
                            code, output, error = run_cli([*remote, *arguments], stdin)
                        except subprocess.TimeoutExpired:
                            failures.append(label)
                            print(
                                "FAIL", label, "bounded 45-second timeout", flush=True
                            )
                            continue
                        sdk_ok = label != "SDK direct command" or any(
                            item.get("type") == "result"
                            and not item.get("is_error")
                            and str(root) in item.get("result", "")
                            for item in (
                                json.loads(line)
                                for line in output.splitlines()
                                if line.startswith("{")
                            )
                        )
                        if label == "SDK parent navigation":
                            sdk_ok = "cwd_error" not in output
                        if (
                            code == 0
                            and expected in output
                            and sdk_ok
                            and len(Proxy.provider_requests) == provider_count
                        ):
                            print("PASS", label, flush=True)
                        else:
                            failures.append(label)
                            print(
                                "FAIL",
                                label,
                                "exit",
                                code,
                                (output + error)[-2000:],
                                flush=True,
                            )
                    for label, arguments, expected in [
                        (
                            "local allows cannot authorize remote index",
                            ["--allowed-tools", "file_index", "index", "."],
                            "Permission denied",
                        ),
                        (
                            "disabled index stays disabled under yolo",
                            [
                                "--yolo",
                                "--disallowed-tools",
                                "file_index",
                                "index",
                                ".",
                            ],
                            "index is disabled",
                        ),
                    ]:
                        code, output, error = run_cli([*remote, *arguments])
                        assert code != 0 and expected in output + error, (
                            label,
                            code,
                            output,
                            error,
                        )
                        print("PASS", label, flush=True)
                    for app in ("caudra", "caudra-debug"):
                        (temp / "config" / app / "permissions.toml").write_text(
                            "[file_index]\ndeny = true\n"
                        )
                    code, output, error = run_cli([*remote, "--yolo", "index", "."])
                    assert code != 0 and "Permission denied" in output + error, (
                        code,
                        output,
                        error,
                    )
                    print("PASS explicit index deny overrides yolo", flush=True)
                    before = len(Proxy.provider_requests)
                    code, output, error = run_cli(
                        [
                            *remote,
                            "--print",
                            "--prompt",
                            "fixture",
                            "--max-turns",
                            "1",
                            "--output-format",
                            "json",
                        ]
                    )
                    assert code == 0 and "Synthetic provider sentinel." in output, (
                        code,
                        output,
                        error,
                    )
                    assert len(Proxy.provider_requests) == before + 1
                    assert '"num_turns":1' in output
                    print("PASS print synthetic one-turn provider", flush=True)
                    if os.environ.get("CAUDRA_TEST_SANDBOX") == "1":
                        sandbox_runtime_checks(
                            run_cli,
                            caudra,
                            isolated,
                            local,
                            temp,
                            proxy,
                            Proxy,
                            args,
                            restart_server,
                        )
                    if failures:
                        raise RuntimeError(
                            "entrypoint checks failed: " + ", ".join(failures)
                        )
        finally:
            egress.shutdown()
            egress.server_close()
            egress_thread.join(timeout=5)
            proxy.shutdown()
            proxy.server_close()
            thread.join(timeout=5)


def sandbox_runtime_checks(
    run_cli, caudra, isolated, local, temp, proxy, handler, server_args, restart_server
):
    for flag in ("server-id", "workspace-id", "root-project-id"):
        server_args[server_args.index("--remote-" + flag) + 1] = "fixture"
    server_args[server_args.index("--remote-principal-id") + 1] = "fixture-owner"
    restart_server()
    digest = "sha256:" + "a" * 64
    topology = "slirp-unrestricted"
    lifecycle: dict[str, Any] = {
        "key": secrets.token_hex(32),
        "discovery": {
            "apiVersion": "1",
            "ownerID": "fixture-owner",
            "serverTime": "2026-09-20T12:00:00Z",
            "authentication": "api_key_namespace",
            "templateID": "base",
            "networkTopology": topology,
            "capabilities": {
                "idempotentCreate": True,
                "operationLookup": True,
                "conditionalMutations": True,
                "explicitCredentials": True,
                "persistentDisk": True,
                "memoryPause": False,
                "egressPolicy": False,
                "cancelCreate": True,
                "templateCatalog": True,
                "conditionalTemplateCreate": True,
                "warmStart": False,
                "localTemplateAdmin": True,
                "httpTemplateAdmin": False,
            },
            "limits": {
                "maxLeaseSeconds": 3600,
                "runtimeAdmission": 4,
                "operationJournalEntries": 4096,
                "listPageSize": 100,
                "resources": {"cpuCount": 4, "memoryMB": 4096, "diskSizeMB": 8192},
                "newKeyMaxAgeSeconds": 300,
                "newKeyFutureSkewSeconds": 30,
            },
            "retention": {
                "pausedDiskMaxAgeSeconds": 0,
                "operationHistorySeconds": 86400,
                "historyStartsAfter": "instance_removed",
            },
            "idempotencyKey": "uuidv7",
            "recovery": "query_operation_never_replay_unknown",
            "credentialScope": "sandbox_lifetime",
            "proxyOrigin": "client_configured",
        },
        "template": {
            "schemaVersion": 1,
            "id": "base",
            "architecture": "x86_64",
            "machine": "q35",
            "minimum": {"cpuCount": 1, "memoryMB": 512, "diskSizeMB": 1024},
            "defaults": {"cpuCount": 2, "memoryMB": 1024, "diskSizeMB": 1024},
            "networkTopology": topology,
            "workcell": {
                "version": "fixture",
                "sha256": "",
                "protocolVersion": "2026-07-28",
                "transferProtocol": "workcell-reviewed-v1",
                "remoteWorkspace": True,
                "workspaceSnapshots": True,
                "reviewedTransfer": True,
            },
            "build": {"recipe": "import", "recipeSHA256": "", "sourceRevision": ""},
            "revision": digest,
            "imageSHA256": digest,
            "warmStart": False,
            "image": {
                "format": "qcow2",
                "fileSizeBytes": 1024,
                "virtualSizeBytes": 1073741824,
                "clusterSize": 65536,
                "backingPolicy": "standalone",
            },
        },
        "instance": {
            "ownerID": "fixture-owner",
            "sandboxID": "fixture",
            "executionID": "execution",
            "revision": 1,
            "state": "running",
            "workspaceGeneration": "integration-workspace-generation",
            "expectedWorkcell": {
                "serverID": "fixture",
                "workspaceID": "fixture",
                "workspaceGeneration": "integration-workspace-generation",
                "projectID": "fixture",
                "principalID": "fixture-owner",
            },
            "template": {"id": "base", "revision": digest, "imageIdentity": digest},
            "resources": {"cpuCount": 2, "memoryMB": 1024, "diskSizeMB": 1024},
            "networkTopology": topology,
            "persistent": True,
            "pauseUnclean": False,
            "leaseDeadline": "2026-09-20T13:00:00Z",
            "retention": {"pausedDiskMaxAgeSeconds": 0, "deadline": None},
            "egress": {
                "enforced": False,
                "revision": "unrestricted",
                "effectiveRevision": "unrestricted",
                "policy": None,
            },
        },
    }
    handler.lifecycle = lifecycle
    origin = f"https://127.0.0.1:{proxy.server_port}"
    for app in ("caudra", "caudra-debug"):
        config = temp / "config" / app / "sandboxes.toml"
        config.write_text(
            f'version = 1\n[sandbox.providers.fixture]\nkind = "e2b-libvirt"\napi_endpoint = "{origin}"\nproxy_endpoint = "{origin}"\ncredential_ref = "sandbox-api:fixture"\n'
        )
        config.chmod(0o600)

    def checked(args, stdin=""):
        code, output, error = run_cli(args, stdin)
        assert code == 0, (
            args,
            code,
            handler.lifecycle_calls[-8:],
            (output + error)[-4000:],
        )
        return output

    checked(["auth", "sandbox", "set", "fixture", "--stdin"], handler.lifecycle["key"])
    checked(
        [
            "sandbox",
            "attach",
            "managed",
            "--provider",
            "fixture",
            "--instance",
            "fixture",
        ]
    )
    sdk = [
        "--no-plugins",
        "--model",
        "anthropic/claude-sonnet-4-6",
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--max-turns",
        "0",
    ]
    output = checked(
        [*sdk, "--sandbox", "managed"],
        json.dumps(
            {"type": "user", "message": {"role": "user", "content": "fixture history"}}
        )
        + "\n",
    )
    source = next(
        json.loads(line)["session_id"]
        for line in output.splitlines()
        if line.startswith("{") and "session_id" in json.loads(line)
    )
    instance = cast("dict[str, Any]", lifecycle["instance"])
    instance.update(state="paused", revision=2, leaseDeadline=None)
    checked(["sandbox", "inspect", "managed"])
    before = len(handler.lifecycle_calls)
    for target in (
        [],
        [
            "--workcell-endpoint",
            origin + "/mcp",
            "--workcell-cwd",
            ".",
            "--workcell-credential-ref",
            "credential:integration",
        ],
    ):
        output = checked([*sdk, "--session", source, "--fork-session", *target])
        assert source not in [
            item.get("session_id")
            for item in (
                json.loads(line) for line in output.splitlines() if line.startswith("{")
            )
        ]
        assert len(handler.lifecycle_calls) == before, (
            "history fork looked up paused source"
        )
    print(
        "PASS SDK paused-source forks: default local and explicit remote, no source lookup",
        flush=True,
    )
    handler.lifecycle["instance"].update(
        state="running", revision=3, leaseDeadline="2026-09-20T13:00:00Z"
    )
    checked(["sandbox", "inspect", "managed"])

    with child(
        [caudra, "--no-plugins", "--model", "anthropic/claude-sonnet-4-6", "acp"],
        cwd=local,
        env=isolated,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    ) as process:

        def rpc(request_id, method, params):
            process.stdin.write(
                (
                    json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "method": method,
                            "params": params,
                        }
                    )
                    + "\n"
                ).encode()
            )
            process.stdin.flush()
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                assert select.select(
                    [process.stdout], [], [], max(0, deadline - time.monotonic())
                )[0], "ACP response timeout"
                line = process.stdout.readline()
                assert line, "ACP exited before response"
                value = json.loads(line)
                if value.get("id") == request_id:
                    assert "error" not in value, value
                    return value["result"]
            raise TimeoutError(method)

        rpc(
            1,
            "initialize",
            {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"},
            },
        )
        rpc(
            2,
            "session/load",
            {"sessionId": source, "cwd": str(local), "mcpServers": []},
        )
        code, _, error = run_cli(["sandbox", "detach", "managed"])
        assert code != 0 and "local runtime" in error, error
        rpc(3, "session/new", {"cwd": str(local), "mcpServers": []})
        checked(["sandbox", "detach", "managed"])
        process.stdin.close()
        assert process.wait(timeout=45) == 0
    print(
        "PASS ACP fresh-server sandbox restore, other-holder busy, per-session release to local",
        flush=True,
    )

    master, slave = pty.openpty()
    termios.tcsetwinsize(slave, (48, 160))
    try:
        terminal_env = {
            **isolated,
            "TERM": "xterm-256color",
            "COLUMNS": "160",
            "LINES": "48",
        }
        with child(
            [
                caudra,
                "--no-plugins",
                "--model",
                "anthropic/claude-sonnet-4-6",
                "--sandbox",
                "managed",
            ],
            cwd=local,
            env=terminal_env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
        ) as process:

            def wait_text(expected):
                transcript = ""
                deadline = time.monotonic() + 45
                while expected not in transcript:
                    assert select.select(
                        [master], [], [], max(0, deadline - time.monotonic())
                    )[0], (expected, transcript[-4000:])
                    transcript += re.sub(
                        r"\x1b\[[0-?]*[ -/]*[@-~]",
                        "",
                        os.read(master, 65536).decode(errors="replace"),
                    )
                    assert time.monotonic() < deadline, (expected, transcript[-4000:])
                return transcript

            wait_text("claude-sonnet-4-6")
            os.write(master, b"/sandbox\r")
            wait_text("managed")
            os.write(master, b"d\x1b[13;5us")
            wait_text("No reconnect or local fallback")
            assert process.wait(timeout=45) == 0
        record = json.loads(checked(["sandbox", "list"]))[0]
        assert record["detached"] is True
        assert handler.lifecycle["instance"]["state"] == "running"
        print(
            "PASS live idle TUI current-runtime exclusive detach; VM untouched",
            flush=True,
        )
    finally:
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()
