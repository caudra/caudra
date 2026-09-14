#!/usr/bin/env python3
"""Run the real authenticated remote adapter suite, without external services."""
import contextlib
import http.client
import http.server
import json
import os
import secrets
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import ClassVar


@contextlib.contextmanager
def child(args, **kwargs):
    process = subprocess.Popen(args, start_new_session=True, **kwargs)
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


def main():
    binary = Path(os.environ["WORKCELL_TEST_BINARY"]).resolve(strict=True)
    fault_mode = os.environ.get("WORKCELL_TEST_FAULT_MODE", "disconnect")
    assert fault_mode in ("disconnect", "http503"), fault_mode
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="caudra-workcell-integration-") as directory:
        temp = Path(directory)
        for name in ("root", "snapshots", "state", "config", "home"):
            (temp / name).mkdir(mode=0o700)
        root = temp / "root"
        (root / "nested").mkdir()
        (root / "fixture.txt").write_text("original\n")
        (root / "nested" / "fixture.txt").write_text("nested original\n")
        (root / "AGENTS.md").write_text("Remote integration instruction sentinel.\n")
        (root / ".agents" / "skills" / "fixture").mkdir(parents=True)
        (root / ".agents" / "skills" / "fixture" / "SKILL.md").write_text(
            "---\nname: fixture\ndescription: Integration sentinel\n---\nFixture skill body.\n")
        for args in (["init", "-q"], ["add", "."],
                     ["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "fixture"]):
            subprocess.run(["git", *args], cwd=root, check=True, timeout=10,
                           env={"PATH": os.environ["PATH"], "HOME": str(temp / "home")})
        token = temp / "token"
        token.write_text(secrets.token_hex(32))
        token.chmod(0o600)
        ca, key = temp / "ca.pem", temp / "key.pem"
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                        "-keyout", str(key), "-out", str(ca), "-days", "1",
                        "-subj", "/CN=Caudra local integration",
                        "-addext", "subjectAltName=IP:127.0.0.1",
                        "-addext", "basicConstraints=critical,CA:TRUE"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
        key.chmod(0o600)
        leaf_key, leaf = temp / "leaf-key.pem", temp / "leaf.pem"
        request, extensions = temp / "leaf.csr", temp / "extensions"
        extensions.write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        for command in (
            ["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(leaf_key), "-out", str(request), "-subj", "/CN=localhost"],
            ["openssl", "x509", "-req", "-in", str(request), "-CA", str(ca), "-CAkey", str(key), "-CAcreateserial", "-out", str(leaf), "-days", "1", "-extfile", str(extensions)],
        ):
            subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
        leaf_key.chmod(0o600)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]

        class Proxy(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            execute_requests: ClassVar[list[tuple[object, object, object]]] = []
            provider_requests: ClassVar[list[dict[str, object]]] = []
            fault_lock = threading.Lock()

            def log_message(self, format: str, *args: object) -> None:
                pass

            def forward(self):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=35)
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if length > 4 * 1024 * 1024:
                        self.send_error(413)
                        return
                    headers = {k: v for k, v in self.headers.items()
                               if k.lower() not in ("host", "connection", "transfer-encoding")}
                    request_body = self.rfile.read(length)
                    if self.path == "/v1/messages":
                        self.provider_requests.append(json.loads(request_body))
                        events = [
                            {"type": "message_start", "message": {"id": "fixture", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "usage": {"input_tokens": 1, "output_tokens": 0}}},
                            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Synthetic provider sentinel."}},
                            {"type": "content_block_stop", "index": 0},
                            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}},
                            {"type": "message_stop"},
                        ]
                        body = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    method = json.loads(request_body).get("method") if request_body else None
                    restart = temp / "drop-operation-response.restart"
                    if method == "server/discover" and restart.exists():
                        restart_server()
                        restart.unlink()
                    preparation = temp / "drop-operation-response.preparation"
                    if method == "ai.workcell/execute" and preparation.exists() and json.loads(request_body)["params"]["preparationId"] == preparation.read_text():
                        request = json.loads(request_body)
                        with self.fault_lock:
                            self.execute_requests.append((request["id"], request["params"]["preparationId"], request["params"]["invocationId"]))
                            count = temp / "drop-operation-response.count"
                            count.write_text(str(len(self.execute_requests)))
                    if (temp / "drop-operation-response").exists() and method == "ai.workcell/status":
                        self.send_error(503)
                        return
                    connection.request(self.command, self.path, request_body, headers)
                    response = connection.getresponse()
                    body = response.read(16 * 1024 * 1024 + 1)
                    if (temp / "drop-operation-response").exists() and method == "ai.workcell/execute":
                        if fault_mode == "http503":
                            self.send_error(503)
                        else:
                            self.close_connection = True
                            self.connection.shutdown(socket.SHUT_RDWR)
                        return
                    if response.status >= 400 and self.headers.get("Authorization"):
                        print("Authenticated upstream failure:", response.status, body[:2000].decode(errors="replace"), flush=True)
                    if len(body) > 16 * 1024 * 1024:
                        self.send_error(502)
                        return
                    self.send_response(response.status)
                    for k, v in response.getheaders():
                        if k.lower() not in ("connection", "transfer-encoding", "content-length"):
                            self.send_header(k, v)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                finally:
                    connection.close()

            do_POST = forward
            do_GET = forward

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(leaf, leaf_key)
        proxy.socket = context.wrap_socket(proxy.socket, server_side=True)
        thread = threading.Thread(target=proxy.serve_forever)
        thread.start()
        try:
            args = [str(binary), str(root), "--transport", "http", "--port", str(port),
                    "--http-token-file", str(token), "--allow-write", "--yolo",
                    "--no-http-proxy", "--snapshot-root", str(temp / "snapshots")]
            for group in ("files", "code_graph", "web", "shell", "python_execution", "transfer"):
                args += ["--tool-group", group]
            for flag in ("server-id", "workspace-id", "workspace-generation", "root-project-id", "principal-id"):
                args += ["--remote-" + flag, "integration-" + flag]
            server_env = {"PATH": os.environ["PATH"], "HOME": str(temp / "home"),
                          "XDG_CONFIG_HOME": str(temp / "config")}
            with (temp / "server.log").open("w+") as log, contextlib.ExitStack() as servers:
                def start_server():
                    process = servers.enter_context(child(args, env=server_env, stdout=log, stderr=log))
                    deadline = time.monotonic() + 30
                    while True:
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError("Workcell startup failed: " + log.read()[-4000:])
                        try:
                            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
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
                    server = start_server()

                env = os.environ.copy()
                env.update(WORKCELL_TEST_ENDPOINT=f"https://127.0.0.1:{proxy.server_port}/mcp",
                           WORKCELL_TEST_TOKEN_FILE=str(token), WORKCELL_TEST_ROOT=str(root),
                           WORKCELL_TEST_STATE=str(temp / "state"), SSL_CERT_FILE=str(ca),
                           WORKCELL_TEST_FAULT=str(temp / "drop-operation-response"))
                command = ["cargo", "test", "--locked", "-p",
                           "caudra-workcell", "--test", "authenticated_local", "--", "--nocapture"]
                with child(command, cwd=repo, env=env) as tests:
                    if tests.wait(timeout=600) != 0:
                        print("Fault execute requests:", Proxy.execute_requests, flush=True)
                        raise RuntimeError("authenticated local integration failed")
                if os.environ.get("CAUDRA_TEST_BINARY"):
                    restart_server()
                    caudra = str(Path(os.environ["CAUDRA_TEST_BINARY"]).resolve(strict=True))
                    local = temp / "client"
                    local.mkdir()
                    isolated = {"PATH": os.environ["PATH"], "HOME": str(temp / "home"),
                                "SSL_CERT_FILE": str(ca), "NO_COLOR": "1",
                                "XDG_CONFIG_HOME": str(temp / "config"),
                                "XDG_STATE_HOME": str(temp / "client-state"),
                                "XDG_DATA_HOME": str(temp / "client-data"),
                                "XDG_CACHE_HOME": str(temp / "client-cache"),
                                "ANTHROPIC_API_KEY": "fixture-not-a-real-key",
                                "ANTHROPIC_BASE_URL": f"https://127.0.0.1:{proxy.server_port}",
                                "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
                                "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost"}
                    for app in ("caudra", "caudra-debug"):
                        config = temp / "config" / app
                        config.mkdir(exist_ok=True)
                        (config / "permissions.toml").write_text('default = "allow"\n[file_index]\nallow = ["*"]\n')

                    def run_cli(arguments, stdin=""):
                        with child([caudra, *arguments], cwd=local, env=isolated,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                            stdout, stderr = process.communicate(stdin.encode(), timeout=45)
                            assert len(stdout) + len(stderr) < 4 * 1024 * 1024
                            return process.returncode, stdout.decode(), stderr.decode()

                    code, _, error = run_cli(["auth", "workcell", "set", "integration", "--stdin"], token.read_text())
                    assert code == 0, error
                    remote = ["--no-plugins", "--model", "anthropic/claude-sonnet-4-6", "--workcell-endpoint", env["WORKCELL_TEST_ENDPOINT"],
                              "--workcell-cwd", ".", "--workcell-credential-ref", "credential:integration"]
                    checks = [
                        ("remote status controller", ["remote", "status"], "", "pending operations: 0"),
                        ("remote reconcile controller", ["remote", "reconcile"], "", "No mutation resent"),
                        ("SDK remote status", ["--print", "--input-format", "stream-json", "--output-format", "stream-json"],
                         json.dumps({"type": "user", "message": {"role": "user", "content": "/remote status"}}) + "\n", "pending operations: 0"),
                        ("SDK parent navigation", ["--print", "--input-format", "stream-json", "--output-format", "stream-json"],
                         "".join(json.dumps({"type": "user", "message": {"role": "user", "content": command}}) + "\n" for command in ["/cd nested", "/cd ..", "/cd nested", "/cd ../nested"]), '"cwd":"."'),
                        ("startup prompt", ["prompt"], "", "Remote integration instruction sentinel"),
                        ("skills discovery", ["skills", "--names"], "", "fixture"),
                        ("skill body", ["skills", "fixture"], "", "Fixture skill body"),
                        ("root index dispatch", ["--yolo", "--allowed-tools", "file_index", "index", "."], "", "fixture.txt"),
                        ("print zero-turn startup", ["--print", "--prompt", "fixture", "--max-turns", "0", "--output-format", "json"], "", '"num_turns":0'),
                        ("SDK direct command", ["--print", "--yolo", "--input-format", "stream-json", "--output-format", "stream-json"],
                         json.dumps({"type": "user", "message": {"role": "user", "content": "!pwd"}}) + "\n", '"num_turns":0'),
                        ("ACP initialize", ["acp"], json.dumps({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}) + "\n", '"protocolVersion":1'),
                    ]
                    failures = []
                    for label, arguments, stdin, expected in checks:
                        provider_count = len(Proxy.provider_requests)
                        try:
                            code, output, error = run_cli([*remote, *arguments], stdin)
                        except subprocess.TimeoutExpired:
                            failures.append(label)
                            print("FAIL", label, "bounded 45-second timeout", flush=True)
                            continue
                        sdk_ok = label != "SDK direct command" or any(item.get("type") == "result" and not item.get("is_error") and str(root) in item.get("result", "") for item in (json.loads(line) for line in output.splitlines() if line.startswith("{")))
                        if label == "SDK parent navigation":
                            sdk_ok = "cwd_error" not in output
                        if code == 0 and expected in output and sdk_ok and len(Proxy.provider_requests) == provider_count:
                            print("PASS", label, flush=True)
                        else:
                            failures.append(label)
                            print("FAIL", label, "exit", code, (output + error)[-2000:], flush=True)
                    for label, arguments, expected in [
                        ("local allows cannot authorize remote index", ["--allowed-tools", "file_index", "index", "."], "Permission denied"),
                        ("disabled index stays disabled under yolo", ["--yolo", "--disallowed-tools", "file_index", "index", "."], "index is disabled"),
                    ]:
                        code, output, error = run_cli([*remote, *arguments])
                        assert code != 0 and expected in output + error, (label, code, output, error)
                        print("PASS", label, flush=True)
                    for app in ("caudra", "caudra-debug"):
                        (temp / "config" / app / "permissions.toml").write_text('[file_index]\ndeny = true\n')
                    code, output, error = run_cli([*remote, "--yolo", "index", "."])
                    assert code != 0 and "Permission denied" in output + error, (code, output, error)
                    print("PASS explicit index deny overrides yolo", flush=True)
                    before = len(Proxy.provider_requests)
                    code, output, error = run_cli([*remote, "--print", "--prompt", "fixture", "--max-turns", "1", "--output-format", "json"])
                    assert code == 0 and "Synthetic provider sentinel." in output, (code, output, error)
                    assert len(Proxy.provider_requests) == before + 1
                    assert '"num_turns":1' in output
                    print("PASS print synthetic one-turn provider", flush=True)
                    if failures:
                        raise RuntimeError("entrypoint checks failed: " + ", ".join(failures))
        finally:
            proxy.shutdown()
            proxy.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
