#!/usr/bin/env python3
"""Real Native writes and bounded change reports; local deterministic model only."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
import urllib.error

from smoke import Daemon, ROOT, wait_for


SECRET = "fixture-changes-private-key"
USER_CONTENT = "BASE\nUSER_STAGED\nUSER_UNSTAGED\n"
TERMINAL = {"completed", "failed", "cancelled"}


class Model(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    hold_started = threading.Event()
    hold_release = threading.Event()
    waiter_started = threading.Event()
    cancel_ready = threading.Event()
    cancel_release = threading.Event()
    calls = []
    errors = []
    lock = threading.Lock()

    def log_message(self, *_args):
        pass

    def packet(self, value):
        text = value if value == "[DONE]" else json.dumps(value, ensure_ascii=False)
        self.wfile.write(("data: " + text + "\n\n").encode("utf-8"))
        self.wfile.flush()

    def delta(self, value, finish=None):
        self.packet({"choices": [{"index": 0, "delta": value, "finish_reason": finish}]})

    def complete(self, text="本地夹具执行完成。"):
        self.delta({"content": text})
        self.delta({}, "stop")
        self.packet("[DONE]")

    def write(self, paths):
        self.delta({"tool_calls": [
            {"index": index, "id": f"write-{index}", "type": "function", "function": {
                "name": "write_file", "arguments": json.dumps({"path": path, "content": content}, ensure_ascii=False),
            }} for index, (path, content) in enumerate(paths)
        ]})
        self.delta({}, "tool_calls")
        self.packet("[DONE]")

    def do_POST(self):
        try:
            self.respond()
        except (BrokenPipeError, ConnectionResetError):
            pass  # The cancelled executor closes its model stream.
        except Exception as error:
            with self.lock:
                self.errors.append(repr(error))

    def respond(self):
        assert self.path == "/v1/chat/completions", self.path
        assert self.headers.get("Authorization") == "Bearer " + SECRET
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["stream"] is True
        goal = [message["content"] for message in body["messages"] if message["role"] == "user"][-1]
        tools = [message for message in body["messages"] if message["role"] == "tool"]
        with self.lock:
            self.calls.append(goal)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.end_headers()
        if tools:
            for tool in tools:
                assert "error" not in json.loads(tool["content"]), tool
            if goal == "FAIL_AFTER_WRITE":
                self.wfile.write(b"data: {malformed-json}\n\n")
                self.wfile.flush()
            elif goal == "CANCEL_AFTER_WRITE":
                self.delta({"content": "真实写入已结束，正在等待取消。"})
                self.cancel_ready.set()
                assert self.cancel_release.wait(30), "Cancel gate timed out"
                self.complete("取消后的迟到成功不得恢复任务。")
            else:
                self.complete()
        elif goal == "NORMAL":
            self.write([
                ("tracked.txt", USER_CONTENT + "TASK_NATIVE\n"),
                ("新增中文.txt", "真实 Native 工具写入。\n"),
                ("ordinary.txt", "updated " + SECRET),
                (".env", "HIDDEN_CONTENT_AFTER"),
            ])
        elif goal == "FAIL_AFTER_WRITE":
            self.write([("failed-write.txt", "失败前的真实写入应保留。\n")])
        elif goal == "CANCEL_AFTER_WRITE":
            self.write([("cancelled-write.txt", "取消前的真实写入应保留。\n")])
        elif goal == "NON_GIT":
            self.write([("非仓库.txt", "非 Git 项目的真实写入。\n")])
        elif goal == "HOLD":
            self.hold_started.set()
            self.delta({"content": "持有同一项目的执行锁。"})
            assert self.hold_release.wait(30), "Project lock gate timed out"
            self.complete()
        elif goal == "QUEUED_WRITE":
            self.waiter_started.set()
            self.write([("queue.txt", "changed while waiting\nWAITER_TASK\n")])
        else:
            raise AssertionError(f"Unexpected goal: {goal}")


def git(root, *args):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0")
    return subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-C", str(root), *args],
        env=env, capture_output=True, check=True, timeout=10,
    ).stdout


def main():
    executable = ROOT / "target" / "debug" / (
        "self-agent-cli.exe" if os.name == "nt" else "self-agent-cli"
    )
    if not executable.is_file():
        raise RuntimeError("Run cargo build -p harness-daemon -p harness-cli first.")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="self-agent-changes-smoke-") as temporary:
        directory = Path(temporary)
        project = directory / "project"
        project.mkdir()
        git(project, "init", "--quiet")
        # Keep fixture bytes identical on Windows: Native writes UTF-8 LF without
        # text-mode translation, so CRLF here would be a genuine interval change.
        (project / "tracked.txt").write_text("BASE\n", encoding="utf-8", newline="\n")
        git(project, "add", "tracked.txt")
        git(project, "-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "commit", "--quiet", "-m", "fixture baseline")
        (project / "tracked.txt").write_text("BASE\nUSER_STAGED\n", encoding="utf-8", newline="\n")
        git(project, "add", "tracked.txt")
        (project / "tracked.txt").write_text(USER_CONTENT, encoding="utf-8", newline="\n")
        (project / "untracked-user.txt").write_text("USER_UNTRACKED_MUST_SURVIVE", encoding="utf-8", newline="\n")
        (project / "ordinary.txt").write_text("initial " + SECRET, encoding="utf-8", newline="\n")
        (project / ".env").write_text("HIDDEN_CONTENT_BEFORE", encoding="utf-8", newline="\n")
        # Git status/refresh could run these repository-controlled filters before
        # tool authorization. Change capture must not launch Git at all.
        filter_markers = []
        attributes = []
        for kind, filename in [("clean", "tracked.txt"), ("process", "process-filter.txt")]:
            if kind == "process":
                (project / filename).write_text("process fixture", encoding="utf-8", newline="\n")
                git(project, "add", filename)
            marker = directory / f"{kind}-executed"
            leak = directory / f"{kind}-credential-leak"
            command = f"printf ran > {shlex.quote(str(marker))}; printf '%s' \"$HARNESS_API_KEY\" > {shlex.quote(str(leak))}; cat"
            git(project, "config", f"filter.hostile-{kind}.{kind}", command)
            git(project, "config", f"filter.hostile-{kind}.required", "true")
            attributes.append(f"{filename} filter=hostile-{kind}")
            filter_markers.extend([marker, leak])
        (project / ".gitattributes").write_text("\n".join(attributes) + "\n", encoding="utf-8", newline="\n")
        (project / "process-filter.txt").write_text("changed process fixture", encoding="utf-8", newline="\n")
        original_index = (project / ".git" / "index").read_bytes()
        daemon = Daemon(directory, {"HARNESS_API_KEY": SECRET})

        def create_session(path, title):
            return daemon.command({"type": "create_session", "title": title, "project_path": str(path)})["session_id"]

        def run(session, goal):
            return daemon.command({"type": "run_agent", "session_id": session, "content": goal})["task_id"]

        def task(identifier):
            return next(value for value in daemon.snapshot()["tasks"] if value["id"] == identifier)

        def wait_report(identifier, status="completed"):
            def ready():
                value = task(identifier)
                summary = value.get("changes_summary") or {}
                return value if value["status"] in TERMINAL and summary.get("state") in {"available", "unavailable", "expired"} else None
            value = wait_for(ready, timeout=25, description=f"{identifier} terminal and finalized change report")
            assert value["status"] == status, value
            assert value["changes_summary"]["state"] == "available", value
            report = daemon.request("GET", f"/api/v1/tasks/{identifier}/changes")
            assert report["scope"] == "observed_execution_interval", report
            assert report["attribution"] == "not_exclusive", report
            assert value["changes_summary"]["visible_files"] == len(report["files"])
            return report

        def cli_report(identifier):
            env = {key: value for key, value in os.environ.items() if not key.startswith("HARNESS_")}
            result = subprocess.run(
                [str(executable), "--data-dir", str(directory), "changes", identifier],
                env=env, capture_output=True, text=True, encoding="utf-8", timeout=10,
            )
            assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
            return json.loads(result.stdout)

        try:
            daemon.start()
            daemon.request("PUT", "/api/v1/settings", {
                "provider_source": "manual", "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "model": "fixture-changes", "api_key": SECRET,
            })
            session = create_session(project, "已有用户修改")
            normal = run(session, "NORMAL")
            normal_report = wait_report(normal)
            assert cli_report(normal) == normal_report
            files = {item["path"]: item for item in normal_report["files"]}
            assert set(files) == {"tracked.txt", "新增中文.txt"}, files
            assert files["tracked.txt"]["status"] == "modified"
            patch = files["tracked.txt"]["diff"]
            assert "+TASK_NATIVE" in patch
            assert "+USER_STAGED" not in patch and "+USER_UNSTAGED" not in patch
            assert files["tracked.txt"]["before_bytes"] == len(USER_CONTENT.encode())
            assert (project / "tracked.txt").read_text(encoding="utf-8") == USER_CONTENT + "TASK_NATIVE\n"
            assert (project / "untracked-user.txt").read_text(encoding="utf-8") == "USER_UNTRACKED_MUST_SURVIVE"
            assert (project / ".git" / "index").read_bytes() == original_index
            assert normal_report["baseline_git"]["state"] == "unavailable"
            assert normal_report["current_git"]["state"] == "unavailable"
            assert normal_report["current_git"]["reason"] == "repository_filter_execution_disabled"
            assert normal_report["partial"] is True
            assert not any(marker.exists() for marker in filter_markers), "Repository filter executed or received daemon credentials"
            print("PASS actual Native diff preserves staged, unstaged, untracked and Git index; HTTP/CLI agree")
            print("PASS hostile clean/process Git filters never execute; Git status explicitly unavailable, not falsely clean")

            serialized = json.dumps(normal_report, ensure_ascii=False)
            assert SECRET not in serialized and "ordinary.txt" not in serialized and ".env" not in serialized
            assert "HIDDEN_CONTENT" not in serialized
            assert normal_report["sensitive_file_count"] >= 1 and normal_report["excluded_count"] >= 1
            snapshot_text = json.dumps(daemon.snapshot())
            assert SECRET not in snapshot_text
            assert "--- a/tracked.txt" not in snapshot_text, "Snapshot embedded an on-demand diff"
            assert "TASK_NATIVE" not in json.dumps(task(normal)["changes_summary"])
            try:
                daemon.request("GET", f"/api/v1/tasks/{normal}/changes", authenticate=False)
                raise AssertionError("Report endpoint did not require authentication")
            except urllib.error.HTTPError as error:
                assert error.code == 401
            try:
                daemon.request("GET", "/api/v1/tasks/missing-task/changes")
                raise AssertionError("Missing task was accepted")
            except urllib.error.HTTPError as error:
                assert error.code in {400, 404}
            print("PASS sensitive and known-key files excluded; reports remain authenticated and on demand")

            failed = run(session, "FAIL_AFTER_WRITE")
            failure_report = wait_report(failed, "failed")
            assert [item["path"] for item in failure_report["files"]] == ["failed-write.txt"]
            assert (project / "failed-write.txt").is_file()
            cancelled = run(session, "CANCEL_AFTER_WRITE")
            assert Model.cancel_ready.wait(20), "Model never reached post-write cancellation gate"
            assert (project / "cancelled-write.txt").is_file()
            daemon.command({"type": "cancel_task", "task_id": cancelled})
            cancel_report = wait_report(cancelled, "cancelled")
            Model.cancel_release.set()
            assert [item["path"] for item in cancel_report["files"]] == ["cancelled-write.txt"]
            assert not task(cancelled).get("result")
            assert cli_report(cancelled) == cancel_report
            print("PASS failure and cancellation retain real changes without changing terminal outcome")

            no_git = directory / "non-git"
            no_git.mkdir()
            no_git_id = run(create_session(no_git, "非 Git 项目"), "NON_GIT")
            non_git_report = wait_report(no_git_id)
            assert non_git_report["baseline_git"]["state"] == "not_git"
            assert non_git_report["current_git"]["state"] == "not_git"
            assert non_git_report["files"][0]["path"] == "非仓库.txt"
            print("PASS non-Git workspace reports explicit status and real text changes")

            (project / "queue.txt").write_text("queue original\n", encoding="utf-8", newline="\n")
            hold = run(create_session(project, "同项目占用锁"), "HOLD")
            assert Model.hold_started.wait(20), "First task did not acquire project lock"
            waiter = run(create_session(project, "同项目等待锁"), "QUEUED_WRITE")
            wait_for(lambda: task(waiter)["status"] in {"queued", "running"}, description="second project task queued")
            assert not Model.waiter_started.wait(0.3), "Second model ran concurrently in same project"
            assert not task(waiter).get("changes_summary"), "Baseline captured before acquiring project lock"
            (project / "queue.txt").write_text("changed while waiting\n", encoding="utf-8", newline="\n")
            (project / "outside-while-waiting.txt").write_text("external user change", encoding="utf-8", newline="\n")
            Model.hold_release.set()
            hold_report = wait_report(hold)
            waiter_report = wait_report(waiter)
            waiter_files = {item["path"]: item for item in waiter_report["files"]}
            assert set(waiter_files) == {"queue.txt"}, waiter_files
            assert waiter_files["queue.txt"]["before_bytes"] == len(b"changed while waiting\n")
            assert "+WAITER_TASK" in waiter_files["queue.txt"]["diff"]
            assert "queue original" not in waiter_files["queue.txt"]["diff"]
            assert any(item["path"] == "outside-while-waiting.txt" for item in hold_report["files"])
            assert (project / ".git" / "index").read_bytes() == original_index
            assert not any(marker.exists() for marker in filter_markers)
            print("PASS baseline begins after project lock, not queue time; external writes remain interval observations")

            before_restart = {normal: normal_report, failed: failure_report, cancelled: cancel_report,
                              no_git_id: non_git_report, waiter: waiter_report}
            counts = list(Model.calls)
            daemon.stop()
            daemon.start()
            for identifier, expected in before_restart.items():
                assert daemon.request("GET", f"/api/v1/tasks/{identifier}/changes") == expected
                assert cli_report(identifier) == expected
            assert task(cancelled)["status"] == "cancelled"
            assert task(failed)["status"] == "failed"
            assert Model.calls == counts, "Restart unexpectedly repeated model execution"
            assert (project / ".git" / "index").read_bytes() == original_index
            assert not Model.errors, Model.errors
            assert Model.calls.count("FAIL_AFTER_WRITE") == 2
            print("PASS restart preserves exact reports, terminal states and user index without replaying writes")
        finally:
            Model.hold_release.set()
            Model.cancel_release.set()
            daemon.stop()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
