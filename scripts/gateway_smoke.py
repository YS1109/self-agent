#!/usr/bin/env python3
"""Real daemon + independent gateway + local Native model, HTTP/WS only.
No browser owner token proxy, live model, real vault, public network or deployment.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid

from smoke import Daemon, ROOT, available_port, wait_for

SECRET = "gateway-model-fixture-canary"
TERMINAL = {"completed", "failed", "cancelled"}
CONTROLS = {"budget": {"max_model_requests": 8, "max_runtime_seconds": 60, "max_retries": 0,
                      "max_repair_attempts": 0, "max_output_bytes": 65536, "max_external_calls": 0},
            "permissions": {"read_files": True, "write_files": "deny", "run_commands": "deny", "managed_tools": "deny"},
            "acceptance_commands": [], "capabilities": {"mcp_servers": [], "skills": []}}


def identity():
    return str(uuid.uuid4())


class Model(BaseHTTPRequestHandler):
    errors = []
    calls = []
    hold = threading.Event()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/chat/completions"
            assert self.headers.get("Authorization") == "Bearer " + SECRET
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            goal = next(m["content"].split("GW_CASE:", 1)[1].splitlines()[0]
                        for m in reversed(body["messages"]) if m["role"] == "user" and "GW_CASE:" in m.get("content", ""))
            outputs = [m for m in body["messages"] if m["role"] == "tool"]
            self.calls.append(goal)
            if goal == "slow":
                self.hold.wait(15)
            if goal == "question" and not outputs:
                message = {"role": "assistant", "content": None, "tool_calls": [{"id": "question-1", "type": "function",
                    "function": {"name": "request_user_input", "arguments": json.dumps({"prompt": "请确认名称 " + SECRET, "timeout_seconds": 30})}}]}
                reason = "tool_calls"
            else:
                message = {"role": "assistant", "content": "本地实际协议夹具完成；" + SECRET + ("；已回答" if outputs else "")}
                reason = "stop"
            payload = json.dumps({"choices": [{"message": message, "finish_reason": reason}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        except Exception as error:
            self.errors.append(type(error).__name__)
            self.send_error(500)


class Gateway:
    def __init__(self, root):
        name = "harness-gateway.exe" if os.name == "nt" else "harness-gateway"
        self.binary = ROOT / "target" / "debug" / name
        if not self.binary.is_file():
            self.binary = ROOT / "crates" / "harness-gateway" / "target" / "debug" / name
        self.path = root / "devices.sqlite"
        enrolled = subprocess.run([str(self.binary), "--db", str(self.path), "enroll", "本地夹具"], capture_output=True,
                                  text=True, encoding="utf-8", check=True, timeout=10)
        enrollment = json.loads(enrolled.stdout)
        self.device = enrollment["device"]["device_id"]
        self.token = enrollment["transport_token"]
        self.port = available_port()
        self.origin = f"http://127.0.0.1:{self.port}"
        self.process = None
        self.log = root / "gateway.log"
        self.handle = None

    def start(self):
        self.handle = self.log.open("ab")
        env = {k: v for k, v in os.environ.items() if not k.startswith("HARNESS_")}
        self.process = subprocess.Popen([str(self.binary), "--db", str(self.path), "serve", "--fixture",
            "--origin", self.origin, "--bind", f"127.0.0.1:{self.port}"], cwd=ROOT, env=env, stdout=self.handle, stderr=self.handle)

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.handle:
            self.handle.close()


class Browser:
    def __init__(self, gateway):
        self.gateway, self.cookie, self.csrf = gateway, "", ""

    def request(self, method, path, value=None, origin=None):
        headers = {"Sec-Fetch-Site": "same-origin", "Cookie": self.cookie}
        if method != "GET":
            headers.update({"Origin": origin or self.gateway.origin, "x-csrf-token": self.csrf, "Content-Type": "application/json"})
        request = urllib.request.Request(self.gateway.origin + "/gateway/api/" + path,
            data=None if value is None else json.dumps(value).encode(), method=method, headers=headers)
        with urllib.request.urlopen(request, timeout=25) as response:
            assert response.headers["Cache-Control"] == "no-store"
            cookie = response.headers.get("Set-Cookie")
            if cookie:
                assert "HttpOnly" in cookie and "SameSite=Strict" in cookie
                self.cookie = cookie.split(";", 1)[0]
            result = json.load(response)
        assert SECRET not in json.dumps(result) and self.gateway.token not in json.dumps(result)
        return result

    def bootstrap(self):
        result = self.request("GET", "session")
        assert result["code"] == "ok"
        self.csrf = result["data"]["csrf"]
        return result["data"]

    def rpc(self, value):
        return self.request("POST", "rpc", value)


def main():
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    model.daemon_threads = True
    threading.Thread(target=model.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-gateway-") as temporary:
            root = Path(temporary).resolve()
            gateway = Gateway(root)
            data = root / "daemon"
            data.mkdir()
            daemon = Daemon(data, {"HARNESS_GATEWAY_FIXTURE_MODE": "1", "HARNESS_GATEWAY_FIXTURE_URL": gateway.origin,
                "HARNESS_GATEWAY_FIXTURE_TOKEN": gateway.token, "HARNESS_API_KEY": SECRET})
            browser = Browser(gateway)
            project = root / "project"
            project.mkdir()
            try:
                gateway.start()
                wait_for(browser.bootstrap, description="gateway bootstrap")
                daemon.start()
                daemon.request("PUT", "/api/v1/settings", {"provider_source": "manual", "execution_mode": "native",
                    "main_executor": "native", "model": "gateway-local-fixture", "base_url": f"http://127.0.0.1:{model.server_port}/v1", "api_key": SECRET})
                session = daemon.command({"type": "create_session", "title": "H5 协议夹具", "project_path": str(project)})["session_id"]
                other = daemon.command({"type": "create_session", "title": "未授权会话", "project_path": str(project)})["session_id"]

                def admin(command):
                    result = daemon.request("POST", "/api/v1/gateway/commands", {"id": identity(), "command": command})
                    assert result["status"] == "completed", result
                    return result

                created = admin({"type": "configure", "config_id": None, "expected_revision": None,
                    "origin": "https://fixture.invalid", "device_id": gateway.device, "enabled": False})
                admin({"type": "configure", "config_id": created["config_id"], "expected_revision": created["revision"],
                    "origin": "https://fixture.invalid", "device_id": gateway.device, "enabled": True})
                wait_for(lambda: daemon.request("GET", "/api/v1/gateway")["connection"]["state"] == "ready", description="outbound authenticated WS")
                assert browser.rpc({"type": "sessions"})["code"] == "forbidden"
                pair = admin({"type": "issue_pairing", "ttl_seconds": 300})["pairing"]
                pending = browser.request("POST", "pair", {"device_id": gateway.device, "code": pair["code"], "label": "夹具手机"})
                assert pending["code"] == "ok" and pending["data"]["status"] == "pending", pending
                principal = pending["data"]["principal_id"]
                assert browser.rpc({"type": "sessions"})["code"] == "forbidden"
                grant = {"session_ids": [session], "permissions": {"view": True, "read_results": True, "read_artifacts": True,
                    "run": True, "cancel": True, "answer": True}, "controls": CONTROLS, "expires_at_ms": int(time.time() * 1000) + 600000}
                admin({"type": "confirm_binding", "principal_id": principal, "expected_revision": 0,
                       "expected_label": "夹具手机", "expected_pairing_id": pair["id"], "grant": grant})
                assert browser.request("GET", "pair-status")["data"]["status"] == "active"
                assert browser.bootstrap()["paired"]
                assert browser.rpc({"type": "sessions"})["data"]["sessions"][0]["id"] == session
                assert browser.rpc({"type": "session", "session_id": other})["code"] == "forbidden"
                assert browser.request("POST", "rpc", {"type": "sessions"}, "https://evil.invalid")["code"] == "forbidden"
                assert browser.rpc({"type": "sessions", "source": {"channel_id": "desktop"}})["code"] == "invalid_request"
                print("PASS actual outbound WS, local-confirmed principal/session scope and CSRF/authority rejection")

                def send(content):
                    command = {"type": "command", "command_id": identity(), "session_id": session,
                               "command": {"type": "run", "content": "GW_CASE:" + content}}
                    first = browser.rpc(command)
                    assert first["code"] == "ok", first
                    assert browser.rpc(command) == first
                    return first["data"]["task_id"], command["command_id"]

                def task(task_id):
                    return next(t for t in daemon.snapshot()["tasks"] if t["id"] == task_id)

                def terminal(task_id):
                    current = task(task_id)
                    return current if current["status"] in TERMINAL else None

                task_id, command_id = send("result")
                assert wait_for(lambda: terminal(task_id))["status"] == "completed"
                assert len([t for t in daemon.snapshot()["tasks"] if t["id"] == task_id]) == 1
                assert browser.rpc({"type": "receipt", "command_id": command_id})["data"]["task_id"] == task_id
                view = browser.rpc({"type": "session", "session_id": session})
                assert view["code"] == "ok" and view["data"]["cursor"] > 0
                events = browser.rpc({"type": "events", "session_id": session, "after": 0})
                assert events["code"] == "ok" and events["data"]["events"]
                artifacts = browser.rpc({"type": "artifacts", "task_id": task_id})
                assert artifacts["code"] == "ok" and artifacts["data"]["artifacts"], artifacts
                resource = artifacts["data"]["artifacts"][0]["id"]
                assert browser.rpc({"type": "artifact", "resource_id": resource})["code"] == "ok"
                assert browser.rpc({"type": "artifact", "resource_id": "../private"})["code"] == "invalid_request"
                assert browser.rpc({"type": "changes", "task_id": task_id})["code"] == "ok"
                print("PASS real local Native result, durable exactly-once command receipt, scoped events/diff/text artifacts")

                question_task, _ = send("question")
                request = wait_for(lambda: next((r for r in daemon.snapshot()["execution_requests"] if r["task_id"] == question_task and r["status"] == "pending"), None), description="actual pending question")
                answer = {"type": "command", "command_id": identity(), "session_id": session,
                          "command": {"type": "answer", "request_id": request["id"], "expected_revision": request["revision"], "answer": "保留原名"}}
                response = browser.rpc(answer)
                assert response["code"] == "ok" and browser.rpc(answer) == response
                assert wait_for(lambda: terminal(question_task))["status"] == "completed"
                assert browser.rpc({"type": "command", "command_id": identity(), "session_id": session,
                    "command": {"type": "approve", "request_id": request["id"]}})["code"] == "invalid_request"
                print("PASS actual durable question answered once, approval absent from remote wire")

                slow, _ = send("slow")
                wait_for(lambda: task(slow)["status"] == "running", description="running task before revoke")
                admin({"type": "revoke_binding", "principal_id": principal, "expected_revision": 1})
                assert wait_for(lambda: terminal(slow))["status"] == "cancelled"
                assert task(slow)["stop_reason"] == "remote_permission_revoked"
                denied = browser.rpc({"type": "artifact", "resource_id": resource})
                assert denied["code"] in {"unauthorized", "forbidden"}, denied
                Model.hold.set()
                print("PASS active remote task revoke and resource authorization revoked without replay")

                old_cookie = browser.cookie
                gateway.stop()
                gateway.start()
                wait_for(lambda: browser.bootstrap(), description="gateway restart")
                assert not browser.bootstrap()["paired"] and browser.cookie != old_cookie
                assert browser.rpc({"type": "sessions"})["code"] == "forbidden"
                assert len(daemon.snapshot()["tasks"]) == 3
                assert not Model.errors, Model.errors
                print("PASS auth-only gateway restart requires re-pair and never replays tasks")
            finally:
                Model.hold.set()
                daemon.stop()
                gateway.stop()
            assert gateway.token not in gateway.log.read_text(encoding="utf-8")
            assert SECRET not in (data / "smoke.log").read_text(encoding="utf-8")
    finally:
        model.shutdown()
        model.server_close()


if __name__ == "__main__":
    main()
