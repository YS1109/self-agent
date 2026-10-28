#!/usr/bin/env python3
"""Offline adversarial MCP stdio fixture: no models, HTTP, auth or shell calls."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

mode = sys.argv[1] if len(sys.argv) > 1 else "success"
trace = Path.cwd() / "mcp-trace.jsonl"
modern = None
calls = 0
children = []


def send(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def stop(_signum=None, _frame=None):
    for child in children:
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=1)
    raise SystemExit(0)


signal.signal(signal.SIGTERM, stop)
Path("mcp-pid").write_text(str(os.getpid()))
if mode == "stdin-stall":
    time.sleep(60)
if mode == "descendant":
    child = subprocess.Popen([sys.executable, "-I", "-c", "import time; time.sleep(60)"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    children.append(child)
    Path("mcp-child-pid").write_text(str(child.pid))

for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    with trace.open("a") as output:
        output.write(json.dumps({"method": method, "id": request.get("id")}) + "\n")
    if method == "notifications/initialized":
        assert modern is False
        Path("mcp-initialized").write_text("yes")
        continue
    params = request.get("params", {})
    if modern is None:
        assert method in ("server/discover", "initialize")
        modern = method == "server/discover"
    if modern:
        assert method != "initialize"
        meta = params["_meta"]
        assert meta["io.modelcontextprotocol/protocolVersion"] == "2026-07-28"
        assert meta["io.modelcontextprotocol/clientCapabilities"] == {}
        assert meta["io.modelcontextprotocol/clientInfo"]["name"] == "self_agent"
        assert "io.modelcontextprotocol/logLevel" not in meta
    else:
        assert "_meta" not in params
        if method != "initialize":
            assert Path("mcp-initialized").exists()
    base = {"resultType": "complete"} if modern else {}
    if method in ("server/discover", "initialize"):
        if mode == "discovery-gate" and Path("pause-discovery").exists():
            Path("discovery-waiting").write_text("yes")
            while Path("pause-discovery").exists():
                time.sleep(0.01)
        if mode == "discover-stall":
            time.sleep(60)
        result = {**base, "capabilities": {"tools": {}}}
        if modern:
            result.update({"supportedVersions": ["2026-07-28"], "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "private-server-info", "version": "fixture"}}})
        else:
            assert params["protocolVersion"] == "2025-11-25"
            assert params["capabilities"] == {}
            result.update({"protocolVersion": "2025-11-25", "serverInfo": {"name": "private-server-info", "version": "fixture"}})
        if mode == "wrong-version":
            result["supportedVersions"] = ["1900-01-01"]
            result["protocolVersion"] = "1900-01-01"
        send({"jsonrpc": "2.0", "id": request["id"], "result": result})
        continue
    if method == "tools/list":
        number = int(params.get("cursor", "0"))
        name = f"echo_{number}" if mode in ("too-many-pages", "repeat-cursor") else "echo"
        tool = {"name": name, "description": "本地模拟工具", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}, "annotations": {"readOnlyHint": True}}
        if mode == "schema-large":
            tool["inputSchema"]["description"] = "x" * 32768
        tools = [tool]
        if mode == "reverse-catalog":
            tools = [{**tool, "name": "z_tool"}, {**tool, "name": "a_tool"}]
        if mode == "duplicate-tool":
            tools.append(tool)
        if mode == "too-many-tools":
            tools = [{**tool, "name": f"tool_{index}"} for index in range(129)]
        result = {**base, "tools": tools}
        if mode in ("repeat-cursor", "too-many-pages"):
            result["nextCursor"] = "1" if mode == "repeat-cursor" else str(number + 1)
        if mode == "multi-page" and number == 0:
            result["nextCursor"] = "1"
        elif mode == "multi-page":
            result["tools"][0]["name"] = "second"
        send({"jsonrpc": "2.0", "id": request["id"], "result": result})
        if mode == "write-stall":
            time.sleep(60)
        continue
    assert method == "tools/call"
    calls += 1
    Path("mcp-calls").write_text(str(calls))
    Path("mcp-call-received").write_text("yes")
    canary = os.environ.get("MCP_TOKEN", "private-mcp-canary")
    if mode == "eof":
        break
    if mode in ("call-stall", "descendant"):
        time.sleep(60)
    if mode == "malformed":
        print("not-json-" + canary, flush=True)
        time.sleep(60)
    if mode == "oversize":
        print("x" * (1024 * 1024 + 1), flush=True)
        time.sleep(60)
    if mode == "no-newline":
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {**base, "content": []}}))
        sys.stdout.flush()
        time.sleep(60)
    if mode == "stderr-large":
        sys.stderr.write(canary + "x" * 70000)
        sys.stderr.flush()
        time.sleep(60)
    if mode == "notifications":
        for _ in range(257):
            send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"data": canary}})
    if mode == "total-large":
        for _ in range(5):
            send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"data": "x" * 1000000}})
    if mode == "reverse-request":
        send({"jsonrpc": "2.0", "id": "server-request", "method": "sampling/createMessage", "params": {"messages": [canary]}})
        time.sleep(60)
    if mode == "rpc-error":
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": canary}})
        continue
    result = {**base, "content": [{"type": "text", "text": params["arguments"].get("text", "模拟结果")}]}
    if mode == "bad-block-null":
        result["content"] = [None]
    if mode == "bad-block-text":
        result["content"] = [{"type": "text", "text": 17}]
    if mode == "bad-block-resource":
        result["content"] = [{"type": "resource", "resource": {"uri": "file:///must-not-read", "text": 17}}]
    if mode == "valid-media":
        result["content"] = [{"type": "image", "data": "", "mimeType": "image/png"}, {"type": "audio", "data": "", "mimeType": "audio/wav"}, {"type": "resource_link", "uri": "file:///must-not-read", "name": "link"}, {"type": "resource", "resource": {"uri": "file:///must-not-read", "blob": ""}}]
    if mode == "structured-scalar":
        result.pop("content")
        result["structuredContent"] = None
    if mode == "tool-error":
        result["isError"] = True
    if mode == "missing-result-type":
        result.pop("resultType", None)
    if mode == "mrtr":
        result = {"resultType": "input_required", "inputRequests": {"login": {"method": "elicitation/create", "params": {"url": "https://must-not-open.invalid/"}}}, "requestState": canary}
    if mode == "task-result":
        result = {**base, "task": {"taskId": "must-not-poll"}}
    if mode == "argv":
        result["structuredContent"] = {"args": sys.argv[2:]}
    if mode == "env":
        result["structuredContent"] = {"explicit": os.environ.get("MCP_TOKEN"), "host": os.environ.get("MCP_HOST_CANARY"), "home": os.environ.get("HOME"), "proxy": os.environ.get("HTTPS_PROXY"), "codex": os.environ.get("CODEX_HOME")}
    if mode == "credential-json-text":
        result["content"][0]["text"] = json.dumps({canary: canary, "nested": json.dumps({"token": canary})})
        result["structuredContent"] = {"host": os.environ.get("HARNESS_API_KEY"), "home": os.environ.get("HOME")}
    if mode == "secret-logs":
        sys.stderr.write(canary + "\n")
        sys.stderr.flush()
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"data": canary}})
        result["content"][0]["text"] = canary
    response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
    if mode == "wrong-id":
        response["id"] += 1
    if mode == "both-result-error":
        response["error"] = {"code": -1, "message": canary}
    send(response)
stop()
