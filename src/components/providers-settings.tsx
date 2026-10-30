import { useEffect, useRef, useState } from "react";
import { createProvidersController, credentialStateLabel, consumeCredentialInput, credentialReason, providerCommandError, type CredentialOperation, type NamedProvider, type NamedProviders, type ProviderCommand, type ProviderDefinition, type ProvidersClient, type ProvidersState } from "../providers-client";
import { Button } from "./ui/button";
import { Select } from "./ui/select";
import { useModelCatalog, ModelSelect, type ModelCatalog } from "./model-select";
import type { AgentProfile, Settings } from "../types";
import "./providers-settings.css";
export function ProvidersSettings({registry,client,disabled,onRegistryChange}:{registry:NamedProviders;client:ProvidersClient;disabled:boolean;onRegistryChange?:(registry:NamedProviders)=>void}) {
 const [state,setState]=useState<ProvidersState>({registry,busy:false,loading:false,notice:"",uncertainId:null,lastOperationId:null});
 const controller=useRef<ReturnType<typeof createProvidersController>|null>(null),notify=useRef(onRegistryChange),initial=useRef(registry);notify.current=onRegistryChange;initial.current=registry;
 useEffect(()=>{const current=createProvidersController(client,initial.current,next=>{setState(next);if(next.registry!==initial.current){initial.current=next.registry;notify.current?.(next.registry);}});controller.current=current;void current.refresh();return()=>{current.dispose();if(controller.current===current)controller.current=null;};},[client]);
 useEffect(()=>{if(controller.current?.getState().registry!==registry)controller.current?.replaceRegistry(registry);},[registry]);
 const blocked=disabled||state.busy;
 return <section className="providers-settings" aria-label="命名模型服务与系统凭据">
  <div className="provider-heading"><h3>命名模型服务</h3><Button type="button" size="small" variant="secondary" disabled={disabled||state.loading} onClick={()=>void controller.current?.refresh()}>{state.loading?"读取中…":"刷新服务与凭据操作"}</Button></div>
  <p className="help-text">Native 全局和角色可分别选择服务；任务固定服务修订，编辑或停用不会把旧任务静默转投其他服务。新服务默认停用。</p>
  <p className="execution-warning">凭据只提交给 Rust 后台，由系统凭据存储保存。系统存储不可用或被锁定时，不退化为明文文件。绑定存在不等于当前可读取，也不证明模型调用成功。</p>
  <p className="help-text">关闭页面只停止观察，不撤回已接受的后台操作。网络结果未知时只刷新，不自动重发密钥；操作 ID 可用于核对记录。</p>
  {state.notice&&<p role="status" className="provider-notice">{state.notice}</p>}
  {state.uncertainId&&<p className="execution-warning">待核对的原操作：<code>{state.uncertainId}</code>。若确需再次提交，须重新输入并明确确认；新尝试使用新的操作 ID。</p>}
  {state.lastOperationId&&<p className="help-text">最近提交的凭据操作：<code>{state.lastOperationId}</code></p>}
  {state.registry.providers.map(provider=><ProviderCard key={`${provider.id}:${provider.revision}`} provider={provider} disabled={blocked} credentialPending={state.registry.credential_operations.some(op=>op.owner_id===provider.id&&["pending","inflight"].includes(op.status))}
    onCommand={command=>controller.current?.command(command)??Promise.resolve()} onSet={(secret,confirmed)=>controller.current?.setCredential(provider,secret,confirmed)??Promise.resolve()} onDelete={confirmed=>controller.current?.deleteCredential(provider,confirmed)??Promise.resolve()} />)}
  <details className="provider-editor"><summary>添加命名服务 · 最多 32 个</summary><ProviderEditor disabled={blocked||state.registry.providers.length>=32} onCommand={command=>controller.current?.command(command)??Promise.resolve()} /></details>
  <section aria-label="系统凭据操作记录"><h4>系统凭据操作记录 · {state.registry.credential_operations.length}</h4>
   <p className="help-text">删除服务或改动地址会解除旧凭据绑定，但系统中的遗留凭据需要显式清理。未知写入可能仍保留旧绑定；删除已解绑也不等于系统中已删除。不要用重新点击推断原操作失败。</p>
   {state.registry.credential_operations.slice().reverse().map(operation=><CredentialOperationCard key={`${operation.id}:${operation.updated_at}`} operation={operation} disabled={blocked} onCleanup={confirmed=>controller.current?.cleanupCredential(operation,confirmed)??Promise.resolve()} />)}
   {!state.registry.credential_operations.length&&<p className="help-text">尚无凭据操作记录</p>}
  </section>
 </section>;
}
export function ProviderEditor({provider,disabled,onCommand}:{provider?:NamedProvider;disabled:boolean;onCommand:(command:ProviderCommand)=>Promise<unknown>}) {
 const [definition,setDefinition]=useState<ProviderDefinition>(provider?.definition??{name:"",base_url:"",models_endpoint:"",model:"",allow_insecure_http:false}),[error,setError]=useState("");
 const changedAddress=!!provider&&(definition.base_url!==provider.definition.base_url||definition.models_endpoint!==provider.definition.models_endpoint);
 return <form className="provider-fields" onSubmit={event=>{event.preventDefault();event.stopPropagation();const command:ProviderCommand={type:"save_provider",...(provider?{id:provider.id,expected_revision:provider.revision}:{}),definition};const error=providerCommandError(command);setError(error??"");if(!error)void onCommand(command);}}>
  <label>服务名称<input aria-label="命名服务名称" value={definition.name} maxLength={80} required disabled={disabled} onChange={event=>setDefinition({...definition,name:event.target.value})}/></label>
  <label>API 基础地址<input aria-label="命名服务 API 基础地址" type="url" value={definition.base_url} maxLength={2048} required disabled={disabled} placeholder="https://example.com/v1" onChange={event=>setDefinition({...definition,base_url:event.target.value,allow_insecure_http:false})}/></label>
  <label>模型列表地址（可选，同源路径或 URL）<input aria-label="命名服务模型列表地址" value={definition.models_endpoint} maxLength={2048} disabled={disabled} placeholder="留空使用 models" onChange={event=>setDefinition({...definition,models_endpoint:event.target.value})}/></label>
  <label>默认模型 ID（可稍后填写）<input aria-label="命名服务默认模型" value={definition.model} maxLength={200} disabled={disabled} onChange={event=>setDefinition({...definition,model:event.target.value})}/></label>
  <label className="execution-checkbox"><input type="checkbox" checked={definition.allow_insecure_http} disabled={disabled} onChange={event=>setDefinition({...definition,allow_insecure_http:event.target.checked})}/>我明确允许非本机明文 HTTP，并知晓密钥及内容可能被窃听或篡改</label>
  {changedAddress&&<p className="execution-warning">地址已变化：保存会解除旧凭据绑定，不向新地址静默转发旧密钥；旧系统凭据可能需要在操作记录中清理。</p>}
  <p className="help-text">名称、地址与模型字段不得填写密钥。这里仅保存元数据，不探测网络，也不自动启用服务或写入凭据。</p>
  {error&&<p role="alert" className="task-error">{error}</p>}
  <Button type="submit" size="small" disabled={disabled}>{provider?"保存服务修订":"保存新服务（默认停用）"}</Button>
 </form>;
}
export function ProviderCard({provider,disabled,credentialPending,onCommand,onSet,onDelete}:{provider:NamedProvider;disabled:boolean;credentialPending:boolean;onCommand:(command:ProviderCommand)=>Promise<unknown>;onSet:(secret:string,confirmed:boolean)=>Promise<unknown>;onDelete:(confirmed:boolean)=>Promise<unknown>}) {
 const submitting=useRef(false);
 const input=useRef<HTMLInputElement>(null),[confirmSet,setConfirmSet]=useState(false),[confirmDelete,setConfirmDelete]=useState(false),[remove,setRemove]=useState(false);
 useEffect(()=>{const field=input.current;return()=>{if(field)field.value="";};},[]);
 const credentialDisabled=disabled||credentialPending;
 return <article className="provider-card" aria-label={`命名服务 ${provider.definition.name}`}>
  <div className="provider-heading"><strong>{provider.definition.name}</strong><span>{provider.enabled?"已启用（不是连接验证）":"已停用"} · 修订 {provider.revision}</span></div>
  <p><code>{provider.definition.base_url}</code></p><p className="help-text">模型列表：{provider.definition.models_endpoint} · 默认模型：{provider.definition.model||"尚未选择"}</p>
  <p className="help-text">凭据：{credentialStateLabel(provider.credential_status)} · 待清理记录 {provider.orphaned_credentials}</p>
  <Button type="button" size="small" variant="secondary" disabled={disabled} onClick={()=>void onCommand({type:"set_provider_enabled",id:provider.id,expected_revision:provider.revision,enabled:!provider.enabled})}>{provider.enabled?"停用服务":"启用服务"}</Button>
  <details className="provider-editor"><summary>编辑服务元数据</summary><ProviderEditor provider={provider} disabled={disabled} onCommand={onCommand}/></details>
  <ProviderModelDiscovery provider={provider} disabled={disabled} onCommand={onCommand}/>
  <form className="provider-fields" aria-label="系统凭据写入" onSubmit={event=>{event.preventDefault();event.stopPropagation();if(!confirmSet||credentialDisabled||submitting.current)return;submitting.current=true;const secret=consumeCredentialInput(input.current);setConfirmSet(false);void onSet(secret,true).finally(()=>{submitting.current=false;});}}>
   <label>新的访问密钥<input ref={input} type="password" aria-label="写入系统存储的新访问密钥" autoComplete="off" spellCheck={false} maxLength={2048} disabled={credentialDisabled} placeholder="不会回填、显示或记录已保存密钥"/></label>
   <label className="execution-checkbox"><input type="checkbox" checked={confirmSet} disabled={credentialDisabled} onChange={event=>setConfirmSet(event.target.checked)}/>确认将本次新输入写入系统凭据存储；已核对服务、地址与修订</label>
   <Button type="submit" size="small" disabled={credentialDisabled||!confirmSet}>提交新凭据操作（输入随即清空）</Button>
  </form>
  {credentialPending&&<p role="status" className="help-text">此服务有待完成的凭据操作；请刷新查看，不重复提交。</p>}
  <div className="provider-actions"><label className="execution-checkbox"><input type="checkbox" checked={confirmDelete} disabled={credentialDisabled||!provider.credential_configured} onChange={event=>setConfirmDelete(event.target.checked)}/>确认解除当前绑定并申请删除系统凭据</label><Button type="button" size="small" variant="secondary" disabled={credentialDisabled||!confirmDelete||!provider.credential_configured} onClick={()=>{setConfirmDelete(false);void onDelete(true);}}>解绑并删除凭据</Button></div>
  <div className="provider-actions"><label className="execution-checkbox"><input type="checkbox" checked={remove} disabled={disabled} onChange={event=>setRemove(event.target.checked)}/>确认删除服务；遗留凭据不自动遗忘</label><Button type="button" size="small" variant="secondary" disabled={disabled||!remove} onClick={()=>void onCommand({type:"delete_provider",id:provider.id,expected_revision:provider.revision})}>删除服务</Button></div>
 </article>;
}
export function CredentialOperationCard({operation,disabled,onCleanup}:{operation:CredentialOperation;disabled:boolean;onCleanup:(confirmed:boolean)=>Promise<unknown>}) {
 const [confirmed,setConfirmed]=useState(false);
 return <article className="provider-operation"><strong>{({set:"写入",delete:"解绑删除",cleanup:"遗留清理"})[operation.kind]??"凭据操作"} · {credentialStateLabel(operation.status)}</strong><p className="help-text">服务 <code>{operation.owner_id}</code> · 操作 <code>{operation.id}</code></p>
  {operation.cleanup_status&&<p className="provider-notice">{operation.cleanup_status==="completed" ? operation.status==="unknown" ? "清理已确认完成；原操作结果仍未知。" : "独立清理已确认完成；不改写原操作结果。" : `独立清理状态：${credentialStateLabel(operation.cleanup_status)}。原操作结果保持不变。`}</p>}
  {operation.reason&&<p className="help-text">{credentialReason(operation.reason)}</p>}
  {operation.status==="unknown"&&<p className="execution-warning">{operation.cleanup_status==="completed" ? "原操作历史结果仍未知；关联遗留凭据已清理，不应把原写入或删除标为成功，也不会自动重试。" : "原操作结果未知；旧绑定可能仍生效，或删除已经脱离服务。只按后台允许的清理入口处理，不自动重放。"}</p>}
  {operation.can_cleanup&&<><label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled} onChange={event=>setConfirmed(event.target.checked)}/>确认清理此记录关联的已脱离凭据</label><Button type="button" variant="secondary" size="small" disabled={disabled||!confirmed} onClick={()=>{setConfirmed(false);void onCleanup(true);}}>申请系统凭据清理</Button></>}
 </article>;
}
export function NamedProviderSelect({providers,value,onChange,disabled,label="命名服务",inheritLabel="请选择命名服务"}:{providers:NamedProvider[];value:string|null|undefined;onChange:(id:string|null)=>void;disabled:boolean;label?:string;inheritLabel?:string}) {
 return <label>{label}<Select aria-label={label} value={value??""} disabled={disabled} onValueChange={value=>onChange(value||null)} options={[{value:"",label:inheritLabel},...providers.map(p=>({value:p.id,label:`${p.definition.name}${p.enabled?"":" · 已停用"}`,disabled:!p.enabled&&p.id!==value})),...(value&&!providers.some(p=>p.id===value)?[{value,label:"原服务不可用，请显式重新选择",disabled:true}]:[])]}/></label>;
}
export function RoleProviderModel({agent,settings,providers,catalog,disabled,onChange,label}:{agent:AgentProfile;settings:Settings;providers:NamedProvider[];catalog:ModelCatalog;disabled:boolean;onChange:(patch:Partial<AgentProfile>)=>void;label:string}) {
 const id=agent.provider_id??(settings.provider_source==="named"?settings.provider_id:null);
 const namedCatalog=useModelCatalog({provider_source:"named",provider_id:id??undefined},!!id&&!disabled);
 const selected=providers.find(provider=>provider.id===id);
 return <><NamedProviderSelect label={`${label} · Native 服务`} providers={providers} value={agent.provider_id} disabled={disabled} inheritLabel="继承全局服务" onChange={provider_id=>onChange({provider_id,model:""})}/>
  <ModelSelect label={`${label} · 预设模型`} value={agent.model} onChange={model=>onChange({model})} catalog={id?namedCatalog:catalog} disabled={disabled} emptyLabel={id?`使用服务默认模型${selected?.definition.model?` · ${selected.definition.model}`:""}`:"跟随主智能体模型（不固定）"}/>
  {id&&<p className="help-text">服务仅固定于新任务；空模型使用该命名服务的默认模型，不借用其他服务的主模型。</p>}</>;
}

export function ProviderModelDiscovery({provider,disabled,onCommand}:{provider:NamedProvider;disabled:boolean;onCommand:(command:ProviderCommand)=>Promise<unknown>}) {
 const [requested,setRequested]=useState(false);
 const catalog=useModelCatalog({provider_source:"named",provider_id:provider.id},requested&&!disabled);
 return <div className="provider-editor"><Button type="button" variant="secondary" size="small" disabled={disabled||catalog.loading} onClick={()=>{setRequested(true);if(requested)catalog.refresh();}}>显式发现此服务的模型列表</Button>
  <p className="help-text">仅使用已保存的服务 ID 和系统绑定凭据，不接受地址或密钥覆盖；已停用配置也可发现。模型列表不是生成请求成功验证。</p>
  {requested&&<ModelSelect label="命名服务默认模型" value={provider.definition.model} catalog={catalog} disabled={disabled} onChange={model=>void onCommand({type:"save_provider",id:provider.id,expected_revision:provider.revision,definition:{...provider.definition,model}})} emptyLabel="请选择服务默认模型"/>}
 </div>;
}
