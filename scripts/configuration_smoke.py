#!/usr/bin/env python3
"""Verify local provider reuse, real request paths and restart without cloud access."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import urllib.error

from smoke import Daemon, wait_for


class Provider(BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *_args):
        pass

    def authenticate(self):
        assert self.headers.get("Authorization") == "Bearer fixture-provider-token"
        assert self.headers.get("x-actor") == "fixture-actor-token"

    def respond(self, value):
        content = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def do_GET(self):
        assert self.path == "/v1/models"
        self.authenticate()
        self.calls.append("models")
        self.respond({"data": [{"id": "fixture-model"}]})

    def do_POST(self):
        assert self.path == "/v1/chat/completions"
        self.authenticate()
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["model"] == "fixture-model"
        if body.get("tools"):
            self.calls.append("worker")
            if any(message["role"] == "tool" for message in body["messages"]):
                message = {"content": "完成 fixture-provider-token fixture-actor-token"}
            else:
                message = {"tool_calls": [{
                    "id": "write-proof", "type": "function",
                    "function": {"name": "write_file", "arguments": json.dumps({
                        "path": "proof.txt", "content": "local provider reuse\n",
                    })},
                }]}
        elif body["messages"][0]["content"].startswith("Return exactly"):
            self.calls.append("check")
            message = {"content": '{"ok":true}'}
        else:
            self.calls.append("coordinator")
            message = {"content": json.dumps({
                "action": "reply", "message": "连接正常 fixture-actor-token",
            })}
        self.respond({"choices": [{"message": {"role": "assistant", **message}}]})


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-configuration-") as temporary:
            root = Path(temporary)
            codex_home = root / "codex"
            codex_home.mkdir()
            config = f'''model = "fixture-model"
model_provider = "fixture"
[model_providers.fixture]
base_url = "http://127.0.0.1:{server.server_port}/v1"
experimental_bearer_token = "fixture-provider-token"
[model_providers.fixture.http_headers]
x-actor = "fixture-actor-token"
'''
            (codex_home / "config.toml").write_text(config, encoding="utf-8")
            project = root / "project"
            project.mkdir()
            daemon = Daemon(root, {"CODEX_HOME": str(codex_home)})
            try:
                daemon.start()
                preview = daemon.request("GET", "/api/v1/provider/local-codex")
                assert preview["configured"] and preview["api_key_configured"]
                assert not daemon.request("GET", "/api/v1/settings")["configured"]
                draft = {"provider_source": "local_codex", "model": ""}
                assert daemon.request("POST", "/api/v1/settings/check", draft)["ok"]
                assert not daemon.request("GET", "/api/v1/settings")["configured"]
                daemon.request("PUT", "/api/v1/settings", draft)
                catalog = daemon.request("POST", "/api/v1/models", {})
                assert catalog["models"][0]["id"] == "fixture-model"
                session = daemon.command({
                    "type": "create_session", "title": "配置回归",
                    "project_path": str(project),
                })["session_id"]
                daemon.command({"type": "send_message", "session_id": session, "content": "回复连接状态"})
                wait_for(lambda: "coordinator" in Provider.calls and any(
                    "连接正常" in message["content"]
                    for item in daemon.snapshot()["sessions"] for message in item["messages"]
                ), description="coordinator using shared credentials")
                task_id = daemon.command({
                    "type": "start_task", "session_id": session, "title": "写入验证文件",
                })["task_id"]
                wait_for(lambda: any(
                    task["id"] == task_id and task["status"] == "completed"
                    for task in daemon.snapshot()["tasks"]
                ), description="worker using shared credentials")
                assert (project / "proof.txt").read_text() == "local provider reuse\n"
                public = json.dumps(daemon.snapshot()) + json.dumps(daemon.request("GET", "/api/v1/settings"))
                for secret in ["fixture-provider-token", "fixture-actor-token"]:
                    assert secret not in public
                daemon.stop()
                daemon.start()
                assert daemon.request("GET", "/api/v1/settings")["configured"]
                assert daemon.request("POST", "/api/v1/settings/check", {})["ok"]
                with sqlite3.connect(root / "harness.sqlite") as db:
                    saved = db.execute("SELECT body FROM settings WHERE id='provider'").fetchone()[0]
                assert json.loads(saved)["provider_source"] == "local_codex"
                assert "fixture-provider-token" not in saved and "fixture-actor-token" not in saved
                assert (codex_home / "config.toml").read_text(encoding="utf-8") == config
                assert set(Provider.calls) == {"models", "check", "worker", "coordinator"}
                print("PASS provider metadata, unsaved connection check, authenticated catalog/master/worker, redaction and restart")

                before = daemon.request("GET", "/api/v1/settings")
                try:
                    daemon.request("PUT", "/api/v1/settings", {"model": "sk-" + "x" * 40})
                    raise AssertionError("Accepted a credential as a model")
                except urllib.error.HTTPError as error:
                    assert error.code == 400
                assert daemon.request("GET", "/api/v1/settings") == before
                print("PASS invalid model rejected without changing saved configuration")
            finally:
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
