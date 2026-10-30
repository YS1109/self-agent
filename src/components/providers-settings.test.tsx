import { renderToStaticMarkup } from "react-dom/server";
import { afterEach,describe,expect,it,vi } from "vitest";
import { createProvidersController,consumeCredentialInput,credentialReceiptNotice,namedModelRequest,providerDefinitionError,providerSecretError,validProviders,type CredentialOperation,type CredentialReceipt,type NamedProviders,type ProviderDefinition,type ProvidersClient,type ProvidersState } from "../providers-client";
import { CredentialOperationCard,NamedProviderSelect,ProviderCard,ProviderEditor,ProvidersSettings,RoleProviderModel } from "./providers-settings";
import { ProviderSettings } from "./provider-settings";
import type { Settings } from "../types";
const id="00000000-0000-0000-0000-000000000001",opId="00000000-0000-0000-0000-000000000002";
const definition:ProviderDefinition={name:"本机夹具",base_url:"http://127.0.0.1:9999/v1",models_endpoint:"",model:"fixture-model",allow_insecure_http:false};
const registry:NamedProviders={providers:[{id,revision:1,definition,enabled:false,credential_configured:true,credential_status:"configured_not_checked",orphaned_credentials:1,updated_at:"now"}],credential_operations:[]};
const operation=(operationId=opId,status="pending"):CredentialOperation=>({id:operationId,owner_id:id,owner_kind:"provider",kind:"set",status,reason:null,can_cleanup:false,created_at:"now",updated_at:"now"});
const receipt=(operationId=opId,status="pending"):CredentialReceipt=>({operation:operation(operationId,status),replayed:false,no_secret_applied:false});
function fixture(){const client:ProvidersClient={registry:vi.fn().mockResolvedValue(registry),command:vi.fn().mockImplementation(async envelope=>({command_id:envelope.id,status:"completed",provider_id:id,revision:2})),setCredential:vi.fn().mockImplementation(async(_id,body)=>receipt(body.id)),deleteCredential:vi.fn().mockImplementation(async(_id,body)=>receipt(body.id)),cleanupCredential:vi.fn().mockImplementation(async(_id,body)=>receipt(body.id))};let serial=1;const states:ProvidersState[]=[];const controller=createProvidersController(client,registry,state=>states.push(state),{uuid:()=>`00000000-0000-0000-0000-${String(++serial).padStart(12,"0")}`,pollMs:25});return{client,controller,states};}
afterEach(()=>vi.useRealTimers());
describe("命名服务定义与界面",()=>{
 it("检查同源、HTTP显式确认和元数据边界，不把凭据塞进URL",()=>{
  expect(providerDefinitionError(definition)).toBeNull();
  expect(providerDefinitionError({...definition,base_url:"https://safe.invalid/v1",models_endpoint:"/models"})).toBeNull();
  expect(providerDefinitionError({...definition,base_url:"http://remote.invalid/v1"})).toContain("显式确认");
  expect(providerDefinitionError({...definition,base_url:"http://remote.invalid/v1",allow_insecure_http:true})).toBeNull();
  for(const base_url of ["https://u:pass@safe.invalid","https://safe.invalid/?token=x","https://safe.invalid/#", " https://safe.invalid", "file:///tmp"])
   expect(providerDefinitionError({...definition,base_url})).not.toBeNull();
  expect(providerDefinitionError({...definition,models_endpoint:"https://other.invalid/models"})).toContain("相同协议");
  expect(providerDefinitionError({...definition,name:"中".repeat(81)})).not.toBeNull();
  expect(providerDefinitionError({...definition,name:"sk-"+"a".repeat(40)})).toContain("凭据");
 });
 it("输入在取出传输值时同步清空，后续提交不会复制先前密钥",()=>{const field={value:"fixture-secret"};expect(consumeCredentialInput(field)).toBe("fixture-secret");expect(field.value).toBe("");expect(consumeCredentialInput(field)).toBe("");});
 it("密钥仅可见ASCII且有字节上限，模型发现只传服务ID不转发旧地址或密钥",()=>{
  expect(providerSecretError("x".repeat(2048))).toBeNull();
  for(const value of ["","x".repeat(2049),"a\nb"," a","密钥"])expect(providerSecretError(value)).not.toBeNull();
  expect(namedModelRequest({provider_source:"named",provider_id:id,base_url:"https://old.invalid",models_endpoint:"/old"},"old-secret")).toEqual({provider_source:"named",provider_id:id});
 });
 it("新配置默认停用、凭据不会回填、写入删除需明确确认",()=>{
  const {client}=fixture();const html=renderToStaticMarkup(<ProvidersSettings registry={registry} client={client} disabled={false}/>);
  expect(html).toContain("默认停用");expect(html).toContain("不退化为明文文件");expect(html).toContain("关闭页面只停止观察");
  expect(html).toContain("未验证系统存储可读");expect(html).toMatch(/type="password"[^>]*autoComplete="off"/i);expect(html).not.toMatch(/type="password"[^>]*value=/);
  expect(html).toMatch(/<button[^>]*disabled[^>]*>提交新凭据操作/);expect(html).toMatch(/<button[^>]*disabled[^>]*>解绑并删除凭据/);
  expect(client.command).not.toHaveBeenCalled();expect(client.setCredential).not.toHaveBeenCalled();
  const editor=renderToStaticMarkup(<ProviderEditor disabled={false} onCommand={vi.fn()}/>);expect(editor).not.toContain('type="password"');expect(editor).toContain("明文 HTTP");
 });
 it("未知与遗留凭据可见，清理只有后台允许且勾选后可提交",()=>{
  const html=renderToStaticMarkup(<CredentialOperationCard operation={{...operation(),status:"unknown",reason:"vault_outcome_unknown",can_cleanup:true}} disabled={false} onCleanup={vi.fn()}/>);
  expect(html).toContain("旧绑定可能仍生效");expect(html).toContain(opId);expect(html).toMatch(/<button[^>]*disabled[^>]*>申请系统凭据清理/);
  const pending=renderToStaticMarkup(<ProviderCard provider={registry.providers[0]} credentialPending disabled={false} onCommand={vi.fn()} onSet={vi.fn()} onDelete={vi.fn()}/>);
  expect(pending).toContain("有待完成的凭据操作");expect(pending).toMatch(/type="password"[^>]*disabled/);
 });
 it("命名全局显示profile默认模型，不暴露旧字段，角色支持独立服务",()=>{
  const settings:Settings={base_url:"https://old.invalid",model:"stale-model",configured:true,execution_mode:"native",provider_source:"named",provider_id:id,agents:[],default_agent_id:"c",codex_binary:"codex"};
  const catalog={models:[],loading:false,error:"",refresh:vi.fn()};
  const html=renderToStaticMarkup(<ProviderSettings providers={registry.providers} settings={settings} saved={settings} onChange={vi.fn()} apiKey="" onKeyChange={vi.fn()} catalog={catalog} disabled={false}/>);
  expect(html).toContain("fixture-model");expect(html).toContain("不使用旧的全局模型字段覆盖");expect(html).not.toContain("stale-model");expect(html).not.toContain('type="password"');expect(html).not.toContain("自定义 Endpoint");
  const role=renderToStaticMarkup(<RoleProviderModel agent={{id:"role",name:"角色",executor:"native",provider_id:id,model:"",instructions:""}} settings={settings} providers={registry.providers} catalog={catalog} disabled={false} onChange={vi.fn()} label="角色1"/>);
  expect(role).toContain("使用服务默认模型");expect(role).toContain("不借用其他服务的主模型");
  const missing=renderToStaticMarkup(<NamedProviderSelect providers={[]} value={id} disabled={false} onChange={vi.fn()}/>);expect(missing).toContain("原服务不可用");
 });
});
describe("凭据操作没有秘密缓存或自动重放",()=>{
 it("双击只发送一次，状态不留密钥；刷新只有GET",async()=>{
  const {controller,client,states}=fixture();const first=controller.setCredential(registry.providers[0],"fixture-secret",true);await controller.setCredential(registry.providers[0],"second-secret",true);await first;
  expect(client.setCredential).toHaveBeenCalledTimes(1);expect(client.setCredential).toHaveBeenCalledWith(id,{id:opId,expected_revision:1,secret:"fixture-secret"},expect.any(AbortSignal));
  expect(JSON.stringify(states)).not.toContain("fixture-secret");expect(JSON.stringify(controller.getState())).not.toContain("second-secret");await controller.refresh();expect(client.setCredential).toHaveBeenCalledTimes(1);controller.dispose();
 });
 it("网络未知只GET恢复；显式新提交生成新UUID而不复用秘密",async()=>{
  const {controller,client,states}=fixture();vi.mocked(client.setCredential).mockRejectedValueOnce(new Error("private-key-detail"));
  await controller.setCredential(registry.providers[0],"first-secret",true);expect(controller.getState().uncertainId).toBe(opId);expect(JSON.stringify(states)).not.toContain("private-key-detail");
  vi.mocked(client.registry).mockResolvedValue({...registry,credential_operations:[operation(opId,"unknown")]});await controller.refresh();expect(controller.getState().uncertainId).toBeNull();expect(client.setCredential).toHaveBeenCalledTimes(1);
  await controller.setCredential(registry.providers[0],"new-secret",true);expect(vi.mocked(client.setCredential).mock.calls[1][1].id).not.toBe(opId);controller.dispose();
 });
 it("重放回执明确新输入未保存，不以completed掩盖no_secret_applied",async()=>{
  const replay={...receipt(opId,"completed"),replayed:true,no_secret_applied:true};expect(credentialReceiptNotice(replay)).toContain("这次输入没有写入");
  const {controller,client}=fixture();vi.mocked(client.setCredential).mockResolvedValue(replay);await controller.setCredential(registry.providers[0],"new-secret",true);expect(controller.getState().notice).toContain("不能视为新凭据已保存");controller.dispose();
 });
 it("操作轮询仅读取直到终态，关闭终止轮询，迟到回应不写旧页面",async()=>{
  vi.useFakeTimers();const {controller,client,states}=fixture();
  vi.mocked(client.registry).mockResolvedValueOnce({...registry,credential_operations:[operation()]}).mockResolvedValue({...registry,credential_operations:[operation(opId,"completed")]});
  await controller.setCredential(registry.providers[0],"secret",true);await vi.advanceTimersByTimeAsync(25);expect(client.registry).toHaveBeenCalledTimes(2);await vi.advanceTimersByTimeAsync(200);expect(client.registry).toHaveBeenCalledTimes(2);
  let finish!:(value:NamedProviders)=>void;vi.mocked(client.registry).mockImplementation(()=>new Promise(resolve=>{finish=resolve;}));const pending=controller.refresh();const count=states.length;controller.dispose();finish(registry);await pending;expect(states).toHaveLength(count);
 });
 it("关闭不撤回已接受的POST，取消观察后迟到结果无副作用",async()=>{
  const {controller,client,states}=fixture();let finish!:(value:CredentialReceipt)=>void;vi.mocked(client.setCredential).mockImplementation(()=>new Promise(resolve=>{finish=resolve;}));
  const pending=controller.setCredential(registry.providers[0],"secret",true);await Promise.resolve();controller.dispose();const size=states.length;expect(vi.mocked(client.setCredential).mock.calls[0][2].aborted).toBe(true);finish(receipt());await pending;expect(states).toHaveLength(size);expect(client.registry).not.toHaveBeenCalled();
 });
 it("过时GET不覆盖新修订，畸形回应不当成功；冲突错误不回显服务器内容",async()=>{
  const {controller,client}=fixture();let finish!:(value:NamedProviders)=>void;vi.mocked(client.registry).mockImplementationOnce(()=>new Promise(resolve=>{finish=resolve;}));const pending=controller.refresh();controller.replaceRegistry({...registry,providers:[]});finish(registry);await pending;expect(controller.getState().registry.providers).toEqual([]);
  vi.mocked(client.registry).mockResolvedValue({providers:[{}],credential_operations:[]} as unknown as NamedProviders);await controller.refresh();expect(controller.getState().notice).toContain("修订已改变");
  vi.mocked(client.command).mockRejectedValue(new Error("server-secret"));await controller.command({type:"delete_provider",id,expected_revision:1});expect(controller.getState().notice).not.toContain("server-secret");expect(client.command).toHaveBeenCalledTimes(1);controller.dispose();
 });
 it("删除与清理绑定确切目标，需要显式确认，不发送明文或私有引用",async()=>{
  const {controller,client}=fixture();await controller.deleteCredential(registry.providers[0],false);expect(client.deleteCredential).not.toHaveBeenCalled();
  await controller.deleteCredential(registry.providers[0],true);expect(client.deleteCredential).toHaveBeenCalledWith(id,{id:opId,expected_revision:1},expect.any(AbortSignal));
  await controller.cleanupCredential(operation(),true);expect(client.cleanupCredential).not.toHaveBeenCalled();await controller.cleanupCredential({...operation(),can_cleanup:true},true);expect(client.cleanupCredential).toHaveBeenCalledTimes(1);controller.dispose();
 });
 it("普通与独立清理记录共允许1280条，超过上限拒绝",()=>{
  for(const count of [1025,1280])expect(validProviders({...registry,credential_operations:Array.from({length:count},()=>operation())})).toBe(true);
  expect(validProviders({...registry,credential_operations:Array.from({length:1281},()=>operation())})).toBe(false);
  expect(validProviders({...registry,credential_operations:[{...operation(),cleanup_status:"completed"}]})).toBe(true);
  expect(validProviders({...registry,credential_operations:[{...operation(),cleanup_status:23}]})).toBe(false);
 });
 it("独立清理完成不改写unknown原操作，也不显示重试或清理按钮",()=>{
  const current={...operation(),status:"unknown",reason:"vault_outcome_unknown",can_cleanup:false,cleanup_status:"completed"};
  const html=renderToStaticMarkup(<CredentialOperationCard operation={current} disabled={false} onCleanup={vi.fn()}/>);
  expect(html).toContain("清理已确认完成；原操作结果仍未知");expect(html).toContain("不应把原写入或删除标为成功");expect(html).not.toContain("申请系统凭据清理");expect(html).not.toContain("旧绑定可能仍生效");
  for(const cleanup_status of ["pending","rejected","unknown"]){
    const pending=renderToStaticMarkup(<CredentialOperationCard operation={{...current,cleanup_status}} disabled={false} onCleanup={vi.fn()}/>);
    expect(pending).toContain("独立清理状态");expect(pending).toContain("原操作结果保持不变");
  }
  expect(validProviders({...registry,credential_operations:[operation()]})).toBe(true);
 });
 it("公开目录校验拒绝缺失字段，凭据内容不属于快照契约",()=>{expect(validProviders(registry)).toBe(true);expect(validProviders({providers:[{}],credential_operations:[]})).toBe(false);});
});
