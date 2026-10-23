#!/usr/bin/env python3
"""Exercise the real daemon over HTTP with isolated data and no cloud credentials."""

import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parent.parent
DESKTOP = {"channel_id": "desktop", "actor_id": "owner", "conversation_id": "main"}
REMOTE = {"channel_id": "simulator", "actor_id": "owner", "conversation_id": "remote-1"}


def available_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(check, timeout=15, description="condition"):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, urllib.error.URLError) as error:
            last_error = error
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for {description}: {last_error}")


class Daemon:
    def __init__(self, directory, extra_env=None):
        self.directory = Path(directory)
        self.port = available_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.token = ""
        self.process = None
        self.log = None
        self.extra_env = extra_env or {}

    def start(self):
        executable = ROOT / "target" / "debug" / (
            "harness-daemon.exe" if os.name == "nt" else "harness-daemon"
        )
        if not executable.is_file():
            raise RuntimeError("Run cargo build -p harness-daemon first.")
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("HARNESS_")
        }
        env.update(
            HARNESS_DATA_DIR=str(self.directory),
            HARNESS_BIND=f"127.0.0.1:{self.port}",
            HARNESS_DEMO_STEP_MS="450",
        )
        env.update(self.extra_env)
        self.log = (self.directory / "smoke.log").open("ab")
        self.process = subprocess.Popen(
            [str(executable)], cwd=ROOT, env=env, stdout=self.log, stderr=self.log
        )

        def ready():
            if self.process.poll() is not None:
                raise RuntimeError((self.directory / "smoke.log").read_text(encoding="utf-8"))
            with urllib.request.urlopen(self.base + "/health", timeout=1) as response:
                health = json.load(response)
            info = json.loads((self.directory / "daemon.json").read_text(encoding="utf-8"))
            if info["pid"] != self.process.pid:
                return False
            assert health["status"] == "ok"
            self.token = info["token"]
            return True

        wait_for(ready, description="daemon startup")

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.log:
            self.log.close()

    def request(self, method, path, body=None, authenticate=True):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"}
        if authenticate:
            headers["Authorization"] = "Bearer " + self.token
        request = urllib.request.Request(
            self.base + path, data=data, method=method, headers=headers
        )
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def snapshot(self):
        return self.request("GET", "/api/v1/snapshot")

    def envelope(self, command, source=DESKTOP):
        return {
            "command_id": str(uuid.uuid4()),
            "source": dict(source),
            "command": command,
        }

    def command(self, command, source=DESKTOP):
        return self.request(
            "POST", "/api/v1/commands", self.envelope(command, source)
        )


def main():
    with tempfile.TemporaryDirectory(prefix="self-agent-smoke-") as temporary:
        daemon = Daemon(temporary)
        project = Path(temporary) / "project"
        project.mkdir()
        try:
            daemon.start()
            try:
                daemon.request("GET", "/api/v1/snapshot", authenticate=False)
                raise AssertionError("Private API accepted an unauthenticated request")
            except urllib.error.HTTPError as error:
                assert error.code == 401, error.code
            print("PASS authenticated local API")

            creation = daemon.envelope(
                {
                    "type": "create_session",
                    "title": "Smoke A",
                    "project_path": str(project),
                }
            )
            receipt = daemon.request("POST", "/api/v1/commands", creation)
            assert receipt == daemon.request("POST", "/api/v1/commands", creation)
            session_a = receipt["session_id"]
            session_b = daemon.command(
                {
                    "type": "create_session",
                    "title": "Smoke B",
                    "project_path": str(project),
                }
            )["session_id"]
            assert len(daemon.snapshot()["sessions"]) == 2
            print("PASS durable idempotent command receipts")

            settings = daemon.request("GET", "/api/v1/settings")
            assert settings["execution_mode"] == "native"
            assert "api_key" not in settings
            daemon.request(
                "PUT",
                "/api/v1/settings",
                {
                    "base_url": "http://127.0.0.1:1/v1",
                    "model": "smoke-no-provider",
                    "execution_mode": "demo",
                },
            )
            daemon.command(
                {"type": "select_session", "session_id": session_a}, REMOTE
            )
            dispatch = daemon.envelope(
                {"type": "start_task", "title": "Exercise async callback"}, REMOTE
            )
            task_receipt = daemon.request("POST", "/api/v1/commands", dispatch)
            task_id = task_receipt["task_id"]
            assert task_receipt == daemon.request(
                "POST", "/api/v1/commands", dispatch
            )
            daemon.command(
                {"type": "select_session", "session_id": session_b}, REMOTE
            )
            wait_for(
                lambda: any(
                    task["id"] == task_id and task["status"] == "running"
                    for task in daemon.snapshot()["tasks"]
                ),
                description="background task execution",
            )
            started = time.monotonic()
            daemon.command(
                {"type": "send_message", "content": "Still responsive?"}, REMOTE
            )
            assert time.monotonic() - started < 1.0
            snapshot = daemon.snapshot()
            target = next(s for s in snapshot["sessions"] if s["id"] == session_b)
            assert any(m["content"] == "Still responsive?" for m in target["messages"])
            assert any(t["id"] == task_id and t["status"] == "running" for t in snapshot["tasks"])
            print("PASS nonblocking management while a worker is running")

            def callback():
                snapshot = daemon.snapshot()
                return next(
                    (
                        d
                        for d in snapshot["deliveries"]
                        if d["task_id"] == task_id and d["status"] == "sent"
                    ),
                    None,
                )

            delivery = wait_for(callback, description="independent callback")
            assert delivery["session_id"] == session_a
            assert delivery["target"] == REMOTE
            task = next(t for t in daemon.snapshot()["tasks"] if t["id"] == task_id)
            assert task["status"] == "completed"
            assert task["execution_mode"] == "demo"
            assert len([t for t in daemon.snapshot()["tasks"] if t["id"] == task_id]) == 1
            print("PASS callback remains bound to its original session and source")

            cancelled_id = daemon.command(
                {"type": "start_task", "session_id": session_b, "title": "Cancel me"}
            )["task_id"]
            daemon.command({"type": "cancel_task", "task_id": cancelled_id})
            wait_for(
                lambda: any(
                    t["id"] == cancelled_id and t["status"] == "cancelled"
                    for t in daemon.snapshot()["tasks"]
                ),
                description="task cancellation",
            )
            print("PASS task cancellation")

            daemon.stop()
            daemon.start()
            snapshot = daemon.snapshot()
            assert {s["id"] for s in snapshot["sessions"]} == {session_a, session_b}
            assert next(t for t in snapshot["tasks"] if t["id"] == task_id)["status"] == "completed"
            assert daemon.request("POST", "/api/v1/commands", creation) == receipt
            assert len([d for d in snapshot["deliveries"] if d["task_id"] == task_id]) == 1
            print("PASS daemon restart preserves sessions, receipts and deliveries")
        finally:
            daemon.stop()
    print("All daemon smoke scenarios passed.")


if __name__ == "__main__":
    main()
