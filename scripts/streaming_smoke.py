#!/usr/bin/env python3
"""Exercise durable Native streaming/replay and CLI following using local fixtures only."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

from smoke import Daemon, ROOT, wait_for


SECRET = "fixture-stream-secret"
FIRST_PREFIX = "开始写入：这是已经逐步提交的中文回复，工具尚未执行。"
FINAL_TEXT = "完成：中文测试。"
CANCEL_PREFIX = "等待取消：这段文字已经生成，请在任务完成前停止。"
TERMINAL = {"completed", "failed", "cancelled"}


class Model(BaseHTTPRequestHandler):
    """The gates prove a response was observed before tools/terminal completion."""

    first_release = threading.Event()
    cancel_release = threading.Event()
    cancel_finished = threading.Event()
    requests = []
    lock = threading.Lock()
    protocol_version = "HTTP/1.0"

    def log_message(self, *_args):
        pass

    def packet(self, value):
        payload = "[DONE]" if value == "[DONE]" else json.dumps(value, ensure_ascii=False)
        data = ("data: " + payload + "\r\n\r\n").encode("utf-8")
        # Deliberately split inside Chinese UTF-8 code points, JSON and CRLF.
        for offset in range(0, len(data), 2):
            self.wfile.write(data[offset:offset + 2])
        self.wfile.flush()

    def delta(self, value, finish=None):
        self.packet({"choices": [{"index": 0, "delta": value, "finish_reason": finish}]})

    def end(self):
        self.packet({"choices": [], "usage": {"prompt_tokens": 123, "completion_tokens": 9}})
        self.packet("[DONE]")

    def do_POST(self):
        try:
            self.respond()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            # Cancellation is supposed to close the model request before late output.
            pass
        finally:
            if getattr(self, "goal", None) == "CANCEL":
                self.cancel_finished.set()

    def respond(self):
        assert self.path == "/v1/chat/completions", self.path
        assert self.headers.get("Authorization") == "Bearer " + SECRET
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["stream"] is True, body
        messages = body["messages"]
        goal = self.goal = [m["content"] for m in messages if m["role"] == "user"][-1]
        with self.lock:
            self.requests.append({"goal": goal, "body": body})
        tool_results = [m for m in messages if m["role"] == "tool"]
        if goal == "JSON":
            data = json.dumps({"choices": [{"message": {
                "role": "assistant", "content": "同一请求的非流式兼容回复。",
            }, "finish_reason": "stop"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if goal == "FIRST" and not tool_results:
            self.delta({"role": "assistant", "content": FIRST_PREFIX})
            assert self.first_release.wait(15), "First stream gate was not released"
            arguments = json.dumps({"path": "中文证明.txt", "content": "真实工具写入。"}, ensure_ascii=False)
            self.delta({"tool_calls": [{"index": 0, "id": "write-1", "type": "function",
                                        "function": {"name": "write_file", "arguments": ""}}]})
            for start in range(0, len(arguments), 3):
                self.delta({"tool_calls": [{"index": 0, "function": {
                    "arguments": arguments[start:start + 3],
                }}]})
            self.delta({}, "tool_calls")
            self.end()
        elif goal == "FIRST":
            assert len(tool_results) == 1, tool_results
            assert tool_results[0]["tool_call_id"] == "write-1"
            assert "error" not in json.loads(tool_results[0]["content"])
            for text in ("完成：", "中文", "测试。"):
                self.delta({"content": text})
            self.delta({}, "stop")
            self.end()
        elif goal == "CANCEL":
            self.delta({"content": CANCEL_PREFIX})
            assert self.cancel_release.wait(15), "Cancellation gate was not released"
            self.delta({"content": "取消后的迟到输出不得保存。"})
            self.delta({}, "stop")
            self.end()
        elif goal in ("TRUNCATED", "MALFORMED"):
            self.delta({"tool_calls": [{"index": 0, "id": "incomplete", "type": "function",
                                        "function": {"name": "write_file", "arguments":
                                            '{"path":"不应存在.txt","content":"unfinished'}}]})
            if goal == "MALFORMED":
                self.wfile.write(b"data: {not-json}\n\n")
                self.wfile.flush()
            # No finish_reason or [DONE]: incomplete calls must never execute.
        elif goal == "SECRET":
            for text in ("隐藏凭据：", "fixture-", "stream-", "secret", "。"):
                self.delta({"content": text})
            self.delta({}, "stop")
            self.end()
        elif goal == "CROSS_TURN_SECRET" and not tool_results:
            self.delta({"content": "fixture-"})
            self.delta({"tool_calls": [{
                "index": 0, "id": "read-between-secret-fragments", "type": "function",
                "function": {"name": "read_file", "arguments": json.dumps({
                    "path": "用户已有改动.txt",
                }, ensure_ascii=False)},
            }]})
            self.delta({}, "tool_calls")
            self.end()
        elif goal == "CROSS_TURN_SECRET":
            assert len(tool_results) == 1, tool_results
            assert tool_results[0]["tool_call_id"] == "read-between-secret-fragments"
            assert "error" not in json.loads(tool_results[0]["content"])
            self.delta({"content": "stream-secret"})
            self.delta({}, "stop")
            self.end()
        else:
            for text in ("持续输出：", goal, "已完成。"):
                self.delta({"content": text})
            self.delta({}, "stop")
            self.end()


class Events:
    """Bounded minimal SSE client; numeric IDs are checked against the body."""

    def __init__(self, daemon, after=None, last_event_id=None):
        suffix = "" if after is None else f"?after={after}"
        headers = {"Authorization": "Bearer " + daemon.token}
        if last_event_id is not None:
            headers["Last-Event-ID"] = str(last_event_id)
        request = urllib.request.Request(daemon.base + "/api/v1/events" + suffix, headers=headers)
        self.response = urllib.request.urlopen(request, timeout=10)

    def close(self):
        self.response.close()

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()

    def next(self):
        fields = {}
        size = 0
        while True:
            line = self.response.readline(262_145)
            assert line, "Event stream unexpectedly closed"
            size += len(line)
            assert size <= 262_144, "Event frame exceeded test client limit"
            if line in (b"\n", b"\r\n"):
                if not fields:
                    size = 0
                    continue
                assert fields.get("event") == "run_event", fields
                value = json.loads(fields["data"])
                assert int(fields["id"]) == value["sequence"], (fields, value)
                return value
            line = line.decode("utf-8").rstrip("\r\n")
            if line.startswith(":"):
                continue
            key, separator, value = line.partition(":")
            if separator:
                fields[key] = value.lstrip(" ")

    def through(self, task_id, kind):
        collected = []
        for _ in range(2000):
            event = self.next()
            collected.append(event)
            if event.get("task_id") == task_id and event["kind"] == kind:
                return collected
        raise AssertionError(f"Did not observe {kind} for task {task_id}")


def main():
    executable = ROOT / "target" / "debug" / (
        "self-agent-cli.exe" if os.name == "nt" else "self-agent-cli"
    )
    if not executable.is_file():
        raise RuntimeError("Run cargo build -p harness-daemon -p harness-cli first.")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="self-agent-streaming-") as directory:
        project = Path(directory) / "project"
        project.mkdir()
        (project / "用户已有改动.txt").write_text("必须保留。", encoding="utf-8")
        daemon = Daemon(directory)

        def task(identifier):
            return next(t for t in daemon.snapshot()["tasks"] if t["id"] == identifier)

        def done(identifier):
            current = task(identifier)
            return current if current["status"] in TERMINAL else None

        def run(goal):
            return daemon.command({"type": "run_agent", "session_id": session, "content": goal})["task_id"]

        def cli(*args, ok=True, lines=False):
            result = subprocess.run(
                [str(executable), "--data-dir", directory, *args],
                text=True, capture_output=True, timeout=20, encoding="utf-8",
            )
            assert (result.returncode == 0) == ok, (result.returncode, result.stdout, result.stderr)
            return [json.loads(line) for line in result.stdout.splitlines()] if lines else json.loads(result.stdout)

        try:
            daemon.start()
            session = daemon.command({"type": "create_session", "title": "流式回归", "project_path": str(project)})["session_id"]
            daemon.request("PUT", "/api/v1/settings", {
                "provider_source": "manual", "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "model": "fixture-stream", "api_key": SECRET,
            })
            initial_cursor = daemon.snapshot()["event_cursor"]
            assert isinstance(initial_cursor, int) and initial_cursor > 0
            # Default is live-only. Opening before dispatch must not replay old session events.
            with Events(daemon) as stream:
                first = run("FIRST")
                observed = stream.through(first, "assistant_delta")
                assert all(event["sequence"] > initial_cursor for event in observed)
                assert task(first)["status"] == "running"
                assert not (project / "中文证明.txt").exists(), "Tool ran before complete arguments arrived"
                cursor = observed[-1]["sequence"]
            Model.first_release.set()
            completed = wait_for(lambda: done(first), description="Native streaming task completion")
            assert completed["status"] == "completed", completed
            assert completed["result"] == FINAL_TEXT
            assert (project / "中文证明.txt").read_text(encoding="utf-8") == "真实工具写入。"
            with Events(daemon, after=cursor) as stream:
                replay = stream.through(first, "task_completed")
            assert all(event["sequence"] > cursor for event in replay)
            combined = observed + replay
            sequences = [event["sequence"] for event in combined]
            assert sequences == sorted(set(sequences)), sequences
            own = [event for event in combined if event.get("task_id") == first]
            deltas = "".join(event["data"]["text"] for event in own if event["kind"] == "assistant_delta")
            assert deltas == FIRST_PREFIX + FINAL_TEXT, deltas
            assert sum(event["kind"] == "tool_started" for event in own) == 1, own
            assert sum(event["kind"] == "tool_finished" for event in own) == 1, own
            assert sum(event["kind"] == "task_completed" for event in own) == 1
            assert any(event["kind"] == "usage" for event in own), own
            assert len([request for request in Model.requests if request["goal"] == "FIRST"]) == 2
            with Events(daemon, last_event_id=cursor) as stream:
                header_replay = stream.through(first, "task_completed")
            assert header_replay == replay, "Last-Event-ID and query cursor disagree"
            print("PASS incremental UTF-8, fragmented tool arguments, exactly-once tool, ordered reconnect replay")

            for invalid in ("not-a-number", str(daemon.snapshot()["event_cursor"] + 1000)):
                try:
                    Events(daemon, after=invalid)
                    raise AssertionError("Invalid cursor accepted")
                except urllib.error.HTTPError as error:
                    assert error.code == 409, error.code
                    assert json.load(error)["code"] == "snapshot_required"
            print("PASS explicit snapshot recovery for invalid/future cursors")

            cancel_cursor = daemon.snapshot()["event_cursor"]
            with Events(daemon, after=cancel_cursor) as stream:
                cancelled = run("CANCEL")
                stream.through(cancelled, "assistant_delta")
                daemon.command({"type": "cancel_task", "task_id": cancelled})
                stream.through(cancelled, "task_cancelled")
            before_late = task(cancelled).get("stream_output", "")
            Model.cancel_release.set()
            assert Model.cancel_finished.wait(10)
            # Give the already-unblocked model reader time to attempt late publication.
            time.sleep(0.3)
            assert task(cancelled)["status"] == "cancelled"
            assert task(cancelled).get("stream_output", "") == before_late
            own = [e for e in daemon.snapshot()["events"] if e.get("task_id") == cancelled]
            terminal_seq = next(e["sequence"] for e in own if e["kind"] == "task_cancelled")
            assert not any(e["kind"] in ("assistant_delta", "task_completed") and e["sequence"] > terminal_seq for e in own), own
            print("PASS cancellation rejects late output and success terminal")

            for goal in ("TRUNCATED", "MALFORMED"):
                identifier = run(goal)
                current = wait_for(lambda: done(identifier), description=goal)
                assert current["status"] == "failed", current
                assert not (project / "不应存在.txt").exists()
                assert len([request for request in Model.requests if request["goal"] == goal]) == 1
            print("PASS truncated/malformed streams never execute incomplete tools or retry")

            fallback = run("JSON")
            assert wait_for(lambda: done(fallback), description="JSON compatibility")["status"] == "completed"
            assert len([request for request in Model.requests if request["goal"] == "JSON"]) == 1
            secret = run("SECRET")
            assert wait_for(lambda: done(secret), description="split-secret redaction")["status"] == "completed"
            assert SECRET not in json.dumps(daemon.snapshot(), ensure_ascii=False)
            print("PASS single-request JSON compatibility and split-secret redaction")

            secret_cursor = daemon.snapshot()["event_cursor"]
            with Events(daemon, after=secret_cursor) as stream:
                cross_turn = run("CROSS_TURN_SECRET")
                secret_events = stream.through(cross_turn, "task_completed")
            cross_turn_task = task(cross_turn)
            assert cross_turn_task["status"] == "completed", cross_turn_task
            secret_deltas = "".join(
                event["data"]["text"] for event in secret_events
                if event.get("task_id") == cross_turn and event["kind"] == "assistant_delta"
            )
            assert secret_deltas and SECRET not in secret_deltas, secret_deltas
            assert "[凭证已隐藏]" in secret_deltas, secret_deltas
            assert cross_turn_task["stream_output"] == secret_deltas, cross_turn_task
            assert SECRET not in json.dumps(daemon.snapshot(), ensure_ascii=False)
            assert len([request for request in Model.requests if request["goal"] == "CROSS_TURN_SECRET"]) == 2
            print("PASS task-lifetime redaction across two model turns and an intervening real tool")

            output = cli("--follow", "--wait", "10", "run", session, "CLI", lines=True)
            assert output[0]["type"] == "receipt", output
            assert any(line["type"] == "event" and line["event"]["kind"] == "assistant_delta" for line in output), output
            assert output[-1]["type"] == "task_snapshot", output
            assert output[-1]["task"]["status"] == "completed", output
            identifier = output[0]["receipt"]["task_id"]
            assert cli("status", identifier)["status"] == "completed"
            existing = cli("--wait", "10", "follow", identifier, lines=True)
            assert existing[0]["type"] == "task_snapshot" and existing[0]["reset"] is True
            assert existing[-1]["task"]["status"] == "completed"
            failed = cli("--wait", "10", "follow", cancelled, lines=True, ok=False)
            assert failed[-1]["task"]["status"] == "cancelled"
            print("PASS opt-in CLI NDJSON following, normal JSON compatibility and terminal exit codes")

            modification = (project / "中文证明.txt").stat().st_mtime_ns
            previous_cursor = daemon.snapshot()["event_cursor"]
            daemon.stop()
            daemon.start()
            with Events(daemon, after=cursor) as stream:
                restored = stream.through(first, "task_completed")
            assert restored == replay
            assert daemon.snapshot()["event_cursor"] >= previous_cursor
            assert task(first)["status"] == "completed"
            assert (project / "中文证明.txt").stat().st_mtime_ns == modification
            assert (project / "用户已有改动.txt").read_text(encoding="utf-8") == "必须保留。"
            print("PASS durable replay after restart without repeated writes; existing user file preserved")
        finally:
            Model.first_release.set()
            Model.cancel_release.set()
            daemon.stop()
    server.shutdown()
    server.server_close()


if __name__ == "__main__":
    main()
