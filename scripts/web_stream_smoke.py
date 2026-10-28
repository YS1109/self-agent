#!/usr/bin/env python3
"""Verify the token-private Vite SSE proxy with local fixtures, never a real model.

Requires `pnpm install --frozen-lockfile`. Uses the supported preview port 1420
and fails if that port is already occupied rather than touching another server.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
ORIGIN = "http://127.0.0.1:1420"


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    token = "fixture-local-token-a"
    seen = []
    errors = []
    disconnected = threading.Event()

    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.headers.get("Authorization") != "Bearer " + type(self).token:
            type(self).errors.append("The proxy did not use the current private fixture token")
            self.send_error(401)
            return
        type(self).seen.append(self.path)
        if self.path.endswith("after=999"):
            body = json.dumps({"code": "snapshot_required", "error": "游标过期"}).encode()
            self.send_response(409)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(
            'id: 1\nevent: run_event\ndata: {"sequence":1,"text":"中文实时增量"}\n\n'.encode()
        )
        self.wfile.flush()
        try:
            for _ in range(200):
                time.sleep(0.025)
                self.wfile.write(b": alive\n\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            type(self).disconnected.set()


def pnpm_command():
    executable = shutil.which("pnpm")
    if not executable:
        raise RuntimeError("pnpm is required; install the version specified in package.json")
    command = [executable, "exec", "vite", "--host", "127.0.0.1", "--port", "1420", "--strictPort"]
    if os.name == "nt" and Path(executable).suffix.lower() in (".cmd", ".bat"):
        # Windows batch shims need cmd.exe; list2cmdline retains paths with spaces.
        # All arguments are fixed test parameters or the resolved local pnpm path.
        shell = subprocess.list2cmdline([os.environ.get("COMSPEC", "cmd.exe")])
        # Pass the complete Windows command line as a string so Popen does not
        # backslash-escape cmd.exe's nested outer/first-argument quote pair.
        return f'{shell} /d /s /c "{subprocess.list2cmdline(command)}"'
    return command


def stop_vite(process):
    """Stop only this test's launcher and its descendants, including pnpm shims."""
    if os.name == "nt":
        if process.poll() is None:
            subprocess.run(
                ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10,
            )
    else:
        # start_new_session makes the process group private to this fixture.
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            process.kill()
        process.wait(timeout=5)


def wait_for(check, *, timeout=5, description="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for {description}")


def open_event(path="/api/v1/events?after=0", origin=ORIGIN):
    request = urllib.request.Request(
        ORIGIN + path, headers={"Origin": origin, "Sec-Fetch-Site": "same-origin"},
    )
    return urllib.request.urlopen(request, timeout=5)


def check_proxy():
    started = time.monotonic()
    with open_event() as response:
        assert response.headers.get_content_type() == "text/event-stream"
        assert response.headers.get("Authorization") is None
        data = b""
        while not data.endswith(b"\n\n"):
            line = response.readline()
            assert line, "Event connection ended before the first complete frame"
            data += line
            assert len(data) < 4096
        assert "中文实时增量" in data.decode()
        assert Backend.token.encode() not in data
        assert time.monotonic() - started < 4, "Proxy buffered SSE until the upstream ended"
    wait_for(Backend.disconnected.is_set, description="upstream close after client disconnect")
    print("PASS: Chinese SSE forwarded incrementally; client disconnect closes upstream")

    count = len(Backend.seen)
    for path, origin in [
        ("/api/v1/events?after=0", "https://example.com"),
        ("/api/v1/events?after=0&target=http://example.com", ORIGIN),
        ("/api/v1/events?after=-1", ORIGIN),
    ]:
        try:
            with open_event(path, origin):
                raise AssertionError("Proxy accepted a forbidden origin or cursor")
        except urllib.error.HTTPError as error:
            with error:
                assert error.code == 403
    assert len(Backend.seen) == count, "Forbidden requests reached the authenticated upstream"
    print("PASS: cross-origin, extra-target and invalid-cursor requests rejected before authentication")

    try:
        with open_event("/api/v1/events?after=999"):
            raise AssertionError("Proxy swallowed the expired-cursor conflict")
    except urllib.error.HTTPError as error:
        with error:
            assert error.code == 409
            assert json.load(error)["code"] == "snapshot_required"


def main():
    # The development proxy deliberately only accepts the documented origin.
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 1420))
    command = pnpm_command()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-web-stream-") as directory:
            metadata = Path(directory) / "daemon.json"
            log_path = Path(directory) / "vite.log"

            def save_metadata():
                metadata.write_text(json.dumps({
                    "base_url": f"http://127.0.0.1:{server.server_port}", "token": Backend.token,
                }), encoding="utf-8")

            save_metadata()
            env = {key: value for key, value in os.environ.items() if not key.startswith("HARNESS_")}
            env["HARNESS_DATA_DIR"] = directory
            options = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {"start_new_session": True}
            with log_path.open("wb") as log:
                process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=log, stderr=log, **options)
                try:
                    def ready():
                        if process.poll() is not None:
                            raise RuntimeError("Vite exited before becoming ready")
                        try:
                            with urllib.request.urlopen(ORIGIN, timeout=1) as response:
                                return response.status == 200
                        except (OSError, urllib.error.URLError):
                            return False

                    wait_for(ready, timeout=30, description="isolated Vite startup")
                    check_proxy()
                    Backend.token = "fixture-local-token-b"
                    save_metadata()
                    Backend.disconnected.clear()
                    with open_event("/api/v1/events?after=1") as response:
                        assert response.readline() == b"id: 1\n"
                    wait_for(Backend.disconnected.is_set, description="rotated upstream disconnect")
                    assert not Backend.errors, Backend.errors
                    print("PASS: cursor conflict preserved; new subscription reads rotated private token")
                except Exception:
                    log.flush()
                    print(log_path.read_text(encoding="utf-8", errors="replace"))
                    raise
                finally:
                    stop_vite(process)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
