/** Local-owner QQ management. Never available through the remote QQ command path. */
import type { TaskControls } from "./types";
import type {CredentialOperation,CredentialReceipt} from "./providers-client";
export interface QqIdentity { app_id:string; sender_open_id:string; kind:"user"|"group"; target_id:string }
export interface QqPermissions { view:boolean; read_results:boolean; run:boolean; cancel:boolean; answer:boolean; orchestrate:boolean; create_session:boolean; share_group_results:boolean }
export interface QqGrant { session_ids:string[]; permissions:QqPermissions; controls:TaskControls; expires_at_ms:number }
export interface QqBinding { principal_id:string; identity:QqIdentity; revision:number; status:"pending"|"active"|"revoked"; grant:QqGrant|null; expires_at_ms:number; created_at_ms:number; updated_at_ms:number; input_after_ms?:number }
export interface QqOutbox { id:string; route_id:string; logical_key:string; status:string; msg_seq:number; attempts:number; platform_id:string|null; reason:string|null }
export interface QqReplyPolicy {window_ms:number;max_user:number;max_group:number}
export interface QqConfiguration {id:string;app_id:string;enabled:boolean;revision:number;mode:"official"|"fixture";reply_policy:QqReplyPolicy;credential_state:"configured"|"missing"}
export interface QqSnapshot {
  configuration:QqConfiguration|null;
  connection:{state:string;updated_at_ms:number|null;reason?:string|null};
  bindings:QqBinding[]; outbox:QqOutbox[];
  pairings:{id:string;app_id:string;expires_at_ms:number;claimed:boolean}[];
  inbox:{inbox_id:string;command_id:string;status:string;route_id:string|null;received_at_ms:number}[];
  credential_operations:CredentialOperation[];
  ignored_inputs?:{count:number;updated_at_ms:number|null;clock_fault_ms?:number};
}
export type QqCommand =
 | {type:"configure";config_id:string|null;expected_revision:number|null;app_id:string;enabled:boolean;reply_policy:QqReplyPolicy}
 | {type:"issue_pairing";ttl_seconds:number}
 | {type:"confirm_binding";principal_id:string;expected_revision:number;identity:QqIdentity;grant:QqGrant}
 | {type:"revoke_binding";principal_id:string;expected_revision:number};
