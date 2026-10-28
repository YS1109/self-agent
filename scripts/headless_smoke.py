#!/usr/bin/env python3
"""Validate the standalone CLI, direct agent history, compaction and callbacks."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import uuid

from smoke import Daemon, ROOT, wait_for


class Model(BaseHTTPRequestHandler):
    compacted = 0
    requests = []
    lock = threading.Lock()

    def log_message(self, *_args):
        pass

    def answer(self, body):
        data = json.dumps(body, ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        assert self.path == "/catalog?api-version=fixture", self.path
        self.answer({"data": [{"id": "fixture-main"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.lock:
            self.requests.append(body)
        messages = body["messages"]
        if not body.get("tools"):
            with self.lock:
                type(self).compacted += 1
            message = {"content": "历史摘要：FIRST_RESULT。已写入result.txt，用户要求接续。"}
        else:
            goal = [m["content"] for m in messages if m["role"] == "user"][-1]
            if goal == "FIRST" and not any(m["role"] == "tool" for m in messages):
                time.sleep(0.7)
                message = {"content": None, "tool_calls": [{
                    "id": "write", "type": "function", "function": {
                        "name": "write_file",
                        "arguments": json.dumps({"path": "result.txt", "content": "REAL_TOOL_WRITE"}),
                    },
                }]}
            elif goal == "FIRST":
                message = {"content": "FIRST_RESULT " + "已经执行并记录。 " * 70}
            elif goal == "SECOND":
                transcript = json.dumps(messages, ensure_ascii=False)
                assert "FIRST_RESULT" in transcript, "Previous result missing from main conversation"
                assert "FUTURE" not in transcript, "Future queued input leaked into current run"
                message = {"content": "SECOND_RESULT：已接续此前真实结果。"}
            elif goal == "SLOW":
                time.sleep(2.5)
                message = {"content": "迟到结果"}
            else:
                message = {"content": "完成：" + goal}
        self.answer({"choices": [{"message": {"role": "assistant", **message}, "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}]})


def main():
    executable = ROOT / "target" / "debug" / (
        "self-agent-cli.exe" if os.name == "nt" else "self-agent-cli"
    )
    if not executable.is_file():
        raise RuntimeError("Run cargo build -p harness-daemon -p harness-cli first.")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="self-agent-headless-") as directory:
        project = Path(directory) / "project"
        project.mkdir()
        daemon = Daemon(directory)

        def cli(*args, stdin=None, ok=True):
            result = subprocess.run(
                [str(executable), "--data-dir", directory, *args],
                input=stdin, text=True, capture_output=True, timeout=20, encoding="utf-8",
            )
            assert (result.returncode == 0) == ok, result.stderr
            return json.loads(result.stdout) if result.stdout else None

        def done(identifier):
            current = next(t for t in daemon.snapshot()["tasks"] if t["id"] == identifier)
            return current if current["status"] in ("completed", "failed", "cancelled") else None

        try:
            daemon.start()
            session = cli("create", "无头会话", str(project))["session_id"]
            assert cli("sessions")[0]["id"] == session
            config = {
                "provider_source": "manual",
                "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "model": "fixture-main",
                "models_endpoint": "/catalog?api-version=fixture",
                "model_contexts": {
                    "fixture-main": {"context_window": 8192, "compression_threshold": 20},
                },
            }
            cli("settings", "-", stdin=json.dumps(config))
            assert cli("models")["models"][0]["id"] == "fixture-main"
            key = str(uuid.uuid4())
            first = cli("--command-id", key, "run", session, "FIRST")
            assert first == cli("--command-id", key, "run", session, "FIRST")
            start = time.monotonic()
            assert cli("status", first["task_id"])["task_kind"] == "main"
            assert time.monotonic() - start < 0.6, "Management blocked on model request"
            second = cli("run", session, "SECOND")
            third = cli("run", session, "FUTURE")
            for receipt in (first, second, third):
                result = wait_for(lambda: done(receipt["task_id"]), description="direct main run")
                assert result["status"] == "completed", result
                assert result["context_config"]["context_window"] == 8192
            assert (project / "result.txt").read_text() == "REAL_TOOL_WRITE"
            assert Model.compacted > 0, "Context threshold did not trigger real summary request"
            snapshot = daemon.snapshot()
            assert snapshot["plans"] == [], "Direct work created an orchestration plan"
            wait_for(
                lambda: len([d for d in daemon.snapshot()["deliveries"] if d["status"] == "sent"]) == 3,
                description="fixed source callbacks",
            )
            assert all(d["target"]["channel_id"] == "cli" for d in daemon.snapshot()["deliveries"])
            assert cli("--wait", "10", "run", session, "-",
                       stdin="等待完成")["status"] == "completed"
            print("PASS standalone CLI, custom model Endpoint, main tools, queue history, compaction and callbacks")

            timed = cli("--wait", "1", "run", session, "SLOW", ok=False)
            assert timed["status"] == "running"
            cli("cancel", timed["id"])
            assert cli("status", timed["id"])["status"] == "cancelled"
            saved = cli("settings")
            daemon.stop()
            daemon.start()
            assert cli("settings")["model_contexts"] == saved["model_contexts"]
            assert any(s["id"] == session for s in cli("sessions"))
            assert cli("status", timed["id"])["status"] == "cancelled"
            print("PASS CLI terminal wait, timeout, cancellation, settings and conversation persistence")
        finally:
            daemon.stop()
    server.shutdown()


if __name__ == "__main__":
    main()
