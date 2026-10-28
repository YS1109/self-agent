import { afterEach, describe, expect, it, vi } from "vitest";
import { api, dispatch, capabilityClient, providersClient } from "./api";
import { cleanupResolutionBody, resolveTaskCleanup } from "./cleanup-resolution";
import { DESKTOP_SOURCE, REMOTE_SOURCE, sameSource } from "./types";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("private daemon transport", () => {
  it("uses same-origin browser API without providing a daemon credential", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi
      .fn()
      .mockResolvedValue({ ok: true, json: async () => ({ sessions: [] }) });
    vi.stubGlobal("fetch", fetcher);
    await expect(api("GET", "/snapshot")).resolves.toEqual({ sessions: [] });
    expect(fetcher).toHaveBeenCalledWith(
      "/api/v1/snapshot",
      expect.objectContaining({ method: "GET", headers: undefined }),
    );
    expect(JSON.stringify(fetcher.mock.calls)).not.toMatch(
      /bearer|authorization/i,
    );
  });

  it("forwards native commands through Rust without fetching directly", async () => {
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    invoke.mockResolvedValue({ status: "accepted" });
    await api("POST", "/commands", { command_id: "one" });
    expect(invoke).toHaveBeenCalledWith("api_request", {
      method: "POST",
      path: "/api/v1/commands",
      body: { command_id: "one" },
    });
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("shows daemon validation errors instead of reporting a successful command", async () => {
    vi.stubGlobal("window", {});
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue({
          ok: false,
          status: 400,
          json: async () => ({ error: "No selected session" }),
        }),
    );
    await expect(api("POST", "/commands", {})).rejects.toThrow(
      "No selected session",
    );
  });

  it("preserves exact remote source and omits an implicit desktop session", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi
      .fn()
      .mockResolvedValue({
        ok: true,
        json: async () => ({ status: "accepted" }),
      });
    vi.stubGlobal("fetch", fetcher);
    await dispatch(REMOTE_SOURCE, { type: "start_task", title: "Build" });
    const envelope = JSON.parse(fetcher.mock.calls[0][1].body);
    expect(envelope.source).toEqual(REMOTE_SOURCE);
    expect(envelope.command).toEqual({ type: "start_task", title: "Build" });
    expect(envelope.command_id).toMatch(/^[a-f0-9-]{36}$/);
    expect(sameSource(REMOTE_SOURCE, DESKTOP_SOURCE)).toBe(false);
    expect(
      sameSource(REMOTE_SOURCE, {
        ...REMOTE_SOURCE,
        conversation_id: "remote-2",
      }),
    ).toBe(false);
  });

  it("桌面主智能体指令保持独立会话，远程编排协议继续可用", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ status: "accepted", task_id: "main-task" }) });
    vi.stubGlobal("fetch", fetcher);
    await dispatch(DESKTOP_SOURCE, { type: "run_agent", session_id: "session-a", content: "继续修复并测试" });
    await dispatch(REMOTE_SOURCE, { type: "send_message", session_id: "session-b", content: "规划远程任务" });
    const first = JSON.parse(fetcher.mock.calls[0][1].body);
    const second = JSON.parse(fetcher.mock.calls[1][1].body);
    expect(first.source).toEqual(DESKTOP_SOURCE);
    expect(first.command).toEqual({ type: "run_agent", session_id: "session-a", content: "继续修复并测试" });
    expect(second.source).toEqual(REMOTE_SOURCE);
    expect(second.command.type).toBe("send_message");
  });
  it("补充与继续保留显式目标和幂等信封，未知网络结果不重送 POST", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi.fn().mockResolvedValue({ok:true,json:async()=>({status:"accepted",task_id:"t",supplement_id:"s"})});
    vi.stubGlobal("fetch",fetcher);
    await dispatch(DESKTOP_SOURCE,{type:"steer_task",task_id:"original",content:"补充"});
    const envelope=JSON.parse(fetcher.mock.calls[0][1].body);
    expect(envelope.command).toEqual({type:"steer_task",task_id:"original",content:"补充"});
    expect(envelope.command_id).toMatch(/^[a-f0-9-]{36}$/);
    fetcher.mockRejectedValue(new Error("network lost"));
    await expect(dispatch(DESKTOP_SOURCE,{type:"continue_task",task_id:"terminal"})).rejects.toThrow("network lost");
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(JSON.parse(fetcher.mock.calls[1][1].body).command).toEqual({type:"continue_task",task_id:"terminal"});
  });
  it("只读改动报告通过私有 Rust 桥且不向前端注入凭据", async () => {
    vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});
    invoke.mockResolvedValue({state:"expired"});
    await expect(api("GET","/tasks/task-1/changes")).resolves.toEqual({state:"expired"});
    expect(invoke).toHaveBeenCalledWith("api_request",{method:"GET",path:"/api/v1/tasks/task-1/changes",body:null});
  });

  it("执行控制和逐次授权信封保留完整绑定，传输失败不重试决定", async () => {
    vi.stubGlobal("window",{});
    const fetcher=vi.fn().mockResolvedValue({ok:true,json:async()=>({status:"accepted",task_id:"t"})});
    vi.stubGlobal("fetch",fetcher);
    await dispatch(DESKTOP_SOURCE,{type:"run_agent",session_id:"s",content:"目标",controls:{budget:{max_runtime_seconds:30},permissions:{run_commands:"ask"}}});
    expect(JSON.parse(fetcher.mock.calls[0][1].body).command.controls).toEqual({budget:{max_runtime_seconds:30},permissions:{run_commands:"ask"}});
    fetcher.mockRejectedValue(new Error("connection lost"));
    await expect(dispatch(DESKTOP_SOURCE,{type:"decide_request",request_id:"r",expected_revision:2,approve:true,action_digest:"a".repeat(64)})).rejects.toThrow("connection lost");
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(JSON.parse(fetcher.mock.calls[1][1].body).command).toEqual({type:"decide_request",request_id:"r",expected_revision:2,approve:true,action_digest:"a".repeat(64)});
  });

  it("能力管理只走本机专用端点并保留幂等信封，未知POST不重发", async () => {
    vi.stubGlobal("window",{});
    const fetcher=vi.fn().mockResolvedValue({ok:true,json:async()=>({mcp_servers:[],skills:[]})});vi.stubGlobal("fetch",fetcher);
    const observer=new AbortController();await capabilityClient.getRegistry(observer.signal);
    expect(fetcher.mock.calls[0][0]).toBe("/api/v1/capabilities");
    const envelope={id:"fixed-id",command:{type:"save_skill" as const,name:"fixture",content:"text"}};
    fetcher.mockRejectedValue(new Error("lost"));await expect(capabilityClient.command(envelope,observer.signal)).rejects.toThrow("lost");
    expect(fetcher).toHaveBeenCalledTimes(2);expect(fetcher.mock.calls[1][0]).toBe("/api/v1/capabilities/commands");expect(JSON.parse(fetcher.mock.calls[1][1].body)).toEqual(envelope);
    expect(JSON.stringify(fetcher.mock.calls)).not.toMatch(/bearer|authorization/i);
  });
  it("关闭桌面观察不能撤回已提交的命令，晚到结果被丢弃", async () => {
    vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});let finish!:(value:unknown)=>void;
    invoke.mockImplementation(()=>new Promise(resolve=>{finish=resolve;}));const observer=new AbortController();
    const pending=api("POST","/capabilities/commands",{id:"one",command:{type:"save_skill"}},observer.signal);
    await vi.waitFor(()=>expect(invoke).toHaveBeenCalledTimes(1));observer.abort();finish({status:"completed"});await expect(pending).rejects.toThrow();
    expect(invoke).toHaveBeenCalledTimes(1);
    await expect(api("GET","/capabilities",undefined,observer.signal)).rejects.toThrow();expect(invoke).toHaveBeenCalledTimes(1);
  });
  it("解除隔离只允许确切终态未知记录与显式确认，传输不会重发", async () => {
    const task={id:"t",session_id:"s",title:"任务",status:"failed",execution_mode:"native" as const,source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[],execution_cleanup:"unknown" as const,execution_cleanup_updated_at:"stamp"};
    expect(()=>cleanupResolutionBody(task,false)).toThrow();expect(()=>cleanupResolutionBody({...task,status:"running"},true)).toThrow();
    expect(()=>cleanupResolutionBody({...task,execution_cleanup:"pending"},true)).toThrow();expect(()=>cleanupResolutionBody({...task,id:"../t"},true)).toThrow();
    vi.stubGlobal("window",{});const fetcher=vi.fn().mockResolvedValue({ok:true,json:async()=>({task_id:"t",resolved:true})});vi.stubGlobal("fetch",fetcher);
    await expect(resolveTaskCleanup(task,true)).resolves.toEqual({task_id:"t",resolved:true});
    expect(fetcher.mock.calls[0][0]).toBe("/api/v1/tasks/t/cleanup-resolution");expect(JSON.parse(fetcher.mock.calls[0][1].body)).toEqual({expected_updated_at:"stamp",confirm_processes_stopped:true});
    fetcher.mockRejectedValue(new Error("lost"));await expect(resolveTaskCleanup(task,true)).rejects.toThrow("lost");expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it("命名凭据只放专用POST body，经Rust桥不回填前端，未知结果不重发",async()=>{
    vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});const id="00000000-0000-0000-0000-000000000001",operation="00000000-0000-0000-0000-000000000002";
    invoke.mockResolvedValue({operation:{id:operation,status:"pending"},replayed:false,no_secret_applied:false});
    await providersClient.setCredential(id,{id:operation,expected_revision:2,secret:"fixture-secret"},new AbortController().signal);
    expect(invoke).toHaveBeenCalledWith("api_request",{method:"POST",path:`/api/v1/providers/${id}/credential`,body:{id:operation,expected_revision:2,secret:"fixture-secret"}});
    expect(invoke.mock.calls[0][1].path).not.toContain("secret");invoke.mockRejectedValue(new Error("lost"));
    await expect(providersClient.deleteCredential(id,{id:operation,expected_revision:2},new AbortController().signal)).rejects.toThrow("lost");expect(invoke).toHaveBeenCalledTimes(2);
    expect(()=>providersClient.setCredential("../x",{id:operation,expected_revision:2,secret:"fixture-secret"},new AbortController().signal)).toThrow();expect(invoke).toHaveBeenCalledTimes(2);
  });

});
