/** The H5 client only speaks the narrow same-origin gateway API. It never
 * imports the desktop owner API, a daemon token, or a path-based file endpoint. */
export type GatewayCode = "ok"|"device_offline"|"outcome_unknown"|"unauthorized"|"forbidden"|"invalid_request"|"conflict"|"busy"|"not_found"|"cursor_expired"|"internal";
export interface GatewayResult<T=unknown> {code:GatewayCode;data?:T}
export interface Permissions {view:boolean;read_results:boolean;read_artifacts:boolean;run:boolean;cancel:boolean;answer:boolean}
export interface BrowserSession {csrf:string;paired:boolean;mode?:"fixture"|"self_hosted";device_id?:string;principal_id?:string;binding_revision?:number;permissions?:Permissions;expires_at_ms?:number}
export interface PairStatus {status:"pending"|"active"|"revoked";principal_id:string;binding_revision:number;expires_at_ms:number;permissions:Permissions;session_ids:string[]}
export type MobileCommand = {type:"run";content:string}|{type:"cancel";task_id:string}|{type:"answer";request_id:string;expected_revision:number;answer:string}|{type:"steer";task_id:string;content:string}|{type:"continue";task_id:string;content:string|null};
export type MobileRpc = {type:"sessions"}|{type:"session";session_id:string}|{type:"task";task_id:string}|{type:"events";session_id:string;after:number}|{type:"changes";task_id:string}|{type:"artifacts";task_id:string}|{type:"artifact";resource_id:string}|{type:"receipt";command_id:string}|{type:"command";command_id:string;session_id:string;command:MobileCommand};
export interface SessionSummary {id:string;title:string}
export interface Message {id:string;role:string;content:string;task_id?:string}
export interface MobileTask {id:string;session_id:string;title:string;status:string;result?:string;stream_output?:string;error?:string;waiting_request_id?:string;execution_cleanup?:"pending"|"settled"|"unknown";source_principal_id?:string;source_binding_revision?:number}
export interface Question {id:string;task_id:string;session_id:string;kind:"question"|"approval";revision:number;status:string;prompt:string;expires_at_ms:number}
export interface SessionSnapshot {session:SessionSummary&{messages:Message[]};tasks:MobileTask[];questions:Question[];cursor:number;truncated?:boolean}
export interface ArtifactSummary {id:string;task_id:string;name:string;bytes:number;truncated:boolean}
export interface TextArtifact extends ArtifactSummary {content:string}
export interface EventBatch {cursor:number;events:unknown[]}
export interface MobileApi {
 session(signal:AbortSignal):Promise<GatewayResult<BrowserSession>>;
 pair(body:{device_id:string;code:string;label:string},signal:AbortSignal):Promise<GatewayResult<PairStatus>>;
 pairStatus(signal:AbortSignal):Promise<GatewayResult<PairStatus>>;
 rpc<T>(rpc:MobileRpc,signal:AbortSignal):Promise<GatewayResult<T>>;
 logout(signal:AbortSignal):Promise<GatewayResult>;
 watch(sessionId:string,after:number,receive:(result:GatewayResult<EventBatch>)=>void,failed:()=>void):()=>void;
}
const CODES = new Set<GatewayCode>(["ok","device_offline","outcome_unknown","unauthorized","forbidden","invalid_request","conflict","busy","not_found","cursor_expired","internal"]);
const RETRYABLE_READ_CODES = new Set<GatewayCode>(["device_offline","busy","internal","outcome_unknown"]);
export function validId(id:string):boolean{return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id);}
export function inputError(text:string):string|null{return !text.trim()||text.includes("\0")||new TextEncoder().encode(text).length>8192?"请输入非空文本，最多 8192 UTF-8 字节。":null;}
export const codeMessage=(code:GatewayCode):string=>({ok:"已确认",device_offline:"设备离线，未排队执行。",outcome_unknown:"结果未知；仅检查原命令回执，不自动重发。",unauthorized:"登录或设备凭据已失效，请重新配对。",forbidden:"当前授权不允许此操作，或授权已撤销。",invalid_request:"请求字段无效，未扩大权限。",conflict:"标识或修订冲突，请刷新核对。",busy:"在线请求已达上限；没有加入离线队列。",not_found:"暂未找到记录；这不能证明未知命令永远不会提交。",cursor_expired:"事件游标已失效，正在读取权威快照。",internal:"后台无法确认请求，请检查原命令回执。"})[code];

