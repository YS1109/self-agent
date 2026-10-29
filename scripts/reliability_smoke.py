#!/usr/bin/env python3
"""Real local daemon/CLI maintenance; no model, QQ, vault or update service calls."""
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import uuid

from smoke import Daemon, ROOT, wait_for

PRIVATE = "fixture-private-maintenance-canary"


def cli(directory, *args, success=True):
    exe = ROOT / "target" / "debug" / ("self-agent-cli.exe" if os.name == "nt" else "self-agent-cli")
    result = subprocess.run([str(exe), "--data-dir", str(directory), *args], cwd=ROOT,
                            capture_output=True, text=True, encoding="utf-8", timeout=45)
    assert (result.returncode == 0) == success, (args[0], result.returncode, result.stderr)
    return json.loads(result.stdout) if success else result


def main():
    with tempfile.TemporaryDirectory(prefix="sa-maintenance-") as temporary:
        root = Path(temporary).resolve()
        data, project, restored = root / "data", root / PRIVATE, root / "restored"
        data.mkdir()
        project.mkdir()
        daemon = Daemon(data, {"HARNESS_DEMO_STEP_MS": "60000"})
        second = None
        try:
            daemon.start()
            daemon.request("PUT", "/api/v1/settings", {"execution_mode": "demo"})
            session = daemon.command({"type": "create_session", "title": PRIVATE, "project_path": str(project)})["session_id"]
            task = daemon.command({"type": "run_agent", "session_id": session, "content": PRIVATE})["task_id"]
            wait_for(lambda: next((t for t in daemon.snapshot()["tasks"] if t["id"] == task and t["status"] == "running"), None), description="demo task running before backup")
            config = daemon.request("POST", "/api/v1/qq/commands", {"id": str(uuid.uuid4()), "command": {
                "type": "configure", "config_id": None, "expected_revision": None,
                "app_id": "fixture_maintenance", "enabled": False,
                "reply_policy": {"window_ms": 300000, "max_user": 4, "max_group": 5}}})
            assert config["status"] == "completed"
            operation = str(uuid.uuid4())
            receipt = cli(data, "--command-id", operation, "--wait", "30", "backup", str(root), "snapshot")
            assert receipt["state"] == "completed"
            manifest = receipt["manifest"]
            assert manifest == cli(data, "backup-inspect", str(root / "snapshot"))
            assert receipt == cli(data, "backup-status", operation)
            assert receipt == cli(data, "--command-id", operation, "backup", str(root), "snapshot")
            assert str(root) not in json.dumps(receipt)
            diagnostics = cli(data, "diagnostics")
            assert PRIVATE not in json.dumps(diagnostics) and str(root) not in json.dumps(diagnostics)
            assert diagnostics["database_schema"] == 4 and diagnostics["vault"] == "not_checked"
            assert diagnostics["connection_check_history"] == "not_recorded"
            print("PASS real online consistent backup, exact receipt replay, offline inspection and allowlisted diagnostics")

            # No implicit stop and no replacement of a live daemon's data.
            cli(data, "restore", str(root / "snapshot"), "--confirm-sha256", manifest["sha256"], success=False)
            assert daemon.process.poll() is None
            assert any(t["id"] == task for t in daemon.snapshot()["tasks"])
            cli(restored, "restore", str(root / "snapshot"), success=False)
            cli(restored, "restore", str(root / "snapshot"), "--confirm-sha256", "0" * 64, success=False)
            assert not (restored / "harness.sqlite").exists()
            print("PASS live lease and explicit digest confirmation reject unsafe restore")

            cli(restored, "restore", str(root / "snapshot"), "--confirm-sha256", manifest["sha256"])
            second = Daemon(restored)
            second.start()
            snap = second.snapshot()
            old = next(t for t in snap["tasks"] if t["id"] == task)
            assert old["status"] == "failed" and old["stop_reason"] == "backup_restored"
            assert not any(t["status"] in {"queued", "running", "waiting_input"} for t in snap["tasks"])
            assert second.request("GET", "/api/v1/qq")["configuration"]["enabled"] is False
            restored_receipts = cli(restored, "backups")["backup_jobs"]
            assert all(r["state"] not in {"pending", "running"} for r in restored_receipts)
            assert not (project / "fixture-output").exists()
            second.stop()
            second = None
            print("PASS actual daemon restart after restore fails old active work and revokes adapter authority without replay")

            # The checksum is damage detection, not authentication. Tampered
            # bytes and newer schemas must both fail before replacing data.
            tampered = root / "tampered"
            shutil.copytree(root / "snapshot", tampered)
            payload = tampered / "database.sqlite"
            with payload.open("r+b") as stream:
                stream.seek(120)
                byte = stream.read(1)
                stream.seek(120)
                stream.write(bytes([byte[0] ^ 1]))
            original = (restored / "harness.sqlite").read_bytes()
            cli(restored, "restore", str(tampered), "--confirm-sha256", manifest["sha256"], success=False)
            assert (restored / "harness.sqlite").read_bytes() == original
            future = root / "future"
            future.mkdir()
            with closing(sqlite3.connect(future / "harness.sqlite")) as db:
                with db:
                    db.executescript("PRAGMA user_version=999; CREATE TABLE preserved(value); INSERT INTO preserved VALUES('untouched');")
            before = hashlib.sha256((future / "harness.sqlite").read_bytes()).hexdigest()
            cli(future, "restore", str(root / "snapshot"), "--confirm-sha256", manifest["sha256"], success=False)
            assert hashlib.sha256((future / "harness.sqlite").read_bytes()).hexdigest() == before
            print("PASS damaged bundle and unsupported newer original database remain unchanged")
        finally:
            if second:
                second.stop()
            daemon.stop()


if __name__ == "__main__":
    main()
