#!/usr/bin/env python3
"""Real daemon + Native + bounded MCP subprocess; exclusively local fixtures."""
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import json
import os
import sqlite3
import sys
import tempfile
import threading
import urllib.error
import uuid
from smoke import Daemon, ROOT, wait_for

SECRET = "fixture-managed-capability-key"
TERMINAL = {"completed", "failed", "cancelled"}

class Model(BaseHTTPRequestHandler):
    errors = []
    bodies = []
    def log_message(self, *_args): pass
    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            self.bodies.append(body)
            goal = next(message["content"].split("CAP_CASE:", 1)[1].splitlines()[0]
                        for message in reversed(body["messages"])
                        if message["role"] == "user" and "CAP_CASE:" in message["content"])
            outputs = [item for item in body["messages"] if item["role"] == "tool"]
            managed = [item["function"] for item in body["tools"] if item["function"]["name"].startswith("mcp_")]
            if goal == "skill":
                assert "SKILL_FIXTURE_FIXED_V1" in json.dumps(body)
                assert "SKILL_FIXTURE_FIXED_V2" not in json.dumps(body)
                assert not managed
            if managed and not outputs:
                args = {"text": SECRET if goal == "secret" else "actual-mcp-result"}
                if goal == "schema": args = {"text": 123}
                message = {"role":"assistant", "content":None,"tool_calls":[{"id":"fixture-mcp-call","type":"function",
                    "function":{"name":managed[0]["name"],"arguments":json.dumps(args)}}]}
                reason = "tool_calls"
            else:
                if outputs:
                    output = json.loads(outputs[-1]["content"])
                    assert SECRET not in json.dumps(output), output
                    if goal != "secret": assert "actual-mcp-result" in json.dumps(output), output
                message = {"role":"assistant","content":"本地 MCP / Skills 夹具验证已结束。"}
                reason = "stop"
            data = json.dumps({"choices":[{"message":message,"finish_reason":reason}]}).encode()
            self.send_response(200); self.send_header("Content-Type","application/json")
            self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError): pass
        except Exception as error:
            self.errors.append(repr(error)); self.send_error(500)

def rejected(call):
    try: call()
    except urllib.error.HTTPError as error:
        assert error.code in (400,409),error.code
        return
    raise AssertionError("Invalid capability operation was accepted")

