#!/usr/bin/env python3
"""Local stdio protocol fixture; never contacts a model or executes shell input."""

import json
import os
import sys
import time
import uuid
from pathlib import Path


def send(message):
    print(json.dumps(message, ensure_ascii=False), flush=True)


def receive(method):
    message = json.loads(sys.stdin.readline())
    assert message["method"] == method
    return message


if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.166.0-fixture")
    raise SystemExit(0)
assert sys.argv[1:] == ["app-server", "--listen", "stdio://"]
init = receive("initialize")
assert init["params"]["capabilities"]["experimentalApi"] is False
send({"id": init["id"], "result": {"userAgent": "local-fixture"}})
receive("initialized")
thread = json.loads(sys.stdin.readline())
if thread["method"] == "model/list":
    assert thread["params"] == {"limit": 100, "includeHidden": False}
    send({"id": thread["id"], "result": {"data": [
        {"id": "internal-id", "model": "fixture-coder", "displayName": "本地编码模型"},
        {"id": "hidden", "model": "fixture-hidden", "hidden": True},
    ], "nextCursor": "page-two"}})
    page = receive("model/list")
    assert page["params"]["cursor"] == "page-two"
    send({"id": page["id"], "result": {"data": [
        {"model": "fixture-coder", "displayName": "本地编码模型"},
        {"model": "fixture-reviewer", "displayName": "本地验证模型"},
    ], "nextCursor": None}})
    raise SystemExit(0)
if thread["method"] == "config/read":
    assert thread["params"]["cwd"] == str(Path.cwd().resolve())
    if Path("fixture-config-error").exists():
        send({"id":thread["id"],"error":{"code":-32000,"message":"secret-config-error"}})
        raise SystemExit(0)
    config = {
        "model":"fixture-default","model_provider":"fixture-provider",
        "model_providers":{"fixture-provider":{"requires_openai_auth":False,"base_url":"http://127.0.0.1:1/v1","wire_api":"responses","http_headers":{"x-fixture-secret":"private-fixture-config-value"}}},
    }
    if Path("fixture-config-changed").exists():
        config["model_providers"]["fixture-provider"]["http_headers"]["x-fixture-secret"] = "private-changed-value"
    send({"id":thread["id"],"result":{"config":config,"origins":{}}})
    next_line = sys.stdin.readline()
    if not next_line:
        raise SystemExit(0)
    thread = json.loads(next_line)
if thread["method"] == "account/read":
    assert thread["params"] == {"refreshToken":False}
    if Path("fixture-account-error").exists():
        send({"id":thread["id"],"error":{"code":-32000,"message":"private-account-error"}})
        raise SystemExit(0)
    account = {"account":None,"requiresOpenaiAuth":False}
    if Path("fixture-account-chatgpt").exists():
        account = {"account":{"type":"chatgpt","email":"private-user@example.test","planType":"team"},"requiresOpenaiAuth":True}
    send({"id":thread["id"],"result":account})
    next_line = sys.stdin.readline()
    if not next_line:
        raise SystemExit(0)
    thread = json.loads(next_line)
assert thread["method"] in ("thread/start", "thread/resume")
params = thread["params"]
root = Path.cwd().resolve()
mode = params["model"]
state_path = root / "fixture-thread-state.json"
state = json.loads(state_path.read_text()) if state_path.exists() else {}
with (root / "fixture-rpc-trace.jsonl").open("a") as trace:
    trace.write(json.dumps({"method":thread["method"],"params":params}) + "\n")
resumed = thread["method"] == "thread/resume"
if resumed:
    assert params["excludeTurns"] is True
    assert "history" not in params and "path" not in params
    if params["threadId"] not in state:
        send({"id":thread["id"],"error":{"code":-32000,"message":"missing fixture thread"}})
        thread = receive("thread/start")
        params = thread["params"]
        resumed = False
        with (root / "fixture-rpc-trace.jsonl").open("a") as trace:
            trace.write(json.dumps({"method":thread["method"],"params":params}) + "\n")
    else:
        assert state[params["threadId"]]["root"] == str(root)
persistent = resumed or params.get("ephemeral") is False
thread_id = params["threadId"] if resumed else ("fixture-main-" + str(uuid.uuid4()) if persistent else "fixture-thread")
turn_count = len(state.get(thread_id,{}).get("inputs",[])) + 1
turn_id = f"fixture-turn-{turn_count}" if persistent else "fixture-turn"
assert params["cwd"] == str(root)
assert params["sandbox"] == "workspace-write"
assert params["approvalPolicy"] == "never"
if not resumed:
    assert params["ephemeral"] is (not persistent)
