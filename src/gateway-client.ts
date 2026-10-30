/** Local-owner gateway management, not the remote H5 RPC client. */
import type {TaskControls} from "./types";
import type {CredentialOperation,CredentialReceipt} from "./providers-client";
import {executionControlsError} from "./execution-controls";
export interface GatewayPermissions {view:boolean;read_results:boolean;read_artifacts:boolean;run:boolean;cancel:boolean;answer:boolean}
export interface GatewayGrant {session_ids:string[];permissions:GatewayPermissions;controls:TaskControls;expires_at_ms:number}
export interface GatewayBinding {principal_id:string;device_id:string;config_revision:number;revision:number;status:"pending"|"active"|"revoked";label:string;pairing_id:string;grant:GatewayGrant|null;expires_at_ms:number;created_at_ms:number;updated_at_ms:number}
export interface GatewayConfiguration {id:string;revision:number;device_id:string;origin:string;enabled:boolean;credential_state:"configured"|"missing";mode?:"self_hosted"|"fixture"}
export interface GatewaySnapshot {truncated?:boolean;configuration:GatewayConfiguration|null;bindings:GatewayBinding[];connection:{state:string;reason?:string|null;updated_at_ms:number|null};credential_operations:CredentialOperation[]}
export type GatewayCommand = {type:"configure";config_id:string|null;expected_revision:number|null;origin:string;device_id:string;enabled:boolean}|{type:"issue_pairing";ttl_seconds:number}|{type:"confirm_binding";principal_id:string;expected_revision:number;expected_label:string;expected_pairing_id:string;grant:GatewayGrant}|{type:"revoke_binding";principal_id:string;expected_revision:number};
export interface GatewayEnvelope {id:string;command:GatewayCommand}
export interface GatewayReceipt {command_id:string;status:"completed"|"rejected";reason?:string;config_id?:string;principal_id?:string;revision?:number;pairing?:{id:string;code?:string;expires_at_ms:number}}
export interface GatewayClient {snapshot(signal:AbortSignal):Promise<GatewaySnapshot>;command(envelope:GatewayEnvelope,signal:AbortSignal):Promise<GatewayReceipt>;receipt(id:string,signal:AbortSignal):Promise<GatewayReceipt|null>}
export interface GatewayCredentialClient {setCredential(configId:string,body:{id:string;expected_revision:number;secret:string},signal:AbortSignal):Promise<CredentialReceipt>;deleteCredential(configId:string,body:{id:string;expected_revision:number},signal:AbortSignal):Promise<CredentialReceipt>;cleanupCredential(targetId:string,body:{id:string},signal:AbortSignal):Promise<CredentialReceipt>;operation(id:string,signal:AbortSignal):Promise<CredentialOperation|null>}
export const EMPTY_GATEWAY:GatewaySnapshot={configuration:null,bindings:[],connection:{state:"disabled",updated_at_ms:null},credential_operations:[]};
export const EMPTY_GATEWAY_PERMISSIONS:GatewayPermissions={view:false,read_results:false,read_artifacts:false,run:false,cancel:false,answer:false};
export function conservativeGatewayControls():TaskControls{return {budget:{max_model_requests:8,max_runtime_seconds:300,max_retries:0,max_repair_attempts:0,max_output_bytes:65536,max_external_calls:0},permissions:{read_files:true,write_files:"deny",run_commands:"deny",managed_tools:"deny"},acceptance_commands:[],capabilities:{mcp_servers:[],skills:[]}};}
const uuid=(value:string)=>typeof value==="string"&&value.length===36&&/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value);
const revision=(value:number)=>Number.isSafeInteger(value)&&value>=0;
const fields=(value:unknown,keys:string[]):value is Record<string,unknown>=>!!value&&typeof value==="object"&&!Array.isArray(value)&&Object.keys(value).length===keys.length&&keys.every(key=>Object.hasOwn(value,key));
/** Grants are explicit authority: never use legacy task normalization to fill omitted permissions. */
export function gatewayControlsError(value:unknown):string|null {
 if(!fields(value,["budget","permissions","acceptance_commands","capabilities"])||!fields(value.budget,["max_model_requests","max_runtime_seconds","max_retries","max_repair_attempts","max_output_bytes","max_external_calls"])||!fields(value.permissions,["read_files","write_files","run_commands","managed_tools"])||!fields(value.capabilities,["mcp_servers","skills"]))return "授权必须明确填写全部预算、权限、验收和能力字段，不能依赖默认值。";
 const p=value.permissions;if(typeof p.read_files!=="boolean"||[p.write_files,p.run_commands,p.managed_tools].some(mode=>typeof mode!=="string"||!["allow","ask","deny"].includes(mode)))return "任务权限必须为明确布尔值或 allow/ask/deny。";
 if(!Array.isArray(value.acceptance_commands)||value.acceptance_commands.some(command=>typeof command!=="string"))return "验收命令必须是完整文本列表。";
 for(const key of ["mcp_servers","skills"]) {const ids=value.capabilities[key];if(!Array.isArray(ids)||ids.some(id=>typeof id!=="string"||!uuid(id)))return "能力范围必须是明确的规范 UUID 列表。";}
 return executionControlsError(value as unknown as TaskControls,"native")??null;
}
export function gatewayCommandError(command:GatewayCommand,now=Date.now()):string|null {
 try {return validateGatewayCommand(command,now);}catch{return "管理请求字段不完整或类型无效；未发送请求。";}
}
function validateGatewayCommand(command:GatewayCommand,now:number):string|null {
 if(command.type==="configure"){
  if(typeof command.enabled!=="boolean")return "启用状态必须明确填写布尔值。";
  if(!uuid(command.device_id))return "设备标识必须是网关本地登记生成的 UUID，不是设备凭据。";
  try{const url=new URL(command.origin);if(url.protocol!=="https:"||url.username||url.password||url.pathname!=="/"||url.search||url.hash)throw new Error();}catch{return "网关地址必须是 HTTPS origin，不能包含路径、参数或认证信息。";}
  if(command.config_id===null){if(command.expected_revision!==null||command.enabled)return "首次配置必须停用创建，再登记独立设备凭据并明确启用。";}
  else if(!uuid(command.config_id)||!revision(command.expected_revision!)||command.expected_revision===0)return "网关配置修订无效。";
  return null;
 }
 if(command.type==="issue_pairing")return Number.isInteger(command.ttl_seconds)&&command.ttl_seconds>=1&&command.ttl_seconds<=600?null:"配对码有效期为 1 至 600 秒。";
 if(!uuid(command.principal_id)||!revision(command.expected_revision))return "远程身份或修订无效。";
 if(command.type==="revoke_binding")return null;
 if(command.type!=="confirm_binding")return "不支持此远程管理操作。";
 if(!command.expected_label.trim()||new TextEncoder().encode(command.expected_label).length>128||/[\x00-\x1f\x7f]/.test(command.expected_label)||!uuid(command.expected_pairing_id))return "待确认浏览器身份无效，请刷新核对。";
 const grant=command.grant;if(!fields(grant.permissions,["view","read_results","read_artifacts","run","cancel","answer"])||Object.values(grant.permissions).some(value=>typeof value!=="boolean"))return "必须明确填写全部动作权限布尔值。";if(!grant.permissions.view)return "请明确勾选查看所选会话。";if(grant.permissions.read_artifacts&&!grant.permissions.read_results)return "文本产物包含任务结果，请同时明确勾选读取结果。";
 if(grant.session_ids.length<1||grant.session_ids.length>32||new Set(grant.session_ids).size!==grant.session_ids.length||grant.session_ids.some(id=>!uuid(id)))return "必须选择 1 至 32 个不同会话。";
 if(!Number.isSafeInteger(grant.expires_at_ms)||grant.expires_at_ms<=now||grant.expires_at_ms-now>30*86400000)return "授权期限必须在未来且不超过 30 天。";
 return gatewayControlsError(grant.controls);
}
function validReceipt(receipt:GatewayReceipt,id:string):boolean {
 if(receipt.command_id!==id||!["completed","rejected"].includes(receipt.status))return false;
 if(receipt.status==="rejected")return !receipt.pairing;
 if(receipt.pairing){const value=receipt.pairing;if(!uuid(value.id)||!Number.isSafeInteger(value.expires_at_ms)||value.expires_at_ms<0||(value.code!==undefined&&(typeof value.code!=="string"||value.code.length!==64||!/^[a-f0-9]{64}$/.test(value.code))))return false;}
 return true;
}
export interface GatewayState {snapshot:GatewaySnapshot;busy:boolean;loading:boolean;notice:string;uncertainId:string|null;pairing:GatewayReceipt["pairing"]|null}
/** No automatic mutation retries. Unknown results are checked with GET only. */
export function createGatewayController(client:GatewayClient,initial:GatewaySnapshot,changed:(state:GatewayState)=>void,options:{uuid?:()=>string}={}) {
 let state:GatewayState={snapshot:initial,busy:false,loading:false,notice:"",uncertainId:null,pairing:null};
 let active=true,generation=0,read:AbortController|undefined,write:AbortController|undefined;
 let pending:Promise<GatewayReceipt|undefined>|undefined,last:GatewayEnvelope|undefined;
 const publish=(patch:Partial<GatewayState>)=>{if(active){state={...state,...patch};changed(state);}};
 async function refresh(){if(!active)return;const version=++generation;read?.abort();read=new AbortController();publish({loading:true});try{const snapshot=await client.snapshot(read.signal);if(active&&version===generation)publish({snapshot});}catch{if(active&&version===generation)publish({notice:"网关 状态读取失败；未发送新的管理操作。"});}finally{if(active&&version===generation)publish({loading:false});}}
 function accept(receipt:GatewayReceipt,id:string){if(!validReceipt(receipt,id))throw new Error("invalid_receipt");publish({uncertainId:null,pairing:receipt.pairing?{id:receipt.pairing.id,code:receipt.pairing.code,expires_at_ms:receipt.pairing.expires_at_ms}:null,notice:receipt.status==="rejected"?"后台已确认未执行此管理操作；请刷新后核对配置、权限范围和修订。":receipt.pairing&&!receipt.pairing.code?"签发结果已确认，但配对码只显示一次。需要新码时请明确重新签发。":"本机管理操作已确认；连接和实际投递仍以各自记录为准。"});}
 function submit(command:GatewayCommand):Promise<GatewayReceipt|undefined>{
  if(!active)return Promise.resolve(undefined);
  if(pending){if(last&&JSON.stringify(last.command)===JSON.stringify(command))return pending;publish({notice:"另一项操作尚未确认，未发送新的管理请求。"});return Promise.resolve(undefined);}
  if(state.uncertainId){publish({notice:"上一个操作结果未知，请先检查原命令收据，不会发送新操作。"});return Promise.resolve(undefined);}
  const error=gatewayCommandError(command);if(error){publish({notice:error});return Promise.resolve(undefined);}
  last={id:(options.uuid??(()=>crypto.randomUUID()))(),command:structuredClone(command)};const envelope=last;write=new AbortController();const signal=write.signal;
  publish({busy:true,notice:"",pairing:null});
  pending=(async()=>{try{const receipt=await Promise.resolve().then(()=>active?client.command(envelope,signal):undefined);if(!active||!receipt)return;accept(receipt,envelope.id);await refresh();return receipt;}catch{publish({uncertainId:envelope.id,notice:"操作结果未知；停止观察不代表后台撤销。请检查原命令收据，不会自动重发。"});return undefined;}finally{pending=undefined;publish({busy:false});}})();return pending;
 }
 async function inspect(){if(!active||!state.uncertainId||pending||state.busy)return;const id=state.uncertainId;write=new AbortController();publish({busy:true});try{const receipt=await client.receipt(id,write.signal);if(!active)return;if(receipt)accept(receipt,id);else publish({notice:"尚未找到原命令收据，结果仍未确认；不会自动重新签发或重发。"});await refresh();}catch{publish({notice:"原命令收据暂不可读，结果仍未知。"});}finally{publish({busy:false});}}
 return {refresh,submit,inspect,getState:()=>state,replaceSnapshot(snapshot:GatewaySnapshot){if(active){++generation;read?.abort();publish({snapshot,loading:false});}},hidePairing(){publish({pairing:null});},dispose(){active=false;++generation;read?.abort();write?.abort();last=undefined;state={...state,pairing:null};}};
}
