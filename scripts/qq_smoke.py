#!/usr/bin/env python3
"""Real daemon + local QQ HTTP/WebSocket + local Native model. No live services."""
import base64
import datetime
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import queue
import socket
import struct
import subprocess
import tempfile
import threading
import time
import uuid
from pathlib import Path

from smoke import Daemon, ROOT, wait_for

QQ_SECRET = "self-agent-qq-fixture-secret"
MODEL_SECRET = "qq-smoke-model-canary"
TERMINAL = {"completed", "failed", "cancelled"}


def frame(value, opcode=1):
    payload = json.dumps(value, ensure_ascii=False).encode() if opcode == 1 else value
    size = len(payload)
    prefix = bytes([128 | opcode, size]) if size < 126 else bytes([128 | opcode, 126]) + struct.pack("!H", size)
    return prefix + payload


def parse_frame(buffer):
    if len(buffer) < 2:
        return None
    opcode, size = buffer[0] & 15, buffer[1] & 127
    assert buffer[1] & 128, "client frames must be masked"
    offset = 2
    if size == 126:
        if len(buffer) < 4:
            return None
        size, offset = struct.unpack("!H", buffer[2:4])[0], 4
    elif size == 127:
        if len(buffer) < 10:
            return None
        size, offset = struct.unpack("!Q", buffer[2:10])[0], 10
    assert size <= 262144
    if len(buffer) < offset + 4 + size:
        return None
    mask = buffer[offset:offset + 4]
    payload = bytes(value ^ mask[i % 4] for i, value in enumerate(buffer[offset + 4:offset + 4 + size]))
    return opcode, payload, buffer[offset + 4 + size:]


class Platform:
    def __init__(self):
        self.lock = threading.Lock()
        self.connections = []
        self.history = []
        self.seq = 1
        self.posts = []
        self.errors = []
        self.identities = []
        self.auths = 0
        self.stopped = threading.Event()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), QQ)
        self.server.daemon_threads = True
        self.server.fixture = self
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def origin(self):
        return f"http://127.0.0.1:{self.server.server_port}/"

    def emit(self, content, user="alice", group=None, message_id=None, original=None):
        with self.lock:
            self.seq += 1
            if original:
                event = json.loads(json.dumps(original))
                event["s"] = self.seq
                event["id"] = "alias-" + str(uuid.uuid4())
            else:
                event = {"op": 0, "s": self.seq, "id": "event-" + str(uuid.uuid4()),
                         "t": "GROUP_AT_MESSAGE_CREATE" if group else "C2C_MESSAGE_CREATE",
                         "d": {"id": message_id or "message-" + str(uuid.uuid4()),
                               "timestamp": (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)).isoformat(),
                               "content": ("<@!fixture_bot> " if group else "") + content,
                               "author": {"member_openid" if group else "user_openid": user}}}
                if group:
                    event["d"]["group_openid"] = group
            self.history.append(event)
            for connection in self.connections:
                connection.put(event)
            return event

    def replies(self, event):
        with self.lock:
            return [post for post in self.posts if post["body"]["msg_id"] == event["d"]["id"]]

    def stop(self):
        self.stopped.set()
        self.server.shutdown()
        self.server.server_close()


