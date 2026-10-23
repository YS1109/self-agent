#!/usr/bin/env python3
"""Local stdio protocol fixture; never contacts a model or executes shell input."""

import json
import os
import sys
import time
from pathlib import Path


def send(message):
    print(json.dumps(message, ensure_ascii=False), flush=True)


def receive(method):
    message = json.loads(sys.stdin.readline())
    assert message["method"] == method
    return message


assert sys.argv[1:] == ["app-server", "--listen", "stdio://"]
init = receive("initialize")
assert init["params"]["capabilities"]["experimentalApi"] is False
send({"id": init["id"], "result": {"userAgent": "local-fixture"}})
receive("initialized")
thread = receive("thread/start")
params = thread["params"]
root = Path.cwd().resolve()
mode = params["model"]
assert params["cwd"] == str(root)
assert params["sandbox"] == "workspace-write"
assert params["approvalPolicy"] == "never"
assert params["ephemeral"] is True
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
if mode == "unsafe":
    sandbox["writableRoots"] = [str(root.parent)]
send(
    {
        "id": thread["id"],
        "result": {
            "thread": {"id": "fixture-thread"},
            "cwd": str(root),
            "approvalPolicy": "never",
            "sandbox": sandbox,
        },
    }
)
if mode == "unsafe":
    sys.stdin.readline()
    raise SystemExit(0)
turn = receive("turn/start")
assert turn["params"]["threadId"] == "fixture-thread"
assert turn["params"]["input"][0]["text"] == "本地测试目标"
assert turn["params"]["sandboxPolicy"] == {
    **sandbox,
    "writableRoots": [str(root)],
}
assert turn["params"]["approvalPolicy"] == "never"
send(
    {
        "id": turn["id"],
        "result": {"turn": {"id": "fixture-turn", "status": "inProgress", "items": []}},
    }
)
Path("fixture-started").write_text(str(os.getpid()))
if mode == "hang":
    interrupt = receive("turn/interrupt")
    assert interrupt["params"] == {
        "threadId": "fixture-thread",
        "turnId": "fixture-turn",
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
            "params": {"threadId": "fixture-thread", "turnId": "fixture-turn"},
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
            "params": {"threadId": "fixture-thread", "turnId": "fixture-turn", **data},
        }
    )


# Final messages from other turns/threads must not leak into this run.
send(
    {
        "method": "item/completed",
        "params": {
            "threadId": "other-thread",
            "turnId": "fixture-turn",
            "item": {"type": "agentMessage", "text": "wrong-thread"},
        },
    }
)
event("item/started", {"item": {"type": "commandExecution"}})
event(
    "item/completed",
    {"item": {"type": "agentMessage", "phase": "final_answer", "text": "测试执行结果"}},
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
            "id": "fixture-turn",
            "status": "failed" if mode == "failure" else "completed",
            "items": [],
            "error": {"message": "secret-provider-error"} if mode == "failure" else None,
        }
    },
)
time.sleep(60)
