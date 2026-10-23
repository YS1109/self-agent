#!/usr/bin/env python3
"""Exercise pause, revisioned patches, real command gates and repair callbacks.

By default a deterministic model drives real native tools. --real-codex replaces
the workers with the installed Codex; the planning responses remain fixtures.
"""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import urllib.error

from smoke import Daemon, DESKTOP, REMOTE, wait_for

CHECK = "python3 test_calc.py" if sys.platform != "win32" else "python test_calc.py"
REAL_CODEX = False
TEST = "from pathlib import Path\nexec(compile(Path('calc.py').read_text(), 'calc.py', 'exec'))\nassert add(2, 3) == 5\nprint('ACCEPTANCE_OK')\n"


def node(identifier, objective, dependencies=None, **fields):
    return {"id": identifier, "title": identifier, "objective": objective,
            "depends_on": dependencies or [], **fields}


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if body.get("tools"):
            message = self.worker(body)
        else:
            payload = json.loads(body["messages"][-1]["content"])
            if payload["job"]["kind"] == "message":
                output = {"action": "plan", "title": "运行控制与修复验收",
                          "message": "隔离项目验证，请启动。",
                          "nodes": [
                              node("write", "PHASE4_WRITE：读取项目规则。创建calc.py，定义add(a,b)返回a-b，这是本次测试故意植入的错误；只创建此文件，不运行测试或修复。",
                                   acceptance_command=None),
                              node("test", f"PHASE4_TEST：只执行命令 {CHECK}，报告真实失败，禁止修改文件。", ["write"],
                                   acceptance_command=CHECK),
                              node("blocked", "因前置失败而阻塞的验收节点", ["test"]),
                          ]}
            else:
                nodes = payload["context"]["plan"]["nodes"]
                if not any(n["id"] == "repair" for n in nodes):
                    output = {"action": "extend", "message": "保留失败并修复后重新验收。",
                              "nodes": [
                                  node("repair", "PHASE4_REPAIR：读取calc.py，把add(a,b)修复为返回a+b；保留test_calc.py原样。",
                                       repairs=["test", "blocked"]),
                                  node("verify", f"PHASE4_VERIFY：禁止修改文件，只执行命令 {CHECK} 并报告真实结果。", ["repair"],
                                       verifies=["test", "blocked"], acceptance_command=CHECK),
                              ]}
                else:
                    output = {"action": "complete", "message": "修复已通过实际命令复验，保留最初失败历史。"}
            message = {"role": "assistant", "content": json.dumps(output, ensure_ascii=False)}
        data = json.dumps({"choices": [{"message": message}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def worker(self, body):
        objective = next(m["content"] for m in body["messages"] if m["role"] == "user")
        outputs = [m for m in body["messages"] if m["role"] == "tool"]
        if outputs:
            return {"role": "assistant", "content": "工具执行结束，结果以记录为准。"}
        if "PHASE4_REPAIR" in objective:
            tool, arguments = "write_file", {"path": "calc.py", "content": "def add(a, b):\n    return a + b\n"}
        elif "PHASE4_VERIFY" in objective or "PHASE4_TEST" in objective:
            assert "节点验收要求：实际执行以下完整命令" in objective
            tool, arguments = "run_command", {"command": CHECK}
        else:
            # Keep the first task in flight long enough to exercise controls.
            time.sleep(1.0)
            tool, arguments = "write_file", {"path": "calc.py", "content": "def add(a, b):\n    return a - b\n"}
        return {"role": "assistant", "content": None, "tool_calls": [{
            "id": "runtime-tool", "type": "function", "function": {
                "name": tool, "arguments": json.dumps(arguments),
            }}]}


def current(daemon, plan_id):
    return next(p for p in daemon.snapshot()["plans"] if p["id"] == plan_id)


def revision_command(daemon, kind, plan_id, **fields):
    for _ in range(30):
        plan = current(daemon, plan_id)
        try:
            return daemon.command({"type": kind, "plan_id": plan_id,
                                   "expected_revision": plan["revision"], **fields})
        except urllib.error.HTTPError as error:
            detail = error.read().decode()
            if error.code != 400 or not any(word in detail for word in ("版本", "revision")):
                raise AssertionError(detail) from error
    raise AssertionError("Plan revisions did not settle")


def main():
    global REAL_CODEX
    parser = argparse.ArgumentParser()
    parser.add_argument("--real-codex", action="store_true")
    args = parser.parse_args()
    REAL_CODEX = args.real_codex
    codex = shutil.which("codex") if REAL_CODEX else None
    if REAL_CODEX and not codex:
        raise RuntimeError("Codex CLI unavailable")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="self-agent-runtime-") as directory:
        project = Path(directory) / "project"
        project.mkdir()
        (project / "test_calc.py").write_text(TEST)
        (project / "AGENTS.md").write_text(
            "仅操作当前隔离测试项目。不要联网，不调用MCP。不改test_calc.py。按任务执行。\n"
        )
        daemon = Daemon(directory, {"HARNESS_ALLOW_COMMANDS": "1"})
        try:
            daemon.start()
            daemon.request("PUT", "/api/v1/settings", {
                "execution_mode": "native",
                "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "model": "runtime-coordinator",
                "agents": [{"id": "coder", "name": "执行角色", "instructions": "",
                            "executor": "codex" if REAL_CODEX else "native", "model": ""}],
                "default_agent_id": "coder", "codex_binary": codex or "codex",
            })
            session = daemon.command({"type": "create_session", "title": "隔离运行验证",
                                      "project_path": str(project)}, REMOTE)["session_id"]
            daemon.command({"type": "send_message", "session_id": session,
                            "content": "生成运行验证计划"}, REMOTE)
            plan = wait_for(lambda: next(iter(daemon.snapshot()["plans"]), None))
            plan_id = plan["id"]
            revision_command(daemon, "start_plan", plan_id)
            wait_for(lambda: current(daemon, plan_id)["nodes"][0]["status"] == "running")
            revision_command(daemon, "pause_plan", plan_id)
            wait_for(lambda: current(daemon, plan_id)["nodes"][0]["status"] == "completed",
                     timeout=240 if REAL_CODEX else 20, description="first task drains while paused")
            plan = current(daemon, plan_id)
            assert plan["status"] == "paused", plan
            assert plan["nodes"][1]["status"] in ("pending", "queued"), plan
            edited = json.loads(json.dumps(plan["nodes"]))
            edited[1]["objective"] += " 中途补充：必须保留失败历史。"
            revision_command(daemon, "patch_plan", plan_id, nodes=edited)
            try:
                daemon.command({"type": "resume_plan", "plan_id": plan_id,
                                "expected_revision": plan["revision"]})
                raise AssertionError("Stale revision accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 400
            assert current(daemon, plan_id)["status"] == "paused"
            revision_command(daemon, "resume_plan", plan_id)
            final = wait_for(
                lambda: (p if (p := current(daemon, plan_id))["status"] in ("completed", "waiting_input") else None),
                timeout=600 if REAL_CODEX else 30, description="failure, repair and real command verification")
            assert final["status"] == "completed", json.dumps(final, ensure_ascii=False)
            by_id = {n["id"]: n for n in final["nodes"]}
            assert by_id["test"]["status"] == "failed"
            assert any(e["exit_code"] != 0 for e in by_id["test"]["evidence"])
            assert by_id["blocked"]["status"] == "blocked"
            assert by_id["test"]["resolved_by"] == by_id["blocked"]["resolved_by"] == "verify"
            assert by_id["verify"]["status"] == "completed"
            assert any(e["command"] == CHECK and e["exit_code"] == 0
                       and "ACCEPTANCE_OK" in e["output"] for e in by_id["verify"]["evidence"])
            assert "a + b" in (project / "calc.py").read_text()
            assert (project / "test_calc.py").read_text() == TEST
            expected = {t["id"] for t in daemon.snapshot()["tasks"] if t.get("plan_id") == plan_id}
            callbacks = wait_for(lambda: (items if len(items := [
                d for d in daemon.snapshot()["deliveries"] if d["task_id"] in expected and d["status"] == "sent"
            ]) == len(expected) else None))
            assert all(d["target"] == REMOTE for d in callbacks)
            print("PASS pause drains current task, pending patch, stale revision rejection and resume")
            print("PASS real command failure -> repair -> command acceptance, preserved history and original callbacks")
            # A second plan is cancelled while it still has queued dependent work.
            daemon.command({"type": "send_message", "session_id": session, "content": "取消场景"})
            second = wait_for(lambda: next((p for p in daemon.snapshot()["plans"]
                                           if p["id"] != plan_id and p["status"] == "draft"), None))
            revision_command(daemon, "start_plan", second["id"])
            daemon.command({"type": "cancel_plan", "plan_id": second["id"]})
            time.sleep(0.4)
            assert current(daemon, second["id"])["status"] == "cancelled"
            assert all(t["status"] == "cancelled" for t in daemon.snapshot()["tasks"]
                       if t.get("plan_id") == second["id"])
            print("PASS cancellation leaves no active plan tasks")
            print("Workers: real Codex CLI" if REAL_CODEX else "Workers: deterministic model with actual native file/shell tools")
        finally:
            daemon.stop()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
