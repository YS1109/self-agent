#!/usr/bin/env python3
"""Actual daemon + CLI verifier/stager, ephemeral signing material only.
No downloads, private-key files, vault, model, artifact execution or install.
Integration will explicitly register scripts/update_fixture.rs as update_fixture example; then build it before this gate.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.error
import uuid

from smoke import Daemon, ROOT


def cli(data, *args, success=True):
    name = "self-agent-cli.exe" if os.name == "nt" else "self-agent-cli"
    result = subprocess.run([str(ROOT / "target/debug" / name), "--data-dir", str(data), *args],
                            cwd=ROOT, capture_output=True, text=True, encoding="utf-8", timeout=40)
    assert (result.returncode == 0) == success, (args[0], result.returncode, result.stderr)
    return json.loads(result.stdout) if success else result


def main():
    name = "update_fixture.exe" if os.name == "nt" else "update_fixture"
    helper = ROOT / "target/debug/examples" / name
    assert helper.is_file() and not helper.is_symlink(), "Build the fixed repository fixture example first"
    material = json.loads(subprocess.run([str(helper)], cwd=ROOT, check=True, capture_output=True,
                                        text=True, encoding="utf-8", timeout=10).stdout)
    assert set(material) == {"fixture_schema", "trust", "records"} and material["fixture_schema"] == 1
    with tempfile.TemporaryDirectory(prefix="self-agent-update-") as temporary:
        root = Path(temporary).resolve()
        data = root / "data"
        data.mkdir()
        trust = root / "trust.json"
        trust.write_text(json.dumps(material["trust"]), encoding="utf-8", newline="\n")
        records = []
        for index, record in enumerate(material["records"]):
            manifest, signature, artifact = [root / f"{kind}-{index}.txt" for kind in ["manifest", "signature", "artifact"]]
            manifest.write_text(record["raw_manifest"], encoding="utf-8", newline="\n")
            signature.write_text(record["signature_hex"], encoding="utf-8", newline="\n")
            artifact.write_text(record["artifact_text"], encoding="utf-8", newline="\n")
            records.append((manifest, signature, artifact))
        daemon = Daemon(data)
        try:
            daemon.start()
            status = cli(data, "updates", "status")
            assert status["mode"] == "verification_disabled" and status["automatic_update_enabled"] is False
            try:
                daemon.request("GET", "/api/v1/reliability/updates", authenticate=False)
                raise AssertionError("Update owner API accepted no bearer")
            except urllib.error.HTTPError as error:
                assert error.code == 401
            configured = cli(data, "updates", "trust", str(trust))
            assert configured["revision"] == 1 and configured["fingerprint"] == material["trust"]["confirmation_fingerprint"]
            backup = cli(data, "--wait", "30", "backup", str(root), "before-verification")
            assert backup["state"] == "completed"
            print("PASS separate authenticated owner trust and offline restorable pre-verification DB")

            first = str(uuid.uuid4())
            receipt = cli(data, "--command-id", first, "updates", "verify", "1", *map(str, records[0]))
            assert receipt["status"] == "verified"
            current = cli(data, "updates", "operation", first)
            assert current["current_stage_verified"] is True and current["installed"] is False
            # Exact original intent gives history; never uploads its bytes again.
            replay = cli(data, "--command-id", first, "updates", "verify", "1", *map(str, records[0]))
            assert replay["status"] == "verified" and replay["stage_authority"] is False
            status = cli(data, "updates", "status")
            assert status["staging"]["files"] == 1 and all(not r["stage_authority"] for r in status["receipts"])
            print("PASS raw signed manifest and actual CLI byte upload; historical replay is not stage authority")

            bad = root / "bad-artifact.txt"
            original = records[1][2].read_bytes()
            bad.write_bytes(b"!" + original[1:])
            rejected = str(uuid.uuid4())
            cli(data, "--command-id", rejected, "updates", "verify", "1", str(records[1][0]), str(records[1][1]), str(bad), success=False)
            rejected_status = cli(data, "updates", "operation", rejected)
            assert rejected_status["receipt"]["status"] == "failed" and rejected_status["current_stage_verified"] is False
            assert cli(data, "updates", "status")["staging"]["files"] == 1
            second = str(uuid.uuid4())
            assert cli(data, "--command-id", second, "updates", "verify", "1", *map(str, records[1]))["status"] == "verified"
            print("PASS tampered bytes fail and do not consume the next signed release floor")

            daemon.stop()
            daemon.start()
            assert cli(data, "updates", "operation", second)["current_stage_verified"] is True
            daemon.stop()
            cli(data, "restore", str(root / "before-verification"), "--confirm-sha256", backup["manifest"]["sha256"])
            daemon.start()
            status = cli(data, "updates", "status")
            assert status["configuration"]["enabled"] is False
            assert not status["receipts"]
            assert max(f["sequence"] for f in status["floors"]) == 2
            assert status["staging"]["files"] == 2  # Orphans never promoted/deleted automatically.
            renewed = dict(material["trust"], expected_revision=status["configuration"]["revision"])
            trust.write_text(json.dumps(renewed), encoding="utf-8", newline="\n")
            revision = cli(data, "updates", "trust", str(trust))["revision"]
            cli(data, "updates", "verify", str(revision), *map(str, records[0]), success=False)
            assert max(f["sequence"] for f in cli(data, "updates", "status")["floors"]) == 2
            print("PASS restart preserves verified bytes; old DB restore revokes authority without lowering external witness floor")
        finally:
            daemon.stop()


if __name__ == "__main__":
    main()
