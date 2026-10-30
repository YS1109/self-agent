/** QQ-only credential observation. State contains public metadata, never secrets. */
import type { QqConfiguration, QqCredentialClient } from "./qq-client";
import { providerSecretError, providerUuid, type CredentialOperation, type CredentialReceipt } from "./providers-client";
export interface QqCredentialState {busy:boolean;notice:string;uncertainId:string|null;operation:CredentialOperation|null}
const pending=(op:CredentialOperation|null)=>!!op&&["pending","inflight"].includes(op.status);
const project=(op:CredentialOperation):CredentialOperation=>({id:op.id,owner_kind:"qq",owner_id:op.owner_id,kind:op.kind,status:op.status,reason:op.reason,can_cleanup:op.can_cleanup,cleanup_status:op.cleanup_status,created_at:op.created_at,updated_at:op.updated_at});
const statuses=["pending","inflight","completed","rejected","unknown","orphaned","cleanup_required","cleaned"];
export function validQqOperation(op:CredentialOperation,owner:string,id?:string):boolean {
 return !!op&&providerUuid(op.id)&&op.owner_kind==="qq"&&op.owner_id===owner&&(!id||op.id===id)&&["set","delete","cleanup"].includes(op.kind)&&statuses.includes(op.status)&&typeof op.can_cleanup==="boolean"&&typeof op.created_at==="string"&&typeof op.updated_at==="string"&&(op.reason===null||typeof op.reason==="string")&&(op.cleanup_status===undefined||op.cleanup_status===null||statuses.includes(op.cleanup_status));
}
export function createQqCredentialController(client:QqCredentialClient,initialConfig:QqConfiguration,changed:(state:QqCredentialState)=>void,options:{uuid?:()=>string;pollMs?:number;onRefresh?:()=>void|Promise<void>}={}) {
 const config={...initialConfig};
 let state:QqCredentialState={busy:false,notice:"",uncertainId:null,operation:null};
 let active=true,flight:Promise<unknown>|undefined,write:AbortController|undefined,read:AbortController|undefined,timer:ReturnType<typeof setTimeout>|undefined,generation=0;
 const publish=(patch:Partial<QqCredentialState>)=>{if(active){state={...state,...patch};changed(state);}};
 const refresh=async()=>{if(active){try{await options.onRefresh?.();}catch{ /* Read refresh failure never reclassifies a confirmed mutation. */ }}};
 function schedule(){clearTimeout(timer);if(active&&(state.uncertainId||pending(state.operation)))timer=setTimeout(()=>void inspect(),options.pollMs??1500);}
 function accept(op:CredentialOperation,id:string){if(!validQqOperation(op,config.id,id))throw new Error("invalid_qq_operation");publish({operation:project(op),uncertainId:null});}
 async function inspect(){
  if(!active||state.busy||flight)return;const id=state.uncertainId??state.operation?.id;if(!id)return;
  const version=++generation;read?.abort();read=new AbortController();publish({busy:true});
  try{const op=await client.operation(id,read.signal);if(!active||version!==generation)return;if(op){accept(op,id);publish({notice:"已只读查询原操作；未重新提交凭据或删除请求。"});}else publish({uncertainId:id,notice:"尚未找到原操作记录，结果仍未知；不代表没有写入，不会自动重发。"});await refresh();}
  catch{if(active&&version===generation)publish({uncertainId:id,notice:"原操作暂不可读，结果仍未确认；不会回显底层错误或重发。"});}
  finally{if(active&&version===generation){publish({busy:false});schedule();}}
 }
 function mutate(send:(id:string,signal:AbortSignal)=>Promise<CredentialReceipt>):Promise<unknown>{
  if(!active||flight||state.busy)return Promise.resolve(undefined);
  if(state.uncertainId||pending(state.operation)){publish({notice:"已有操作尚未确认，请只读检查原操作，不会发送新的凭据操作。"});return Promise.resolve(undefined);}
  if(!providerUuid(config.id)||!Number.isSafeInteger(config.revision)||config.revision<0||config.revision>=Number.MAX_SAFE_INTEGER){publish({notice:"QQ 配置修订无效，请刷新后核对。"});return Promise.resolve(undefined);}
  const id=(options.uuid??(()=>crypto.randomUUID()))();++generation;read?.abort();clearTimeout(timer);write=new AbortController();const signal=write.signal;publish({busy:true,notice:"",uncertainId:id});
  flight=(async()=>{try{const receipt=await Promise.resolve().then(()=>active?send(id,signal):undefined);if(!active||!receipt)return;if(typeof receipt.replayed!=="boolean"||typeof receipt.no_secret_applied!=="boolean")throw new Error("invalid_receipt");accept(receipt.operation,id);publish({notice:receipt.replayed||receipt.no_secret_applied?"返回已有操作记录；这次输入没有写入。请核对原操作，不能视为新凭据已保存。":"凭据操作已登记；请核对持久状态，绑定存在不代表系统存储可读或 QQ 已连接。"});await refresh();return receipt;}
   catch{publish({uncertainId:id,notice:"操作结果未知；停止观察不代表后台撤销。只查询原操作，不自动重发。"});}
   finally{flight=undefined;publish({busy:false});schedule();}})();return flight;
 }
 return {getState:()=>state,inspect,
  observe(operations:CredentialOperation[]){if(!active||state.busy||flight||state.uncertainId)return;const known=state.operation&&operations.find(op=>op.id===state.operation!.id);const op=known??operations.find(op=>validQqOperation(op,config.id)&&pending(op));if(op&&validQqOperation(op,config.id)){publish({operation:project(op)});schedule();}},
  setCredential(secret:string,confirmed:boolean){const error=providerSecretError(secret);if(!confirmed||error){secret="";publish({notice:error??"请独立确认写入 QQ 专用系统凭据。"});return Promise.resolve(undefined);}return mutate(async(id,signal)=>{const body={id,expected_revision:config.revision,secret};secret="";try{return await client.setCredential(config.id,body,signal);}finally{body.secret="";}});},
  deleteCredential(confirmed:boolean){if(!confirmed){publish({notice:"请确认停用 QQ、撤销旧绑定并删除凭据。"});return Promise.resolve(undefined);}return mutate((id,signal)=>client.deleteCredential(config.id,{id,expected_revision:config.revision},signal));},
  cleanupCredential(op:CredentialOperation,confirmed:boolean){if(!confirmed||!validQqOperation(op,config.id)||!op.can_cleanup){publish({notice:"请核对 QQ 待清理记录并明确确认清理。"});return Promise.resolve(undefined);}return mutate((id,signal)=>client.cleanupCredential(op.id,{id},signal));},
  dispose(){active=false;++generation;read?.abort();write?.abort();clearTimeout(timer);},
 };
}
