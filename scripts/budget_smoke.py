#!/usr/bin/env python3
"""Persistent Native budgets, permission ceilings and repair gates; local fixtures only."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
import threading
import time
import urllib.error

from smoke import Daemon, wait_for

TERMINAL = {"completed", "failed", "cancelled"}
SECRET = "fixture-budget-secret"


def python_command(script):
    arguments = [sys.executable, script]
    if os.name == "nt":
        # Native commands use PowerShell; preserve both spaces and exit codes.
        quoted = " ".join("'" + argument.replace("'", "''") + "'" for argument in arguments)
        return f"& {quoted}; exit $LASTEXITCODE"
    return shlex.join(arguments)


class Model(BaseHTTPRequestHandler):
    counts = {}
    summary_counts = {}
    errors = []
    lock = threading.Lock()
    deadline_release = threading.Event()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions"
            assert self.headers.get("Authorization") == "Bearer " + SECRET
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            model = request["model"]
            with self.lock:
                turn = self.counts.get(model, 0) + 1
                self.counts[model] = turn
                if not request.get("tools"):
                    self.summary_counts[model] = self.summary_counts.get(model, 0) + 1
            if model in {"fixture-retry", "fixture-retry-limit"} and turn == 1:
                self.send_response(503)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if model == "fixture-deadline":
                assert self.deadline_release.wait(15), "Deadline test did not release its cancelled response"
            if not request.get("tools"):
                message = {"role": "assistant", "content": "早期历史摘要：保留目标与已完成事实。"}
            elif model == "fixture-stdout":
                message = self.call("run_command", {"command": python_command("loud.py")}, turn)
            elif model == "fixture-command-deadline":
                message = self.call("run_command", {"command": python_command("long.py")}, turn)
            elif model in {"fixture-limit", "fixture-snapshot"}:
                message = self.call("list_files", {"path": "."}, turn)
            elif model == "fixture-output":
                message = {"role": "assistant", "content": "界" * 2000}
            elif model == "fixture-denied":
                message = self.call("write_file", {"path": "denied.txt", "content": "must not appear"}, turn)
            elif model in {"fixture-repair", "fixture-repair-zero"} and turn % 2 == 1:
                message = self.call("write_file", {"path": "result.txt", "content": "broken" if turn == 1 else "fixed"}, turn)
            else:
                # Deliberately untrustworthy completion. Only actual command
                # results may make the daemon's acceptance gate pass.
                message = {"role": "assistant", "content": "所有测试均通过（模型声明，必须以真实退出码验证）。"}
            data = json.dumps({"choices": [{"message": message, "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}]}).encode()
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

    @staticmethod
    def call(name, arguments, turn):
        return {"role": "assistant", "content": None, "tool_calls": [{"id": f"call-{turn}", "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-budget-") as temporary:
            root = Path(temporary).resolve()
            daemon = Daemon(root, {"HARNESS_ALLOW_COMMANDS": "1", "HARNESS_API_KEY": SECRET})

            def task(identifier):
                return next(item for item in daemon.snapshot()["tasks"] if item["id"] == identifier)

            def finished(identifier):
                item = task(identifier)
                return item if item["status"] in TERMINAL else None

            def run(model, controls, prepare=None):
                project = root / model
                project.mkdir()
                (project / "user-existing.txt").write_text("保留用户文件", encoding="utf-8")
                if prepare:
                    prepare(project)
                daemon.request("PUT", "/api/v1/settings", {"model": model})
                session = daemon.command({"type": "create_session", "title": model, "project_path": str(project)})["session_id"]
                receipt = daemon.command({"type": "run_agent", "session_id": session, "content": model, "controls": controls})
                result = wait_for(lambda: finished(receipt["task_id"]), timeout=25, description=model)
                assert (project / "user-existing.txt").read_text(encoding="utf-8") == "保留用户文件"
                return result, project

            def checker(project):
                script = "from pathlib import Path\nimport sys\nvalue = Path('result.txt').read_text()\nprint('actual check:', value)\nsys.exit(0 if value == 'fixed' else 1)\n"
                (project / "check.py").write_text(script)
                (project / "check_second.py").write_text(script)
                (project / "result.txt").write_text("broken")

            def long_command(project):
                # An ordinary child in the command's process group writes a
                # delayed marker unless cancellation reaps the whole tree.
                (project / "child.py").write_text("from pathlib import Path\nimport time\nPath('child-started').write_text('ready')\ntime.sleep(3)\nPath('child-escaped').write_text('bad')\n")
                (project / "long.py").write_text("import subprocess, sys, time\nfrom pathlib import Path\nchild=subprocess.Popen([sys.executable,'child.py'])\nwhile not Path('child-started').exists(): time.sleep(0.01)\nPath('parent-started').write_text('ready')\nchild.wait()\n")

            try:
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {"provider_source": "manual", "main_executor": "native", "base_url": f"http://127.0.0.1:{server.server_port}/v1", "model": "fixture-limit", "api_key": SECRET})
                limited, _ = run("fixture-limit", {"budget": {"max_model_requests": 2}})
                assert limited["status"] == "failed", limited
                assert limited["stop_reason"] == "model_budget_exhausted", limited
                assert limited["budget_usage"]["model_requests"] == 2
                assert Model.counts["fixture-limit"] == 2
                print("PASS model request budget stops at exact durable boundary")

                output, _ = run("fixture-output", {"budget": {"max_output_bytes": 1024}})
                assert output["status"] == "failed", output
                assert output["stop_reason"] == "output_budget_exhausted", output
                assert output["budget_usage"]["output_bytes"] <= 1024
                print("PASS output limit preserves bounded committed data")

                def loud(project):
                    (project / "loud.py").write_text("print('OUTPUT_SENTINEL_' * 750)\n")

                for model, acceptance in [("fixture-stdout", []), ("fixture-verification-stdout", [python_command("loud.py")])]:
                    overflowing, _ = run(model, {"budget": {"max_output_bytes": 1024}, "acceptance_commands": acceptance}, loud)
                    assert overflowing["status"] == "failed", overflowing
                    assert overflowing["stop_reason"] == "output_budget_exhausted", overflowing
                    assert overflowing["budget_usage"]["output_bytes"] <= 1024
                    assert "OUTPUT_SENTINEL_" not in json.dumps(overflowing), overflowing
                    if acceptance:
                        attempts = overflowing["verification_attempts"]
                        assert attempts[-1]["exit_code"] == 0, overflowing
                        assert not attempts[-1]["output"], overflowing
                    else:
                        evidence = overflowing["evidence"]
                        assert evidence[-1]["exit_code"] == 0, overflowing
                        assert not evidence[-1]["output"], overflowing
                print("PASS real 10KiB stdout cannot bypass output budget through evidence or verification")

                deadline, _ = run("fixture-deadline", {"budget": {"max_runtime_seconds": 1}})
                assert deadline["status"] == "failed", deadline
                assert deadline["stop_reason"] == "runtime_exhausted", deadline
                # The provider has not responded: terminal state must come
                # from the daemon deadline, not the fixture's final response.
                assert not Model.deadline_release.is_set()
                Model.deadline_release.set()
                print("PASS runtime deadline cancels in-flight provider request")

                command_timeout, project = run("fixture-command-deadline", {"budget": {"max_runtime_seconds": 1}}, long_command)
                assert command_timeout["status"] == "failed", command_timeout
                assert command_timeout["stop_reason"] == "runtime_exhausted", command_timeout
                assert (project / "parent-started").exists(), "Fixture shell/child never started"
                time.sleep(3.2)
                assert not (project / "child-escaped").exists(), "Cancelled command child escaped process-tree cleanup"
                print("PASS runtime deadline reaps real command and ordinary delayed child")

                denied, project = run("fixture-denied", {"permissions": {"write_files": "deny", "run_commands": "deny"}})
                assert denied["status"] == "failed", denied
                assert not (project / "denied.txt").exists()
                assert denied.get("stop_reason") == "permission_denied", denied
                print("PASS narrowed task policy prevents real file effect")

                def private_file(project):
                    (project / "PRIVATE_FILENAME_SENTINEL.txt").write_text("PRIVATE_CONTENT_SENTINEL")

                no_reads, _ = run("fixture-no-reads", {"permissions": {"read_files": False, "run_commands": "deny"}}, private_file)
                assert no_reads["status"] == "completed", no_reads
                no_reads = wait_for(lambda: task(no_reads["id"]) if task(no_reads["id"]).get("changes_summary", {}).get("state") else None, description="read policy report")
                assert no_reads["changes_summary"]["state"] == "unavailable", no_reads
                report = daemon.request("GET", f"/api/v1/tasks/{no_reads['id']}/changes")
                assert report["state"] == "unavailable", report
                public = json.dumps({"task": no_reads, "report": report})
                assert "PRIVATE_FILENAME_SENTINEL" not in public and "PRIVATE_CONTENT_SENTINEL" not in public
                print("PASS read-denied task can explain without collecting or disclosing project changes")

                saved_root = root / "saved-root"
                outside_root = root / "outside-root"
                saved_root.mkdir()
                outside_root.mkdir()
                (outside_root / "sentinel.txt").write_text("outside must remain unchanged")
                daemon.request("PUT", "/api/v1/settings", {"model": "fixture-root-symlink"})
                root_session = daemon.command({"type": "create_session", "title": "saved root replacement", "project_path": str(saved_root)})["session_id"]
                saved_root.rename(root / "original-root")
                try:
                    saved_root.symlink_to(outside_root, target_is_directory=True)
                except (PermissionError, NotImplementedError) as error:
                    print(f"SKIP saved-root symlink fixture: platform denied symlink creation: {error}")
                else:
                    rooted = daemon.command({"type": "run_agent", "session_id": root_session, "content": "must reject replaced project root"})["task_id"]
                    rooted = wait_for(lambda: finished(rooted), timeout=25)
                    assert rooted["status"] == "failed", rooted
                    assert Model.counts.get("fixture-root-symlink", 0) == 0
                    assert (outside_root / "sentinel.txt").read_text() == "outside must remain unchanged"
                    assert list(outside_root.iterdir()) == [outside_root / "sentinel.txt"]
                    print("PASS replaced saved project root rejected before model/tool execution")

                daemon.request("PUT", "/api/v1/settings", {"execution_controls": {"budget": {"max_retries": 1}}})
                retried, _ = run("fixture-retry", {"budget": {"max_retries": 1}})
                assert retried["status"] == "completed", retried
                assert retried["budget_usage"]["retries"] == 1
                assert retried["budget_usage"]["model_requests"] == 2
                retry_limit, _ = run("fixture-retry-limit", {"budget": {"max_retries": 1, "max_model_requests": 1}})
                assert retry_limit["status"] == "failed", retry_limit
                assert retry_limit["stop_reason"] == "model_budget_exhausted", retry_limit
                assert Model.counts["fixture-retry-limit"] == 1
                daemon.request("PUT", "/api/v1/settings", {"execution_controls": {"budget": {"max_retries": 0}}})
                print("PASS explicit transient rejection retry consumes shared request budget")

                repaired, project = run("fixture-repair", {"acceptance_commands": [python_command("check.py"), python_command("check_second.py")]}, checker)
                assert repaired["status"] == "completed", repaired
                attempts = repaired["verification_attempts"]
                assert [attempt["exit_code"] for attempt in attempts] == [1, 1, 0, 0], repaired
                assert attempts[0]["output"] and "broken" in attempts[0]["output"]
                assert repaired["budget_usage"]["repair_attempts"] == 1
                assert attempts[-1]["mutation_generation"] == attempts[-2]["mutation_generation"] == repaired["budget_usage"]["mutation_generation"]
                assert (project / "result.txt").read_text() == "fixed"
                print("PASS real failure -> repair -> exact-command reverify, original failure retained")

                exhausted, project = run("fixture-repair-zero", {"budget": {"max_repair_attempts": 0}, "acceptance_commands": [python_command("check.py")]}, checker)
                assert exhausted["status"] == "failed", exhausted
                assert exhausted["stop_reason"] == "repair_budget_exhausted", exhausted
                assert exhausted["verification_attempts"][0]["exit_code"] == 1
                assert (project / "result.txt").read_text() == "broken"
                print("PASS model claim cannot bypass actual failed acceptance or repair budget")

                compact_project = root / "compact-history"
                compact_project.mkdir()
                daemon.request("PUT", "/api/v1/settings", {"model": "fixture-compaction", "model_contexts": {"fixture-compaction": {"context_window": 32768, "compression_threshold": 80}}})
                compact_session = daemon.command({"type": "create_session", "title": "真实历史压缩预算", "project_path": str(compact_project)})["session_id"]
                seed = daemon.command({"type": "run_agent", "session_id": compact_session, "content": "history " * 3000})["task_id"]
                assert wait_for(lambda: finished(seed), timeout=25)["status"] == "completed"
                before = Model.counts["fixture-compaction"]
                daemon.request("PUT", "/api/v1/settings", {"model_contexts": {"fixture-compaction": {"context_window": 8192, "compression_threshold": 40}}})
                compacted = daemon.command({"type": "run_agent", "session_id": compact_session, "content": "继续读取真实历史", "controls": {"budget": {"max_model_requests": 1}}})["task_id"]
                compacted = wait_for(lambda: finished(compacted), timeout=25)
                assert compacted["status"] == "failed", compacted
                assert compacted["stop_reason"] == "model_budget_exhausted", compacted
                assert compacted["budget_usage"]["model_requests"] == 1
                assert Model.counts["fixture-compaction"] == before + 1
                assert Model.summary_counts["fixture-compaction"] == 1
                daemon.request("PUT", "/api/v1/settings", {"model_contexts": {}})
                print("PASS actual history compaction consumes same request budget before normal completion")

                snapshot, _ = run("fixture-snapshot", {"budget": {"max_model_requests": 1}})
                original_controls = snapshot["controls"]
                original_usage = snapshot["budget_usage"]
                daemon.request("PUT", "/api/v1/settings", {"execution_controls": {"budget": {"max_model_requests": 7}}})
                assert task(snapshot["id"])["controls"] == original_controls
                daemon.stop()
                daemon.start()
                restored = task(snapshot["id"])
                assert restored["controls"] == original_controls
                assert restored["budget_usage"] == original_usage
                assert Model.counts["fixture-snapshot"] == 1
                deliveries = [item for item in daemon.snapshot()["deliveries"] if item["task_id"] == snapshot["id"]]
                assert len(deliveries) == 1
                assert SECRET not in json.dumps(daemon.snapshot())
                assert not Model.errors, Model.errors
                print("PASS snapshots and counters survive settings edits/restart without replay or duplicate callback")
            finally:
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()
    print("All execution budget scenarios passed; no cloud model or external service was called.")


if __name__ == "__main__":
    main()