def main():
    server=ThreadingHTTPServer(("127.0.0.1",0),Model)
    server.daemon_threads=True
    threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="self-agent-capabilities-") as temporary:
            root=Path(temporary).resolve(); daemon=Daemon(root,{"HARNESS_API_KEY":SECRET,"MCP_HOST_CANARY":"must-not-inherit"})
            def registry(): return daemon.request("GET","/api/v1/capabilities")
            def manage(command, identifier=None):
                return daemon.request("POST","/api/v1/capabilities/commands",{"id":identifier or str(uuid.uuid4()),"command":command})
            def item(kind, identifier): return next(entry for entry in registry()[kind] if entry["id"]==identifier)
            def current(identifier): return next(entry for entry in daemon.snapshot()["tasks"] if entry["id"]==identifier)
            def terminal(identifier):
                value=current(identifier)
                return value if value["status"] in TERMINAL else None
            def enabled(kind, identifier, value):
                plural="mcp_servers" if kind=="mcp" else "skills"
                return manage({"type":"set_capability_enabled","kind":kind,"id":identifier,"expected_revision":item(plural,identifier)["revision"],"enabled":value})
            def configure(ids, skills=None, mode="allow", budget=32):
                return daemon.request("PUT","/api/v1/settings",{"execution_controls":{"permissions":{"managed_tools":mode},"budget":{"max_external_calls":budget},"capabilities":{"mcp_servers":ids,"skills":skills or []}}})
            sequence=0
            def run(goal, before=None):
                nonlocal sequence
                sequence+=1; project=root/f"project-{sequence}";project.mkdir()
                if before: before(project)
                session=daemon.command({"type":"create_session","title":goal,"project_path":str(project)})["session_id"]
                receipt=daemon.command({"type":"run_agent","session_id":session,"content":"CAP_CASE:"+goal})
                return receipt["task_id"],project
            def save_server(mode="success",protocol="modern2026"):
                receipt=manage({"type":"save_mcp_server","definition":{"name":"本地夹具 "+mode,"program":str(Path(sys.executable).resolve()),"args":["-I","-X","utf8",str(ROOT/"scripts"/"mcp_fixture.py"),mode],"protocol":protocol,"enabled":False,"credential_refs":{}}})
                identifier=receipt["capability_id"]
                probe=manage({"type":"probe_mcp_server","id":identifier,"expected_revision":receipt["revision"],"confirm_start":True})
                result=wait_for(lambda:(entry if (entry:=item("mcp_servers",identifier))["status"]!="checking" else None),description="MCP discovery")
                return identifier,result,probe
            try:
                daemon.start()
                daemon.request("PUT","/api/v1/settings",{"base_url":f"http://127.0.0.1:{server.server_port}/v1","model":"fixture","provider_source":"manual","main_executor":"native","execution_mode":"native"})
                assert registry()=={"mcp_servers":[],"skills":[],"credential_operations":[]}
                creation={"type":"save_skill","name":"固定技能","content":"SKILL_FIXTURE_FIXED_V1"}
                command_id=str(uuid.uuid4()); skill=manage(creation,command_id)
                assert manage(creation,command_id)==skill
                rejected(lambda:manage({**creation,"content":"different"},command_id))
                sid=skill["capability_id"];enabled("skill",sid,True)
                configure([], [sid]); identifier,project=run("skill")
                result=wait_for(lambda:terminal(identifier),description="skill task")
                assert result["status"]=="completed",result
                assert result["managed_capabilities"]["skills"][0]["id"]==sid
                assert "SKILL_FIXTURE_FIXED_V1" not in json.dumps(daemon.snapshot())
                print("PASS skill context is actually used, metadata-only snapshot and idempotent owner commands")
                rejected(lambda:manage({"type":"save_skill","name":"secret","content":SECRET}))
                mid,status,_=save_server()
                assert status["status"]=="ready",status
                enabled("mcp",mid,True);configure([mid])
                for goal in ("success","secret"):
                    identifier,project=run(goal);result=wait_for(lambda:terminal(identifier),description="MCP tool task")
                    assert result["status"]=="completed",result
                    assert (project/"mcp-calls").read_text()=="1"
                    assert result["capability_calls"][0]["status"]=="completed",result
                    assert result["budget_usage"]["external_calls"]==1
                    assert SECRET not in json.dumps(daemon.snapshot())
                print("PASS actual opaque-alias MCP calls, output redaction and single external-call accounting")
                configure([mid],mode="deny"); identifier,project=run("denied")
                result=wait_for(lambda:terminal(identifier),description="denied MCP")
                assert result["status"]=="failed" and not (project/"mcp-pid").exists(),result
                configure([mid],mode="allow",budget=0);identifier,project=run("budget")
                result=wait_for(lambda:terminal(identifier),description="MCP budget")
                assert result["stop_reason"]=="external_call_budget_exhausted" and not (project/"mcp-pid").exists(),result
                print("PASS denied and exhausted external budgets never start a process")
                configure([mid],mode="ask");identifier,project=run("approval")
                pending=wait_for(lambda:next((entry for entry in daemon.snapshot()["execution_requests"] if entry["task_id"]==identifier and entry["status"]=="pending"),None),description="MCP approval")
                assert not (project/"mcp-pid").exists()
                assert "actual-mcp-result" in json.dumps(pending)
                daemon.command({"type":"decide_request","request_id":pending["id"],"expected_revision":pending["revision"],"approve":True,"action_digest":pending["action_digest"]})
                assert wait_for(lambda:terminal(identifier))["status"]=="completed"
                configure([mid],mode="ask");identifier,project=run("revoke")
                wait_for(lambda:next((entry for entry in daemon.snapshot()["execution_requests"] if entry["task_id"]==identifier and entry["status"]=="pending"),None))
                enabled("mcp",mid,False)
                assert wait_for(lambda:terminal(identifier))["status"]=="failed" and not (project/"mcp-pid").exists()
                print("PASS exact-call approval and live revoke prevent unauthorized process starts")
                gate,status,_=save_server("discovery-gate");assert status["status"]=="ready";enabled("mcp",gate,True);configure([gate])
                identifier,project=run("tighten",lambda project:(project/"pause-discovery").write_text("pause"))
                wait_for(lambda:(project/"discovery-waiting").exists(),description="stalled discovery")
                configure([gate],mode="ask")
                (project/"pause-discovery").unlink()
                result=wait_for(lambda:terminal(identifier),description="tightened MCP permission")
                assert result["status"]=="failed" and result["stop_reason"]=="permission_revoked",result
                assert not (project/"mcp-call-received").exists()
                reverse,status,_=save_server("reverse-catalog");assert status["status"]=="ready";enabled("mcp",reverse,True);configure([reverse])
                identifier,project=run("reordered")
                assert wait_for(lambda:terminal(identifier))["status"]=="completed"
                assert (project/"mcp-calls").read_text()=="1"
                print("PASS permission tightening during discovery blocks call; catalog ordering is canonical")
                eof,status,_=save_server("eof");assert status["status"]=="ready";enabled("mcp",eof,True);configure([eof])
                identifier,project=run("unknown");result=wait_for(lambda:terminal(identifier),description="unknown MCP outcome")
                assert result["status"]=="failed" and result["capability_calls"][0]["status"]=="unknown",result
                assert (project/"mcp-calls").read_text()=="1"
                daemon.stop();daemon.start()
                assert current(identifier)["capability_calls"][0]["status"]=="unknown"
                assert (project/"mcp-calls").read_text()=="1"
                print("PASS EOF after tool receipt remains unknown across restart without replay")
                # Simulate persisted pending cleanup at an interrupted daemon boundary.
                # This fixture edits only its isolated database while daemon is stopped.
                original=current(identifier)
                daemon.stop()
                # Connection context managers commit/rollback but do not close;
                # release the fixture handle before restarting/removing on Windows.
                with closing(sqlite3.connect(root/"harness.sqlite")) as db, db:
                    saved=json.loads(db.execute("SELECT body FROM tasks WHERE id=?",(identifier,)).fetchone()[0])
                    saved["execution_cleanup"]="pending"
                    db.execute("UPDATE tasks SET body=? WHERE id=?",(json.dumps(saved),identifier))
                daemon.start()
                quarantined=current(identifier)
                assert quarantined["execution_cleanup"]=="unknown"
                session=quarantined["session_id"]
                blocked=daemon.command({"type":"run_agent","session_id":session,"content":"CAP_CASE:blocked"})["task_id"]
                assert wait_for(lambda:terminal(blocked))["stop_reason"]=="workspace_cleanup_unknown"
                resolution={"expected_updated_at":quarantined["execution_cleanup_updated_at"],"confirm_processes_stopped":True}
                endpoint=f"/api/v1/tasks/{identifier}/cleanup-resolution"
                rejected(lambda:daemon.request("POST",endpoint,{**resolution,"confirm_processes_stopped":False}))
                rejected(lambda:daemon.request("POST",endpoint,{**resolution,"expected_updated_at":"stale"}))
                assert daemon.request("POST",endpoint,resolution)["resolved"]
                assert daemon.request("POST",endpoint,resolution)["resolved"]
                assert current(identifier)["status"]==original["status"]
                assert current(identifier)["capability_calls"][0]["status"]=="unknown"
                print("PASS uncertain cleanup quarantines project until explicit conditional owner attestation")
                assert not Model.errors,Model.errors
            finally: daemon.stop()
    finally: server.shutdown();server.server_close()
if __name__=="__main__":main()
