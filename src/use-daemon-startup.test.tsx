// @vitest-environment jsdom
import {act} from "react";
import {createRoot} from "react-dom/client";
import {afterEach,describe,it,expect,vi} from "vitest";
const events=vi.hoisted(()=>({listen:vi.fn(),invoke:vi.fn()}));
vi.mock("@tauri-apps/api/event",()=>({listen:events.listen}));
vi.mock("@tauri-apps/api/core",()=>({invoke:events.invoke}));
import {useDaemonStartup} from "./use-daemon-startup";
function View(){const state=useDaemonStartup();return <output data-phase={state.phase}>{state.message}</output>;}
afterEach(()=>{delete (window as unknown as Record<string,unknown>).__TAURI_INTERNALS__;events.listen.mockReset();events.invoke.mockReset();});
describe("native startup observation lifecycle",()=>{
 it("shows progress and actionable timeout, then clears on late readiness",async()=>{
  Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});Object.assign(window,{__TAURI_INTERNALS__:{}});
  let receive!:(event:{payload:unknown})=>void;const stop=vi.fn();events.listen.mockImplementation(async(_name,callback)=>{receive=callback;return stop;});
  const node=document.createElement("div"),root=createRoot(node);
  await act(async()=>{root.render(<View/>);});
  expect(events.listen).toHaveBeenCalledTimes(1);
  await act(async()=>receive({payload:{revision:1,status:"starting",message:"正在加载7秒，不重复启动"}}));
  expect(node.textContent).toContain("7秒");expect(node.querySelector("output")?.dataset.phase).toBe("starting");
  await act(async()=>receive({payload:{revision:2,status:"startup_error",message:"等待超过30秒，可只读刷新；未重新启动"}}));
  expect(node.textContent).toContain("未重新启动");expect(node.querySelector("output")?.dataset.phase).toBe("error");
  await act(async()=>receive({payload:{revision:3,status:"connected"}}));expect(node.textContent).toBe("");expect(node.querySelector("output")?.dataset.phase).toBe("idle");
  await act(async()=>root.unmount());expect(stop).toHaveBeenCalledTimes(1);
  await act(async()=>receive({payload:{revision:4,status:"starting",message:"late"}}));expect(node.childNodes).toHaveLength(0);
 });
 it("recovers an early failure and ignores a delayed snapshot behind newer events",async()=>{
  Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});Object.assign(window,{__TAURI_INTERNALS__:{}});
  let receive!:(event:{payload:unknown})=>void;events.listen.mockImplementation(async(_name,callback)=>{receive=callback;return vi.fn();});
  events.invoke.mockResolvedValueOnce({revision:1,status:"startup_error",message:"协议不兼容，未替换后台"});
  const node=document.createElement("div"),root=createRoot(node);await act(async()=>root.render(<View/>));expect(node.textContent).toContain("协议不兼容");
  await act(async()=>receive({payload:{revision:2,status:"connected"}}));expect(node.textContent).toBe("");
  await act(async()=>receive({payload:{revision:1,status:"startup_error",message:"旧错误"}}));expect(node.textContent).toBe("");await act(async()=>root.unmount());
  let resolve!:(value:unknown)=>void;events.invoke.mockReturnValueOnce(new Promise(r=>resolve=r));const node2=document.createElement("div"),root2=createRoot(node2);await act(async()=>root2.render(<View/>));
  await act(async()=>receive({payload:{revision:4,status:"connected"}}));await act(async()=>resolve({revision:3,status:"startup_error",message:"旧快照"}));expect(node2.textContent).toBe("");await act(async()=>root2.unmount());
 });
 it("unmount before native subscription settles releases its late handle",async()=>{
  Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});Object.assign(window,{__TAURI_INTERNALS__:{}});
  let resolve!:(value:()=>void)=>void;const pending=new Promise<()=>void>(r=>resolve=r);events.listen.mockReturnValue(pending);
  const node=document.createElement("div"),root=createRoot(node);await act(async()=>root.render(<View/>));await act(async()=>root.unmount());
  const stop=vi.fn();await act(async()=>resolve(stop));expect(stop).toHaveBeenCalledTimes(1);
 });
});
