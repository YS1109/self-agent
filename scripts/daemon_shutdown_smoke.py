#!/usr/bin/env python3
"""Actual daemon owner drain/exit and restart, local demo fixtures only.

HTTP 200 with the original response's authoritative stopped snapshot is the
confirmation; refused connections, test terminate(), or missing PID are NOT.
Windows exercises owner HTTP; Unix additionally exercises actual SIGINT and SIGTERM.
"""
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import urllib.error
import urllib.request
import uuid

from smoke import Daemon, ROOT, REMOTE, wait_for



def binary(name):
    return ROOT / "target" / "debug" / (name + (".exe" if os.name == "nt" else ""))


def verify_fresh_binaries():
    dependencies = {
        "harness-daemon": ("harness-core", "harness-daemon"),
        "self-agent-cli": ("harness-core", "harness-daemon", "harness-cli"),
    }
    for name, crates in dependencies.items():
        inputs = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
        for crate in crates:
            package = ROOT / "crates" / crate
            inputs.append(package / "Cargo.toml")
            # CLI links the daemon library, not the daemon's binary main.
            # Cargo correctly need not relink CLI after a main-only change.
            inputs.extend(path for path in (package / "src").rglob("*.rs")
                          if not (name == "self-agent-cli" and crate == "harness-daemon"
                                  and path == package / "src" / "main.rs"))
        executable = binary(name)
        if not executable.is_file() or any(path.stat().st_mtime_ns > executable.stat().st_mtime_ns for path in inputs):
            raise RuntimeError("请先针对最终冻结源码重新 cargo build -p harness-daemon -p harness-cli；不使用旧二进制验收关闭。")


def cli(directory, *args):
    # The explicit selected data directory avoids ambient daemon discovery.
    # Global options precede the subcommand in the actual CLI grammar.
    env = {key: value for key, value in os.environ.items() if not key.startswith("HARNESS_")}
    result = subprocess.run(
        [str(binary("self-agent-cli")), "--data-dir", str(Path(directory).resolve()), *args],
        cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8", timeout=40,
    )
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
    return json.loads(result.stdout)


def fixture(directory):
    daemon = Daemon(directory, {"HARNESS_DEMO_STEP_MS": "5000"})
    try:
        daemon.start()
        daemon.request("PUT", "/api/v1/settings", {
            "execution_mode": "demo", "base_url": "http://127.0.0.1:1/v1",
            "model": "shutdown-no-live-model",
        })
        project = Path(directory).resolve() / "project"
        project.mkdir(exist_ok=True)
        sid = daemon.command({"type": "create_session", "title": "shutdown fixture",
                              "project_path": str(project)})["session_id"]
        return daemon, sid
    except BaseException:
        daemon.stop()
        raise



def enqueue(daemon, sid):
    return daemon.command({"type": "run_agent", "session_id": sid,
                           "content": "演示取消，不调用真实模型或服务"})["task_id"]


def running(daemon, task_id):
    return any(t["id"] == task_id and t["status"] == "running"
               for t in daemon.snapshot()["tasks"])


