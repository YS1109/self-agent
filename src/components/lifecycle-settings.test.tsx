// @vitest-environment jsdom
import {act} from "react";
import {createRoot} from "react-dom/client";
import {renderToStaticMarkup} from "react-dom/server";
import {describe,it,expect,vi,beforeAll,afterAll} from "vitest";
beforeAll(()=>Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true}));
afterAll(()=>Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:false}));
import {LifecycleSettings} from "./lifecycle-settings";
import type {LifecycleClient,LifecycleRecovery} from "../lifecycle-client";
const ID="2c751aff-1bfe-44fa-9e0d-b7e17806a7b0";
const running={instance_id:ID,state:"running",phase:"not_requested",outcome:null,workers:{state:"running",accepting_work:true,active_admissions:0,pending_workers:0,settled_workers:0,unknown_workers:0,unknown_admissions:0}};
function memory():LifecycleRecovery{let value:string|null=null;return {read:()=>value,save:v=>{value=v;},clear:()=>{value=null;}};}
describe("owner lifecycle settings",()=>{
 it("states global impact and does not claim offline means stopped",()=>{const c:LifecycleClient={status:async()=>running,shutdown:async()=>running};const text=renderToStaticMarkup(<LifecycleSettings client={c} recovery={memory()}/>);expect(text).toContain("QQ、H5");expect(text).toContain("关闭桌面窗口本身不会停止后台");expect(text).toContain("只发送一次关闭请求");expect(text).not.toContain("green");});
 it("explicit confirmation is required and unmount does not repeat accepted shutdown",async()=>{
  const c:LifecycleClient={status:vi.fn(async()=>running),shutdown:vi.fn(()=>new Promise(()=>{}))};const recovery=memory();const node=document.createElement("div"),root=createRoot(node);
  await act(async()=>root.render(<LifecycleSettings client={c} recovery={recovery}/>));
  const stop=()=>Array.from(node.querySelectorAll("button")).find(b=>b.textContent==="只发送一次关闭请求")!;
  expect(stop().disabled).toBe(true);await act(async()=>node.querySelector<HTMLInputElement>("input[type=checkbox]")!.click());expect(stop().disabled).toBe(false);await act(async()=>stop().click());expect(c.shutdown).toHaveBeenCalledTimes(1);expect(recovery.read()).toBe(ID);
  await act(async()=>root.unmount());expect(vi.mocked(c.shutdown).mock.calls[0][1].aborted).toBe(true);
  const next=createRoot(node);await act(async()=>next.render(<LifecycleSettings client={c} recovery={recovery}/>));expect(node.textContent).toContain("固定观察实例");expect(c.shutdown).toHaveBeenCalledTimes(1);await act(async()=>next.unmount());
 });
});