assert params["config"]["sandbox_workspace_write"] == {
    "writable_roots": [],
    "network_access": False,
    "exclude_tmpdir_env_var": True,
    "exclude_slash_tmp": True,
}
if mode == "rpc-error":
    send({"id": thread["id"], "error": {"message": "secret-rpc-error"}})
    raise SystemExit(0)
sandbox = {
    "type": "workspaceWrite",
    "writableRoots": [],
    "networkAccess": False,
    "excludeTmpdirEnvVar": True,
    "excludeSlashTmp": True,
}
if mode in ("unsafe", "resume-unsafe"):
    sandbox["writableRoots"] = [str(root.parent)]
thread_result = {
    "thread": {"id": "wrong-thread" if mode == "resume-wrong-id" and resumed else thread_id,
        "ephemeral":not persistent,"status":{"type":"active" if mode == "resume-active" else "idle"}},
    "model":mode,"modelProvider":"fixture-provider",
    "cwd":str(root),"approvalPolicy":"never","sandbox":sandbox,
}
if mode == "model-missing":
    thread_result.pop("model")
if mode == "model-null":
    thread_result["model"] = None
if mode == "model-mismatch":
    thread_result["model"] = "other-model"
if mode == "provider-missing":
    thread_result.pop("modelProvider")
if mode == "provider-null":
    thread_result["modelProvider"] = None
if mode == "provider-mismatch":
    thread_result["modelProvider"] = "other-provider"
send({"id":thread["id"],"result":thread_result})
if mode in ("unsafe", "resume-unsafe"):
    sys.stdin.readline()
    raise SystemExit(0)
next_line = sys.stdin.readline()
if not next_line:
    raise SystemExit(0)
turn = json.loads(next_line)
if turn["method"] == "thread/turns/list":
    assert resumed
    assert turn["params"] == {"threadId":thread_id,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}
    with (root / "fixture-rpc-trace.jsonl").open("a") as trace:
        trace.write(json.dumps({"method":turn["method"],"params":turn["params"]}) + "\n")
    if mode == "resume-list-error":
        send({"id":turn["id"],"error":{"code":-32000,"message":"private-list-error"}})
    else:
        latest = {"id": "external-turn" if mode == "resume-external" else f"fixture-turn-{turn_count-1}",
            "status":"failed" if mode == "resume-failed" else "completed","items":[],"error":None}
        send({"id":turn["id"],"result":{"data":[latest],"nextCursor":None}})
    next_line = sys.stdin.readline()
    if not next_line:
        raise SystemExit(0)
    turn = json.loads(next_line)
assert turn["method"] == "turn/start"
assert turn["params"]["threadId"] == thread_id
if not persistent:
    assert turn["params"]["input"][0]["text"] == "本地测试目标"
else:
    entry = state.setdefault(thread_id,{"root":str(root),"inputs":[]})
    entry["inputs"].append(turn["params"]["input"][0]["text"])
    state_path.write_text(json.dumps(state,ensure_ascii=False))
with (root / "fixture-rpc-trace.jsonl").open("a") as trace:
    trace.write(json.dumps({"method":"turn/start","params":turn["params"]}) + "\n")
if mode == "unknown-start":
    Path("fixture-unknown-write").write_text("accepted once")
    raise SystemExit(0)
assert turn["params"]["sandboxPolicy"] == {
    **sandbox,
    "writableRoots": [str(root)],
}
assert turn["params"]["approvalPolicy"] == "never"
if mode in ("pre-ack", "turn-id-mismatch"):
    send({"method":"turn/started","params":{"threadId":thread_id,"turn":{"id":"mismatch" if mode == "turn-id-mismatch" else turn_id,"status":"inProgress","items":[]}}})
    send({"method":"item/agentMessage/delta","params":{"threadId":thread_id,"turnId":turn_id,"itemId":"answer-1","delta":"测试"}})
send(
    {
        "id": turn["id"],
        "result": {"turn": {"id": turn_id, "status": "inProgress", "items": []}},
    }
)
Path("fixture-started").write_text(str(os.getpid()))
if mode == "hang":
    interrupt = receive("turn/interrupt")
    assert interrupt["params"] == {
        "threadId": thread_id,
        "turnId": turn_id,
    }
    Path("fixture-interrupted").write_text("yes")
    time.sleep(60)
    raise SystemExit(0)
if mode == "eof":
    raise SystemExit(0)
if mode == "malformed":
    print("invalid-json", flush=True)
    time.sleep(60)
    raise SystemExit(0)
