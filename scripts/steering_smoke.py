#!/usr/bin/env python3
"""Native/Codex task-bound steering, queueing and continuation; local fixtures only."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import shlex
import sys
from pathlib import Path
import tempfile
import threading
import time
import urllib.error

from smoke import Daemon, ROOT, wait_for


TERMINAL = {"completed", "failed", "cancelled"}
SECRET = "fixture-steering-secret"


class Model(BaseHTTPRequestHandler):
    requests = []
    errors = []
    lock = threading.Lock()
    gates = {name: (threading.Event(), threading.Event()) for name in (
        "RUN_WITH_STEER", "STEER_CHANGE", "FINAL_RACE", "CANCEL_GOAL", "UNKNOWN_GOAL", "DROP_THIS_STEER",
    )}

    def log_message(self, *_args):
        pass

    def pause(self, goal):
        started, released = self.gates[goal]
        started.set()
        assert released.wait(15), f"Gate not released: {goal}"

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions"
            assert self.headers.get("Authorization") == "Bearer " + SECRET
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert body.get("tools"), "No summary request is needed by this small fixture"
            goal = [message["content"] for message in body["messages"] if message["role"] == "user"][-1]
            with self.lock:
                self.requests.append({"goal": goal, "body": body})
            if goal in ("RUN_WITH_STEER", "UNKNOWN_GOAL"):
                self.pause(goal)
                calls = [{"id": name, "type": "function", "function": {
                    "name": "write_file", "arguments": json.dumps({"path": name + ".txt", "content": "original tool " + name}),
                }} for name in ("a", "b")]
                message = {"role": "assistant", "content": None, "tool_calls": calls}
                reason = "tool_calls"
            else:
                if goal == "DROP_THIS_STEER":
                    self.pause(goal)
                    self.close_connection = True  # Request received; no response headers: outcome unknown.
                    return
                if goal in ("FINAL_RACE", "CANCEL_GOAL"):
                    self.pause(goal)
                message = {"role": "assistant", "content": goal + "_RESULT"}
                reason = "stop"
            data = json.dumps({"choices": [{"message": message, "finish_reason": reason}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if goal == "STEER_CHANGE":
                self.wfile.flush()
                self.pause(goal)  # HTTP accepted, but no completion body yet.
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        except Exception as error:
            with self.lock:
                self.errors.append(repr(error))


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-steering-") as temporary:
            root = Path(temporary).resolve()
            project = root / "project"
            project.mkdir()
            daemon = Daemon(root, {"HARNESS_API_KEY": SECRET})

            def task(identifier):
                return next(t for t in daemon.snapshot()["tasks"] if t["id"] == identifier)

            def terminal(identifier):
                current = task(identifier)
                return current if current["status"] in TERMINAL else None

            def run(goal):
                return daemon.command({"type": "run_agent", "session_id": session, "content": goal})["task_id"]

            def gate(goal):
                assert Model.gates[goal][0].wait(10), (goal, Model.errors)

            def release(goal):
                Model.gates[goal][1].set()

            def steer(identifier, content):
                return daemon.command({"type": "steer_task", "task_id": identifier, "content": content})

            try:
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {
                    "provider_source": "manual", "main_executor": "native",
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1", "model": "fixture-steering", "api_key": SECRET,
                })
                session = daemon.command({"type": "create_session", "title": "补充与继续回归", "project_path": str(project)})["session_id"]
                first = run("RUN_WITH_STEER")
                gate("RUN_WITH_STEER")
                envelope = daemon.envelope({"type": "steer_task", "task_id": first, "content": "STEER_CHANGE"})
                receipt = daemon.request("POST", "/api/v1/commands", envelope)
                assert receipt == daemon.request("POST", "/api/v1/commands", envelope)
                pending = task(first)["supplements"]
                assert len(pending) == 1 and pending[0]["id"] == receipt["supplement_id"]
                assert pending[0]["status"] == "pending", pending
                queued = run("QUEUED_NEXT")
                assert task(queued)["status"] == "queued"
                release("RUN_WITH_STEER")
                gate("STEER_CHANGE")
                applied = wait_for(lambda: task(first)["supplements"][0] if task(first)["supplements"][0]["status"] == "applied" else None,
                                   description="accepted Native steering request")
                assert applied["applied_via"] == "native_request", applied
                assert task(first)["status"] == "running", "Applied must not imply completed or followed"
                assert all((project / (name + ".txt")).read_text() == "original tool " + name for name in ("a", "b"))
                request = next(r["body"] for r in Model.requests if r["goal"] == "STEER_CHANGE")
                assert json.dumps(request["messages"]).count("STEER_CHANGE") == 1
                assert len([m for m in request["messages"] if m["role"] == "tool"]) == 2
                release("STEER_CHANGE")
                assert wait_for(lambda: terminal(first))["status"] == "completed"
                assert wait_for(lambda: terminal(queued))["status"] == "completed"
                queued_request = next(r["body"] for r in Model.requests if r["goal"] == "QUEUED_NEXT")
                assert "STEER_CHANGE" in json.dumps(queued_request), "Applied instruction lost from authoritative history"
                assert task(queued)["supplements"] == [], ("Task-bound input contaminated the next task", task(queued)["supplements"])
                estimates = [event["data"] for event in daemon.snapshot()["events"]
                             if event.get("task_id") == first and event["kind"] == "context_estimate"]
                assert estimates and all(e["source"] == "native_local" for e in estimates)
                assert estimates[-1]["phase"] == "request" and estimates[-1]["estimated_input_tokens"] > 0
                print("PASS idempotent task-bound steering waits for safe boundary, ACKs 2xx, preserves tools/history and queue order")

                final = run("FINAL_RACE")
                gate("FINAL_RACE")
                steer(final, "TOO_LATE_FOR_FINAL")
                release("FINAL_RACE")
                done = wait_for(lambda: terminal(final))
                assert done["status"] == "completed" and done["supplements"][0]["status"] == "not_applied", done
                assert not any(r["goal"] == "TOO_LATE_FOR_FINAL" for r in Model.requests)
                try:
                    steer(final, "AFTER_TERMINAL")
                    raise AssertionError("Accepted steering on a completed task")
                except urllib.error.HTTPError as error:
                    assert error.code == 400, error.code
                print("PASS final-response race records not_applied without adding a model turn; terminal steering is rejected")

                cancelled = run("CANCEL_GOAL")
                gate("CANCEL_GOAL")
                steer(cancelled, "CANCELLED_PENDING_INPUT")
                daemon.command({"type": "cancel_task", "task_id": cancelled})
                done = wait_for(lambda: terminal(cancelled))
                assert done["status"] == "cancelled" and done["supplements"][0]["status"] == "not_applied", done
                release("CANCEL_GOAL")
                continued = daemon.command({"type": "continue_task", "task_id": cancelled, "content": "CONTINUE_GOAL"})["task_id"]
                assert continued != cancelled
                done = wait_for(lambda: terminal(continued))
                assert done["status"] == "completed" and done["supplements"] == [], done
                assert done["continued_from"] == cancelled, done
                assert task(cancelled)["status"] == "cancelled"
                print("PASS cancellation finalizes pending inputs; explicit continuation creates a new linked task")

                unknown = run("UNKNOWN_GOAL")
                gate("UNKNOWN_GOAL")
                steer(unknown, "DROP_THIS_STEER")
                release("UNKNOWN_GOAL")
                gate("DROP_THIS_STEER")
                assert task(unknown)["supplements"][0]["status"] == "delivering"
                release("DROP_THIS_STEER")
                done = wait_for(lambda: terminal(unknown))
                assert done["status"] == "failed" and done["supplements"][0]["status"] == "unknown", done
                count = len(Model.requests)
                daemon.stop()
                daemon.start()
                time.sleep(0.25)
                assert len(Model.requests) == count, "Uncertain request replayed automatically"
                assert task(unknown)["supplements"][0]["status"] == "unknown"
                assert not Model.errors, Model.errors
                print("PASS uncertain transport stays unknown across restart without replaying requests or tools")
            finally:
                for _, release_event in Model.gates.values():
                    release_event.set()
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()


def codex_smoke():
    if os.name == "nt":
        print("SKIP Codex executable steering fixture on Windows (Native HTTP coverage still runs)")
        return
    with tempfile.TemporaryDirectory(prefix="self-agent-codex-steering-") as temporary:
        root = Path(temporary).resolve()
        project = root / "project"
        project.mkdir()
        codex_home = root / "codex-home"
        codex_home.mkdir()
        wrapper = root / "codex-fixture"
        wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " " +
                           shlex.quote(str(ROOT / "scripts" / "codex_fixture.py")) + ' "$@"\n')
        wrapper.chmod(0o700)
        daemon = Daemon(root, {"CODEX_HOME": str(codex_home)})
        try:
            daemon.start()
            for mode, task_status, input_status in (
                ("steer-silent", "completed", "applied"),
                ("steer-partial", "completed", "applied"),
                ("steer-reject", "completed", "not_applied"),
                ("steer-disconnect", "failed", "unknown"),
                ("steer-complete-first", "completed", "unknown"),
                ("steer-cancel", "cancelled", "unknown"),
            ):
                daemon.request("PUT", "/api/v1/settings", {
                    "provider_source": "manual", "main_executor": "codex",
                    "model": mode, "codex_binary": str(wrapper),
                })
                for marker in ("fixture-started", "fixture-steer-received", "fixture-interrupted"):
                    (project / marker).unlink(missing_ok=True)
                session = daemon.command({"type": "create_session", "title": mode, "project_path": str(project)})["session_id"]
                identifier = daemon.command({"type": "run_agent", "session_id": session, "content": "本地补充回归"})["task_id"]

                def task():
                    return next(t for t in daemon.snapshot()["tasks"] if t["id"] == identifier)

                wait_for(lambda: (project / "fixture-started").exists(), description=mode + " started")
                envelope = daemon.envelope({"type": "steer_task", "task_id": identifier, "content": "运行中补充目标"})
                receipt = daemon.request("POST", "/api/v1/commands", envelope)
                assert receipt == daemon.request("POST", "/api/v1/commands", envelope)
                if mode == "steer-cancel":
                    wait_for(lambda: (project / "fixture-steer-received").exists(), description="Codex in-flight supplement")
                    assert task()["supplements"][0]["status"] == "delivering"
                    daemon.command({"type": "cancel_task", "task_id": identifier})
                    wait_for(lambda: (project / "fixture-interrupted").exists(), description="Codex interrupt")
                done = wait_for(lambda: task() if task()["status"] in TERMINAL else None, description=mode + " terminal")
                assert done["status"] == task_status, done
                assert len(done["supplements"]) == 1, done
                supplement = done["supplements"][0]
                assert supplement["id"] == receipt["supplement_id"] and supplement["status"] == input_status, supplement
                if input_status == "applied":
                    assert supplement["applied_via"] == "codex_turn_steer", supplement
                assert "private-steer-error" not in json.dumps(daemon.snapshot())
                print(f"PASS Codex {mode}: task={task_status}, supplement={input_status}, duplicate receipt remains single")
            entries = [json.loads(line) for line in (project / "fixture-rpc-trace.jsonl").read_text().splitlines()]
            assert len([entry for entry in entries if entry["method"] == "turn/steer"]) == 6
            daemon.stop()
            daemon.start()
            time.sleep(0.25)
            assert len((project / "fixture-rpc-trace.jsonl").read_text().splitlines()) == len(entries), "Codex steering replayed after restart"
        finally:
            daemon.stop()


if __name__ == "__main__":
    main()
    codex_smoke()
