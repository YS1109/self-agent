"""Optional local Chromium UI fixture, not a real gateway/daemon deployment.

Build with `pnpm exec vite build --config vite.mobile.config.ts` first. Requires
the official Playwright Python package and a local Chromium binary; no browser
download, external messages, credentials or network service is used here.
"""
import json
import os
from pathlib import Path
import socket
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist-mobile"
DEVICE = "11111111-1111-4111-8111-111111111111"
PRINCIPAL = "22222222-2222-4222-8222-222222222222"
SESSION = "33333333-3333-4333-8333-333333333333"
TASK = "44444444-4444-4444-8444-444444444444"
QUESTION = "55555555-5555-4555-8555-555555555555"
ARTIFACT = "66666666-6666-4666-8666-666666666666"
PERMISSIONS = dict(view=True, read_results=True, read_artifacts=True, run=True, cancel=True, answer=True)
STATE = {"commands": [], "receipts": {}, "status": "waiting_input",
         "list_reads": 0, "session_reads": 0, "event_reads": 0}
LOCK = threading.Lock()


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(DIST), **kwargs)

    def log_message(self, *_):
        pass

    def reply(self, data=None, code="ok"):
        body = json.dumps({"code": code, "data": data}, ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("cache-control", "no-store")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    def do_GET(self):
        if self.path == "/gateway/api/session":
            self.reply(dict(csrf="a" * 64, paired=True, mode="fixture", device_id=DEVICE,
                            principal_id=PRINCIPAL, binding_revision=1, permissions=PERMISSIONS,
                            expires_at_ms=int(time.time() * 1000) + 3600000))
        elif self.path.startswith("/gateway/api/events?"):
            with LOCK:
                STATE["event_reads"] += 1
                offline = STATE["event_reads"] == 1
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-store")
            self.end_headers()
            try:
                if offline:
                    self.wfile.write(b'event: gateway\ndata: {"code":"device_offline"}\n\n')
                    self.wfile.flush()
                    return
                self.wfile.write(b'event: gateway\ndata: {"code":"ok","data":{"cursor":0,"events":[]}}\n\n')
                self.wfile.flush()
                for _ in range(10):
                    time.sleep(.2)
                    self.wfile.write(b": fixture\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass
        else:
            if self.path == "/":
                self.path = "/mobile.html"
            super().do_GET()

    def do_POST(self):
        size = int(self.headers.get("content-length", "0"))
        assert size <= 262144
        body = json.loads(self.rfile.read(size))
        assert self.headers.get("x-csrf-token") == "a" * 64
        assert self.headers.get("authorization") is None
        assert self.path == "/gateway/api/rpc"
        kind = body["type"]
        if kind == "sessions":
            with LOCK:
                STATE["list_reads"] += 1
                offline = STATE["list_reads"] == 1
            if offline:
                self.reply(code="device_offline")
                return
            self.reply({"sessions": [{"id": SESSION, "title": "远程会话 · 本地协议夹具"}]})
        elif kind == "session":
            with LOCK:
                STATE["session_reads"] += 1
                offline = STATE["session_reads"] == 1
            if offline:
                self.reply(code="device_offline")
                return
            self.reply({"session": {"id": SESSION, "title": "远程会话", "messages": [
                {"id": "m1", "role": "user", "content": "请检查测试失败，并保留原验证证据。"},
                {"id": "m2", "role": "assistant", "content": "已定位失败测试；继续操作仍遵循本机确认的权限。\n<script>window.fixtureInjected=true</script>"}]},
                "tasks": [{"id": TASK, "session_id": SESSION, "title": "修复并复验 · 模拟任务", "status": STATE["status"],
                           "waiting_request_id": QUESTION, "stream_output": "读取项目结构 → 发现验证失败 → 等待回答"}],
                "questions": [{"id": QUESTION, "task_id": TASK, "session_id": SESSION, "kind": "approval", "revision": 1,
                               "status": "pending", "prompt": "写操作必须由本机核对具体动作。", "expires_at_ms": int(time.time() * 1000) + 60000}],
                "cursor": 0})
        elif kind == "command":
            with LOCK:
                STATE["commands"].append(body)
                STATE["receipts"][body["command_id"]] = {"command_id": body["command_id"], "status": "accepted", "session_id": SESSION, "task_id": TASK}
                if body["command"]["type"] == "cancel":
                    STATE["status"] = "cancelled"
                    result = STATE["receipts"][body["command_id"]]
                else:
                    result = None
            if result:
                self.reply(result)
            else:
                # Commit was observed but the HTTP result is deliberately lost.
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
        elif kind == "receipt":
            self.reply(STATE["receipts"].get(body["command_id"]))
        elif kind == "changes":
            self.reply({"state": "available", "attribution": "not_exclusive", "files": [{"path": "src/example.rs", "patch": "- failing\n+ verified"}]})
        elif kind == "artifacts":
            self.reply({"artifacts": [{"id": ARTIFACT, "task_id": TASK, "name": "验收证据.txt", "bytes": 18, "truncated": False}]})
        elif kind == "artifact":
            time.sleep(.4)
            self.reply({"artifact": {"id": ARTIFACT, "task_id": TASK, "name": "验收证据.txt", "bytes": 18, "truncated": False, "content": "exit_code=0\nfixture"}})
        else:
            self.reply(code="invalid_request")


def main():
    assert (DIST / "mobile.html").exists(), "Build the H5 bundle first"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=os.environ.get("CHROMIUM_BIN", "/usr/bin/chromium"),
                                        headless=True, args=["--no-sandbox", "--disable-dev-shm-usage"])
            page = browser.new_page(viewport={"width": 390, "height": 844}, device_scale_factor=1)
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(f"http://127.0.0.1:{server.server_port}/mobile.html")
            page.get_by_text("本地协议夹具 · 不是已部署的公网网关", exact=False).wait_for()
            page.get_by_role("button", name="取消任务", exact=True).wait_for()
            page.get_by_text("设备在线 · 事件已连接", exact=True).wait_for()
            assert STATE["list_reads"] >= 2 and STATE["session_reads"] >= 3 and STATE["event_reads"] >= 2
            assert STATE["commands"] == [], "Read recovery must never submit a command"
            assert page.get_by_text("设备离线，未排队执行。", exact=True).count() == 0
            assert page.evaluate("window.fixtureInjected") is None
            assert page.get_by_role("button", name="批准", exact=True).count() == 0
            page.screenshot(path="/tmp/gateway-h5-mobile.png", full_page=True)
            page.get_by_role("button", name="取消任务", exact=True).click()
            page.get_by_text("已取消", exact=True).wait_for()
            page.get_by_label("在此会话发起新任务", exact=True).fill("commit fixture then lose response")
            page.get_by_role("button", name="发送新任务", exact=True).click()
            page.get_by_role("heading", name="这次提交尚未确认").wait_for()
            page.get_by_text("结果未知；仅检查原命令回执", exact=False).wait_for()
            command_count = len(STATE["commands"])
            page.reload()
            page.get_by_role("button", name="只读检查原回执").wait_for()
            assert page.get_by_role("button", name="发送新任务", exact=True).is_disabled()
            page.get_by_role("button", name="只读检查原回执").click()
            page.get_by_text("原命令已有权威回执；没有重发。", exact=True).wait_for()
            assert len(STATE["commands"]) == command_count == 2
            page.get_by_role("button", name="查看 diff / 文本产物").click()
            page.get_by_role("button", name="验收证据.txt", exact=False).wait_for()
            page.get_by_role("button", name="验收证据.txt", exact=False).click()
            page.get_by_role("button", name="关闭", exact=True).click()
            page.wait_for_timeout(600)
            assert page.get_by_role("dialog").count() == 0
            assert not errors, errors
            assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
            browser.close()
        print("PASS H5 Chromium: offline snapshot/event recovery without commands, narrow viewport, escaped text, no approval, one-shot/refresh/read-only recovery, Close ignores late artifact")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
