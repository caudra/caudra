import base64
import hashlib
import json
import os
import sys
from pathlib import Path
from urllib.parse import parse_qs, urlparse


def response(uri, scenario):
    parsed = urlparse(uri)
    if parsed.netloc == "api.github.com":
        if "/tags/" in parsed.path:
            data = scenario["explicit"]
        else:
            page = int(parse_qs(parsed.query)["page"][0])
            pages = scenario["pages"]
            data = pages[page - 1] if page <= len(pages) else []
        if isinstance(data, dict) and "http_status" in data:
            return data["http_status"], b"request failed"
        return 200, (data if isinstance(data, str) else json.dumps(data)).encode()
    if parsed.netloc != "github.com" or not parsed.path.startswith(
        "/caudra/caudra/releases/download/"
    ):
        raise ValueError("unexpected request origin")
    name = parsed.path.rsplit("/", 1)[1]
    if scenario.get("download_failure") == name:
        return 503, b"download failed"
    archive = Path(os.environ["TEST_ARCHIVE"])
    marker = archive.with_suffix(".name")
    if name != "sha256sums.txt":
        marker.write_text(name)
        content = archive.read_bytes()
        if scenario.get("checksum") == "corrupt":
            content += b"corruption"
        return 200, content
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    entry = f"{digest}  {marker.read_text()}\n"
    mode = scenario.get("checksum")
    if mode == "duplicate":
        entry += entry
    elif mode == "missing":
        entry = f"{digest}  unrelated.tar.gz\n"
    elif mode == "malformed":
        entry = entry.replace(digest, "x" * 64)
    elif mode == "suffix":
        entry = entry.rstrip() + ".extra\n"
    elif mode == "traversal":
        entry = entry.replace("  caudra", "  ../caudra")
    elif mode == "malformed_other":
        entry += "not a checksum\n"
    elif mode == "crlf":
        entry = entry.replace("\n", "\r\n")
    elif mode == "binary":
        entry = entry.replace("  ", " *")
    return 200, entry.encode()


def main():
    scenario = json.loads(Path(os.environ["TEST_SCENARIO"]).read_text())
    if len(sys.argv) == 3 and sys.argv[1] == "--response":
        uri = sys.argv[2]
        status, content = response(uri, scenario)
        length = len(content)
        location = ""
        kind = (
            "api"
            if urlparse(uri).netloc == "api.github.com"
            else "checksum"
            if uri.endswith("/sha256sums.txt")
            else "archive"
        )
        if scenario.get("oversize_kind") == kind:
            limit = {
                "api": 4 * 1024 * 1024,
                "checksum": 1024 * 1024,
                "archive": 2 * 1024 * 1024 * 1024,
            }[kind]
            mode = scenario["oversize_mode"]
            length = (
                limit + 1 if mode == "advertised" else -1 if mode == "unknown" else 1
            )
            content = b"x" if mode == "advertised" else b"x" * (limit + 65536)
        if scenario.get("truncated_kind") == kind:
            length += 1
        if scenario.get("redirect_kind") == kind:
            status = 302
            location = scenario["redirect_location"]
        print(
            json.dumps(
                {
                    "status": status,
                    "content": base64.b64encode(content).decode(),
                    "length": length,
                    "location": location,
                }
            )
        )
        return
    args = sys.argv[1:]
    uri = next(arg for arg in args if arg.startswith("https://"))
    with Path(os.environ["TEST_REQUEST_LOG"]).open("a") as log:
        log.write(json.dumps(args) + "\n")
    status, content = response(uri, scenario)
    if status >= 400 or status == 0:
        sys.exit(22 if status else 28)
    Path(args[args.index("-o") + 1]).write_bytes(content)
    if "-w" in args:
        print(status, end="")


if __name__ == "__main__":
    main()