export function createHttpMobileApi(fetcher:typeof fetch=fetch,eventSource:(url:string)=>EventSource=url=>new EventSource(url)):MobileApi {
 let csrf="";
 async function request<T>(path:string,method:string,body:unknown,signal:AbortSignal):Promise<GatewayResult<T>>{
  const response=await fetcher(`/gateway/api/${path}`,{method,credentials:"same-origin",cache:"no-store",redirect:"error",headers:method==="GET"?undefined:{"Content-Type":"application/json","x-csrf-token":csrf},body:method==="GET"?undefined:JSON.stringify(body),signal:AbortSignal.any([signal,AbortSignal.timeout(25000)])});
  if(!response.body)throw new Error("网关响应为空");
  const reader=response.body.getReader(),parts:Uint8Array[]=[];let size=0;
  try{while(true){const item=await reader.read();if(item.done)break;size+=item.value.byteLength;if(size>262144){await reader.cancel();throw new Error("网关响应超过限制");}parts.push(item.value);}}finally{reader.releaseLock();}
  const bytes=new Uint8Array(size);let offset=0;for(const part of parts){bytes.set(part,offset);offset+=part.byteLength;}const text=new TextDecoder("utf-8",{fatal:true}).decode(bytes);
  const value:GatewayResult<T>=JSON.parse(text);if(!value||!CODES.has(value.code))throw new Error("网关响应无效");
  return value;
 }
 return {
  session:async signal=>{const result=await request<BrowserSession>("session","GET",null,signal);if(result.code==="ok"&&result.data)csrf=result.data.csrf;return result;},
  pair:(body,signal)=>request("pair","POST",body,signal),pairStatus:signal=>request("pair-status","GET",null,signal),
  rpc:<T>(rpc:MobileRpc,signal:AbortSignal)=>request<T>("rpc","POST",rpc,signal),
  logout:signal=>request("logout","POST",{},signal),
  watch:(sessionId,after,receive,failed)=>{
   if(!validId(sessionId)||!Number.isSafeInteger(after)||after<0)throw new Error("事件范围无效");
   const source=eventSource(`/gateway/api/events?session_id=${encodeURIComponent(sessionId)}&after=${after}`);let closed=false;
   const close=()=>{if(!closed){closed=true;source.close();}};
   source.addEventListener("gateway",event=>{if(closed)return;try{const text=(event as MessageEvent<string>).data;if(new TextEncoder().encode(text).length>262144)throw new Error();const value=JSON.parse(text) as GatewayResult<EventBatch>;if(!CODES.has(value.code))throw new Error();receive(value);}catch{close();failed();}});
   source.onerror=()=>{if(closed)return;close();failed();};return close;
  },
 };
}

export function validCommandReceipt(data:unknown,commandId:string,sessionId:string):boolean {
 if(!data||typeof data!=="object")return false;
 const receipt=data as Record<string,unknown>;
 return receipt.command_id===commandId&&receipt.session_id===sessionId&&receipt.status==="accepted"&&
  (receipt.task_id===undefined||(typeof receipt.task_id==="string"&&validId(receipt.task_id)))&&
  (receipt.supplement_id===undefined||(typeof receipt.supplement_id==="string"&&validId(receipt.supplement_id)));
}
export interface UnknownCommand {command_id:string;session_id:string;device_id:string;principal_id:string}
export interface MobileState {session:BrowserSession|null;sessions:SessionSummary[];selected:string|null;snapshot:SessionSnapshot|null;busy:boolean;notice:string;unknown:UnknownCommand|null;pairing:PairStatus|null;connected:boolean}
export interface MobileStorage {getItem(key:string):string|null;setItem(key:string,value:string):void;removeItem(key:string):void}
const UNKNOWN_KEY="self-agent.gateway.unknown-command.v1";
const initial=():MobileState=>({session:null,sessions:[],selected:null,snapshot:null,busy:false,notice:"",unknown:null,pairing:null,connected:false});

/** Controller is single-flight for mutations and never retries one. Read-only
 * reconciliation uses the original identity after timeout, Close or refresh. */
