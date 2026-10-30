#!/usr/bin/env python3
"""Verify persistent suggestions, graph layout and cross-entry plan control.

Default mode uses deterministic model responses and real native tools.
--live requires explicit HARNESS_BASE_URL / HARNESS_MODEL and uses the actual
coordinator model and installed Codex. It is never enabled by default CI.
"""

import argparse
import io
import json
import os
from pathlib import Path
import shutil
import tempfile
import threading
import time
import urllib.error
from http.server import ThreadingHTTPServer

from runtime_smoke import Model, CHECK, TEST
from smoke import Daemon, DESKTOP, REMOTE, wait_for

FIELDS = ("id", "title", "objective", "depends_on", "agent_id",
          "repairs", "verifies", "acceptance_command")
CHANGE = (
    "请只修改现有尚未开始的 test 节点：在任务描述末尾追加"
    "「补充要求：保留原始测试，报告实际退出码」。保留所有节点ID、依赖、"
    "执行角色和验收命令以及已完成节点，不创建新计划。"
)
GOAL = (
    "这是隔离项目中的失败修复验收实验。请制定恰好三个节点的草稿，"
    "使用指定ID：write、test、blocked。write 无依赖，只创建 calc.py，"
    "定义 add(a,b) 返回 a-b（实验故意植入的错误，不得提前改成加法），"
    "不得运行测试或修复。test 依赖 write，只运行完整命令 "
    f"{CHECK}，acceptance_command 也使用该命令，不得修改文件。"
    "blocked 依赖 test，代表等待前置成功的验收。三节点使用 coder 角色。"
    "执行失败后再由主智能体回顾，追加修复与验收节点，分别用 repairs 和"
    "verifies 引用所有未解决的 failed/blocked 节点；验收必须依赖修复，"
    f"并实际运行 {CHECK}。始终保留 test_calc.py 和历史失败记录。"
)


def inputs(nodes):
    return [{key: item[key] for key in FIELDS if key in item} for item in nodes]


