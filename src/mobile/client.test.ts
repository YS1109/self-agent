import {describe,it,expect,vi} from "vitest";
import {createHttpMobileApi,createMobileController,inputError,validCommandReceipt} from "./client";
import type {BrowserSession,EventBatch,GatewayResult,MobileApi,MobileRpc,MobileState,SessionSnapshot} from "./client";
const DEVICE="11111111-1111-4111-8111-111111111111",PRINCIPAL="22222222-2222-4222-8222-222222222222",SESSION="33333333-3333-4333-8333-333333333333",OTHER="44444444-4444-4444-8444-444444444444",COMMAND="55555555-5555-4555-8555-555555555555";
const browser:BrowserSession={csrf:"a".repeat(64),paired:true,device_id:DEVICE,principal_id:PRINCIPAL,binding_revision:1,permissions:{view:true,run:true,cancel:true,answer:true,read_results:true,read_artifacts:true}};
const snapshot=(id=SESSION,cursor=0):SessionSnapshot=>({session:{id,title:id,messages:[]},tasks:[],questions:[],cursor});
function deferred<T>(){let resolve!:(v:T)=>void,reject!:(v?:unknown)=>void;const promise=new Promise<T>((yes,no)=>{resolve=yes;reject=no;});return{promise,resolve,reject};}
function fixture(){const watchers:{session:string;after:number;receive:(r:GatewayResult<EventBatch>)=>void;failed:()=>void;closed:boolean}[]=[];const values=new Map<string,string>();const api:MobileApi={session:vi.fn().mockResolvedValue({code:"ok",data:browser}),pair:vi.fn(),pairStatus:vi.fn(),logout:vi.fn().mockResolvedValue({code:"ok"}),rpc:vi.fn().mockImplementation(async(rpc:MobileRpc)=>rpc.type==="sessions"?{code:"ok",data:{sessions:[{id:SESSION,title:"one"},{id:OTHER,title:"two"}]}}:rpc.type==="session"?{code:"ok",data:snapshot(rpc.session_id)}:{code:"ok",data:{command_id:COMMAND,session_id:SESSION,status:"accepted"}}),watch:(session,after,receive,failed)=>{const item={session,after,receive,failed,closed:false};watchers.push(item);return()=>{item.closed=true;};}};const states:MobileState[]=[];const storage={getItem:(k:string)=>values.get(k)??null,setItem:(k:string,v:string)=>{values.set(k,v);},removeItem:(k:string)=>{values.delete(k);}};const controller=createMobileController(api,s=>states.push(s),{uuid:()=>COMMAND,storage});return{api,watchers,values,states,controller,storage};}
const flush=()=>new Promise(resolve=>setTimeout(resolve,0));
describe("H5 scoped one-shot controller",()=>{
 it.each(["device_offline","busy","network"] as const)("recovers an initial %s session list without a command",async failure=>{
  vi.useFakeTimers();const f=fixture();const base=f.api.rpc;let first=true;
  f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>{
   if(rpc.type==="sessions"&&first){first=false;return failure==="network"?Promise.reject(new Error("network lost")):Promise.resolve({code:failure});}
   return base(rpc,signal);
  });
  try{
   await f.controller.open();expect(f.controller.getState().selected).toBeNull();
   await vi.advanceTimersByTimeAsync(2000);
   expect(f.controller.getState().snapshot?.session.id).toBe(SESSION);
   expect(f.watchers).toHaveLength(1);
   expect(vi.mocked(f.api.rpc).mock.calls.every(c=>c[0].type!=="command")).toBe(true);
  }finally{f.controller.dispose();vi.useRealTimers();}
 });
 it.each(["device_offline","busy","internal"] as const)("recovers %s events through a fresh read without replaying an unknown command",async code=>{
  vi.useFakeTimers();const f=fixture();
  try{
   await f.controller.open();
   const base=f.api.rpc;
   f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>rpc.type==="command"?Promise.reject(new Error("response lost")):rpc.type==="session"?Promise.resolve({code:"ok",data:snapshot(SESSION,7)}):base(rpc,signal));
   await f.controller.submit({type:"run",content:"only once"});
   const old=f.watchers[0];old.receive({code});
   expect(old.closed).toBe(true);
   await vi.advanceTimersByTimeAsync(2000);
   expect(f.controller.getState().snapshot?.cursor).toBe(7);
   expect(f.watchers.at(-1)?.after).toBe(7);
   f.watchers.at(-1)!.receive({code:"ok",data:{cursor:7,events:[]}});
   expect(f.controller.getState().connected).toBe(true);
   expect(f.controller.getState().notice).toContain("结果未知");
   expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);
   expect(vi.mocked(f.api.rpc).mock.calls.filter(c=>c[0].type==="command")).toHaveLength(1);
  }finally{f.controller.dispose();vi.useRealTimers();}
 });
 it("retries a read-only snapshot when the selected device starts offline",async()=>{
  vi.useFakeTimers();const f=fixture();const base=f.api.rpc;let online=false;
  f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>rpc.type==="session"&&!online?Promise.resolve({code:"device_offline"}):base(rpc,signal));
  try{
   await f.controller.open();expect(f.watchers).toHaveLength(0);
   online=true;await vi.advanceTimersByTimeAsync(2000);
   expect(f.controller.getState().snapshot?.session.id).toBe(SESSION);
   expect(f.watchers).toHaveLength(1);
   f.watchers[0].receive({code:"ok",data:{cursor:0,events:[]}});
   expect(f.controller.getState().notice).toBe("");
   expect(vi.mocked(f.api.rpc).mock.calls.every(c=>c[0].type!=="command")).toBe(true);
  }finally{f.controller.dispose();vi.useRealTimers();}
 });
 it("transport recovery refreshes the snapshot and is cancelled on selection changes or disposal",async()=>{
  vi.useFakeTimers();const f=fixture();
  try{
   await f.controller.open();const old=f.watchers[0];old.failed();
   await f.controller.select(OTHER);const count=vi.mocked(f.api.rpc).mock.calls.length;
   old.failed();expect(f.watchers.at(-1)?.closed).toBe(false);
   await vi.advanceTimersByTimeAsync(2000);
   expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(count);
   f.watchers.at(-1)!.failed();
   await vi.advanceTimersByTimeAsync(2000);
   expect(vi.mocked(f.api.rpc).mock.calls.at(-1)?.[0]).toEqual({type:"session",session_id:OTHER});
   expect(f.watchers.at(-1)?.session).toBe(OTHER);
   f.watchers.at(-1)!.failed();f.controller.dispose();
   const finalCount=vi.mocked(f.api.rpc).mock.calls.length;
   await vi.advanceTimersByTimeAsync(4000);
   expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(finalCount);
  }finally{f.controller.dispose();vi.useRealTimers();}
 });
 it.each(["unauthorized","forbidden","invalid_request"] as const)("does not reconnect an event stream rejected with %s",async code=>{
  vi.useFakeTimers();const f=fixture();
  try{
   await f.controller.open();const count=vi.mocked(f.api.rpc).mock.calls.length;
   f.watchers[0].receive({code});await vi.advanceTimersByTimeAsync(6000);
   expect(f.watchers).toHaveLength(1);
   expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(count);
   expect(f.controller.getState().connected).toBe(false);
  }finally{f.controller.dispose();vi.useRealTimers();}
 });
 it("opens through read-only state and never submits a task implicitly",async()=>{const f=fixture();await f.controller.open();expect(f.controller.getState().selected).toBe(SESSION);expect(vi.mocked(f.api.rpc).mock.calls.map(c=>c[0].type)).toEqual(["sessions","session"]);expect(f.watchers[0].after).toBe(0);f.controller.dispose();expect(f.watchers[0].closed).toBe(true);});
 it("double-click/unknown preserves the original ID and only reconciles reads",async()=>{const f=fixture();await f.controller.open();const pending=deferred<GatewayResult>();const base=f.api.rpc;f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>rpc.type==="command"?pending.promise:rpc.type==="receipt"?Promise.resolve({code:"not_found"}):base(rpc,signal));const first=f.controller.submit({type:"run",content:"do one thing"});await f.controller.submit({type:"run",content:"duplicate"});expect(vi.mocked(f.api.rpc).mock.calls.filter(c=>c[0].type==="command")).toHaveLength(1);pending.reject(new Error("connection lost"));await first;expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);await f.controller.reconcile();expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);await f.controller.submit({type:"run",content:"not automatically restarted"});expect(vi.mocked(f.api.rpc).mock.calls.filter(c=>c[0].type==="command")).toHaveLength(1);expect(f.values.size).toBe(1);f.controller.dispose();});
 it("refresh restores only same-principal unknown metadata, never payload or a replay",async()=>{const f=fixture();await f.controller.open();f.api.rpc=vi.fn().mockRejectedValue(new Error());await f.controller.submit({type:"run",content:"SECRET_PROMPT_NOT_STORED"});expect([...f.values.values()].join()).not.toContain("SECRET_PROMPT");f.controller.dispose();const next=createMobileController({...f.api,rpc:vi.fn().mockResolvedValue({code:"ok",data:{sessions:[]}})},()=>{}, {storage:f.storage});await next.open();expect(next.getState().unknown?.command_id).toBe(COMMAND);next.dispose();const different=createMobileController({...f.api,session:vi.fn().mockResolvedValue({code:"ok",data:{...browser,principal_id:OTHER}}),rpc:vi.fn().mockResolvedValue({code:"ok",data:{sessions:[]}})},()=>{},{storage:f.storage});await different.open();expect(different.getState().unknown).toBeNull();different.dispose();});
 it("Close aborts observation but retains original mutation identity and ignores late success",async()=>{const f=fixture();await f.controller.open();const pending=deferred<GatewayResult>();f.api.rpc=vi.fn().mockReturnValue(pending.promise);const write=f.controller.submit({type:"run",content:"one"});f.controller.dispose();const count=f.states.length;pending.resolve({code:"ok",data:{command_id:COMMAND,session_id:SESSION,status:"accepted"}});await write;expect(f.states).toHaveLength(count);expect(f.values.size).toBe(1);});
 it("switching session during an old read cannot overwrite selection or lose the new stream",async()=>{const f=fixture();await f.controller.open();const old=deferred<GatewayResult<SessionSnapshot>>();const base=f.api.rpc;f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>rpc.type==="session"&&rpc.session_id===SESSION?old.promise:base(rpc,signal));const read=f.controller.refresh();await f.controller.select(OTHER);expect(f.controller.getState().snapshot?.session.id).toBe(OTHER);expect(f.watchers.at(-1)?.session).toBe(OTHER);old.resolve({code:"ok",data:snapshot(SESSION,30)});await read;expect(f.controller.getState().snapshot?.session.id).toBe(OTHER);f.controller.dispose();});
 it("duplicate event cursors do not duplicate reads; revoked streams stop",async()=>{const f=fixture();await f.controller.open();const count=vi.mocked(f.api.rpc).mock.calls.length;f.watchers[0].receive({code:"ok",data:{cursor:0,events:[]}});await flush();expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(count);f.watchers[0].receive({code:"ok",data:{cursor:1,events:[{}]}});await flush();expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(count+1);f.watchers[0].receive({code:"forbidden"});expect(f.watchers[0].closed).toBe(true);expect(f.controller.getState().notice).toContain("撤销");f.controller.dispose();});
 it("expired cursor resets to the authoritative lower snapshot and fences old streams",async()=>{const f=fixture();const base=f.api.rpc;let latest=80;f.api.rpc=vi.fn().mockImplementation((rpc:MobileRpc,signal:AbortSignal)=>rpc.type==="session"?Promise.resolve({code:"ok",data:snapshot(rpc.session_id,latest)}):base(rpc,signal));await f.controller.open();const old=f.watchers[0];expect(old.after).toBe(80);latest=3;old.receive({code:"cursor_expired"});await flush();expect(old.closed).toBe(true);expect(f.watchers.at(-1)?.after).toBe(3);const count=vi.mocked(f.api.rpc).mock.calls.length;old.receive({code:"ok",data:{cursor:999,events:[{}]}});await flush();expect(vi.mocked(f.api.rpc).mock.calls).toHaveLength(count);latest=4;f.watchers.at(-1)!.receive({code:"ok",data:{cursor:4,events:[{}]}});await flush();expect(f.controller.getState().snapshot?.cursor).toBe(4);f.controller.dispose();});
 it("a mismatched success receipt never clears the original unknown command",async()=>{const f=fixture();await f.controller.open();f.api.rpc=vi.fn().mockResolvedValue({code:"ok",data:{command_id:OTHER,session_id:SESSION,status:"accepted"}});await f.controller.submit({type:"steer",task_id:OTHER,content:"one supplement"});expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);await f.controller.reconcile();expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);expect(vi.mocked(f.api.rpc).mock.calls.map(c=>c[0].type)).toEqual(["command","receipt"]);f.controller.dispose();});
 it("definite offline rejection clears waiting state and does not queue a retry",async()=>{const f=fixture();await f.controller.open();f.api.rpc=vi.fn().mockResolvedValue({code:"device_offline"});await f.controller.submit({type:"run",content:"one"});expect(f.controller.getState().unknown).toBeNull();expect(f.values.size).toBe(0);expect(f.api.rpc).toHaveBeenCalledTimes(1);f.controller.dispose();});
 it("only an exact accepted original-session receipt resolves uncertainty",()=>{const good={command_id:COMMAND,session_id:SESSION,status:"accepted",task_id:OTHER};expect(validCommandReceipt(good,COMMAND,SESSION)).toBe(true);for(const bad of [undefined,null,{},true,{...good,command_id:OTHER},{...good,session_id:OTHER},{...good,status:"completed"},{...good,task_id:"../path"}])expect(validCommandReceipt(bad,COMMAND,SESSION)).toBe(false);});
 it("ok without a receipt remains unknown on submit and read-only reconcile",async()=>{const f=fixture();await f.controller.open();f.api.rpc=vi.fn().mockResolvedValue({code:"ok"});await f.controller.submit({type:"run",content:"one"});expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);await f.controller.reconcile();expect(f.controller.getState().unknown?.command_id).toBe(COMMAND);expect(f.controller.getState().notice).toContain("仍未知");f.controller.dispose();});
 it("UTF-8 input bounds count bytes and reject NUL",()=>{expect(inputError("中".repeat(2730))).toBeNull();expect(inputError("中".repeat(2731))).not.toBeNull();expect(inputError("x\0y")).not.toBeNull();});
});
describe("H5 same-origin HTTP boundary",()=>{
 it("uses cookie and CSRF but no daemon bearer, with redirect and cache disabled",async()=>{const fetcher=vi.fn().mockResolvedValueOnce(new Response(JSON.stringify({code:"ok",data:browser}))).mockResolvedValueOnce(new Response(JSON.stringify({code:"ok",data:{sessions:[]}})));const api=createHttpMobileApi(fetcher as typeof fetch);await api.session(new AbortController().signal);await api.rpc({type:"sessions"},new AbortController().signal);expect(fetcher.mock.calls[1][0]).toBe("/gateway/api/rpc");const init=fetcher.mock.calls[1][1];expect(init.credentials).toBe("same-origin");expect(init.redirect).toBe("error");expect(init.cache).toBe("no-store");expect(init.headers["x-csrf-token"]).toBe(browser.csrf);expect(JSON.stringify(init.headers)).not.toMatch(/authorization|bearer/i);});
 it("oversized response is rejected without accepting its payload",async()=>{const fetcher=vi.fn().mockResolvedValue(new Response("x".repeat(262145)));const api=createHttpMobileApi(fetcher as typeof fetch);await expect(api.session(new AbortController().signal)).rejects.toThrow("超过限制");});
});
