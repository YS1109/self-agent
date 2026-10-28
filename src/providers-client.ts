/** Only public metadata belongs in controller state. Never retain secret request bodies. */
export interface ProviderDefinition { name:string;base_url:string;models_endpoint:string;model:string;allow_insecure_http:boolean }
export interface NamedProvider { id:string;revision:number;definition:ProviderDefinition;enabled:boolean;credential_configured:boolean;credential_status:string;orphaned_credentials:number;updated_at:string }
export interface CredentialOperation { id:string;owner_kind:string;owner_id:string;kind:string;status:string;reason:string|null;can_cleanup:boolean;cleanup_status?:string|null;created_at:string;updated_at:string }
export interface NamedProviders {providers:NamedProvider[];credential_operations:CredentialOperation[]}
export type ProviderCommand = {type:"save_provider";id?:string;expected_revision?:number;definition:ProviderDefinition}|{type:"set_provider_enabled";id:string;expected_revision:number;enabled:boolean}|{type:"delete_provider";id:string;expected_revision:number};
export interface ProviderEnvelope {id:string;command:ProviderCommand}
export interface ProviderReceipt {command_id:string;status:"completed";provider_id:string;revision:number}
export interface CredentialReceipt {operation:CredentialOperation;replayed:boolean;no_secret_applied:boolean}
export interface ProvidersClient {
  registry(signal:AbortSignal):Promise<NamedProviders>;
  command(envelope:ProviderEnvelope,signal:AbortSignal):Promise<ProviderReceipt>;
  setCredential(providerId:string,body:{id:string;expected_revision:number;secret:string},signal:AbortSignal):Promise<CredentialReceipt>;
  deleteCredential(providerId:string,body:{id:string;expected_revision:number},signal:AbortSignal):Promise<CredentialReceipt>;
  cleanupCredential(targetId:string,body:{id:string},signal:AbortSignal):Promise<CredentialReceipt>;
}
export const EMPTY_PROVIDERS:NamedProviders={providers:[],credential_operations:[]};
export const providerUuid=(value:string)=>/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value);
const revision=(value:number)=>Number.isSafeInteger(value)&&value>0;
const bytes=(value:string)=>new TextEncoder().encode(value).length;
export function providerDefinitionError(value:ProviderDefinition):string|null {
 if(!value.name.trim()||Array.from(value.name).length>80||/[\x00-\x1f\x7f-\x9f]/.test(value.name))return "名称须为 1–80 个字符，不能包含控制字符。";
 if(bytes(value.model)>200||/[\x00-\x1f\x7f-\x9f]/.test(value.model))return "模型 ID 最多 200 字节，不能包含控制字符。";
 for(const text of [value.name,value.base_url,value.models_endpoint,value.model])if(/(?:\bsk-[\w-]{20,}|\bghp_|\bgithub_pat_|^Bearer )/.test(text))return "元数据疑似包含凭据，请仅在专用凭据输入框提交。";
 for(const text of [value.base_url,value.models_endpoint])if(bytes(text)>2048||text.trim()!==text||/[\x00-\x20\x7f-\x9f]/.test(text)||/[?#]/.test(text))return "地址最多 2048 字节，不能包含空白、查询参数或片段。";
 try {
  const base=new URL(value.base_url),models=new URL(value.models_endpoint||"models",base.href.replace(/\/+$/,"")+"/");
  if(!["http:","https:"].includes(base.protocol)||!base.hostname||base.username||base.password||models.username||models.password)return "地址只支持无用户凭证的 HTTP(S) URL。";
  if(models.origin!==base.origin)return "模型列表必须与模型服务使用相同协议、主机和端口。";
  const host=base.hostname.toLowerCase();const loopback=host==="localhost"||host==="[::1]"||/^127(?:\.\d{1,3}){3}$/.test(host);
  if(base.protocol==="http:"&&!loopback&&!value.allow_insecure_http)return "非本机明文 HTTP 可能暴露密钥和内容，必须显式确认风险。";
 }catch{return "请输入有效的模型服务绝对地址和同源列表路径。";}
 return null;
}
export function providerCommandError(command:ProviderCommand):string|null {
 if(command.id!==undefined&&(!providerUuid(command.id)||!revision(command.expected_revision!)))return "服务标识或修订无效，请刷新后核对。";
 return command.type==="save_provider"?providerDefinitionError(command.definition):null;
}
export function providerSecretError(secret:string):string|null {return !secret||bytes(secret)>2048||!/^[\x21-\x7e]+$/.test(secret)?"凭据须为 1–2048 字节的可见 ASCII 字符，不能包含空格或换行。":null;}
export const credentialStateLabel=(state:string)=>({pending:"已登记，等待系统凭据操作",inflight:"系统凭据操作中，结果未确定",completed:"操作完成",rejected:"操作被拒绝，未确认应用",unknown:"结果未知，不自动重放",orphaned:"已写入但未绑定，待清理",cleanup_required:"已脱离服务，待清理",cleaned:"清理已确认完成",configured_not_checked:"存在凭据绑定（未验证系统存储可读）",bound_not_checked:"存在凭据绑定（未验证系统存储可读）",not_configured:"未绑定凭据"})[state]??"状态未知，请刷新核对";
export const providerSafeError=(_error:unknown)=>"请求未确认或修订已改变。请刷新记录核对，必要时重新输入；不会自动重发，也不会回显凭据或底层错误。";
export function credentialReceiptNotice(receipt:CredentialReceipt):string {
 if(receipt.no_secret_applied||receipt.replayed)return "返回的是已有操作记录；这次输入没有写入。请核对原操作状态，不能视为新凭据已保存。";
 return receipt.operation.status==="completed"?"系统凭据操作已报告完成；绑定状态以刷新后的服务记录为准，不代表模型连接已验证。":"凭据操作已登记；尚不代表保存或删除完成，请查看操作状态。";
}
function object(value:unknown):value is Record<string,unknown>{return !!value&&typeof value==="object"&&!Array.isArray(value);}
export function validProviders(value:unknown):value is NamedProviders {
 if(!object(value)||!Array.isArray(value.providers)||value.providers.length>32||!Array.isArray(value.credential_operations)||value.credential_operations.length>1280)return false;
 return value.providers.every(p=>object(p)&&typeof p.id==="string"&&providerUuid(p.id)&&revision(p.revision as number)&&object(p.definition)&&["name","base_url","models_endpoint","model"].every(key=>typeof (p.definition as Record<string,unknown>)[key]==="string")&&typeof p.definition.allow_insecure_http==="boolean"&&typeof p.enabled==="boolean"&&typeof p.credential_configured==="boolean"&&typeof p.credential_status==="string"&&Number.isSafeInteger(p.orphaned_credentials)&&typeof p.updated_at==="string")
 &&value.credential_operations.every(validOperation);
}
function validOperation(value:unknown):value is CredentialOperation{return object(value)&&["id","owner_id","owner_kind","kind","status","created_at","updated_at"].every(key=>typeof value[key]==="string")&&providerUuid(value.id as string)&&typeof value.can_cleanup==="boolean"&&(value.reason===null||typeof value.reason==="string")&&(value.cleanup_status===undefined||value.cleanup_status===null||typeof value.cleanup_status==="string");}
export interface ProvidersState {registry:NamedProviders;busy:boolean;loading:boolean;notice:string;uncertainId:string|null;lastOperationId:string|null}
export function createProvidersController(client:ProvidersClient,initial:NamedProviders,changed:(state:ProvidersState)=>void,options:{uuid?:()=>string;pollMs?:number}={}) {
 let state:ProvidersState={registry:initial,busy:false,loading:false,notice:"",uncertainId:null,lastOperationId:null};
 let active=true,generation=0,read:AbortController|undefined,write:AbortController|undefined,timer:ReturnType<typeof setTimeout>|undefined;
 let pending:Promise<unknown>|undefined;
 const publish=(patch:Partial<ProvidersState>)=>{if(active){state={...state,...patch};changed(state);}};
 const poll=()=>{clearTimeout(timer);if(active&&(state.uncertainId||state.registry.credential_operations.some(op=>["pending","inflight"].includes(op.status))))timer=setTimeout(()=>void refresh(),options.pollMs??1500);};
 async function refresh(){if(!active)return;const version=++generation;read?.abort();read=new AbortController();publish({loading:true});try{const registry=await client.registry(read.signal);if(!active||version!==generation)return;if(!validProviders(registry))throw new Error("invalid_registry");publish({registry,...(state.uncertainId&&registry.credential_operations.some(op=>op.id===state.uncertainId)?{uncertainId:null,notice:"已通过只读查询找到原凭据操作，请以其持久状态为准；没有重新提交密钥。"}:{})});}catch{if(active&&version===generation)publish({notice:providerSafeError(null)});}finally{if(active&&version===generation){publish({loading:false});poll();}}}
 function mutate(send:(id:string,signal:AbortSignal)=>Promise<ProviderReceipt|CredentialReceipt>,credential:boolean):Promise<unknown>{
  if(!active||pending)return Promise.resolve(undefined);
  const id=(options.uuid??(()=>crypto.randomUUID()))();write=new AbortController();const signal=write.signal;publish({busy:true,notice:"",lastOperationId:credential?id:state.lastOperationId});
  pending=(async()=>{try{const receipt=await Promise.resolve().then(()=>send(id,signal));if(!active)return;
    if(credential){const result=receipt as CredentialReceipt;if(!validOperation(result.operation)||result.operation.id!==id||typeof result.replayed!=="boolean"||typeof result.no_secret_applied!=="boolean")throw new Error("invalid_receipt");publish({notice:credentialReceiptNotice(result),uncertainId:null});}
    else{const result=receipt as ProviderReceipt;if(result.command_id!==id||result.status!=="completed")throw new Error("invalid_receipt");publish({notice:"服务元数据已确认；启用、系统凭据可读与模型实际调用仍是独立状态。",uncertainId:null});}
    await refresh();return receipt;
  }catch{publish({uncertainId:id,notice:"提交结果未知，请用刷新恢复。不会自动重发；输入已清空，新尝试必须由你显式提交新的操作。"});return undefined;}finally{pending=undefined;publish({busy:false});poll();}})();return pending;
 }
 return {refresh,getState:()=>state,
  command(command:ProviderCommand){const error=providerCommandError(command);if(error){publish({notice:error});return Promise.resolve(undefined);}return mutate((id,signal)=>client.command({id,command},signal),false);},
  setCredential(provider:NamedProvider,secret:string,confirmed:boolean){const error=providerSecretError(secret);if(!confirmed||error||!providerUuid(provider.id)||!revision(provider.revision)){publish({notice:error??"请先显式确认写入本机系统凭据存储。"});return Promise.resolve(undefined);}return mutate(async(id,signal)=>{try{return await client.setCredential(provider.id,{id,expected_revision:provider.revision,secret},signal);}finally{secret="";}},true);},
  deleteCredential(provider:NamedProvider,confirmed:boolean){if(!confirmed||!providerUuid(provider.id)||!revision(provider.revision)){publish({notice:"请先显式确认解绑并删除此服务的凭据。"});return Promise.resolve(undefined);}return mutate((id,signal)=>client.deleteCredential(provider.id,{id,expected_revision:provider.revision},signal),true);},
  cleanupCredential(operation:CredentialOperation,confirmed:boolean){if(!confirmed||!operation.can_cleanup||!providerUuid(operation.id)){publish({notice:"此操作尚不可清理，或未显式确认。"});return Promise.resolve(undefined);}return mutate((id,signal)=>client.cleanupCredential(operation.id,{id},signal),true);},
  replaceRegistry(registry:NamedProviders){if(active){++generation;read?.abort();publish({registry,loading:false});poll();}},
  dispose(){active=false;++generation;read?.abort();write?.abort();clearTimeout(timer);},
 };
}

export function namedModelRequest(settings:{provider_source?:string;provider_id?:string|null;base_url:string;models_endpoint?:string},apiKey?:string) {
 return settings.provider_source==="named"?{provider_source:"named",provider_id:settings.provider_id??undefined}:{provider_source:settings.provider_source??"manual",base_url:settings.base_url,models_endpoint:settings.models_endpoint??"",...(apiKey?{api_key:apiKey}:{})};
}

export function consumeCredentialInput(input:{value:string}|null):string {const secret=input?.value??"";if(input)input.value="";return secret;}
export function credentialReason(reason:string|null):string {return ({vault_rejected:"系统凭据操作被拒绝或不可用，没有退化为明文存储。",vault_outcome_unknown:"系统操作结果无法确认，请检查持久记录，不自动重放。",daemon_interrupted_no_replay:"后台在操作期间中断，结果未知且没有重放。",provider_revision_changed:"服务修订已变化，新写入没有绑定当前服务，可能需要清理。",credential_detached:"地址或服务配置改变，旧凭据已脱离绑定。",credential_deleted:"已确认删除目标系统凭据。"})[reason??""]??(reason?"后台记录了失败或待清理原因；未展示底层错误。":"");}