def wait_stop(daemon):
    instance = daemon.request("GET", "/api/v1/lifecycle")["instance_id"]
    data = json.dumps({"expected_instance": instance}).encode()
    request = urllib.request.Request(
        daemon.base + "/api/v1/lifecycle/shutdown?wait_ms=30000", data=data,
        headers={"Authorization": "Bearer " + daemon.token,
                 "Content-Type": "application/json", "X-Self-Agent-Protocol": "1"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=35) as response:
        assert response.status == 200, response.status
        body = json.load(response)
    assert body["instance_id"] == instance, body
    assert body["state"] == "stopped" and body["outcome"] == "stopped", body
    assert body["workers"]["pending_workers"] == 0, body
    assert daemon.process.wait(timeout=10) == 0


def persisted(directory, task_id):
    # Read only AFTER the actual process exits. Explicit closing matters on
    # Windows: a connection context manager alone does not release file handles.
    with closing(sqlite3.connect(Path(directory) / "harness.sqlite")) as connection:
        row = connection.execute("SELECT body FROM tasks WHERE id=?", (task_id,)).fetchone()
        return json.loads(row[0])


def main():
    urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))
    verify_fresh_binaries()
    with tempfile.TemporaryDirectory(prefix="self-agent-shutdown-http-") as temporary:
        daemon, sid = fixture(temporary)
        try:
            task_id = enqueue(daemon, sid)
            wait_for(lambda: running(daemon, task_id), description="actual demo worker")
            try:
                daemon.request("POST", "/api/v1/lifecycle/shutdown", {}, authenticate=False)
                raise AssertionError("Unauthenticated shutdown was accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 401
            snapshot = daemon.request("GET", "/api/v1/lifecycle")
            assert snapshot["state"] == "running"
            try:
                daemon.request("POST", "/api/v1/lifecycle/shutdown",
                               {"expected_instance": str(uuid.uuid4())})
                raise AssertionError("A different instance ID stopped the current daemon")
            except urllib.error.HTTPError as error:
                assert error.code == 409
            incompatible = urllib.request.Request(
                daemon.base + "/api/v1/lifecycle/shutdown",
                data=json.dumps({"expected_instance": snapshot["instance_id"]}).encode(),
                headers={"Authorization": "Bearer " + daemon.token,
                         "Content-Type": "application/json", "X-Self-Agent-Protocol": "999"},
                method="POST",
            )
            try:
                urllib.request.urlopen(incompatible, timeout=5)
                raise AssertionError("An incompatible protocol admitted shutdown")
            except urllib.error.HTTPError as error:
                assert error.code == 426
            unchanged = daemon.request("GET", "/api/v1/lifecycle")
            assert unchanged["state"] == "running" and unchanged["instance_id"] == snapshot["instance_id"]
            wait_stop(daemon)
            result = persisted(temporary, task_id)
            assert result["status"] == "cancelled" and result["stop_reason"] == "daemon_shutdown", result
            daemon.stop()  # process already exited; only closes fixture log
            daemon.start()
            after = next(t for t in daemon.snapshot()["tasks"] if t["id"] == task_id)
            assert after["status"] == "cancelled", after
            wait_stop(daemon)
            print("PASS actual authenticated wait response confirms drain before exit; restart does not replay cancelled work")
        finally:
            daemon.stop()
    with tempfile.TemporaryDirectory(prefix="self-agent-shutdown-drop-") as temporary:
        daemon, sid = fixture(temporary)
        try:
            task_id = enqueue(daemon, sid)
            wait_for(lambda: running(daemon, task_id), description="demo worker before disconnect")
            instance = daemon.request("GET", "/api/v1/lifecycle")["instance_id"]
            body = json.dumps({"expected_instance": instance})
            with socket.create_connection(("127.0.0.1", daemon.port), timeout=5) as connection:
                request = ("POST /api/v1/lifecycle/shutdown?wait_ms=30000 HTTP/1.1\r\n"
                           "Host: localhost\r\nAuthorization: Bearer " + daemon.token +
                           "\r\nContent-Type: application/json\r\nContent-Length: " + str(len(body)) + "\r\nConnection: close\r\n\r\n" + body)
                connection.sendall(request.encode())
                # Establish admission via a separate observer, never by
                # interpreting a refused connection as a stopped confirmation.
                wait_for(lambda: daemon.process.poll() is not None or
                         daemon.request("GET", "/api/v1/lifecycle")["state"] != "running",
                         description="shutdown accepted before observer disconnect")
                # Disconnect without reading the original response. The
                # process/SQLite outcome below is an independent fixture check.
                connection.shutdown(socket.SHUT_WR)
            assert daemon.process.wait(timeout=15) == 0
            assert persisted(temporary, task_id)["status"] == "cancelled"
            print("PASS actual dropped shutdown observer does not cancel supervisor or replay effects")
        finally:
            daemon.stop()
    with tempfile.TemporaryDirectory(prefix="self-agent-shutdown-cli-") as temporary:
        daemon, sid = fixture(temporary)
        try:
            task_id = enqueue(daemon, sid)
            wait_for(lambda: running(daemon, task_id), description="demo worker before real CLI shutdown")
            before = cli(temporary, "daemon-status")
            assert before["state"] == "running" and before["outcome"] is None, before
            assert before["instance_id"] == daemon.request("GET", "/api/v1/lifecycle")["instance_id"]
            # Exactly one CLI stop invocation; lost/invalid responses fail the
            # fixture rather than causing a second POST or substitute daemon.
            after = cli(temporary, "--wait", "30", "daemon-stop", "--confirm")
            assert after["instance_id"] == before["instance_id"], after
            assert after["state"] == "stopped" and after["outcome"] == "stopped", after
            assert after["phase"] == "finished" and after["workers"]["accepting_work"] is False, after
            assert all(after["workers"][key] == 0 for key in (
                "active_admissions", "pending_workers", "unknown_workers", "unknown_admissions")), after
            assert daemon.process.wait(timeout=10) == 0
            result = persisted(temporary, task_id)
            assert result["status"] == "cancelled" and result["stop_reason"] == "daemon_shutdown", result
            print("PASS real CLI status and single confirmed stop bind the original daemon and confirm cancelled work before exit")
        finally:
            daemon.stop()
    if os.name != "nt":
        with tempfile.TemporaryDirectory(prefix="self-agent-shutdown-paused-") as temporary:
            daemon, sid = fixture(temporary)
            try:
                daemon.command({"type": "send_message", "session_id": sid,
                                "content": "生成固定演示计划，不调用模型"}, REMOTE)
                plan = wait_for(lambda: next((item for item in daemon.snapshot()["plans"]
                                              if item["session_id"] == sid), None),
                                description="demo plan before shutdown")
                plan_id = plan["id"]

                def current_plan():
                    return next(item for item in daemon.snapshot()["plans"] if item["id"] == plan_id)

                daemon.command({"type": "start_plan", "plan_id": plan_id,
                                "expected_revision": plan["revision"]})
                wait_for(lambda: any(item["plan_id"] == plan_id and item["status"] == "running"
                                     for item in daemon.snapshot()["tasks"]),
                         description="running demo child before pause")
                plan = current_plan()
                daemon.command({"type": "pause_plan", "plan_id": plan_id,
                                "expected_revision": plan["revision"]})
                assert current_plan()["status"] == "paused"
                daemon.process.send_signal(signal.SIGTERM)
                assert daemon.process.wait(timeout=15) == 0
                with closing(sqlite3.connect(Path(temporary) / "harness.sqlite")) as connection:
                    stopped = json.loads(connection.execute(
                        "SELECT body FROM plans WHERE id=?", (plan_id,)).fetchone()[0])
                assert stopped["status"] == "cancelled", stopped
                assert all(node["status"] not in ("pending", "queued", "running", "paused", "waiting_input")
                           for node in stopped["nodes"]), stopped
                daemon.stop()
                daemon.start()
                restored = current_plan()
                assert restored["status"] == "cancelled"
                try:
                    daemon.command({"type": "resume_plan", "plan_id": plan_id,
                                    "expected_revision": restored["revision"]})
                    raise AssertionError("Graceful shutdown plan was resumed")
                except urllib.error.HTTPError as error:
                    assert error.code == 400
                    assert "只有已暂停的计划可以恢复" in error.read().decode("utf-8")
                assert not any(item["plan_id"] == plan_id and item["status"] in ("queued", "running", "paused", "waiting_input")
                               for item in daemon.snapshot()["tasks"])
                wait_stop(daemon)
                print("PASS graceful SIGTERM cancels paused plan; restart preserves cancellation and rejects resume")
            finally:
                daemon.stop()
        for name, signal_number in (("SIGINT", signal.SIGINT), ("SIGTERM", signal.SIGTERM)):
            with tempfile.TemporaryDirectory(prefix="self-agent-shutdown-" + name.lower() + "-") as temporary:
                daemon, sid = fixture(temporary)
                try:
                    task_id = enqueue(daemon, sid)
                    wait_for(lambda: running(daemon, task_id), description="demo worker before " + name)
                    daemon.process.send_signal(signal_number)
                    # Reap this real child daemon and inspect committed state;
                    # a signal exit code or fixture kill is not graceful proof.
                    assert daemon.process.wait(timeout=15) == 0
                    result = persisted(temporary, task_id)
                    assert result["status"] == "cancelled" and result["stop_reason"] == "daemon_shutdown", result
                    daemon.stop()  # Already exited; close the fixture log.
                    daemon.start()
                    restored = [item for item in daemon.snapshot()["tasks"] if item["id"] == task_id]
                    assert len(restored) == 1 and restored[0]["status"] == "cancelled", restored
                    assert restored[0]["stop_reason"] == "daemon_shutdown", restored
                    wait_stop(daemon)
                    print("PASS actual Unix " + name + " drains active work, exits cleanly and never replays on restart")
                finally:
                    daemon.stop()
    else:
        print("NOT RUN Windows console Ctrl-C injection; real owner HTTP drain/exit is covered")


if __name__ == "__main__":
    main()
