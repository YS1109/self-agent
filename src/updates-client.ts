/** Local-owner verification/staging observations; never downloads or installs. */
import { providerUuid } from "./providers-client";
export const UPDATE_TARGETS = ["linux-x86_64", "linux-aarch64", "windows-x86_64", "windows-aarch64", "macos-x86_64", "macos-aarch64"] as const;
export type UpdateTarget = typeof UPDATE_TARGETS[number];
export interface UpdateTrust { revision:number; key_id:string; source:string; public_key:number[]; fingerprint:string; enabled:boolean }
export interface UpdateMetadata { version:string; sequence:number; target:UpdateTarget; bytes:number; sha256:string; manifest_sha256:string }
export interface UpdateReceipt { id:string; config_revision:number; metadata:UpdateMetadata; status:"pending"|"verifying"|"verified"|"failed"|"interrupted"|"unknown"; stage_authority:boolean; reason:string|null; created_at_ms:number; updated_at_ms:number }
export interface UpdateFloor { target:UpdateTarget; version:string; sequence:number; manifest_sha256:string|null; sha256:string|null }
export interface UpdatesStatus { mode:"verification_only"|"verification_disabled"|"verification_blocked"; installed:false; automatic_update_enabled:false; configuration:UpdateTrust|null; floors:UpdateFloor[]; receipts:UpdateReceipt[]; staging:{files:number;bytes:number}|null; blocked_reason:"witness_unavailable"|"staging_unavailable"|null }
export interface UpdateObservation { receipt:UpdateReceipt; current_stage_verified:boolean; installed:false; automatic_update_enabled:false }
export interface UpdatesClient { status(signal:AbortSignal):Promise<unknown>; receipt(id:string,signal:AbortSignal):Promise<unknown> }
const object = (v:unknown):v is Record<string,unknown> => !!v && typeof v === "object" && !Array.isArray(v);
const exact = (v:Record<string,unknown>,k:string[]) => Object.keys(v).length === k.length && k.every(key=>Object.hasOwn(v,key));
const int = (v:unknown,max=Number.MAX_SAFE_INTEGER):v is number => Number.isSafeInteger(v) && Number(v)>=0 && Number(v)<=max;
const hash = (v:unknown):v is string => typeof v === "string" && /^[a-f0-9]{64}$/.test(v);
const version = (v:unknown):v is string => typeof v === "string" && v.length<=80 && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(v);
const target = (v:unknown):v is UpdateTarget => typeof v === "string" && (UPDATE_TARGETS as readonly string[]).includes(v);
const error = () => new Error("更新验证记录格式不受支持；没有下载、安装或自动重发。");
const reason = (v:unknown) => v===null || ["trust_changed","verification_rejected","verification_cancelled","verification_outcome_unknown","verification_interrupted_no_replay","backup_restored_no_authority","staging_discarded","cancellation_requested"].includes(String(v));
function trust(v:unknown):v is UpdateTrust {
  if (!object(v)||!exact(v,["revision","key_id","source","public_key","fingerprint","enabled"])||!int(v.revision)||v.revision===0||typeof v.enabled!=="boolean"||typeof v.key_id!=="string"||!/^[A-Za-z0-9_.-]{1,64}$/.test(v.key_id)||typeof v.source!=="string"||v.source.length>2048||!hash(v.fingerprint)||!Array.isArray(v.public_key)||v.public_key.length!==32||!v.public_key.every(n=>int(n,255))) return false;
  try { const u=new URL(v.source); return u.protocol==="https:" && !!u.hostname && !u.username && !u.password && !u.search && !u.hash && u.href===v.source; } catch { return false; }
}
function metadata(v:unknown):v is UpdateMetadata { return object(v)&&exact(v,["version","sequence","target","bytes","sha256","manifest_sha256"])&&version(v.version)&&int(v.sequence)&&v.sequence>0&&target(v.target)&&int(v.bytes,256*1024*1024)&&v.bytes>0&&hash(v.sha256)&&hash(v.manifest_sha256); }
export function parseUpdateReceipt(v:unknown):UpdateReceipt {
  if (!object(v)||!exact(v,["id","config_revision","metadata","status","stage_authority","reason","created_at_ms","updated_at_ms"])||typeof v.id!=="string"||!providerUuid(v.id)||!int(v.config_revision)||v.config_revision===0||!metadata(v.metadata)||!["pending","verifying","verified","failed","interrupted","unknown"].includes(String(v.status))||typeof v.stage_authority!=="boolean"||!reason(v.reason)||!int(v.created_at_ms)||!int(v.updated_at_ms)||v.updated_at_ms<v.created_at_ms||(v.stage_authority&&v.status!=="verified")) throw error();
  return structuredClone(v) as unknown as UpdateReceipt;
}
export function parseUpdatesStatus(v:unknown):UpdatesStatus {
  if (!object(v)||!exact(v,["mode","installed","automatic_update_enabled","configuration","floors","receipts","staging","blocked_reason"])||new TextEncoder().encode(JSON.stringify(v)).length>512*1024||v.installed!==false||v.automatic_update_enabled!==false||(v.configuration!==null&&!trust(v.configuration))||!Array.isArray(v.floors)||v.floors.length!==6||!Array.isArray(v.receipts)||v.receipts.length>32) throw error();
  if (!["verification_only","verification_disabled","verification_blocked"].includes(String(v.mode))||!(v.blocked_reason===null||v.blocked_reason==="witness_unavailable"||v.blocked_reason==="staging_unavailable")) throw error();
  if ((v.mode==="verification_blocked")!==(v.blocked_reason!==null)||(v.mode==="verification_only"&&!v.configuration?.enabled)||(v.mode==="verification_disabled"&&v.configuration?.enabled)) throw error();
  if (v.staging!==null&&(!object(v.staging)||!exact(v.staging,["files","bytes"])||!int(v.staging.files,512)||!int(v.staging.bytes,512*1024*1024))) throw error();
  for (const f of v.floors) if (!object(f)||!exact(f,["target","version","sequence","manifest_sha256","sha256"])||!target(f.target)||!version(f.version)||!int(f.sequence)||!(f.manifest_sha256===null||hash(f.manifest_sha256))||!(f.sha256===null||hash(f.sha256))||(f.manifest_sha256===null)!==(f.sha256===null)) throw error();
  if (new Set(v.floors.map(f=>(f as UpdateFloor).target)).size!==6) throw error();
  const receipts=v.receipts.map(parseUpdateReceipt); if (receipts.some(r=>r.stage_authority)||new Set(receipts.map(r=>r.id)).size!==receipts.length) throw error();
  return {...structuredClone(v),receipts} as unknown as UpdatesStatus;
}
export function parseUpdateObservation(v:unknown,id:string):UpdateObservation {
  if (!object(v)||!exact(v,["receipt","current_stage_verified","installed","automatic_update_enabled"])||v.installed!==false||v.automatic_update_enabled!==false||typeof v.current_stage_verified!=="boolean") throw error();
  const receipt=parseUpdateReceipt(v.receipt); if (receipt.id!==id||receipt.stage_authority!==v.current_stage_verified) throw error();
  return {receipt,current_stage_verified:v.current_stage_verified,installed:false,automatic_update_enabled:false};
}
export interface UpdatesView { status:UpdatesStatus|null; selected:UpdateObservation|null; loading:boolean; notice:string }
export const emptyUpdatesView = ():UpdatesView => ({status:null,selected:null,loading:false,notice:""});
export function createUpdatesController(client:UpdatesClient,changed:(state:UpdatesView)=>void) {
  let state=emptyUpdatesView(),generation=0,disposed=false,request:AbortController|null=null;
  const publish=(patch:Partial<UpdatesView>)=>{state={...state,...patch};if(!disposed) changed(state);};
  const load=async(id?:string)=>{
    if(disposed|| (id!==undefined&&!providerUuid(id))) return;
    request?.abort(); const own=new AbortController();request=own;const current=++generation;
    publish({loading:true,notice:"",selected:null});
    try {
      const raw=id?await client.receipt(id,own.signal):await client.status(own.signal);
      if(disposed||current!==generation)return;
      publish(id?{selected:parseUpdateObservation(raw,id)}:{status:parseUpdatesStatus(raw)});
    } catch { if(!disposed&&current===generation) publish({notice:"尚未确认更新验证状态；请只读刷新或查询原操作 UUID，不要重发上传。"}); }
    finally {if(!disposed&&current===generation)publish({loading:false});}
  };
  return {refresh:()=>load(),inspect:(id:string)=>load(id),getState:()=>state,dispose(){disposed=true;generation++;request?.abort();}};
}