export interface QqEnvelope { id:string; command:QqCommand }
export interface QqReceipt { command_id:string;status:"completed"|"rejected";reason?:string;config_id?:string;principal_id?:string;revision?:number;pairing?:{id:string;code?:string;expires_at_ms:number} }
export interface QqClient {
 snapshot(signal:AbortSignal):Promise<QqSnapshot>;
 command(envelope:QqEnvelope,signal:AbortSignal):Promise<QqReceipt>;
 receipt(commandId:string,signal:AbortSignal):Promise<QqReceipt|null>;
}
export interface QqCredentialClient {
 setCredential(configId:string,body:{id:string;expected_revision:number;secret:string},signal:AbortSignal):Promise<CredentialReceipt>;
 deleteCredential(configId:string,body:{id:string;expected_revision:number},signal:AbortSignal):Promise<CredentialReceipt>;
 cleanupCredential(targetId:string,body:{id:string},signal:AbortSignal):Promise<CredentialReceipt>;
 operation(id:string,signal:AbortSignal):Promise<CredentialOperation|null>;
}
export const EMPTY_QQ:QqSnapshot={configuration:null,connection:{state:"disabled",updated_at_ms:0},bindings:[],outbox:[],pairings:[],inbox:[],credential_operations:[]};
export const EMPTY_QQ_PERMISSIONS:QqPermissions={view:false,read_results:false,run:false,cancel:false,answer:false,orchestrate:false,create_session:false,share_group_results:false};
export const QQ_STATES:Record<string,string>={pending:"等待处理",sending:"正在发送，结果尚未确认",sent:"已确认送达",failed:"明确失败",unknown:"结果未知，不自动重发",expired:"被动回复窗口已过期",revoked:"绑定已撤销，未继续发送",active:"已授权",disabled:"已停用",connecting:"正在连接",ready:"连接已就绪",disconnected:"连接已断开",error:"连接不可用",configured:"已配置，尚未连接"};
export const qqStateLabel=(state:string)=>QQ_STATES[state]??"状态未知，请刷新";
const uuid=(value:string)=>/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value);
const revision=(value:number)=>Number.isSafeInteger(value)&&value>=0;
export function qqCommandError(command:QqCommand,now=Date.now()):string|null {
 if(command.type==="configure") {
  if(!/^[A-Za-z0-9_-]{1,128}$/.test(command.app_id))return "AppID 须为 1–128 个字母、数字、下划线或连字符，不能填写密钥或地址。";
  if(command.config_id===null){if(command.expected_revision!==null||command.enabled)return "首次配置必须停用创建，登记独立凭据后再明确启用。";}
  else if(!uuid(command.config_id)||!revision(command.expected_revision!))return "QQ 配置修订无效，请刷新。";
  const p=command.reply_policy;
  if(!Number.isInteger(p.window_ms)||p.window_ms<1||p.window_ms>300000||!Number.isInteger(p.max_user)||p.max_user<1||p.max_user>4||!Number.isInteger(p.max_group)||p.max_group<1||p.max_group>5)return "被动回复只能收紧：窗口 1–300000 毫秒，单聊 1–4 次，群聊 1–5 次。";
  return null;
 }
 if(command.type==="issue_pairing") return Number.isInteger(command.ttl_seconds)&&command.ttl_seconds>=1&&command.ttl_seconds<=600?null:"配对码有效期须为 1 至 600 秒。";
 if(!uuid(command.principal_id)||!revision(command.expected_revision))return "绑定标识或修订无效，请刷新。";
 if(command.type==="revoke_binding")return null;
 const {grant,identity}=command;
 if(!identity.app_id||!identity.sender_open_id||!identity.target_id||!["user","group"].includes(identity.kind))return "待确认身份无效，请刷新后重新核对。";
 if(grant.session_ids.length<1||grant.session_ids.length>32||new Set(grant.session_ids).size!==grant.session_ids.length||grant.session_ids.some(id=>!uuid(id)))return "必须明确选择 1 至 32 个不同会话。";
 if(!grant.permissions.view)return "必须明确勾选查看所选会话权限。";
 if(!Number.isSafeInteger(grant.expires_at_ms)||grant.expires_at_ms<=now||grant.expires_at_ms-now>30*86400000)return "绑定有效期须在未来且不超过 30 天。";
 return null;
}
function validReceipt(receipt:QqReceipt,id:string):boolean {
 if(receipt.command_id!==id||!["completed","rejected"].includes(receipt.status))return false;
 if(receipt.status==="rejected")return !receipt.pairing;
 if(receipt.pairing){const value=receipt.pairing;if(!uuid(value.id)||!Number.isSafeInteger(value.expires_at_ms)||value.expires_at_ms<0||(value.code!==undefined&&!/^[a-f0-9]{32}$/.test(value.code)))return false;}
 return true;
}
export interface QqState {snapshot:QqSnapshot;busy:boolean;loading:boolean;notice:string;uncertainId:string|null;pairing:QqReceipt["pairing"]|null}
/** No automatic mutation retries. Unknown results are checked with GET only. */
export function createQqController(client:QqClient,initial:QqSnapshot,changed:(state:QqState)=>void,options:{uuid?:()=>string}={}) {
 let state:QqState={snapshot:initial,busy:false,loading:false,notice:"",uncertainId:null,pairing:null};
 let active=true,generation=0,read:AbortController|undefined,write:AbortController|undefined;
 let pending:Promise<QqReceipt|undefined>|undefined,last:QqEnvelope|undefined;
 const publish=(patch:Partial<QqState>)=>{if(active){state={...state,...patch};changed(state);}};
 async function refresh(){if(!active)return;const version=++generation;read?.abort();read=new AbortController();publish({loading:true});try{const snapshot=await client.snapshot(read.signal);if(active&&version===generation)publish({snapshot});}catch{if(active&&version===generation)publish({notice:"QQ 状态读取失败；未发送新的管理操作。"});}finally{if(active&&version===generation)publish({loading:false});}}
 function accept(receipt:QqReceipt,id:string){if(!validReceipt(receipt,id))throw new Error("invalid_receipt");publish({uncertainId:null,pairing:receipt.pairing??null,notice:receipt.status==="rejected"?"后台已确认未执行此管理操作；请刷新后核对配置、权限范围和修订。":receipt.pairing&&!receipt.pairing.code?"签发结果已确认，但配对码只显示一次。需要新码时请明确重新签发。":"本机管理操作已确认；连接和实际投递仍以各自记录为准。"});}
 function submit(command:QqCommand):Promise<QqReceipt|undefined>{
  if(!active)return Promise.resolve(undefined);
  if(pending){if(last&&JSON.stringify(last.command)===JSON.stringify(command))return pending;publish({notice:"另一项操作尚未确认，未发送新的管理请求。"});return Promise.resolve(undefined);}
  if(state.uncertainId){publish({notice:"上一个操作结果未知，请先检查原命令收据，不会发送新操作。"});return Promise.resolve(undefined);}
  const error=qqCommandError(command);if(error){publish({notice:error});return Promise.resolve(undefined);}
  last={id:(options.uuid??(()=>crypto.randomUUID()))(),command:structuredClone(command)};const envelope=last;write=new AbortController();const signal=write.signal;
  publish({busy:true,notice:"",pairing:null});
  pending=(async()=>{try{const receipt=await Promise.resolve().then(()=>client.command(envelope,signal));if(!active)return;accept(receipt,envelope.id);await refresh();return receipt;}catch{publish({uncertainId:envelope.id,notice:"操作结果未知；停止观察不代表后台撤销。请检查原命令收据，不会自动重发。"});return undefined;}finally{pending=undefined;publish({busy:false});}})();return pending;
 }
 async function inspect(){if(!active||!state.uncertainId||pending||state.busy)return;const id=state.uncertainId;write=new AbortController();publish({busy:true});try{const receipt=await client.receipt(id,write.signal);if(!active)return;if(receipt)accept(receipt,id);else publish({notice:"尚未找到原命令收据，结果仍未确认；不会自动重新签发或重发。"});await refresh();}catch{publish({notice:"原命令收据暂不可读，结果仍未知。"});}finally{publish({busy:false});}}
 return {refresh,submit,inspect,getState:()=>state,replaceSnapshot(snapshot:QqSnapshot){if(active){++generation;read?.abort();publish({snapshot,loading:false});}},hidePairing(){publish({pairing:null});},dispose(){active=false;++generation;read?.abort();write?.abort();last=undefined;state={...state,pairing:null};}};
}