class QQ(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def answer(self, status, body, headers=None):
        data = json.dumps(body, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        fixture = self.server.fixture
        try:
            if self.path == "/gateway":
                assert self.headers.get("Authorization") == "QQBot fixture-token"
                self.answer(200, {"url": fixture.origin.replace("http:", "ws:") + "websocket"})
                return
            assert self.path == "/websocket"
            accept = base64.b64encode(hashlib.sha1((self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            self.connection.sendall(frame({"op": 10, "d": {"heartbeat_interval": 1000}}))
            self.connection.settimeout(0.1)
            pending, buffer, identified = queue.Queue(), b"", False
            with fixture.lock:
                fixture.connections.append(pending)
            try:
                while not fixture.stopped.is_set():
                    if identified:
                        while not pending.empty():
                            self.connection.sendall(frame(pending.get_nowait()))
                    try:
                        data = self.connection.recv(4096)
                        if not data:
                            return
                        buffer += data
                    except socket.timeout:
                        continue
                    while True:
                        parsed = parse_frame(buffer)
                        if parsed is None:
                            break
                        opcode, payload, buffer = parsed
                        if opcode == 8:
                            return
                        if opcode == 9:
                            self.connection.sendall(frame(payload, 10))
                            continue
                        assert opcode == 1
                        value = json.loads(payload)
                        if value["op"] in (2, 6):
                            assert value["d"]["token"] == "QQBot fixture-token"
                            with fixture.lock:
                                fixture.identities.append(value)
                                if value["op"] == 6:
                                    replay = [event for event in fixture.history if event["s"] > value["d"]["seq"]]
                                    for event in replay:
                                        self.connection.sendall(frame(event))
                                    fixture.seq += 1
                                    ready = {"op": 0, "s": fixture.seq, "t": "RESUMED", "d": {}}
                                else:
                                    ready = {"op": 0, "s": fixture.seq, "t": "READY", "d": {"session_id": "smoke-session", "user": {"id": "fixture_bot"}}}
                            self.connection.sendall(frame(ready))
                            identified = True
                        elif value["op"] == 1:
                            self.connection.sendall(frame({"op": 11}))
                        else:
                            raise AssertionError("unexpected client opcode")
            finally:
                with fixture.lock:
                    fixture.connections.remove(pending)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, socket.timeout):
            pass
        except Exception as error:
            with fixture.lock:
                fixture.errors.append(repr(error))
        finally:
            self.close_connection = True

    def do_POST(self):
        fixture = self.server.fixture
        try:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/app/getAppAccessToken":
                assert body == {"appId": "app_1", "clientSecret": QQ_SECRET}
                with fixture.lock:
                    fixture.auths += 1
                self.answer(200, {"access_token": "fixture-token", "expires_in": "7200"})
                return
            assert self.headers.get("Authorization") == "QQBot fixture-token"
            assert self.path.startswith(("/v2/users/", "/v2/groups/")) and self.path.endswith("/messages")
            assert body["msg_type"] == 0 and 1 <= body["msg_seq"] <= 5
            assert len(body["content"].encode()) <= 4000
            assert QQ_SECRET not in body["content"] and MODEL_SECRET not in body["content"]
            with fixture.lock:
                fixture.posts.append({"path": self.path, "body": body})
                attempts = len([item for item in fixture.posts if item["body"]["msg_id"] == body["msg_id"] and item["body"]["msg_seq"] == body["msg_seq"]])
            if body["msg_id"].startswith("lost-") and body["msg_seq"] == 1:
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            if body["msg_id"].startswith("rate-") and body["msg_seq"] == 1 and attempts == 1:
                self.answer(429, {"code": 429}, {"Retry-After": "1"})
                return
            self.answer(200, {"id": "sent-" + str(uuid.uuid4())})
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        except Exception as error:
            with fixture.lock:
                fixture.errors.append(repr(error))
            self.answer(500, {"error": "fixture"})


class Model(BaseHTTPRequestHandler):
    errors, calls = [], []
    slow = threading.Event()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions"
            assert self.headers.get("Authorization") == "Bearer " + MODEL_SECRET
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if not body.get("tools"):
                message = {"role": "assistant", "content": json.dumps({"action": "plan", "message": "本地编排夹具计划草稿", "title": "QQ fixture plan", "nodes": [{"id": "step-1", "title": "fixture step", "objective": "QQ_CASE:done", "depends_on": []}]}, ensure_ascii=False)}
                if '"kind":"review"' in body["messages"][-1]["content"]:
                    message["content"] = json.dumps({"action": "complete", "message": "fixture plan complete"})
                finish = "stop"
            else:
                prompt = next(item["content"] for item in reversed(body["messages"]) if item["role"] == "user" and "QQ_CASE:" in item["content"])
                goal = prompt.split("QQ_CASE:", 1)[1].splitlines()[0].strip()
                outputs = [item for item in body["messages"] if item["role"] == "tool"]
                self.calls.append(goal)
                if goal == "slow":
                    self.slow.wait(20)
                tool = None
                if not outputs:
                    if goal == "read":
                        tool = ("read_file", {"path": "approved.txt"})
                    elif goal == "question":
                        tool = ("request_user_input", {"prompt": "选择保留名称", "timeout_seconds": 60})
                    elif goal == "approval":
                        tool = ("write_file", {"path": "not-approved.txt", "content": "must-not-write"})
                if tool:
                    message = {"role": "assistant", "content": None, "tool_calls": [{"id": "qq-tool-1", "type": "function", "function": {"name": tool[0], "arguments": json.dumps(tool[1], ensure_ascii=False)}}]}
                    finish = "tool_calls"
                else:
                    message = {"role": "assistant", "content": "PRIVATE_RESULT_CANARY fixture completed"}
                    finish = "stop"
            data = json.dumps({"choices": [{"message": message, "finish_reason": finish}]}, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        except Exception as error:
            self.errors.append(repr(error))
            self.send_error(500)


def main():
    platform = Platform()
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    model.daemon_threads = True
    threading.Thread(target=model.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-qq-") as temporary:
            root = Path(temporary).resolve()
            project = root / "project"
            project.mkdir()
            (project / "approved.txt").write_text("fixture project evidence", encoding="utf-8")
            daemon = Daemon(root, {"HARNESS_QQ_FIXTURE_MODE": "1", "HARNESS_QQ_FIXTURE_URL": platform.origin, "HARNESS_API_KEY": MODEL_SECRET})
            def cli(*arguments, stdin=None):
                executable = ROOT / "target" / "debug" / ("self-agent-cli.exe" if os.name == "nt" else "self-agent-cli")
                result = subprocess.run([str(executable), "--data-dir", str(root), *arguments], input=stdin,
                                        text=True, encoding="utf-8", capture_output=True, timeout=20)
                assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
                assert QQ_SECRET not in result.stdout + result.stderr and MODEL_SECRET not in result.stdout + result.stderr
                return json.loads(result.stdout)
            def qq():
                return daemon.request("GET", "/api/v1/qq")
            def admin(command):
                result = daemon.request("POST", "/api/v1/qq/commands", {"id": str(uuid.uuid4()), "command": command})
                assert result["status"] == "completed", result
                return result
            def handled(event):
                route = next((post for post in platform.replies(event)), None)
                return route
            def task(goal):
                return next((item for item in reversed(daemon.snapshot()["tasks"]) if item.get("task_kind") == "main" and item.get("objective", "").strip() == "QQ_CASE:" + goal), None)
            def done(goal):
                value = task(goal)
                return value if value and value["status"] in TERMINAL else None
            def bind(user, group=None, share=False):
                pair = admin({"type": "issue_pairing", "ttl_seconds": 300})
                event = platform.emit("/配对 " + pair["pairing"]["code"], user=user, group=group)
                binding = wait_for(lambda: next((item for item in qq()["bindings"] if item["identity"]["sender_open_id"] == user and item["identity"]["target_id"] == (group or user) and item["status"] == "pending"), None), description="pending QQ pairing")
                assert not platform.replies(event)
                grant = {"session_ids": [session], "permissions": {"view": True, "read_results": True, "run": True, "cancel": True, "answer": True, "orchestrate": True, "create_session": True, "share_group_results": share}, "controls": {"permissions": {"read_files": True, "write_files": "ask", "run_commands": "deny", "managed_tools": "deny"}, "budget": {"max_runtime_seconds": 300, "max_output_bytes": 32768}, "capabilities": {"mcp_servers": [], "skills": []}}, "expires_at_ms": int(time.time() * 1000) + 600000}
                result = admin({"type": "confirm_binding", "principal_id": binding["principal_id"], "expected_revision": binding["revision"], "identity": binding["identity"], "grant": grant})
                # Simulated QQ clock leads by the permitted 30s; a newly emitted
                # event is strictly beyond the immutable confirmation fence.
                time.sleep(0.01)
                selected = platform.emit("/选择 " + session, user=user, group=group)
                wait_for(lambda: handled(selected), description="scoped session selection")
                return result
            try:
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {"provider_source": "manual", "main_executor": "native", "model": "fixture-qq", "base_url": f"http://127.0.0.1:{model.server_port}/v1", "api_key": MODEL_SECRET})
                session = daemon.command({"type": "create_session", "title": "QQ approved project", "project_path": str(project)})["session_id"]
                config = admin({"type": "configure", "config_id": None, "expected_revision": None, "app_id": "app_1", "enabled": False, "reply_policy": {"window_ms": 300000, "max_user": 4, "max_group": 5}})
                assert cli("qq", "snapshot")["configuration"]["id"] == config["config_id"]
                admin({"type": "configure", "config_id": config["config_id"], "expected_revision": config["revision"], "app_id": "app_1", "enabled": True, "reply_policy": {"window_ms": 300000, "max_user": 4, "max_group": 5}})
                command_id = str(uuid.uuid4())
                pairing = cli("--command-id", command_id, "qq", "pair", "30")
                replay = cli("qq", "receipt", command_id)
                assert pairing["status"] == "completed" and "code" in pairing["pairing"]
                assert pairing["pairing"]["code"] not in json.dumps(replay)
                assert cli("qq", "credential-operation", str(uuid.uuid4())) is None
                wait_for(lambda: qq()["connection"]["state"] == "ready", description="real loopback QQ gateway")
                assert qq()["configuration"]["mode"] == "fixture"
                alice = bind("alice")
                unpaired = platform.emit("QQ_CASE:unauthorized", user="unpaired")
                time.sleep(0.3)
                assert task("unauthorized") is None and not platform.replies(unpaired)
                print("PASS official-shape HTTP/WS handshake, local confirmation and unpaired fail-closed")

                event = platform.emit("QQ_CASE:read")
                current = wait_for(lambda: done("read"), description="actual Native read task")
                assert current["status"] == "completed" and current["source"]["actor_id"] == alice["principal_id"]
                assert current["controls"]["permissions"]["run_commands"] == "deny"
                wait_for(lambda: len(platform.replies(event)) == 2, description="acceptance and completion outbox")
                assert all("PRIVATE_RESULT_CANARY" not in item["body"]["content"] for item in platform.replies(event))
                before = len(daemon.snapshot()["tasks"])
                platform.emit("", original=event)
                time.sleep(0.3)
                assert len(daemon.snapshot()["tasks"]) == before and len(platform.replies(event)) == 2
                result = platform.emit("/结果 " + current["id"])
                assert "PRIVATE_RESULT_CANARY" in wait_for(lambda: handled(result))["body"]["content"]
                print("PASS native tool evidence, immutable restrictions, fixed-route dual replies and input deduplication")

                question = platform.emit("QQ_CASE:question")
                waiting = wait_for(lambda: task("question") if task("question") and task("question")["status"] == "waiting_input" else None)
                request = next(item for item in daemon.snapshot()["execution_requests"] if item["id"] == waiting["waiting_request_id"])
                query = platform.emit("/问题 " + waiting["id"])
                assert request["id"] in wait_for(lambda: handled(query))["body"]["content"]
                answer = platform.emit(f"/回答 {request['id']} {request['revision']} 保留原名")
                wait_for(lambda: handled(answer))
                assert wait_for(lambda: done("question"))["status"] == "completed"
                stored = next(item for item in daemon.snapshot()["execution_requests"] if item["id"] == request["id"])
                assert stored["decision_actor"] == "qq:" + alice["principal_id"]
                platform.emit("QQ_CASE:approval")
                waiting = wait_for(lambda: task("approval") if task("approval") and task("approval")["status"] == "waiting_input" else None)
                request = next(item for item in daemon.snapshot()["execution_requests"] if item["id"] == waiting["waiting_request_id"])
                assert request["kind"] == "approval"
                denial = platform.emit(f"/回答 {request['id']} {request['revision']} 批准")
                assert "未执行" in wait_for(lambda: handled(denial))["body"]["content"]
                assert not (project / "not-approved.txt").exists()
                daemon.command({"type": "decide_request", "request_id": request["id"], "expected_revision": request["revision"], "action_digest": request["action_digest"], "approve": False})
                wait_for(lambda: done("approval"))
                print("PASS scoped question answer and strict separation from local action approval")

                platform.emit("QQ_CASE:slow")
                slow = wait_for(lambda: task("slow") if task("slow") and task("slow")["status"] == "running" else None)
                query = platform.emit("/状态 " + slow["id"])
                wait_for(lambda: handled(query), timeout=5, description="status while model blocked")
                cancel = platform.emit("/取消 " + slow["id"])
                wait_for(lambda: handled(cancel), timeout=5)
                assert wait_for(lambda: done("slow"))["status"] == "cancelled"
                Model.slow.set()
                print("PASS management stays responsive while model is blocked and remote cancellation works")

                bind("bob", "group_1")
                group = platform.emit("/结果 " + current["id"], user="bob", group="group_1")
                reply = wait_for(lambda: handled(group))
                assert reply["path"] == "/v2/groups/group_1/messages" and "PRIVATE_RESULT_CANARY" not in reply["body"]["content"] and "未执行" in reply["body"]["content"]
                rate = platform.emit("/状态 " + current["id"], message_id="rate-" + str(uuid.uuid4()))
                wait_for(lambda: len(platform.replies(rate)) == 2)
                assert platform.replies(rate)[0]["body"] == platform.replies(rate)[1]["body"]
                lost = platform.emit("/状态 " + current["id"], message_id="lost-" + str(uuid.uuid4()))
                wait_for(lambda: handled(lost))
                wait_for(lambda: any(item["status"] == "unknown" for item in qq()["outbox"]))
                daemon.stop()
                daemon.start()
                wait_for(lambda: qq()["connection"]["state"] == "ready")
                platform.emit("", original=lost)
                time.sleep(0.5)
                assert len(platform.replies(lost)) == 1
                assert any(item["status"] == "unknown" for item in qq()["outbox"])
                assert any(item["op"] == 6 for item in platform.identities)
                print("PASS group audience scope, fixed 429 retry and restart Resume without replaying unknown send")

                plan_event = platform.emit("/编排 制定并完成 QQ_CASE:done")
                plan = wait_for(lambda: next((item for item in daemon.snapshot()["plans"] if item["source"]["channel_id"] == "qq"), None))
                list_plans = platform.emit("/计划")
                assert plan["id"] in wait_for(lambda: handled(list_plans))["body"]["content"]
                start = platform.emit(f"/启动计划 {plan['id']} {plan['revision']}")
                wait_for(lambda: handled(start))
                finished = wait_for(lambda: next((item for item in daemon.snapshot()["plans"] if item["id"] == plan["id"] and item["status"] in TERMINAL), None), timeout=25)
                assert finished["status"] == "completed", finished["status"]
                wait_for(lambda: len(platform.replies(plan_event)) >= 3)
                assert len(platform.replies(plan_event)) == 3, "worker nodes must not consume passive reply quota"
                admin({"type": "revoke_binding", "principal_id": alice["principal_id"], "expected_revision": alice["revision"]})
                denied = platform.emit("QQ_CASE:after-revoke")
                time.sleep(0.4)
                assert task("after-revoke") is None and not platform.replies(denied)
                assert not Model.errors and not platform.errors, (Model.errors, platform.errors)
                assert QQ_SECRET not in json.dumps(daemon.snapshot()) + json.dumps(qq())
                print("PASS actual scoped orchestration, parent-only completion and revoked principal cannot enqueue")
            finally:
                Model.slow.set()
                daemon.stop()
    finally:
        platform.stop()
        model.shutdown()
        model.server_close()


if __name__ == "__main__":
    main()
