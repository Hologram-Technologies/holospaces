#!/usr/bin/env python3
"""Serve the local Pages build and rebuild it when source inputs change."""

from __future__ import annotations

import http.server
import os
import socket
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parent.parent
SITE_DIR = ROOT / "_site"
BUILD_SCRIPT = ROOT / "scripts" / "build-site.sh"
POLL_SECONDS = 1.0

WATCH_PATHS = [
    ROOT / "Cargo.lock",
    ROOT / "Cargo.toml",
    ROOT / "crates" / "holospaces-web" / "Cargo.lock",
    ROOT / "crates" / "holospaces-web" / "Cargo.toml",
    ROOT / "crates" / "holospaces-web" / "src",
    ROOT / "crates" / "holospaces-web" / "web",
    ROOT / "scripts" / "build-site.sh",
    ROOT / "vv" / "artifacts" / "cc9" / "linux" / "holospaces.dtb",
    ROOT / "vv" / "artifacts" / "cc11" / "Image.gz",
    ROOT / "vv" / "artifacts" / "cc13" / "vendor",
    ROOT / "vv" / "artifacts" / "cc14" / "kernel" / "Image.gz",
    ROOT / "vv" / "artifacts" / "cc16" / "image",
    ROOT / "vv" / "artifacts" / "cc16" / "kernel" / "Image.gz",
    ROOT / "vv" / "artifacts" / "cc22" / "image",
]

IGNORED_PARTS = {
    ".git",
    ".vscode-test-web",
    "node_modules",
    "pkg",
    "target",
}

IGNORED_NAMES = {
    "fixture.holo",
    "fixture.kappa",
    "fixture-userland.wasm",
}

DEV_RELOAD_SNIPPET = b"""
<script>
(() => {
  const events = new EventSource('/__holospaces_dev/events');
  events.addEventListener('reload', () => location.reload());
})();
</script>
"""


class State:
    def __init__(self) -> None:
        self.condition = threading.Condition()
        self.version = 0
        self.last_error = ""

    def bump(self, error: str = "") -> None:
        with self.condition:
            self.version += 1
            self.last_error = error
            self.condition.notify_all()


STATE = State()


def iter_watch_files() -> list[Path]:
    files: list[Path] = []
    for path in WATCH_PATHS:
        if not path.exists():
            continue
        if path.is_file():
            files.append(path)
            continue
        for child in path.rglob("*"):
            relative_parts = set(child.relative_to(ROOT).parts)
            if child.is_file() and child.name not in IGNORED_NAMES and not (IGNORED_PARTS & relative_parts):
                files.append(child)
    return files


def snapshot() -> dict[str, tuple[int, int]]:
    current: dict[str, tuple[int, int]] = {}
    for path in iter_watch_files():
        try:
            stat = path.stat()
        except FileNotFoundError:
            continue
        current[str(path.relative_to(ROOT))] = (stat.st_mtime_ns, stat.st_size)
    return current


def run_build() -> bool:
    print("site-dev: building _site", flush=True)
    result = subprocess.run([str(BUILD_SCRIPT)], cwd=ROOT)
    if result.returncode == 0:
        print("site-dev: build complete", flush=True)
        STATE.bump()
        return True

    message = f"build failed with exit code {result.returncode}"
    print(f"site-dev: {message}", file=sys.stderr, flush=True)
    STATE.bump(message)
    return False


def watch_loop() -> None:
    previous = snapshot()
    while True:
        time.sleep(POLL_SECONDS)
        current = snapshot()
        if current == previous:
            continue
        previous = current
        run_build()
        previous = snapshot()


class DevHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args: object, **kwargs: object) -> None:
        super().__init__(*args, directory=str(SITE_DIR), **kwargs)

    def log_message(self, fmt: str, *args: object) -> None:
        print("site-dev: " + fmt % args, flush=True)

    def do_GET(self) -> None:
        if urlparse(self.path).path == "/__holospaces_dev/events":
            self.serve_events()
            return
        super().do_GET()

    def end_headers(self) -> None:
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def send_head(self):  # type: ignore[no-untyped-def]
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            for index in ("index.html", "index.htm"):
                index_path = os.path.join(path, index)
                if os.path.exists(index_path):
                    path = index_path
                    break
            else:
                return self.list_directory(path)

        if not path.endswith(".html"):
            return super().send_head()

        try:
            content = Path(path).read_bytes()
        except OSError:
            self.send_error(404, "File not found")
            return None

        marker = b"</body>"
        if marker in content:
            content = content.replace(marker, DEV_RELOAD_SNIPPET + marker, 1)
        else:
            content += DEV_RELOAD_SNIPPET

        self.send_response(200)
        self.send_header("Content-type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        return BytesBody(content)

    def serve_events(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        seen = STATE.version
        try:
            self.wfile.write(f": connected {seen}\n\n".encode())
            self.wfile.flush()
            while True:
                with STATE.condition:
                    STATE.condition.wait_for(lambda: STATE.version != seen, timeout=15)
                    if STATE.version == seen:
                        self.wfile.write(b": keepalive\n\n")
                        self.wfile.flush()
                        continue
                    seen = STATE.version
                    error = STATE.last_error
                if error:
                    self.wfile.write(f"event: build-error\ndata: {error}\n\n".encode())
                else:
                    self.wfile.write(f"event: reload\ndata: {seen}\n\n".encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


class BytesBody:
    def __init__(self, content: bytes) -> None:
        self.content = content
        self.sent = False

    def read(self, _size: int = -1) -> bytes:
        if self.sent:
            return b""
        self.sent = True
        return self.content

    def close(self) -> None:
        return None


class ReuseServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def pick_port(start: int) -> int:
    for port in range(start, start + 20):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            try:
                sock.bind(("127.0.0.1", port))
            except OSError:
                continue
            return port
    raise RuntimeError(f"no available port found from {start} to {start + 19}")


def main() -> int:
    requested = int(sys.argv[1]) if len(sys.argv) > 1 else int(os.environ.get("SITE_PORT", "8000"))
    port = pick_port(requested)

    if not run_build():
        return 1

    watcher = threading.Thread(target=watch_loop, daemon=True)
    watcher.start()

    with ReuseServer(("0.0.0.0", port), DevHandler) as httpd:
        print(f"site-dev: serving http://localhost:{port}", flush=True)
        if port != requested:
            print(f"site-dev: requested port {requested} was busy", flush=True)
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\nsite-dev: stopped", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
