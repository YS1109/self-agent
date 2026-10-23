#!/usr/bin/env python3
"""Verify role snapshots and mixed executors through the real authenticated API."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading
import urllib.error

from smoke import Daemon, DESKTOP, REMOTE, ROOT, wait_for


class ModelFixture(BaseHTTPRequestHandler):
    observations = []

    def log_message(self, *_arguments):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if request.get("tools"):
            message = self.worker(request)
        else:
            payload = json.loads(request["messages"][-1]["content"])
            assert request["model"] == "fixture-coordinator", request["model"]
            assert {a["id"] for a in payload["context"]["agents"]} == {"implementer", "verifier"}
            if payload["job"]["kind"] == "review":
                assert all(n["status"] == "completed" for n in payload["context"]["plan"]["nodes"])
                output = {"action": "complete", "message": "本地协议夹具已完成，两个执行角色的真实文件结果已验证。"}
            else:
                output = {
                    "action": "plan", "title": "混合执行器验证", "message": "请审阅角色分配。",
                    "nodes": [
                        {"id": "write", "title": "写入证据", "objective": "本地测试目标",
                         "depends_on": [], "agent_id": "implementer"},
                        {"id": "verify", "title": "读取证据", "objective": "VERIFY_ROLE",
                         "depends_on": ["write"], "agent_id": "verifier"},
                    ],
                }
            message = {"role": "assistant", "content": json.dumps(output, ensure_ascii=False)}
        encoded = json.dumps({"choices": [{"message": message}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def worker(self, request):
        objective = next(m["content"] for m in request["messages"] if m["role"] == "user")
        assert request["model"] == "fixture-role", request["model"]
        outputs = [json.loads(m["content"]) for m in request["messages"] if m["role"] == "tool"]
        verify = "VERIFY_ROLE" in objective
        if verify:
            assert "角色快照验证" in objective, objective
            assert "测试执行结果" in objective, objective
        if not outputs:
            name = "read_file" if verify else "write_file"
            arguments = {"path": "fixture-result.txt"}
            if not verify:
                arguments["content"] = "真实本地夹具写入"
            return {
                "role": "assistant", "content": None,
                "tool_calls": [{"id": "role-tool", "type": "function", "function": {
                    "name": name, "arguments": json.dumps(arguments),
                }}],
            }
        assert "error" not in outputs[-1], outputs
        if verify:
            assert "真实本地夹具写入" in json.dumps(outputs[-1], ensure_ascii=False), outputs
        self.observations.append(("verify" if verify else "write", request["model"]))
        return {"role": "assistant", "content": "已读取并验证真实文件。" if verify else "测试执行结果"}


def expect_rejected(daemon, patch):
    before = daemon.request("GET", "/api/v1/settings")
    try:
        daemon.request("PUT", "/api/v1/settings", patch)
        raise AssertionError("Invalid role configuration was accepted")
    except urllib.error.HTTPError as error:
        assert error.code == 400, error.code
    assert daemon.request("GET", "/api/v1/settings") == before


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with tempfile.TemporaryDirectory(prefix="self-agent-roles-") as directory:
        daemon = Daemon(directory)
        project = Path(directory) / "project"
        project.mkdir()
        try:
            daemon.start()
            expect_rejected(daemon, {"agents": []})
            expect_rejected(daemon, {"default_agent_id": "missing", "model": "must-not-be-saved"})
            expect_rejected(daemon, {"codex_binary": "invalid\ncommand"})
            print("PASS role settings validation is atomic")

            codex = os.name != "nt"
            agents = [
                {"id": "implementer", "name": "编码", "instructions": "",
                 "executor": "codex" if codex else "native",
                 "model": "success" if codex else "fixture-role"},
                {"id": "verifier", "name": "验证", "instructions": "角色快照验证",
                 "executor": "native", "model": "fixture-role"},
            ]
            daemon.request("PUT", "/api/v1/settings", {
                "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "model": "fixture-coordinator", "agents": agents,
                "default_agent_id": "implementer", "execution_mode": "native",
                "codex_binary": str(ROOT / "scripts" / "codex_fixture.py") if codex else "codex",
                "api_key": "fixture-secret-not-for-storage",
            })
            session_id = daemon.command({
                "type": "create_session", "title": "角色协议验证", "project_path": str(project),
            }, REMOTE)["session_id"]
            daemon.command({"type": "send_message", "session_id": session_id, "content": "执行角色验证"}, REMOTE)
            plan = wait_for(
                lambda: next((p for p in daemon.snapshot()["plans"] if p["status"] == "draft"), None),
                description="role-aware coordinator plan",
            )
            assert [n["agent_id"] for n in plan["nodes"]] == ["implementer", "verifier"]
            daemon.request("PUT", "/api/v1/settings", {
                "agents": [{"id": "changed", "name": "新角色", "instructions": "不得进入旧计划",
                            "executor": "native", "model": "wrong-model"}],
                "default_agent_id": "changed",
            })
            # A desktop start must preserve the original remote origin.
            daemon.command({"type": "start_plan", "plan_id": plan["id"],
                            "expected_revision": plan["revision"]}, DESKTOP)
            final = wait_for(
                lambda: next((p for p in daemon.snapshot()["plans"]
                              if p["id"] == plan["id"] and p["status"] == "completed"), None),
                description="mixed executor plan completion",
            )
            assert final["agents"] == agents
            assert (project / "fixture-result.txt").read_text(encoding="utf-8") == "真实本地夹具写入"
            snapshot = daemon.snapshot()
            children = [t for t in snapshot["tasks"] if t.get("parent_task_id") == final["task_id"]]
            assert {t["agent"]["id"] for t in children} == {"implementer", "verifier"}
            assert all(t["status"] == "completed" for t in children)
            expected_ids = {t["id"] for t in children} | {final["task_id"]}

            def delivered():
                deliveries = [d for d in daemon.snapshot()["deliveries"]
                              if d["task_id"] in expected_ids and d["status"] == "sent"]
                return deliveries if len(deliveries) == 3 else None

            callbacks = wait_for(delivered, description="role task and parent callbacks")
            assert all(d["target"] == REMOTE and d["session_id"] == session_id for d in callbacks)
            assert ("verify", "fixture-role") in ModelFixture.observations
            assert "fixture-secret-not-for-storage" not in json.dumps(daemon.snapshot())
            print("PASS snapshotted roles, per-role models, dependency evidence and original callbacks")
            print("PASS Codex stdio + native tools in one plan" if codex else
                  "SKIP Codex executable fixture on Windows; native role pipeline passed")
            daemon.stop()
            daemon.start()
            saved = daemon.request("GET", "/api/v1/settings")
            assert saved["default_agent_id"] == "changed"
            assert next(p for p in daemon.snapshot()["plans"] if p["id"] == plan["id"])["agents"] == agents
            assert not saved["api_key_configured"]
            print("PASS role persistence, historical plan snapshots and credential exclusion")
        finally:
            daemon.stop()
            server.shutdown()
            server.server_close()
    print("All agent role scenarios passed.")


if __name__ == "__main__":
    main()
