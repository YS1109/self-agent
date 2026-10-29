/** Local-owner process lifecycle. Unknown/offline/new-instance observations are
 * never converted into proof that an earlier process stopped. */
export interface LifecycleSnapshot {
  instance_id:string; state:"running"|"draining"|"stopped";
  phase:"not_requested"|"initial_quiesce"|"workers"|"final_quiesce"|"finished";
  outcome:"stopped"|"unknown"|null;
  workers:{state:"running"|"draining"|"stopped";accepting_work:boolean;active_admissions:number;pending_workers:number;settled_workers:number;unknown_workers:number;unknown_admissions:number};
}
export interface LifecycleClient { status(signal:AbortSignal):Promise<unknown>; shutdown(instance:string,signal:AbortSignal):Promise<unknown> }
export interface LifecycleRecovery { read():string|null; save(id:string):void; clear():void }
const id=(v:unknown):v is string=>typeof v==="string"&&/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(v);
const object=(v:unknown):v is Record<string,unknown>=>!!v&&typeof v==="object"&&!Array.isArray(v);
const exact=(v:Record<string,unknown>,keys:string[])=>Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
const state=(v:unknown)=>["running","draining","stopped"].includes(String(v));
export function parseLifecycle(v:unknown):LifecycleSnapshot {
  const invalid=()=>new Error("后台生命周期响应无效；未确认停止");
  if(!object(v)||!exact(v,["instance_id","state","phase","outcome","workers"])||!id(v.instance_id)||!state(v.state)||!["not_requested","initial_quiesce","workers","final_quiesce","finished"].includes(String(v.phase))||!(v.outcome===null||v.outcome==="stopped"||v.outcome==="unknown")||!object(v.workers))throw invalid();
  const w=v.workers;
  if(!exact(w,["state","accepting_work","active_admissions","pending_workers","settled_workers","unknown_workers","unknown_admissions"])||!state(w.state)||typeof w.accepting_work!=="boolean")throw invalid();
  for(const key of ["active_admissions","pending_workers","settled_workers","unknown_workers","unknown_admissions"]) if(!Number.isSafeInteger(w[key])||Number(w[key])<0)throw invalid();
  if((v.state==="stopped"||v.outcome==="stopped")&&(v.state!=="stopped"||v.outcome!=="stopped"||v.phase!=="finished"||w.state!=="stopped"||w.accepting_work||["active_admissions","pending_workers","unknown_workers","unknown_admissions"].some(k=>w[k]!==0)))throw invalid();
  return structuredClone(v) as unknown as LifecycleSnapshot;
}
const KEY="self-agent.lifecycle-observation.v1";
export const lifecycleRecovery:LifecycleRecovery={
 read(){const value=sessionStorage.getItem(KEY);if(value!==null&&!id(value))throw new Error("invalid");return value;},
 save(value){if(!id(value))throw new Error("invalid");sessionStorage.setItem(KEY,value);if(sessionStorage.getItem(KEY)!==value)throw new Error("storage unavailable");},
 clear(){sessionStorage.removeItem(KEY);},
};
export interface LifecycleView { snapshot:LifecycleSnapshot|null; pendingId:string|null; reading:boolean; writing:boolean; fresh:boolean; storageAvailable:boolean; notice:string }
export function createLifecycleController(client:LifecycleClient,recovery:LifecycleRecovery,changed:(state:LifecycleView)=>void){
 let initial:string|null=null,available=true;
 try{initial=recovery.read();if(initial!==null&&!id(initial))throw new Error("invalid");}catch{initial=null;available=false;}
 let view:LifecycleView={snapshot:null,pendingId:initial,reading:false,writing:false,fresh:false,storageAvailable:available,notice:initial?"恢复原实例的只读观察；不会重新发送关闭请求。":available?"":"本机观察存储不可用，已禁用关闭操作；仍可只读查询。"};
 let disposed=false,epoch=0,request:AbortController|null=null;
 const publish=(patch:Partial<LifecycleView>)=>{view={...view,...patch};if(!disposed)changed(view);};
 async function refresh(){
  if(disposed||view.writing)return;
  request?.abort();const own=new AbortController();request=own;const generation=++epoch;
  publish({reading:true,fresh:false});
  try{const snapshot=parseLifecycle(await client.status(own.signal));if(disposed||generation!==epoch)return;
   const changedInstance=!!view.pendingId&&view.pendingId!==snapshot.instance_id;
   publish({snapshot,fresh:!changedInstance,notice:changedInstance?"现在连接的是新实例；不能用它证明原实例已停止。原观察保留，未发送新关闭请求。":snapshot.outcome==="stopped"?"该实例已确认完成两次持久化检查与真实工作清理。":view.pendingId?"仅观察原实例；清理仍可能继续，未知结果不会自动重放。":"已读取当前实例；关闭前请核对影响。"});
  }catch{if(!disposed&&generation===epoch)publish({fresh:false,notice:"生命周期读取未确认；离线、连接中断或新实例均不能证明原实例已停止。未重发请求或启动替代后台。"});}
  finally{if(!disposed&&generation===epoch)publish({reading:false});}
 }
 async function shutdown(confirmed:boolean){
  if(disposed||!confirmed||view.writing||view.reading||view.pendingId||!view.storageAvailable||!view.fresh||view.snapshot?.state!=="running")return;
  const instance=view.snapshot.instance_id;
  try{recovery.save(instance);}catch{publish({storageAvailable:false,notice:"无法保存只读恢复标识，未发送关闭请求。"});return;}
  request?.abort();const own=new AbortController();request=own;const generation=++epoch;
  publish({pendingId:instance,writing:true,fresh:false,notice:"仅发送一次关闭请求，等待原实例确认；关闭页面只停止观察，不取消清理。"});
  try{const snapshot=parseLifecycle(await client.shutdown(instance,own.signal));if(disposed||generation!==epoch)return;
   if(snapshot.instance_id!==instance)throw new Error("instance changed");
   publish({snapshot,fresh:true,notice:snapshot.outcome==="stopped"?"原实例已确认停止；已写入文件保留，不自动启动替代后台。":"原实例停止尚未确认；只读刷新，不重新发送请求。"});
  }catch{if(!disposed&&generation===epoch)publish({fresh:false,notice:"关闭响应未确认，原清理可能继续；只读刷新，不能将连接中断记为停止。"});}
  finally{if(!disposed&&generation===epoch)publish({writing:false});}
 }
 return {refresh,shutdown,getState:()=>view,finishObservation(confirmed:boolean){if(!confirmed||view.writing||disposed)return false;try{recovery.clear();}catch{return false;}publish({pendingId:null,fresh:false,notice:"仅结束本机观察，不改变原实例结果。必须重新读取当前实例才能执行新操作。"});return true;},dispose(){disposed=true;epoch++;request?.abort();}};
}