class ChangeModel(Model):
    entered = threading.Event()
    release = threading.Event()
    hold = False

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw)
        payload = None if body.get("tools") else json.loads(body["messages"][-1]["content"])
        if not payload or payload["job"]["kind"] != "change":
            self.rfile = io.BytesIO(raw)
            return super().do_POST()
        self.entered.set()
        if self.hold and not self.release.wait(12):
            raise AssertionError("Suggestion fixture was not released")
        nodes = inputs(payload["context"]["plan"]["nodes"])
        target = next(item for item in nodes if item["id"] == "test")
        target["objective"] += " 补充要求：保留原始测试，报告实际退出码。"
        output = {"action": "change", "message": "仅补充后续测试要求，请确认后应用。",
                  "nodes": nodes}
        encoded = json.dumps({"choices": [{"message": {
            "role": "assistant", "content": json.dumps(output, ensure_ascii=False),
        }, "finish_reason": "stop"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def plan_for(daemon, plan_id):
    return next(item for item in daemon.snapshot()["plans"] if item["id"] == plan_id)


def change_for(daemon, change_id):
    return next(item for item in daemon.snapshot()["plan_changes"] if item["id"] == change_id)


def settled_change(daemon, change_id, timeout):
    def settled():
        change = change_for(daemon, change_id)
        return change if change["status"] in ("ready", "failed", "discarded") else None
    change = wait_for(settled, timeout=timeout, description="plan change generation")
    assert change["status"] == "ready", change.get("error", change["status"])
    return change


def control(daemon, kind, plan_id, **fields):
    for _ in range(30):
        plan = plan_for(daemon, plan_id)
        try:
            return daemon.command({"type": kind, "plan_id": plan_id,
                                   "expected_revision": plan["revision"], **fields})
        except urllib.error.HTTPError as error:
            detail = error.read().decode()
            if error.code != 400 or not any(word in detail for word in ("版本", "revision")):
                raise AssertionError(detail) from error
    raise AssertionError("Plan revisions did not settle")


def rejected(daemon, command):
    try:
        daemon.command(command)
    except urllib.error.HTTPError as error:
        assert error.code == 400, error.code
        return
    raise AssertionError("Invalid/stale command was accepted")


def run(live=False, evidence_path=None):
    server = None
    provider_env = {}
    if live:
        for key in ("HARNESS_BASE_URL", "HARNESS_MODEL"):
            if not os.environ.get(key):
                raise RuntimeError(f"--live requires explicit {key}")
        if not shutil.which("codex"):
            raise RuntimeError("--live requires installed and configured Codex")
        provider_env = {key: os.environ[key] for key in
                        ("HARNESS_BASE_URL", "HARNESS_MODEL", "HARNESS_API_KEY")
                        if os.environ.get(key)}
    else:
        server = ThreadingHTTPServer(("127.0.0.1", 0), ChangeModel)
        threading.Thread(target=server.serve_forever, daemon=True).start()
    timeout = 180 if live else 20
    with tempfile.TemporaryDirectory(prefix="self-agent-plan-change-") as directory:
        project = Path(directory) / "project"
        project.mkdir()
        (project / "test_calc.py").write_text(TEST, encoding="utf-8")
        (project / "AGENTS.md").write_text(
            "仅操作当前隔离项目，不联网不调用MCP，不修改test_calc.py。"
            "按当前节点目标操作；实验故意植入的减法只能在明确修复节点修改。\n",
            encoding="utf-8",
        )
        daemon = Daemon(directory, {**provider_env, "HARNESS_ALLOW_COMMANDS": "1"})
        try:
            daemon.start()
            settings = {
                "execution_mode": "native",
                "base_url": provider_env.get("HARNESS_BASE_URL") if live else f"http://127.0.0.1:{server.server_port}/v1",
                "model": provider_env.get("HARNESS_MODEL") if live else "plan-change-fixture",
                "agents": [{"id": "coder", "name": "执行角色", "instructions": "",
                            "executor": "codex" if live else "native", "model": ""}],
                "default_agent_id": "coder", "codex_binary": shutil.which("codex") or "codex",
            }
            daemon.request("PUT", "/api/v1/settings", settings)
            session_id = daemon.command({"type": "create_session", "title": "计划补充验收",
                                         "project_path": str(project)}, REMOTE)["session_id"]
            daemon.command({"type": "send_message", "session_id": session_id,
                            "content": GOAL if live else "生成运行验证计划"}, REMOTE)
            plan = wait_for(lambda: next(iter(daemon.snapshot()["plans"]), None),
                            timeout=timeout, description="initial coordinator plan")
            plan_id = plan["id"]
            assert {n["id"] for n in plan["nodes"]} == {"write", "test", "blocked"}
            positions = {"write": {"x": 70, "y": 40}, "test": {"x": 370, "y": 220}}
            daemon.command({"type": "set_plan_layout", "plan_id": plan_id,
                            "expected_layout_revision": 0, "layout": positions})
            assert plan_for(daemon, plan_id)["revision"] == plan["revision"]
            rejected(daemon, {"type": "set_plan_layout", "plan_id": plan_id,
                              "expected_layout_revision": 0, "layout": positions})
            print("PASS layout uses independent revision and rejects stale writes")

            control(daemon, "start_plan", plan_id)
            wait_for(lambda: plan_for(daemon, plan_id)["nodes"][0]["status"] == "running",
                     timeout=timeout, description="first worker starts")
            control(daemon, "pause_plan", plan_id)
            wait_for(lambda: plan_for(daemon, plan_id)["nodes"][0]["status"] == "completed",
                     timeout=300 if live else 20, description="first worker drains while paused")
            plan = plan_for(daemon, plan_id)
            assert plan["status"] == "paused"
            assert all(n["status"] in ("pending", "queued") for n in plan["nodes"][1:])
            # Move both entry selections elsewhere, then explicitly target the original plan.
            other_id = daemon.command({"type": "create_session", "title": "另一会话",
                                       "project_path": str(project)}, DESKTOP)["session_id"]
            daemon.command({"type": "select_session", "session_id": other_id}, REMOTE)
            if not live:
                ChangeModel.hold = True
                ChangeModel.entered.clear()
                ChangeModel.release.clear()
            envelope = daemon.envelope({"type": "request_plan_change", "plan_id": plan_id,
                                        "expected_revision": plan["revision"], "content": CHANGE}, DESKTOP)
            before = time.monotonic()
            receipt = daemon.request("POST", "/api/v1/commands", envelope)
            assert time.monotonic() - before < 3, "Model request blocked input acknowledgment"
            assert daemon.request("POST", "/api/v1/commands", envelope) == receipt
            assert receipt["session_id"] == session_id
            assert receipt["plan_id"] == plan_id
            change_id = receipt["change_id"]
            if not live:
                assert ChangeModel.entered.wait(5)
            # An in-flight model never blocks structured session controls or snapshot queries.
            assert daemon.snapshot()["sessions"]
            daemon.command({"type": "select_session", "session_id": session_id}, DESKTOP)
            assert plan_for(daemon, plan_id)["revision"] == plan["revision"]
            if not live:
                ChangeModel.release.set()
                ChangeModel.hold = False
            change = settled_change(daemon, change_id, timeout)
            assert change["source"] == DESKTOP
            assert plan_for(daemon, plan_id)["revision"] == plan["revision"]
            assert "补充要求" in next(n["objective"] for n in change["nodes"] if n["id"] == "test")
            daemon.command({"type": "apply_plan_change", "change_id": change_id,
                            "expected_revision": plan["revision"]}, DESKTOP)
            assert change_for(daemon, change_id)["status"] == "applied"
            updated = plan_for(daemon, plan_id)
            assert updated["source"] == REMOTE and updated["status"] == "paused"
            assert updated["nodes"][0] == plan["nodes"][0]
            assert updated["layout"] == positions
            print("PASS asynchronous idempotent suggestions target explicit plan and require confirmation")

            # Default CI additionally exercises a stale suggestion, discard and restart.
            if not live:
                stale_id = control(daemon, "request_plan_change", plan_id, content=CHANGE)["change_id"]
                settled_change(daemon, stale_id, timeout)
                control(daemon, "patch_plan", plan_id, nodes=inputs(updated["nodes"]))
                current = plan_for(daemon, plan_id)
                rejected(daemon, {"type": "apply_plan_change", "change_id": stale_id,
                                  "expected_revision": current["revision"]})
                assert change_for(daemon, stale_id)["status"] == "ready"
                assert plan_for(daemon, plan_id) == current
                daemon.command({"type": "discard_plan_change", "change_id": stale_id})
                daemon.crash()  # Persistence recovery, not global shutdown cancellation.
                daemon.start()
                assert plan_for(daemon, plan_id)["status"] == "paused"
                assert plan_for(daemon, plan_id)["layout"] == positions
                assert change_for(daemon, change_id)["status"] == "applied"
                assert change_for(daemon, stale_id)["status"] == "discarded"
                print("PASS stale suggestion is atomic, discarded explicitly, layout survives restart")

            control(daemon, "resume_plan", plan_id)
            def terminal():
                plan = plan_for(daemon, plan_id)
                return plan if plan["status"] in ("completed", "failed", "waiting_input") else None
            final = wait_for(terminal, timeout=900 if live else 30,
                             description="failure -> repair -> real command acceptance")
            assert final["status"] == "completed", final.get("error") or final.get("summary")
            by_id = {n["id"]: n for n in final["nodes"]}
            assert by_id["test"]["status"] == "failed"
            assert by_id["blocked"]["status"] == "blocked"
            for failed_id in ("test", "blocked"):
                target = by_id[failed_id]
                verifier = by_id[target["resolved_by"]]
                assert verifier["status"] == "completed"
                assert any(e["command"] == CHECK and e["exit_code"] == 0 for e in verifier["evidence"])
            assert any(e["exit_code"] != 0 for e in by_id["test"]["evidence"])
            assert (project / "test_calc.py").read_text(encoding="utf-8") == TEST
            assert "a + b" in (project / "calc.py").read_text(encoding="utf-8")
            task_ids = {t["id"] for t in daemon.snapshot()["tasks"] if t.get("plan_id") == plan_id}
            def delivered():
                rows = [d for d in daemon.snapshot()["deliveries"]
                        if d["task_id"] in task_ids and d["status"] == "sent"]
                return rows if len(rows) == len(task_ids) else None
            deliveries = wait_for(delivered, timeout=20, description="original callbacks")
            assert all(d["target"] == REMOTE and d["session_id"] == session_id for d in deliveries)
            print("PASS original failures retained, actual command repair accepted, callbacks keep origin")
            if evidence_path:
                evidence = {"mode": "real-coordinator-and-codex" if live else "deterministic-model-native-tools",
                            "status": final["status"], "change_status": "applied",
                            "nodes": [{"id": n["id"], "status": n["status"], "resolved_by": n.get("resolved_by"),
                                       "evidence": [{"command": e["command"], "exit_code": e["exit_code"]}
                                                    for e in n.get("evidence", [])]} for n in final["nodes"]],
                            "callback_count": len(deliveries), "callbacks_kept_origin": True}
                Path(evidence_path).write_text(json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8")
        finally:
            ChangeModel.release.set()
            daemon.stop()
            if server:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="Use real configured coordinator and Codex; consumes model quota")
    parser.add_argument("--evidence", help="Save a credential-free verification summary")
    args = parser.parse_args()
    run(args.live, args.evidence)