export function createMobileController(api:MobileApi,changed:(state:MobileState)=>void,options:{uuid?:()=>string;storage?:MobileStorage}={}) {
 let state=initial(),disposed=false,epoch=0,readRevision=0,closeStream:(()=>void)|null=null,reconnect:ReturnType<typeof setTimeout>|null=null;
 let cursor=0,refreshingEpoch:number|null=null,refreshAgain=false,restartAgain=false,recoveryNotice:string|null=null;const observers=new Set<AbortController>();
 const emit=(patch:Partial<MobileState>)=>{if(disposed)return;state={...state,...patch};changed(state);};
 const observer=()=>{const controller=new AbortController();observers.add(controller);return controller;};
 const runRead=async<T>(f:(signal:AbortSignal)=>Promise<T>)=>{const c=observer();try{return await f(c.signal);}finally{observers.delete(c);}};
 const persist=(value:UnknownCommand|null)=>{try{if(value)options.storage?.setItem(UNKNOWN_KEY,JSON.stringify(value));else options.storage?.removeItem(UNKNOWN_KEY);}catch{/* Memory still keeps the original command; UI never claims durable login. */}};
 function restoreUnknown(session:BrowserSession):UnknownCommand|null {try{const raw=options.storage?.getItem(UNKNOWN_KEY);if(!raw)return null;const value=JSON.parse(raw) as UnknownCommand;if(validId(value.command_id)&&validId(value.session_id)&&value.device_id===session.device_id&&value.principal_id===session.principal_id)return value;}catch{}return null;}
 function stopStream(){closeStream?.();closeStream=null;if(reconnect)clearTimeout(reconnect);reconnect=null;}
 function recoverRead(viewEpoch:number,notice:string){
  if(disposed||viewEpoch!==epoch||!state.session?.paired)return;
  stopStream();
  recoveryNotice=notice;emit({connected:false,notice});
  // Refresh authoritative state before resubscribing. This timer never retries
  // a mutation or clears the original unknown command's reconciliation ID.
  reconnect=setTimeout(()=>{reconnect=null;if(disposed||viewEpoch!==epoch)return;if(state.selected)void refresh(viewEpoch,true);else void list();},2000);
 }
 function startStream(viewEpoch:number){
  stopStream();if(disposed||viewEpoch!==epoch||!state.selected||!state.session?.paired)return;
  const sessionId=state.selected;
  closeStream=api.watch(sessionId,cursor,result=>{
   if(disposed||viewEpoch!==epoch)return;
   if(result.code==="cursor_expired"){stopStream();const recoveryEpoch=++epoch;++readRevision;cursor=0;emit({connected:false,notice:codeMessage(result.code)});void refresh(recoveryEpoch,true);return;}
   if(result.code!=="ok"){if(RETRYABLE_READ_CODES.has(result.code))recoverRead(viewEpoch,codeMessage(result.code));else{stopStream();emit({connected:false,notice:codeMessage(result.code)});}return;}
   const next=result.data?.cursor;if(typeof next!=="number"||!Number.isSafeInteger(next)||next<cursor)return;
   emit({connected:true,...(state.notice===recoveryNotice?{notice:state.unknown?codeMessage("outcome_unknown"):""}:{})});recoveryNotice=null;if(next>cursor){cursor=next;void refresh(viewEpoch,false);}
  },()=>recoverRead(viewEpoch,"连接中断；任务仍由本机后台保存。正在只读恢复事件。"));
 }
 async function refresh(viewEpoch=epoch,restart=false):Promise<void>{
  if(!state.selected||!state.session?.paired||viewEpoch!==epoch)return;
  if(refreshingEpoch===viewEpoch){refreshAgain=true;restartAgain ||=restart;return;}refreshingEpoch=viewEpoch;refreshAgain=false;restartAgain=false;const selected=state.selected,revision=++readRevision;
  try{const result=await runRead(signal=>api.rpc<SessionSnapshot>({type:"session",session_id:selected},signal));if(disposed||viewEpoch!==epoch||revision!==readRevision)return;
   if(result.code!=="ok"||!result.data){if(RETRYABLE_READ_CODES.has(result.code))recoverRead(viewEpoch,codeMessage(result.code));else{stopStream();emit({notice:codeMessage(result.code),connected:false});}return;}
   if(result.data.session.id!==selected||!Number.isSafeInteger(result.data.cursor)||result.data.cursor<0)throw new Error();
   cursor=Math.max(cursor,result.data.cursor);emit({snapshot:result.data});if(restart)startStream(viewEpoch);
  }catch{if(!disposed&&viewEpoch===epoch&&revision===readRevision)recoverRead(viewEpoch,"暂时无法读取会话，正在只读恢复；未发送执行命令。");}
  finally{if(refreshingEpoch===viewEpoch){refreshingEpoch=null;if(refreshAgain&&!disposed){const nextRestart=restartAgain;refreshAgain=false;restartAgain=false;void refresh(epoch,nextRestart);}}}
 }
 async function select(id:string){if(!validId(id)||!state.sessions.some(s=>s.id===id))return;stopStream();epoch++;readRevision++;cursor=0;emit({selected:id,snapshot:null,connected:false});await refresh(epoch,true);}
 async function list(){
  const viewEpoch=epoch;
  try{
   const result=await runRead(signal=>api.rpc<{sessions:SessionSummary[]}>({type:"sessions"},signal));
   if(disposed||viewEpoch!==epoch)return;
   if(result.code==="ok"&&result.data){
    emit({sessions:result.data.sessions});
    if(!state.selected&&result.data.sessions[0])await select(result.data.sessions[0].id);
   }else if(RETRYABLE_READ_CODES.has(result.code)){
    recoverRead(viewEpoch,codeMessage(result.code));
   }else{
    stopStream();emit({notice:codeMessage(result.code),connected:false});
   }
  }catch{
   if(!disposed&&viewEpoch===epoch)recoverRead(viewEpoch,"暂时无法读取会话列表，正在只读恢复；未发送执行命令。");
  }
 }
 async function open(){const current=++epoch;try{const result=await runRead(signal=>api.session(signal));if(disposed||current!==epoch)return;if(result.code!=="ok"||!result.data){emit({notice:codeMessage(result.code)});return;}emit({session:result.data,unknown:restoreUnknown(result.data),notice:result.data.paired?"":"请使用本机签发的配对码。网关重启后需要重新配对。"});if(result.data.paired)await list();}catch{if(!disposed&&current===epoch)emit({notice:"无法连接自托管网关。没有向设备发送命令。"});}}
 async function pair(body:{device_id:string;code:string;label:string}){
  if(state.busy||state.session?.paired||!validId(body.device_id)||!/^[0-9a-f]{64}$/.test(body.code)||!body.label.trim()||body.label.length>80)return;
  emit({busy:true});try{const result=await runRead(signal=>api.pair(body,signal));if(disposed)return;emit({pairing:result.data??null,notice:result.code==="ok"?"请在本机核对身份并确认；尚未获得任务权限。":codeMessage(result.code)});}catch{emit({notice:"配对结果未知，请只读检查配对状态，不自动重交配对码。"});}finally{emit({busy:false});}
 }
 async function pairStatus(){if(state.busy)return;emit({busy:true});try{const result=await runRead(signal=>api.pairStatus(signal));if(disposed)return;emit({pairing:result.data??null,notice:codeMessage(result.code)});if(result.code==="ok"&&result.data?.status==="active")await open();}catch{emit({notice:"无法读取配对状态，未重新提交配对。"});}finally{emit({busy:false});}}
 async function submit(command:MobileCommand){
  const session=state.session,selected=state.selected;if(state.busy||state.unknown||!session?.paired||!session.device_id||!session.principal_id||!selected)return;
  if((command.type==="run"||command.type==="steer")&&inputError(command.content))return;
  if(command.type==="answer"&&inputError(command.answer))return;
  if(command.type==="continue"&&command.content!==null&&inputError(command.content))return;
  const pending={command_id:(options.uuid??(()=>crypto.randomUUID()))(),session_id:selected,device_id:session.device_id,principal_id:session.principal_id};persist(pending);emit({busy:true,unknown:pending,notice:"正在提交一次命令；断连不会自动重发。"});
  try{const result=await runRead(signal=>api.rpc({type:"command",command_id:pending.command_id,session_id:selected,command},signal));if(disposed)return;
   if(result.code==="ok"&&!validCommandReceipt(result.data,pending.command_id,pending.session_id))throw new Error("receipt mismatch");
   if(result.code!=="outcome_unknown"&&result.code!=="internal"){persist(null);emit({unknown:null});}emit({notice:codeMessage(result.code)});if(result.code==="ok")await refresh();
  }catch{emit({notice:codeMessage("outcome_unknown")});}finally{emit({busy:false});}
 }
 async function reconcile(){const pending=state.unknown;if(!pending||state.busy)return;emit({busy:true});try{const result=await runRead(signal=>api.rpc({type:"receipt",command_id:pending.command_id},signal));if(disposed)return;if(result.code==="ok"&&validCommandReceipt(result.data,pending.command_id,pending.session_id)){persist(null);emit({unknown:null,notice:"原命令已有权威回执；没有重发。"});await refresh();}else emit({notice:result.code==="ok"?"回执标识不匹配，原命令仍未知；不会自动重发。":codeMessage(result.code)});}catch{emit({notice:"原命令仍未确认；保留标识，不自动执行新命令。"});}finally{emit({busy:false});}}
 async function logout(){if(state.busy)return;stopStream();epoch++;emit({busy:true});try{const result=await runRead(signal=>api.logout(signal));if(disposed)return;if(result.code==="ok"){state=initial();changed(state);await open();}else emit({notice:codeMessage(result.code)});}catch{emit({notice:"退出结果未确认，请关闭页面并检查本机绑定；未宣称凭据已撤销。"});}finally{emit({busy:false});}}
 return {getState:()=>state,open,pair,pairStatus,select,refresh,submit,reconcile,logout,
  read:<T>(rpc:Exclude<MobileRpc,{type:"command"}>)=>runRead(signal=>api.rpc<T>(rpc,signal)),
  dispose(){disposed=true;epoch++;stopStream();for(const c of observers)c.abort();observers.clear();},
 };
}
