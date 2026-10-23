#!/usr/bin/env python3
"""Run the real native tool loop against a deterministic local model fixture."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading

from smoke import Daemon, wait_for


class ModelFixture(BaseHTTPRequestHandler):
    observations = []

    def log_message(self, *_arguments):
        pass

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers["Content-Length"])
        request = json.loads(self.rfile.read(length))
        messages = request["messages"]
        tools = {tool["function"]["name"] for tool in request["tools"]}
        assert "write_file" in tools
        assert "run_command" not in tools
        objective = next(message["content"] for message in messages if message["role"] == "user")
        traversal = "TRAVERSAL" in objective
        tool_results = [message for message in messages if message["role"] == "tool"]
        if not tool_results:
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "fixture-write",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": json.dumps(
                                {
                                    "path": "../outside.txt" if traversal else "proof.txt",
                                    "content": "Written by the native agent tool loop.\n",
                                }
                            ),
                        },
                    }
                ],
            }
        else:
            output = json.loads(tool_results[-1]["content"])
            assert tool_results[-1]["tool_call_id"] == "fixture-write"
            assert any(message.get("tool_calls") for message in messages)
            if traversal:
                assert "error" in output
            else:
                assert "error" not in output, output
            ModelFixture.observations.append({"traversal": traversal, "output": output})
            message = {
                "role": "assistant",
                "content": "Traversal denied." if traversal else "Created proof.txt with the requested content.",
            }
        response = json.dumps({"choices": [{"message": message}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-provider-") as temporary:
            daemon = Daemon(temporary)
            project = Path(temporary) / "project"
            project.mkdir()
            try:
                daemon.start()
                daemon.request(
                    "PUT",
                    "/api/v1/settings",
                    {
                        "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                        "model": "local-contract-fixture",
                        "api_key": "fixture-not-a-real-secret",
                        "execution_mode": "native",
                    },
                )
                session_id = daemon.command(
                    {
                        "type": "create_session",
                        "title": "Native provider fixture",
                        "project_path": str(project),
                    }
                )["session_id"]
                for objective in ["Write proof.txt", "TRAVERSAL attempt"]:
                    task_id = daemon.command(
                        {"type": "start_task", "session_id": session_id, "title": objective}
                    )["task_id"]

                    def finished():
                        task = next(
                            task for task in daemon.snapshot()["tasks"] if task["id"] == task_id
                        )
                        return task if task["status"] in {"completed", "failed", "cancelled"} else None

                    task = wait_for(finished, description=objective)
                    assert task["status"] == "completed", task
                    assert task["execution_mode"] == "native"
                assert (project / "proof.txt").read_text() == "Written by the native agent tool loop.\n"
                assert not (Path(temporary) / "outside.txt").exists()
                assert len(ModelFixture.observations) == 2
                assert "fixture-not-a-real-secret" not in json.dumps(daemon.snapshot())
                assert "fixture-not-a-real-secret" not in json.dumps(
                    daemon.request("GET", "/api/v1/settings")
                )
                print("PASS native HTTP provider -> tool call -> real file write -> tool result -> final response")
                print("PASS workspace traversal denied and shell tool disabled by default")
                print("PASS model credentials absent from API responses")
            finally:
                daemon.stop()
    finally:
        server.shutdown()
        server.server_close()
    print("All native provider fixture scenarios passed; no cloud model was called.")


if __name__ == "__main__":
    main()