if mode == "oversize":
    print("x" * (256 * 1024 + 1), flush=True)
    time.sleep(60)
    raise SystemExit(0)
if mode == "approval":
    send(
        {
            "id": "approval-42",
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": thread_id, "turnId": turn_id},
        }
    )
    response = json.loads(sys.stdin.readline())
    assert response == {"id": "approval-42", "result": {"decision": "cancel"}}
    Path("fixture-denied").write_text("yes")
    time.sleep(60)
    raise SystemExit(0)


def event(method, data):
    send(
        {
            "method": method,
            "params": {"threadId": thread_id, "turnId": turn_id, **data},
        }
    )


# Final messages from other turns/threads must not leak into this run.
send(
    {
        "method": "item/completed",
        "params": {
            "threadId": "other-thread",
            "turnId": turn_id,
            "item": {"type": "agentMessage", "text": "wrong-thread"},
        },
    }
)
event("item/started", {"item": {"type": "commandExecution", "id":"command-1"}})
command = {
    "type": "commandExecution",
    "id": "command-1",
    "command": "/bin/zsh -lc 'python3 check.py'",
    "cwd": str(root),
    "commandActions": [],
    "status": "failed" if mode == "evidence-failure" else "completed",
    "exitCode": 1 if mode == "evidence-failure" else 0,
    "aggregatedOutput": "测试失败" if mode == "evidence-failure" else "测试通过",
}
if mode == "config-secret":
    command["aggregatedOutput"] = "private-fixture-config-value"
if mode == "config-secret-boundary":
    secret = "private-fixture-config-value"
    command["command"] = "fixture-check"
    command["aggregatedOutput"] = "x" * (16384 - (len(secret) - 1)) + secret
if mode == "evidence-large":
    command["aggregatedOutput"] = "证据" * 10000
if mode == "evidence-wrong-turn":
    send(
        {
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "turnId": "other-turn",
                "item": command,
            },
        }
    )
elif mode == "evidence-many":
    for number in range(70):
        event(
            "item/completed",
            {
                "item": {
                    **command,
                    "id": f"command-{number}",
                    "status": "failed" if number == 64 else "completed",
                    "exitCode": 1 if number == 64 else 0,
                    "aggregatedOutput": "第 65 条命令失败" if number == 64 else "测试通过",
                }
            },
        )
elif mode != "evidence-text":
    event("item/completed", {"item": command})
    # A repeated lifecycle event must not duplicate persisted evidence.
    event("item/completed", {"item": command})
if persistent or mode in ("stream-events","pre-ack","stream-secret","config-secret"):
    if mode != "pre-ack":
        event("item/agentMessage/delta", {"itemId":"answer-1","delta":"fixture-" if mode == "stream-secret" else "private-fixture-" if mode == "config-secret" else "测试"})
    event("item/agentMessage/delta", {"itemId":"answer-1","delta":"stream-secret" if mode == "stream-secret" else "config-value" if mode == "config-secret" else "执行结果"})
    event("thread/tokenUsage/updated", {"tokenUsage":{"total":{"inputTokens":10*turn_count,"outputTokens":5*turn_count},"last":{"inputTokens":10,"outputTokens":5}}})
    event("thread/tokenUsage/updated", {"tokenUsage":{"total":{"inputTokens":10*turn_count,"outputTokens":5*turn_count},"last":{"inputTokens":10,"outputTokens":5}}})
    event("item/started", {"item":{"type":"contextCompaction","id":"compact-1"}})
    event("item/completed", {"item":{"type":"contextCompaction","id":"compact-1"}})
event(
    "item/completed",
    {
        "item": {
            "type": "agentMessage",
            "id":"answer-1",
            "phase": "final_answer",
            "text": (
                '{"command":"python3 check.py","exitCode":0,"aggregatedOutput":"通过"}'
                if mode == "evidence-text"
                else "fixture-stream-secret" if mode == "stream-secret" else "private-fixture-config-value" if mode == "config-secret" else "测试执行结果"
            ),
        }
    },
)
event(
    "item/completed",
    {"item": {"type": "agentMessage", "phase": "commentary", "text": "不能覆盖最终结果"}},
)
if mode == "success":
    Path("fixture-result.txt").write_text("真实本地夹具写入", encoding="utf-8")
event(
    "turn/completed",
    {
        "turn": {
            "id": turn_id,
            "status": "failed" if mode in ("failure", "evidence-failure") else "completed",
            "items": [],
            "error": {"message": "secret-provider-error"} if mode == "failure" else None,
        }
    },
)
time.sleep(60)
