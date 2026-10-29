import {describe,it,expect,vi} from "vitest";
import {createLifecycleController,parseLifecycle,type LifecycleClient,type LifecycleRecovery,type LifecycleSnapshot} from "./lifecycle-client";
const A="2c751aff-1bfe-44fa-9e0d-b7e17806a7b0",B="2c751aff-1bfe-44fa-9e0d-b7e17806a7b1";
function snapshot(instance=A,stopped=false):LifecycleSnapshot{return {instance_id:instance,state:stopped?"stopped":"running",phase:stopped?"finished":"not_requested",outcome:stopped?"stopped":null,workers:{state:stopped?"stopped":"running",accepting_work:!stopped,active_admissions:0,pending_workers:0,settled_workers:2,unknown_workers:0,unknown_admissions:0}};}
function recovery(initial:string|null=null){let saved=initial;return {read:()=>saved,save:(id:string)=>{saved=id;},clear:()=>{saved=null;}} satisfies LifecycleRecovery;}
const client=():LifecycleClient=>({status:vi.fn(async()=>snapshot()),shutdown:vi.fn(async()=>snapshot(A,true))});
function deferred<T>(){let resolve!:(v:T)=>void,reject!:(error:Error)=>void;const promise=new Promise<T>((a,b)=>{resolve=a;reject=b;});return {promise,resolve,reject};}
describe("owner lifecycle observation",()=>{
 it("validates exact fields and never infers Stopped with pending/unknown work",()=>{
  expect(parseLifecycle(snapshot(A,true)).outcome).toBe("stopped");
  for(const key of ["pending_workers","active_admissions","unknown_workers","unknown_admissions"] as const){const v=snapshot(A,true);v.workers[key]=1;expect(()=>parseLifecycle(v)).toThrow();}
  expect(()=>parseLifecycle({...snapshot(),private:"do not display"})).toThrow();
  expect(()=>parseLifecycle({...snapshot(A,true),outcome:null})).toThrow();
 });
 it("requires a fresh read and explicit confirmation; duplicate click is one fixed-instance POST",async()=>{
  const c=client(),pending=deferred<unknown>();c.shutdown=vi.fn(()=>pending.promise);const r=recovery(),controller=createLifecycleController(c,r,()=>{});
  await controller.shutdown(true);expect(c.shutdown).not.toHaveBeenCalled();await controller.refresh();await controller.shutdown(false);expect(c.shutdown).not.toHaveBeenCalled();
  const write=controller.shutdown(true);await controller.shutdown(true);expect(c.shutdown).toHaveBeenCalledTimes(1);expect(c.shutdown).toHaveBeenCalledWith(A,expect.any(AbortSignal));expect(r.read()).toBe(A);
  pending.resolve(snapshot(A,true));await write;expect(controller.getState().snapshot?.outcome).toBe("stopped");controller.dispose();
 });
 it("lost response persists only the original instance and remount only reads",async()=>{
  const c=client(),r=recovery();c.shutdown=vi.fn(async()=>{throw new Error("private raw failure");});const first=createLifecycleController(c,r,()=>{});await first.refresh();await first.shutdown(true);first.dispose();
  const next=createLifecycleController(c,r,()=>{});await next.refresh();await next.shutdown(true);expect(c.shutdown).toHaveBeenCalledTimes(1);expect(next.getState().pendingId).toBe(A);expect(next.getState().notice).not.toContain("private");next.dispose();
 });
 it("a different daemon cannot confirm the old instance or receive a hidden replacement stop",async()=>{
  const c=client();c.status=vi.fn(async()=>snapshot(B,true));const controller=createLifecycleController(c,recovery(A),()=>{});await controller.refresh();expect(controller.getState().fresh).toBe(false);expect(controller.getState().notice).toContain("新实例");await controller.shutdown(true);expect(c.shutdown).not.toHaveBeenCalled();
  expect(controller.finishObservation(false)).toBe(false);expect(controller.finishObservation(true)).toBe(true);expect(controller.getState().fresh).toBe(false);controller.dispose();
 });
 it("a mismatched POST response remains unconfirmed",async()=>{const c=client();c.shutdown=vi.fn(async()=>snapshot(B,true));const controller=createLifecycleController(c,recovery(),()=>{});await controller.refresh();await controller.shutdown(true);expect(controller.getState().fresh).toBe(false);expect(controller.getState().snapshot?.outcome).toBeNull();expect(controller.getState().pendingId).toBe(A);controller.dispose();});
 it("cannot POST if durable local observation storage fails",async()=>{const c=client(),r=recovery();r.save=()=>{throw new Error("blocked");};const controller=createLifecycleController(c,r,()=>{});await controller.refresh();await controller.shutdown(true);expect(c.shutdown).not.toHaveBeenCalled();expect(controller.getState().storageAvailable).toBe(false);controller.dispose();});
 it("dispose aborts only observation and ignores late result without retry",async()=>{const c=client(),pending=deferred<unknown>(),changed=vi.fn();c.shutdown=vi.fn(()=>pending.promise);const r=recovery(),controller=createLifecycleController(c,r,changed);await controller.refresh();const write=controller.shutdown(true);const signal=vi.mocked(c.shutdown).mock.calls[0][1];controller.dispose();const before=changed.mock.calls.length;pending.resolve(snapshot(A,true));await write;expect(signal.aborted).toBe(true);expect(changed).toHaveBeenCalledTimes(before);expect(r.read()).toBe(A);expect(c.shutdown).toHaveBeenCalledTimes(1);});
 it("late prior GET cannot replace newer same-controller observation",async()=>{const c=client(),old=deferred<unknown>();c.status=vi.fn().mockReturnValueOnce(old.promise).mockResolvedValueOnce(snapshot(B));const controller=createLifecycleController(c,recovery(),()=>{});const first=controller.refresh();await controller.refresh();old.resolve(snapshot(A));await first;expect(controller.getState().snapshot?.instance_id).toBe(B);controller.dispose();});
 it("failed fresh GET disables action even if last snapshot was running",async()=>{const c=client(),controller=createLifecycleController(c,recovery(),()=>{});await controller.refresh();c.status=vi.fn(async()=>{throw new Error("offline");});await controller.refresh();await controller.shutdown(true);expect(c.shutdown).not.toHaveBeenCalled();expect(controller.getState().notice).toContain("不能证明");controller.dispose();});
});
