#!/usr/bin/env python3
"""Exercise persisted orchestration through HTTP and actual workspace tools."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading
import time
import urllib.error

from smoke import Daemon, DESKTOP, REMOTE, wait_for


class ModelFixture(BaseHTTPRequestHandler):
    review_started = threading.Event()
    release_review = threading.Event()
    worker_started = threading.Event()
    release_worker = threading.Event()
    observations = []

    def log_message(self, *_arguments):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        try:
            if request.get("tools"):
                message = self.worker(request)
            else:
                message = self.coordinator(request)
            response = json.dumps({"choices": [{"message": message}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)
        except (BrokenPipeError, ConnectionResetError):
            pass

    @staticmethod
    def final(content):
        return {"role": "assistant", "content": content}

    @staticmethod
    def call(name, arguments, number=1):
        return {
            "role": "assistant",
            "content": None,
            "tool_calls": [{
                "id": f"fixture-{number}",
                "type": "function",
                "function": {"name": name, "arguments": json.dumps(arguments)},
            }],
        }

    def worker(self, request):
        objective = next(m["content"] for m in request["messages"] if m["role"] == "user")
        outputs = [json.loads(m["content"]) for m in request["messages"] if m["role"] == "tool"]
        if "SLOW_WORKER" in objective:
            ModelFixture.worker_started.set()
            ModelFixture.release_worker.wait(timeout=12)
            return self.final("后台等待已结束。")
        if "FAIL_WORKER" in objective:
            return self.final("")  # A real provider protocol failure, not a claimed success.
        if "FLOW_WRITE" in objective:
            if not outputs:
                return self.call("write_file", {"path": "seed.txt", "content": "真实依赖结果\n"})
            assert "error" not in outputs[-1], outputs
            return self.final("FLOW_SEED_RESULT：已写入 seed.txt。")
        if "FLOW_READ" in objective:
            assert "FLOW_SEED_RESULT" in objective, objective
            if not outputs:
                return self.call("read_file", {"path": "seed.txt"})
            if len(outputs) == 1:
                assert "真实依赖结果" in json.dumps(outputs[-1], ensure_ascii=False), outputs
                return self.call("write_file", {"path": "report.txt", "content": "依赖已读取并验证\n"}, 2)
            assert "error" not in outputs[-1], outputs
            return self.final("FLOW_REPORT_RESULT：已读取依赖并写入 report.txt。")
        if "FLOW_VERIFY" in objective:
            assert "FLOW_REPORT_RESULT" in objective, objective
            if not outputs:
                return self.call("read_file", {"path": "report.txt"})
            assert "依赖已读取并验证" in json.dumps(outputs[-1], ensure_ascii=False), outputs
            return self.final("FLOW_VERIFIED：已通过文件工具验证报告内容。")
        return self.final("本地测试子任务已完成。")

    def coordinator(self, request):
        content = "\n".join(
            m["content"] for m in request["messages"]
            if m["role"] == "user" and isinstance(m.get("content"), str)
        )
        payload = json.loads(content)
        context = payload.get("context", payload)
        job = payload.get("job", context.get("job"))
        plan = context.get("plan")
        if job["kind"] == "review":
            marker = plan["title"]
            ModelFixture.observations.append(("review", marker, plan["round"]))
            if marker == "CANCEL_REVIEW":
                ModelFixture.review_started.set()
                ModelFixture.release_review.wait(timeout=12)
                result = {"action": "complete", "message": "不应提交的迟到回顾。"}
            elif marker == "FAIL_CASE":
                result = {"action": "complete", "message": "错误地声称全部成功。"}
            elif marker == "BUDGET_CASE":
                result = {
                    "action": "extend", "message": "验证轮次预算。",
                    "nodes": [self.node(f"extra-{plan['round']}", "继续一轮", "BUDGET_WORKER")],
                }
            elif len(plan["nodes"]) == 2:
                assert all(n["status"] == "completed" for n in plan["nodes"]), plan
                result = {
                    "action": "extend", "message": "补充读取验证步骤。",
                    "nodes": [self.node("verify", "验证报告", "FLOW_VERIFY", ["consume"])],
                }
            else:
                assert all(n["status"] == "completed" for n in plan["nodes"]), plan
                result = {"action": "complete", "message": "已完成三项子任务，文件内容已验证。"}
        else:
            objective = job["content"]
            if "PING" in objective:
                result = {"action": "reply", "message": f"主智能体仍可响应：{objective}"}
            elif "FLOW" in objective:
                result = {
                    "action": "plan", "message": "已生成两步草稿，请审阅后启动。",
                    "title": "FLOW",
                    "nodes": [
                        self.node("seed", "写入源文件", "FLOW_WRITE"),
                        self.node("consume", "读取依赖生成报告", "FLOW_READ", ["seed"]),
                    ],
                }
            elif "FAIL_CASE" in objective:
                result = {
                    "action": "plan", "message": "验证失败后的人工介入。",
                    "title": "FAIL_CASE",
                    "nodes": [
                        self.node("fail", "触发模型协议失败", "FAIL_WORKER"),
                        self.node("blocked", "依赖失败后阻塞", "SHOULD_NOT_RUN", ["fail"]),
                    ],
                }
            else:
                marker = next(
                    name for name in ("CANCEL_REVIEW", "BUDGET_CASE", "NONBLOCK_CASE")
                    if name in objective
                )
                result = {
                    "action": "plan", "message": "已生成测试草稿。", "title": marker,
                    "nodes": [self.node(
                        "initial", "执行测试任务",
                        "SLOW_WORKER" if marker == "NONBLOCK_CASE" else "SIMPLE_WORKER",
                    )],
                }
        return self.final(json.dumps(result, ensure_ascii=False))

    @staticmethod
    def node(node_id, title, objective, dependencies=None):
        return {
            "id": node_id, "title": title, "objective": objective,
            "depends_on": dependencies or [],
        }


def find_plan(daemon, plan_id):
    return next(p for p in daemon.snapshot()["plans"] if p["id"] == plan_id)


def draft(daemon, session_id, objective, source=DESKTOP):
    envelope = daemon.envelope(
        {"type": "send_message", "session_id": session_id, "content": objective}, source
    )
    started = time.monotonic()
    receipt = daemon.request("POST", "/api/v1/commands", envelope)
    assert time.monotonic() - started < 1, "Message receipt waited for the model"
    assert receipt == daemon.request("POST", "/api/v1/commands", envelope)
    assert receipt.get("job_id"), receipt
    return wait_for(
        lambda: next(
            (p for p in daemon.snapshot()["plans"]
             if p["session_id"] == session_id and p["title"] == objective and p["status"] == "draft"),
            None,
        ),
        description=f"{objective} model-generated draft",
    )


def rejected(daemon, command):
    try:
        daemon.command(command)
    except urllib.error.HTTPError as error:
        assert error.code in {400, 409}, error.code
        return json.load(error)
    raise AssertionError(f"Invalid command accepted: {command}")


def start(daemon, plan):
    return daemon.command({
        "type": "start_plan", "plan_id": plan["id"], "expected_revision": plan["revision"],
    })


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-orchestration-") as temporary:
            project = Path(temporary) / "project"
            project.mkdir()
            daemon = Daemon(temporary)
            try:
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                    "model": "orchestration-fixture", "api_key": "fixture-secret-no-leak",
                    "execution_mode": "native",
                })
                session = daemon.command({
                    "type": "create_session", "title": "主 Agent 编排测试", "project_path": str(project),
                })["session_id"]
                plan = draft(daemon, session, "FLOW", REMOTE)
                editable = [
                    {key: node[key] for key in ("id", "title", "objective", "depends_on")}
                    for node in plan["nodes"]
                ]
                cyclic = json.loads(json.dumps(editable))
                cyclic[0]["depends_on"] = ["consume"]
                rejected(daemon, {
                    "type": "update_plan", "plan_id": plan["id"],
                    "expected_revision": plan["revision"], "nodes": cyclic,
                })
                editable[0]["title"] = "人工修改后的源文件任务"
                patch = daemon.envelope({
                    "type": "update_plan", "plan_id": plan["id"],
                    "expected_revision": plan["revision"], "nodes": editable,
                })
                saved = daemon.request("POST", "/api/v1/commands", patch)
                assert saved == daemon.request("POST", "/api/v1/commands", patch)
                rejected(daemon, {
                    "type": "update_plan", "plan_id": plan["id"],
                    "expected_revision": plan["revision"], "nodes": editable,
                })
                plan = find_plan(daemon, plan["id"])
                assert plan["nodes"][0]["title"] == editable[0]["title"]
                assert plan["revision"] >= 2
                print("PASS asynchronous model planning, idempotency, draft edits, revision conflicts and DAG validation")
                start(daemon, plan)
                other = daemon.command({
                    "type": "create_session", "title": "切换后的远程会话", "project_path": str(project),
                })["session_id"]
                daemon.command({"type": "select_session", "session_id": other}, REMOTE)
                completed = wait_for(
                    lambda: (p if (p := find_plan(daemon, plan["id"]))["status"] in {"completed", "failed", "waiting_input"} else None),
                    timeout=25, description="model review and continuation",
                )
                assert completed["status"] == "completed", completed
                assert len(completed["nodes"]) == 3, completed
                assert (project / "report.txt").read_text() == "依赖已读取并验证\n"
                parent_id = completed["task_id"]
                callback = wait_for(
                    lambda: next(
                        (d for d in daemon.snapshot()["deliveries"] if d["task_id"] == parent_id and d["status"] == "sent"),
                        None,
                    ), description="final plan callback",
                )
                assert callback["target"] == REMOTE
                assert callback["session_id"] == session
                assert len([d for d in daemon.snapshot()["deliveries"] if d["task_id"] == parent_id]) == 1
                print("PASS dependent real tools, result forwarding, review extension and fixed-origin final callback")

                failed = draft(daemon, session, "FAIL_CASE")
                start(daemon, failed)
                waiting = wait_for(
                    lambda: (p if (p := find_plan(daemon, failed["id"]))["status"] in {"waiting_input", "completed", "failed"} else None),
                    description="failed dependency handling",
                )
                assert waiting["status"] == "waiting_input", waiting
                assert any(n["status"] == "blocked" for n in waiting["nodes"]), waiting
                assert not any(
                    t.get("plan_id") == failed["id"] and t.get("node_id") == "blocked"
                    for t in daemon.snapshot()["tasks"]
                )
                print("PASS failed dependencies block successors and cannot be reported as successful")

                background = draft(daemon, session, "NONBLOCK_CASE")
                start(daemon, background)
                wait_for(ModelFixture.worker_started.is_set, description="pending worker model request")
                daemon.command({
                    "type": "send_message", "session_id": session, "content": "PING_DURING_WORKER",
                })
                wait_for(
                    lambda: any(
                        m["content"] == "主智能体仍可响应：PING_DURING_WORKER"
                        for s in daemon.snapshot()["sessions"] if s["id"] == session
                        for m in s["messages"]
                    ), timeout=5, description="main agent replies during a pending child request",
                )
                assert find_plan(daemon, background["id"])["status"] == "running"
                daemon.command({"type": "cancel_plan", "plan_id": background["id"]})
                ModelFixture.release_worker.set()
                print("PASS main agent responds while a child model request remains pending")

                cancelled = draft(daemon, session, "CANCEL_REVIEW")
                start(daemon, cancelled)
                wait_for(ModelFixture.review_started.is_set, description="blocked review request")
                daemon.command({"type": "cancel_plan", "plan_id": cancelled["id"]})
                ping = daemon.command({"type": "send_message", "session_id": session, "content": "PING_AFTER_CANCEL"})
                assert ping.get("job_id"), ping
                wait_for(
                    lambda: any(
                        m["content"] == "主智能体仍可响应：PING_AFTER_CANCEL"
                        for s in daemon.snapshot()["sessions"] if s["id"] == session
                        for m in s["messages"]
                    ),
                    timeout=5, description="coordinator releases cancelled model request",
                )
                ModelFixture.release_review.set()
                assert find_plan(daemon, cancelled["id"])["status"] == "cancelled"
                print("PASS cancellation interrupts a pending review without reviving the plan")

                bounded = draft(daemon, session, "BUDGET_CASE")
                start(daemon, bounded)
                waiting = wait_for(
                    lambda: (p if (p := find_plan(daemon, bounded["id"]))["status"] == "waiting_input" else None),
                    timeout=25, description="bounded continuation",
                )
                assert waiting["round"] <= 3, waiting
                assert len(waiting["nodes"]) <= 12, waiting
                print("PASS automatic continuation respects round and node budgets")
                daemon.stop()
                daemon.start()
                snapshot = daemon.snapshot()
                assert next(p for p in snapshot["plans"] if p["id"] == plan["id"])["status"] == "completed"
                assert next(p for p in snapshot["plans"] if p["id"] == cancelled["id"])["status"] == "cancelled"
                assert "fixture-secret-no-leak" not in json.dumps(snapshot)
                print("PASS plans and terminal callbacks survive restart without exposing model credentials")
            finally:
                ModelFixture.release_review.set()
                ModelFixture.release_worker.set()
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()
    print("All orchestration scenarios passed with a local model fixture; no cloud model was called.")


if __name__ == "__main__":
    main()
