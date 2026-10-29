#!/usr/bin/env python3
"""Verify durable Native summaries and Codex continuation with local fixtures only."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import csv
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading
import time

from smoke import Daemon, ROOT, wait_for


SECRET = "fixture-continuation-secret"
SUMMARY = "SUMMARY_CACHE_V1：FIRST_RESULT，保留原始历史但不重复发送。"
FIRST_RESULT = "FIRST_RESULT " + "OLD_RAW_DETAIL " * 700
TERMINAL = {"completed", "failed", "cancelled"}
PUBLIC_CONTEXT_FIELDS = {
    "session_id", "executor", "state", "reason", "covered_messages",
    "source_task_id", "updated_at",
}


class Model(BaseHTTPRequestHandler):
    requests = []
    errors = []
    lock = threading.Lock()
    cancel_started = threading.Event()
    cancel_release = threading.Event()
    cancel_finished = threading.Event()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions", self.path
            assert self.headers.get("Authorization") == "Bearer " + SECRET
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with self.lock:
                self.requests.append(body)
            if not body.get("tools"):
                text = SUMMARY + " " + SECRET
            else:
                goal = [m["content"] for m in body["messages"] if m["role"] == "user"][-1]
                if goal == "FIRST":
                    text = FIRST_RESULT
                elif goal == "CANCELLED_GOAL":
                    self.cancel_started.set()
                    assert self.cancel_release.wait(15), "Cancellation gate was not released"
                    text = "LATE_RESULT_MUST_NOT_PERSIST"
                else:
                    text = goal + "_RESULT"
            data = json.dumps({"choices": [{"message": {"role": "assistant", "content": text},
                                            "finish_reason": "stop"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Cancellation deliberately disconnects an outstanding request.
        except Exception as error:
            with self.lock:
                self.errors.append(repr(error))
        finally:
            if locals().get("goal") == "CANCELLED_GOAL":
                self.cancel_finished.set()


def task(daemon, identifier):
    return next(t for t in daemon.snapshot()["tasks"] if t["id"] == identifier)


def completed(daemon, identifier):
    current = task(daemon, identifier)
    return current if current["status"] in TERMINAL else None


def dispatch(daemon, session, goal):
    return daemon.command({"type": "run_agent", "session_id": session, "content": goal})["task_id"]


def run(daemon, session, goal, status="completed"):
    identifier = dispatch(daemon, session, goal)
    result = wait_for(lambda: completed(daemon, identifier), description=goal)
    assert result["status"] == status, result
    return result


def create(daemon, project, title):
    return daemon.command({"type": "create_session", "title": title,
                           "project_path": str(project)})["session_id"]


def context(daemon, session, executor):
    states = daemon.snapshot()["context_states"]
    for state in states:
        assert set(state) == PUBLIC_CONTEXT_FIELDS, state
    public = json.dumps(states, ensure_ascii=False)
    for private in (SUMMARY, SECRET, "prefix_fingerprint", "summary_config", "thread_id"):
        assert private not in public, private
    return next(s for s in states if s["session_id"] == session and s["executor"] == executor)


def request_for(goal):
    found = [r for r in Model.requests if r.get("tools") and
             [m["content"] for m in r["messages"] if m["role"] == "user"][-1] == goal]
    assert len(found) == 1, (goal, len(found), Model.errors)
    return found[0]


def transcript(request):
    return json.dumps(request["messages"], ensure_ascii=False)


def summary_requests():
    return [r for r in Model.requests if not r.get("tools")]


def native_smoke():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-native-memory-") as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            preserved = project / "用户已有改动.txt"
            preserved.write_text("已有修改不得覆盖。", encoding="utf-8")
            daemon = Daemon(root, {"HARNESS_API_KEY": SECRET})
            try:
                daemon.start()
                session = create(daemon, project, "持久摘要回归")
                daemon.request("PUT", "/api/v1/settings", {
                    "provider_source": "manual", "main_executor": "native",
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                    "model": "fixture-memory", "api_key": SECRET,
                    "model_contexts": {"fixture-memory": {
                        "context_window": 8192, "compression_threshold": 50,
                    }},
                })
                first = run(daemon, session, "FIRST")
                assert first["result"] == FIRST_RESULT, (len(first["result"]), first["result"][-80:])
                assert not summary_requests(), "No prior history should be persisted on the first task"
                assert context(daemon, session, "native")["state"] == "empty"
                second = run(daemon, session, "SECOND")
                summaries = summary_requests()
                assert summaries, "Long first result did not trigger actual preflight summarization"
                source = "".join(transcript(r) for r in summaries)
                assert "FIRST_RESULT" in source and "OLD_RAW_DETAIL" in source
                assert "SECOND" not in source, "Current objective was incorrectly included in persisted summary"
                saved = context(daemon, session, "native")
                assert saved["state"] == "available" and saved["covered_messages"] == 2, saved
                assert saved["source_task_id"] == second["id"], saved
                second_input = transcript(request_for("SECOND"))
                assert SUMMARY in second_input and "OLD_RAW_DETAIL" not in second_input
                assert SECRET not in second_input, "Persisted summary was not redacted"
                run(daemon, session, "THIRD")
                third_input = transcript(request_for("THIRD"))
                assert SUMMARY in third_input and "SECOND_RESULT" in third_input
                assert "OLD_RAW_DETAIL" not in third_input
                assert len(summary_requests()) == len(summaries), "Cached prefix was summarized again unnecessarily"
                print("PASS Native three tasks: bounded preflight summary, exact prefix, latest suffix and redaction")

                daemon.stop()
                daemon.start()
                assert context(daemon, session, "native")["covered_messages"] == 2
                run(daemon, session, "FOURTH")
                fourth_input = transcript(request_for("FOURTH"))
                assert SUMMARY in fourth_input and "SECOND_RESULT" in fourth_input and "THIRD_RESULT" in fourth_input
                assert "OLD_RAW_DETAIL" not in fourth_input
                assert len(summary_requests()) == len(summaries)
                print("PASS Native summary survives daemon restart without losing later task results")

                cancelled = dispatch(daemon, session, "CANCELLED_GOAL")
                assert Model.cancel_started.wait(10), Model.errors
                daemon.command({"type": "cancel_task", "task_id": cancelled})
                cancellation = wait_for(lambda: completed(daemon, cancelled), description="Native cancellation")
                assert cancellation["status"] == "cancelled", cancellation
                Model.cancel_release.set()
                assert Model.cancel_finished.wait(10)
                run(daemon, session, "AFTER_CANCEL")
                after_cancel = transcript(request_for("AFTER_CANCEL"))
                assert SUMMARY in after_cancel and "CANCELLED_GOAL" in after_cancel
                assert cancellation["error"] in after_cancel
                assert "LATE_RESULT_MUST_NOT_PERSIST" not in after_cancel
                other = create(daemon, project, "独立会话")
                run(daemon, other, "OTHER_SESSION")
                isolated = transcript(request_for("OTHER_SESSION"))
                assert SUMMARY not in isolated and "FIRST_RESULT" not in isolated
                assert "CANCELLED_GOAL" not in isolated
                assert context(daemon, other, "native")["state"] == "empty"
                print("PASS Native cancelled-task history remains visible; summaries never cross sessions")

                daemon.request("PUT", "/api/v1/settings", {
                    "model": "fixture-memory-changed",
                    "model_contexts": {"fixture-memory-changed": {
                        "context_window": 32768, "compression_threshold": 90,
                    }},
                })
                run(daemon, session, "AFTER_MODEL_CHANGE")
                changed = transcript(request_for("AFTER_MODEL_CHANGE"))
                assert "OLD_RAW_DETAIL" in changed and "AFTER_CANCEL_RESULT" in changed
                assert SUMMARY not in changed
                assert context(daemon, session, "native")["state"] == "invalidated"
                assert preserved.read_text(encoding="utf-8") == "已有修改不得覆盖。"
                assert not Model.errors, Model.errors
                print("PASS Native model/policy change invalidates cache; full history and safe public state remain")
            finally:
                Model.cancel_release.set()
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()


def trace(project):
    path = project / "fixture-rpc-trace.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def starts(project):
    return [entry for entry in trace(project) if entry["method"] in ("thread/start", "thread/resume")]


def codex_smoke():
    if os.name == "nt":
        print("SKIP Codex stdio executable fixture on Windows (Native HTTP coverage still runs)")
        return
    with tempfile.TemporaryDirectory(prefix="self-agent-codex-continuation-") as temporary:
        root = Path(temporary)
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
            session = create(daemon, project, "Codex 持久会话回归")
            daemon.request("PUT", "/api/v1/settings", {
                "provider_source": "manual", "main_executor": "codex",
                "model": "success", "codex_binary": str(wrapper),
            })
            for goal in ("CODEX_FIRST", "CODEX_SECOND", "CODEX_THIRD"):
                result = run(daemon, session, goal)
                assert result["result"] == "测试执行结果", result
            initial = starts(project)
            assert [entry["method"] for entry in initial] == ["thread/start", "thread/resume", "thread/resume"], initial
            state = json.loads((project / "fixture-thread-state.json").read_text())
            assert len(state) == 1, state
            thread_id, thread_state = next(iter(state.items()))
            assert thread_state["inputs"] == ["CODEX_FIRST", "CODEX_SECOND", "CODEX_THIRD"], thread_state
            assert all(entry["params"]["threadId"] == thread_id for entry in initial[1:])
            assert context(daemon, session, "codex")["state"] == "idle"
            assert (project / "fixture-result.txt").read_text(encoding="utf-8") == "真实本地夹具写入"
            print("PASS Codex three tasks use one persistent thread; resumed turns send only the new objective")

            daemon.stop()
            daemon.start()
            run(daemon, session, "CODEX_FOURTH")
            assert starts(project)[-1]["method"] == "thread/resume"
            assert starts(project)[-1]["params"]["threadId"] == thread_id
            state = json.loads((project / "fixture-thread-state.json").read_text())
            assert state[thread_id]["inputs"][-1] == "CODEX_FOURTH"
            daemon.request("PUT", "/api/v1/settings", {"model": "stream-events"})
            run(daemon, session, "CODEX_CHANGED")
            assert starts(project)[-1]["method"] == "thread/start"
            state = json.loads((project / "fixture-thread-state.json").read_text())
            assert len(state) == 2, state
            bootstrap = next(value["inputs"][0] for key, value in state.items() if key != thread_id)
            for goal in ("CODEX_FIRST", "CODEX_SECOND", "CODEX_THIRD", "CODEX_FOURTH", "CODEX_CHANGED"):
                assert bootstrap.count(goal) == 1, bootstrap
            print("PASS Codex completed thread resumes after restart; model change bootstraps full history once")

            daemon.request("PUT", "/api/v1/settings", {"model": "failure"})
            for goal in ("CODEX_FAILED_ONE", "CODEX_FAILED_TWO"):
                run(daemon, session, goal, status="failed")
                assert starts(project)[-1]["method"] == "thread/start"
                assert context(daemon, session, "codex")["state"] == "invalidated"
            daemon.request("PUT", "/api/v1/settings", {"model": "unknown-start"})
            unknown = run(daemon, session, "CODEX_UNKNOWN_WRITE", status="failed")
            assert (project / "fixture-unknown-write").read_text() == "accepted once"
            before = trace(project)
            daemon.stop()
            daemon.start()
            time.sleep(0.25)
            assert task(daemon, unknown["id"])["status"] == "failed"
            assert trace(project) == before, "Unknown accepted turn was automatically replayed"
            print("PASS Codex failures invalidate continuation; unknown accepted write is not replayed on restart")

            daemon.request("PUT", "/api/v1/settings", {"model": "hang"})
            (project / "fixture-started").unlink(missing_ok=True)
            inflight = dispatch(daemon, session, "CODEX_RESTART_INFLIGHT")
            wait_for(lambda: (project / "fixture-started").exists() and
                     context(daemon, session, "codex")["state"] == "inflight", description="in-flight Codex turn")
            assert task(daemon, inflight)["status"] == "running"
            before = trace(project)
            daemon.crash()  # Real interruption, not graceful SIGTERM drain.
            daemon.start()
            recovered = task(daemon, inflight)
            assert recovered["status"] == "failed" and "没有自动重放" in recovered["error"], recovered
            assert context(daemon, session, "codex")["state"] == "invalidated"
            time.sleep(0.25)
            assert trace(project) == before, "Daemon recovery replayed an in-flight turn"
            print("PASS Codex daemon interruption fails unknown task durably without replaying the turn")
            assert recovered["execution_cleanup"] == "unknown", recovered
            blocked = run(daemon, session, "CODEX_AFTER_UNKNOWN_CLEANUP", status="failed")
            assert blocked["stop_reason"] == "workspace_cleanup_unknown", blocked
            assert trace(project) == before, "Quarantined project launched another Codex process"
            assert task(daemon, inflight)["execution_cleanup"] == "unknown"
            print("PASS crash leaves original cleanup unknown and fences same-project work without automatic resolution")

            # Deadline/output budgets are separate cases, not permission to
            # reuse the crashed project's uncertain process/write authority.
            project = root / "budget-project"
            project.mkdir()
            project = project.resolve()
            session = create(daemon, project, "Codex 独立预算回归")
            for phase in ["config", "version"]:
                marker = project / f"fixture-{phase}-stall"
                marker.write_text("local deadline fixture")
                (project / "fixture-probe-pid").unlink(missing_ok=True)
                daemon.request("PUT", "/api/v1/settings", {"model": "success"})
                before = len(starts(project))
                identifier = daemon.command({"type": "run_agent", "session_id": session,
                    "content": f"CODEX_{phase.upper()}_DEADLINE",
                    "controls": {"budget": {"max_runtime_seconds": 1}}})["task_id"]
                failed = wait_for(lambda: completed(daemon, identifier), timeout=15, description=f"Codex {phase} deadline")
                assert failed["status"] == "failed" and failed["stop_reason"] == "runtime_exhausted", failed
                assert (project / "fixture-probe-pid").exists(), "Fixture preflight process did not start"
                pid = int((project / "fixture-probe-pid").read_text())
                def reaped():
                    if os.name == "nt":
                        listing = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
                            capture_output=True, text=True, check=True, timeout=5)
                        return not any(len(row) > 1 and row[1] == str(pid)
                            for row in csv.reader(listing.stdout.splitlines()))
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        return True
                    return False
                wait_for(reaped, timeout=5, description="Codex preflight child reaped")
                assert len(starts(project)) == before, "Deadline must prevent thread/turn side effects"
                marker.unlink()
            print("PASS Codex version/config stalls obey task runtime deadline and reap preflight child")

            daemon.request("PUT", "/api/v1/settings", {"model": "evidence-large"})
            identifier = daemon.command({"type": "run_agent", "session_id": session,
                "content": "CODEX_OUTPUT_BUDGET", "controls": {"budget": {"max_output_bytes": 1024}}})["task_id"]
            bounded = wait_for(lambda: completed(daemon, identifier), timeout=15, description="Codex evidence budget")
            assert bounded["status"] == "failed" and bounded["stop_reason"] == "output_budget_exhausted", bounded
            assert bounded["budget_usage"]["output_bytes"] <= 1024, bounded
            assert bounded["evidence"][-1]["exit_code"] == 0, bounded
            assert not bounded["evidence"][-1]["output"], bounded
            assert "证据" * 100 not in json.dumps(bounded, ensure_ascii=False), bounded
            print("PASS Codex large command evidence obeys output cap without falsifying exit status")
        finally:
            daemon.stop()


if __name__ == "__main__":
    native_smoke()
    codex_smoke()
