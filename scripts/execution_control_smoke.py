#!/usr/bin/env python3
"""Durable questions/approvals and task controls over HTTP; local models only."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import subprocess
import uuid
from pathlib import Path
import tempfile
import threading
import urllib.error

from smoke import Daemon, ROOT, wait_for


SECRET = "fixture-execution-control-secret"
TERMINAL = {"completed", "failed", "cancelled"}


class Model(BaseHTTPRequestHandler):
    errors = []
    calls = []
    lock = threading.Lock()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions"
            assert self.headers.get("Authorization") == "Bearer " + SECRET
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert body.get("tools"), "Small fixture must not need context compaction"
            goal = next(message["content"].split("CONTROL_CASE:", 1)[1].splitlines()[0]
                        for message in reversed(body["messages"])
                        if message["role"] == "user" and "CONTROL_CASE:" in message["content"])
            outputs = [message for message in body["messages"] if message["role"] == "tool"]
            with self.lock:
                self.calls.append({"goal": goal, "tool_outputs": len(outputs), "body": body})
            tool = None
            if goal.startswith("question") and not outputs:
                tool = ("request_user_input", {"prompt": "保留哪个名称？" + (SECRET if goal == "question-canary" else ""), "timeout_seconds": 1 if goal == "question-timeout" else 30})
            elif goal.startswith("question") and len(outputs) == 1:
                answer = json.loads(outputs[0]["content"])["answer"]
                assert answer == "保留原名", answer
                tool = ("write_file", {"path": "result.txt", "content": "answered:" + answer})
            elif goal.startswith("approval") or goal == "model-budget":
                if not outputs:
                    tool = ("write_file", {"path": "result.txt", "content": SECRET + "x" * 10000 if goal == "approval-preview" else "fixture-approved-write"})
            if tool:
                name, arguments = tool
                message = {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "fixture-call-" + str(len(outputs)), "type": "function",
                    "function": {"name": name, "arguments": json.dumps(arguments, ensure_ascii=False)},
                }]}
                finish = "tool_calls"
            else:
                message = {"role": "assistant", "content": "界" * 3000 if goal == "output-budget" else "本地夹具任务已结束。"}
                finish = "stop"
            data = json.dumps({"choices": [{"message": message, "finish_reason": finish}]}, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        except Exception as error:
            with self.lock:
                self.errors.append(repr(error))
            self.send_error(500)


def rejected(call):
    try:
        call()
    except urllib.error.HTTPError as error:
        assert error.code in (400, 409), error.code
        return
    raise AssertionError("Rejected control command was accepted")


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-execution-control-") as temporary:
            root = Path(temporary).resolve()
            daemon = Daemon(root, {"HARNESS_API_KEY": SECRET})
            sequence = 0

            def task(identifier):
                return next(item for item in daemon.snapshot()["tasks"] if item["id"] == identifier)

            def terminal(identifier):
                current = task(identifier)
                return current if current["status"] in TERMINAL else None

            def requests(identifier):
                return [item for item in daemon.snapshot()["execution_requests"] if item["task_id"] == identifier]

            def pending(identifier):
                values = requests(identifier)
                return next((item for item in values if item["status"] == "pending"), None)

            def run(goal, controls=None):
                nonlocal sequence
                sequence += 1
                project = root / f"project-{sequence}"
                project.mkdir()
                session = daemon.command({"type": "create_session", "title": goal, "project_path": str(project)})["session_id"]
                command = {"type": "run_agent", "session_id": session, "content": "CONTROL_CASE:" + goal}
                if controls is not None:
                    command["controls"] = controls
                return daemon.command(command)["task_id"], project

            def decide(request, approve):
                return {"type": "decide_request", "request_id": request["id"],
                        "expected_revision": request["revision"], "approve": approve,
                        "action_digest": request["action_digest"]}

            def assert_one_delivery(identifier):
                assert len([item for item in daemon.snapshot()["deliveries"] if item["task_id"] == identifier]) == 1

            def cli(*arguments, stdin=None, success=True):
                executable = ROOT / "target" / "debug" / ("self-agent-cli.exe" if os.name == "nt" else "self-agent-cli")
                result = subprocess.run([str(executable), "--data-dir", str(daemon.directory), *map(str, arguments)],
                                        input=stdin, text=True, encoding="utf-8", capture_output=True, timeout=20)
                assert (result.returncode == 0) == success, (result.returncode, result.stdout, result.stderr)
                assert SECRET not in result.stdout + result.stderr
                return result

            def final_cli_task(result, identifier):
                records = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
                assert records[0]["type"] == "receipt", records
                assert records[0]["receipt"]["task_id"] == identifier, records
                snapshots = [item["task"] for item in records if item["type"] == "task_snapshot"]
                assert snapshots and snapshots[-1]["id"] == identifier, records
                assert snapshots[-1]["status"] in TERMINAL, snapshots[-1]
                return records, snapshots[-1]

            ask_write = {"permissions": {"write_files": "ask", "run_commands": "deny"}}
            try:
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {
                    "provider_source": "manual", "main_executor": "native", "model": "fixture-control",
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1", "api_key": SECRET,
                })

                identifier, project = run("question-answer")
                request = wait_for(lambda: pending(identifier), description="durable question")
                assert request["kind"] == "question" and request["action"] is None
                assert task(identifier)["status"] == "waiting_input"
                assert not (project / "result.txt").exists()
                unrelated, _ = run("management-responsive")
                assert wait_for(lambda: terminal(unrelated))["status"] == "completed"
                rejected(lambda: daemon.command(decide(request, True)))
                answer = {"type": "answer_request", "request_id": request["id"], "expected_revision": request["revision"], "answer": "保留原名"}
                envelope = daemon.envelope(answer)
                receipt = daemon.request("POST", "/api/v1/commands", envelope)
                assert daemon.request("POST", "/api/v1/commands", envelope) == receipt
                assert wait_for(lambda: terminal(identifier))["status"] == "completed"
                assert (project / "result.txt").read_text(encoding="utf-8") == "answered:保留原名"
                rejected(lambda: daemon.command(answer))
                stored = requests(identifier)[0]
                assert stored["status"] == "answered" and stored["consumed_at"]
                assert stored["decision_actor"] and stored["decision_actor"] != "model"
                assert_one_delivery(identifier)
                print("PASS durable question, explicit answer, command deduplication and responsive unrelated task")

                identifier, project = run("approval-allow", ask_write)
                request = wait_for(lambda: pending(identifier), description="write approval")
                assert request["kind"] == "approval" and request["action_digest"]
                assert request["action"]["details"]["content_preview"] == "fixture-approved-write"
                assert request["action"]["arguments_digest"]
                assert not (project / "result.txt").exists()
                rejected(lambda: daemon.command({**decide(request, True), "action_digest": "wrong"}))
                rejected(lambda: daemon.command({"type": "answer_request", "request_id": request["id"], "expected_revision": 0, "answer": "yes"}))
                rejected(lambda: daemon.command({**decide(request, True), "expected_revision": 9}))
                envelope = daemon.envelope(decide(request, True))
                receipt = daemon.request("POST", "/api/v1/commands", envelope)
                assert daemon.request("POST", "/api/v1/commands", envelope) == receipt
                assert wait_for(lambda: terminal(identifier))["status"] == "completed"
                assert (project / "result.txt").read_text(encoding="utf-8") == "fixture-approved-write"
                assert requests(identifier)[0]["status"] == "approved"
                assert_one_delivery(identifier)
                print("PASS scoped exact-action approval; wrong kind, stale revision and wrong digest rejected")

                identifier, project = run("approval-deny", ask_write)
                request = wait_for(lambda: pending(identifier), description="deniable write")
                daemon.command(decide(request, False))
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed" and done["stop_reason"] == "approval_denied", done
                assert not (project / "result.txt").exists()
                assert requests(identifier)[0]["status"] == "denied"
                assert_one_delivery(identifier)
                print("PASS approval denial stops dependent work without write or automatic retry")

                identifier, project = run("question-timeout")
                wait_for(lambda: pending(identifier), description="expiring question")
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed" and done["stop_reason"] == "input_expired", done
                assert requests(identifier)[0]["status"] == "expired"
                assert not (project / "result.txt").exists()
                assert_one_delivery(identifier)
                print("PASS question expiry never synthesizes an answer or continues writes")

                identifier, project = run("approval-cancel", ask_write)
                request = wait_for(lambda: pending(identifier), description="cancellable approval")
                daemon.command({"type": "cancel_task", "task_id": identifier})
                assert wait_for(lambda: terminal(identifier))["status"] == "cancelled"
                rejected(lambda: daemon.command(decide(request, True)))
                assert requests(identifier)[0]["status"] == "cancelled"
                assert not (project / "result.txt").exists()
                assert_one_delivery(identifier)
                print("PASS cancellation invalidates outstanding approval and rejects late grant")

                identifier, project = run("approval-revoked", ask_write)
                request = wait_for(lambda: pending(identifier), description="revocable approval")
                daemon.request("PUT", "/api/v1/settings", {"execution_controls": {"permissions": {"write_files": "deny", "run_commands": "deny"}}})
                daemon.command(decide(request, True))
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed" and done["stop_reason"] == "permission_denied", done
                assert not (project / "result.txt").exists()
                preserved = daemon.request("PUT", "/api/v1/settings", {"execution_controls": {}})
                assert preserved["execution_controls"]["permissions"]["write_files"] == "deny", preserved
                daemon.request("PUT", "/api/v1/settings", {"execution_controls": {"permissions": {"write_files": "allow", "run_commands": "allow"}}})
                assert_one_delivery(identifier)
                print("PASS policy revocation while awaiting user cannot be bypassed by approval")

                identifier, project = run("question-restart")
                request = wait_for(lambda: pending(identifier), description="restart waiting question")
                before = len(Model.calls)
                daemon.crash()  # Preserve crash-recovery interrupted_wait coverage.
                daemon.start()
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed" and done["stop_reason"] == "interrupted_wait", done
                rejected(lambda: daemon.command({"type": "answer_request", "request_id": request["id"], "expected_revision": 0, "answer": "保留原名"}))
                assert requests(identifier)[0]["status"] == "cancelled"
                assert not (project / "result.txt").exists()
                assert len(Model.calls) == before, "Restart unexpectedly replayed a model request"
                assert_one_delivery(identifier)
                print("PASS restart invalidates waiter, retains audit and never replays an unknown action")

                identifier, project = run("model-budget", {"budget": {"max_model_requests": 1}})
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed", done
                assert done["budget_usage"]["model_requests"] == 1, done
                assert (project / "result.txt").exists(), "Budget stop must preserve already completed tools"
                assert_one_delivery(identifier)
                print("PASS task model-request budget stops next request and preserves completed file result")

                identifier, project = run("question-canary")
                request = wait_for(lambda: pending(identifier), description="redacted question")
                assert SECRET not in json.dumps(request, ensure_ascii=False)
                assert "凭证已隐藏" in request["prompt"], request
                daemon.command({"type": "answer_request", "request_id": request["id"], "expected_revision": request["revision"], "answer": "保留原名"})
                assert wait_for(lambda: terminal(identifier))["status"] == "completed"
                assert_one_delivery(identifier)
                print("PASS model-supplied credential canary is redacted before durable question persistence")

                identifier, project = run("approval-preview", ask_write)
                request = wait_for(lambda: pending(identifier), description="bounded approval preview")
                preview = request["action"]["details"]["content_preview"]
                assert len(preview.encode()) <= 8192 and request["action"]["details"]["content_preview_truncated"]
                assert SECRET not in json.dumps(request, ensure_ascii=False)
                assert "凭证已隐藏" in preview, preview[:100]
                daemon.command(decide(request, True))
                assert wait_for(lambda: terminal(identifier))["status"] == "completed"
                assert (project / "result.txt").read_text(encoding="utf-8") == SECRET + "x" * 10000
                assert_one_delivery(identifier)
                print("PASS bounded redacted file preview retains exact opaque action binding")

                identifier, project = run("output-budget", {"budget": {"max_output_bytes": 1024}})
                done = wait_for(lambda: terminal(identifier))
                assert done["status"] == "failed" and done["stop_reason"] == "output_budget_exhausted", done
                assert done["budget_usage"]["output_bytes"] <= 1024
                assert len(done.get("stream_output", "").encode()) <= 1024
                assert not (project / "result.txt").exists()
                assert_one_delivery(identifier)
                print("PASS oversized Unicode model output cannot exceed durable task output budget")

                identifier, project = run("question-cli")
                request = wait_for(lambda: pending(identifier), description="CLI answer request")
                listed = json.loads(cli("requests", identifier).stdout)
                assert len(listed) == 1 and listed[0]["id"] == request["id"], listed
                result = cli("--follow", "--wait", "10", "answer", request["id"], request["revision"], "-", stdin="保留原名")
                _, done = final_cli_task(result, identifier)
                assert done["status"] == "completed" and (project / "result.txt").read_text(encoding="utf-8") == "answered:保留原名"
                assert_one_delivery(identifier)
                print("PASS standalone CLI lists task requests and answers via stdin with authoritative terminal follow output")

                identifier, project = run("approval-cli-allow", ask_write)
                request = wait_for(lambda: pending(identifier), description="CLI approve request")
                cli("approve", request["id"], request["revision"], "truncated", success=False)
                assert pending(identifier) and not (project / "result.txt").exists()
                command_id = str(uuid.uuid4())
                args = ("--command-id", command_id, "--follow", "--wait", "10", "approve", request["id"], request["revision"], request["action_digest"])
                first, done = final_cli_task(cli(*args), identifier)
                assert done["status"] == "completed" and done["budget_usage"]["mutation_generation"] == 1, done
                calls_before = len([item for item in Model.calls if item["goal"] == "approval-cli-allow"])
                repeated, unchanged = final_cli_task(cli(*args), identifier)
                assert first[0] == repeated[0] and unchanged["budget_usage"]["mutation_generation"] == 1
                assert len([item for item in Model.calls if item["goal"] == "approval-cli-allow"]) == calls_before
                assert len(requests(identifier)) == 1 and (project / "result.txt").read_text(encoding="utf-8") == "fixture-approved-write"
                assert_one_delivery(identifier)
                print("PASS standalone CLI approval follow, full digest validation and repeated command ID execute one mutation")

                identifier, project = run("approval-cli-deny", ask_write)
                request = wait_for(lambda: pending(identifier), description="CLI deny request")
                result = cli("--follow", "--wait", "10", "deny", request["id"], request["revision"], request["action_digest"], success=False)
                _, done = final_cli_task(result, identifier)
                assert done["status"] == "failed" and done["stop_reason"] == "approval_denied", done
                assert not (project / "result.txt").exists()
                assert_one_delivery(identifier)
                print("PASS standalone CLI denial follows terminal failure with nonzero exit and no write")

                serialized = json.dumps(daemon.snapshot(), ensure_ascii=False)
                assert SECRET not in serialized
                assert not Model.errors, Model.errors
            finally:
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
