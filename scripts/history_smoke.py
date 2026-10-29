#!/usr/bin/env python3
"""Real owner HTTP/CLI bounded history; synthetic stopped-db history only."""
from contextlib import closing
import copy
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import urllib.error
import urllib.request
from smoke import Daemon, ROOT, wait_for


def main():
    executable = ROOT / "target/debug" / ("self-agent-cli.exe" if os.name == "nt" else "self-agent-cli")
    with tempfile.TemporaryDirectory(prefix="self-agent-history-") as temporary:
        root = Path(temporary).resolve()
        data, project = root / "data", root / "project"
        data.mkdir(); project.mkdir()
        daemon = Daemon(data, {"HARNESS_DEMO_STEP_MS": "10"})
        wire = {}
        def get(path, limit=12 * 1024 * 1024):
            request = urllib.request.Request(daemon.base + path, headers={"Authorization": "Bearer " + daemon.token})
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read(limit + 1)
            assert len(raw) <= limit, (path, len(raw), limit)
            return json.loads(raw)
        def status(path, expected):
            try:
                get(path)
                raise AssertionError((path, "unexpected success"))
            except urllib.error.HTTPError as error:
                assert error.code == expected, (path, error.code, expected)
                return json.load(error)
        def cli(*args, ok=True):
            result = subprocess.run([str(executable), "--data-dir", str(data), *args], capture_output=True, text=True, encoding="utf-8", timeout=30)
            assert (result.returncode == 0) == ok, (args, result.stderr)
            return json.loads(result.stdout) if result.stdout and ok else result
        try:
            daemon.start()
            daemon.request("PUT", "/api/v1/settings", {"execution_mode": "demo"})
            session_id = daemon.command({"type": "create_session", "title": "bounded history", "project_path": str(project)})["session_id"]
            task_id = daemon.command({"type": "run_agent", "session_id": session_id, "content": "fixture"})["task_id"]
            wait_for(lambda: get(f"/api/v1/tasks/{task_id}", 8 * 1024 * 1024)["task"]["status"] == "completed", description="demo fixture completed")
            daemon.stop()
            reply = "history-fixture:" + "x" * (64 * 1024)
            with closing(sqlite3.connect(data / "harness.sqlite")) as db:
                with db:
                    original = json.loads(db.execute("SELECT body FROM tasks WHERE id=?", (task_id,)).fetchone()[0])
                    session = json.loads(db.execute("SELECT body FROM sessions WHERE id=?", (session_id,)).fetchone()[0])
                    expected = {task_id}
                    for n in range(180):
                        task = copy.deepcopy(original)
                        task.update(id=f"history-task-{n:04}", result=reply, stream_output=reply, main_sequence=n + 2)
                        db.execute("INSERT INTO tasks(id,body) VALUES(?,?)", (task["id"], json.dumps(task)))
                        expected.add(task["id"])
                        session["messages"].extend([
                            {"id": f"history-user-{n}", "role": "user", "content": f"history goal {n}", "created_at": original["created_at"], "task_id": task["id"]},
                            {"id": f"history-reply-{n}", "role": "assistant", "content": reply, "created_at": original["created_at"], "task_id": task["id"]},
                        ])
                    oversized = copy.deepcopy(original)
                    oversized.update(id="history-oversize", result="z" * (9 * 1024 * 1024), stream_output="", main_sequence=182)
                    db.execute("INSERT INTO tasks(id,body) VALUES(?,?)", (oversized["id"], json.dumps(oversized)))
                    expected.add(oversized["id"])
                    db.execute("UPDATE sessions SET body=? WHERE id=?", (json.dumps(session), session_id))
                    message_ids = {message["id"] for message in session["messages"]}
            daemon.start()
            assert status("/api/v1/snapshot", 413)["code"] == "record_too_large"
            snapshot = get("/api/v1/snapshot?window=1")
            wire["snapshot"] = snapshot
            assert snapshot["history"]["collections"]["tasks"]["total"] == len(expected)
            assert snapshot["history"]["collections"]["tasks"]["has_more"]
            assert len(snapshot["tasks"]) < len(expected)
            assert all(task["result"] == reply for task in snapshot["tasks"])
            assert cli("status")["history"]["version"] == 1
            single = cli("status", "history-task-0000")
            assert single["result"] == reply and single["stream_output"] == reply
            assert single["id"] not in {task["id"] for task in snapshot["tasks"]}
            follow = subprocess.run([str(executable), "--data-dir", str(data), "--wait", "5", "follow", "history-task-0000"], capture_output=True, text=True, encoding="utf-8", timeout=10)
            assert follow.returncode == 0, follow.stderr
            assert json.loads(follow.stdout.splitlines()[0])["task"]["result"] == reply
            assert cli("sessions")["history"]["session_messages"][session_id]["has_more_before"]
            print("PASS explicit window, legacy fail-closed and real CLI single task/follow beyond snapshot window")
            first = get("/api/v1/history/tasks", 2 * 1024 * 1024)
            wire["tasks"] = first
            cursor, seen, unavailable = None, set(), set()
            while True:
                page = get("/api/v1/history/tasks" + (f"?cursor={cursor}" if cursor else ""), 2 * 1024 * 1024)
                assert page["total"] == len(expected) and page["returned"] == len(page["items"])
                assert page["anchor"] == snapshot["history"]["anchor"]
                for task in page["items"]:
                    assert task["id"] not in seen
                    seen.add(task["id"])
                    if task["id"].startswith("history-task-"):
                        assert task["result"] == reply and task["stream_output"] == reply
                unavailable.update(item["id"] for item in page["unavailable"])
                cursor = page["next_cursor"]
                if cursor is None: break
            assert seen | unavailable == expected and unavailable == {"history-oversize"}
            assert status("/api/v1/tasks/history-oversize", 413)["code"] == "record_too_large"
            assert status("/api/v1/tasks/missing", 404)["code"] == "not_found"
            cli("status", "history-oversize", ok=False)
            assert cli("history", "tasks")["total"] == len(expected)
            wire["detail"] = get("/api/v1/tasks/history-task-0000", 8 * 1024 * 1024)
            wire["sessions"] = get("/api/v1/history/sessions", 2 * 1024 * 1024)
            cursor, messages = None, set()
            while True:
                page = get(f"/api/v1/history/sessions/{session_id}/messages" + (f"?cursor={cursor}" if cursor else ""), 2 * 1024 * 1024)
                if cursor is None: wire["messages"] = page
                assert page["total"] == len(message_ids) and not page["unavailable"]
                for message in page["items"]:
                    assert message["id"] not in messages
                    messages.add(message["id"])
                    if message["id"].startswith("history-reply-"): assert message["content"] == reply
                cursor = page["next_cursor"]
                if cursor is None: break
            assert messages == message_ids
            print("PASS every historical task/message paged once, original text preserved, oversize explicitly unavailable")
            cursor = first["next_cursor"]
            assert cursor
            daemon.stop(); daemon.start()
            assert status(f"/api/v1/history/tasks?cursor={cursor}", 409)["code"] == "history_reset"
            assert get("/api/v1/snapshot?window=1")["history"]["collections"]["tasks"]["total"] == len(expected)
            assert status("/api/v1/snapshot?window=1&window=1", 400)["code"] == "invalid_history_query"
            print("PASS restart invalidates old history cursor without replay and rejects unsupported query variants")
            if os.environ.get("HARNESS_HISTORY_WIRE_FIXTURE"):
                target = Path(os.environ["HARNESS_HISTORY_WIRE_FIXTURE"])
                with target.open("x", encoding="utf-8") as stream: json.dump(wire, stream, ensure_ascii=False)
        finally:
            daemon.stop()


if __name__ == "__main__":
    main()
