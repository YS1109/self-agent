import {describe,it,expect,vi} from "vitest";
import {renderToStaticMarkup} from "react-dom/server";
import {QqSettings,QqBindingCard} from "./qq-settings";
import {QqConfigurationForm} from "./qq-configuration";
import {createQqController,EMPTY_QQ,EMPTY_QQ_PERMISSIONS,qqCommandError} from "../qq-client";
import type {QqClient,QqBinding,QqEnvelope,QqReceipt,QqSnapshot,QqState} from "../qq-client";
import {normalizeExecutionControls} from "../execution-controls";
const ID="4b33f2ee-8c30-412d-9a3c-49a70e377e3b";
const identity={app_id:"app_1",sender_open_id:"user_1",kind:"group" as const,target_id:"group_1"};
const binding:QqBinding={principal_id:ID,identity,revision:2,status:"pending",grant:null,expires_at_ms:Date.now()+600000,created_at_ms:0,updated_at_ms:0};
const snapshot:QqSnapshot={...EMPTY_QQ,configuration:{id:ID,app_id:"app_1",enabled:true,revision:1,mode:"fixture",reply_policy:{window_ms:300000,max_user:4,max_group:5},credential_state:"configured"},bindings:[binding],outbox:[{id:ID,route_id:ID,logical_key:`completion:${ID}`,status:"unknown",msg_seq:2,attempts:1,platform_id:null,reason:null}]};
function client():QqClient{return {snapshot:vi.fn().mockResolvedValue(snapshot),command:vi.fn().mockImplementation(async(envelope:QqEnvelope)=>({command_id:envelope.id,status:"completed"})),receipt:vi.fn().mockResolvedValue(null)};}
function controller(api=client()){const states:QqState[]=[];let n=0;const control=createQqController(api,snapshot,state=>states.push(state),{uuid:()=>`request-${++n}`});return {api,control,states};}
function deferred<T>(){let resolve!:(value:T)=>void;let reject!:(reason?:unknown)=>void;const promise=new Promise<T>((ok,bad)=>{resolve=ok;reject=bad});return{promise,resolve,reject};}
describe("QQ 本机管理与未知结果",()=>{
 it("配置固定官方端点，创建不能启用，回复限额只可收紧",()=>{
  const command={type:"configure" as const,config_id:null,expected_revision:null,app_id:"app_1",enabled:false,reply_policy:{window_ms:300000,max_user:4,max_group:5}};
  expect(qqCommandError(command)).toBeNull();expect(qqCommandError({...command,enabled:true})).toContain("首次配置必须停用");expect(qqCommandError({...command,app_id:"https://other.example"})).not.toBeNull();expect(qqCommandError({...command,reply_policy:{...command.reply_policy,window_ms:300001}})).not.toBeNull();expect(qqCommandError({...command,reply_policy:{...command.reply_policy,max_user:5}})).not.toBeNull();expect(qqCommandError({...command,reply_policy:{...command.reply_policy,max_group:6}})).not.toBeNull();
  const html=renderToStaticMarkup(<QqConfigurationForm configuration={null} disabled={false} onSubmit={vi.fn()}/>);expect(html).toContain("api.bot.qq.com");expect(html).toContain("停用创建 QQ 配置");expect(html).toContain("不是 ClientSecret");expect(html).toMatch(/type="checkbox"[^>]*disabled/);
 });
 it("初始渲染不发操作，明确夹具、群受众、未知投递与本机审批",()=>{
  const api=client();const html=renderToStaticMarkup(<QqSettings initialSnapshot={snapshot} client={api} sessions={[{id:ID,title:"<script>"}]} executor="native"/>);
  expect(api.command).not.toHaveBeenCalled();expect(api.snapshot).not.toHaveBeenCalled();expect(html).toContain("本地协议夹具");expect(html).toContain("不代表真实 QQ");expect(html).toContain("其他成员看到");expect(html).toContain("结果未知，不自动重发");expect(html).toContain("一次性配对码");expect(html).not.toContain("/配对 ");
 });
 it("确认后安全窗口和忽略汇总明确显示，不把旧消息变成待执行队列",()=>{
  const value={...snapshot,bindings:[{...binding,status:"active" as const,input_after_ms:Date.now()+30000}],ignored_inputs:{count:12000,updated_at_ms:Date.now(),clock_fault_ms:Date.now()+60000},connection:{state:"disconnected",updated_at_ms:Date.now(),reason:"credential_unavailable"}};
  const html=renderToStaticMarkup(<QqSettings initialSnapshot={value} client={client()} sessions={[]} executor="native"/>);
  expect(html).toContain("等待安全接收窗口");expect(html).toContain("旧消息不会在之后自动执行");expect(html).toContain("不占控制指令队列");expect(html).toContain("12000");expect(html).toContain("明确保存一次配置");expect(html).toContain("平台消息时间异常");expect(html).toContain("不重发旧指令");
 });
 it("待绑定默认全拒绝，完整身份修订可核对，确认/撤销都需独立勾选",()=>{
  const html=renderToStaticMarkup(<QqBindingCard binding={binding} sessions={[{id:ID,title:"<script>"}]} executor="native" disabled={false} onSubmit={vi.fn()}/>);
  expect(html).toContain("user_1");expect(html).toContain("group_1");expect(html).toContain("&lt;script&gt;");expect(html).not.toContain("<script>");expect(html).toMatch(/<button[^>]*disabled[^>]*>确认此身份与范围<\/button>/);expect(html).toMatch(/<button[^>]*disabled[^>]*>撤销此绑定<\/button>/);expect(html).toContain("默认全部拒绝");
 });
 it("绑定范围校验默认拒绝、期限、重复会话和非法标识",()=>{
  const grant={session_ids:[ID],permissions:{...EMPTY_QQ_PERMISSIONS},controls:normalizeExecutionControls(undefined),expires_at_ms:100000};
  const command={type:"confirm_binding" as const,principal_id:ID,expected_revision:2,identity,grant};
  expect(qqCommandError(command,0)).toContain("明确勾选");grant.permissions.view=true;expect(qqCommandError(command,0)).toBeNull();grant.session_ids.push(ID);expect(qqCommandError(command,0)).toContain("不同会话");grant.session_ids=[ID];grant.expires_at_ms=31*86400000;expect(qqCommandError(command,0)).toContain("30 天");expect(qqCommandError({type:"issue_pairing",ttl_seconds:601},0)).not.toBeNull();
 });
 it("重复点击只发送一次，另一指令不能占用当前收据",async()=>{
  const api=client(),wait=deferred<QqReceipt>();vi.mocked(api.command).mockReturnValue(wait.promise);const {control}=controller(api);const command={type:"issue_pairing" as const,ttl_seconds:300};const first=control.submit(command),second=control.submit(command);
  expect(first).toBe(second);expect(await control.submit({type:"revoke_binding",principal_id:ID,expected_revision:2})).toBeUndefined();await Promise.resolve();expect(api.command).toHaveBeenCalledTimes(1);wait.resolve({command_id:"request-1",status:"completed"});await first;expect(control.getState().busy).toBe(false);control.dispose();
 });
 it("未知结果仅GET查原收据，不重新签发或POST，缺失收据仍未知",async()=>{
  const api=client();vi.mocked(api.command).mockRejectedValue(new Error("private-secret-error"));const {control}=controller(api);await control.submit({type:"issue_pairing",ttl_seconds:300});expect(control.getState().uncertainId).toBe("request-1");expect(control.getState().notice).not.toContain("private-secret");await control.submit({type:"issue_pairing",ttl_seconds:300});expect(api.command).toHaveBeenCalledTimes(1);await control.inspect();expect(api.receipt).toHaveBeenCalledWith("request-1",expect.any(AbortSignal));expect(control.getState().uncertainId).toBe("request-1");vi.mocked(api.receipt).mockResolvedValue({command_id:"request-1",status:"completed",pairing:{id:ID,expires_at_ms:Date.now()+10000}});await control.inspect();expect(control.getState().uncertainId).toBeNull();expect(control.getState().notice).toContain("只显示一次");expect(api.command).toHaveBeenCalledTimes(1);control.dispose();
 });
 it("耐久拒绝收据清除未知状态，但不声称操作已执行",async()=>{
  const api=client();vi.mocked(api.command).mockResolvedValue({command_id:"request-1",status:"rejected",reason:"scope_or_state_unavailable"});const {control}=controller(api);await control.submit({type:"issue_pairing",ttl_seconds:300});expect(control.getState().uncertainId).toBeNull();expect(control.getState().notice).toContain("已确认未执行");expect(control.getState().pairing).toBeNull();expect(api.command).toHaveBeenCalledTimes(1);control.dispose();
 });
 it("关闭只停止观察，迟到回执不能恢复配对码或覆盖新状态",async()=>{
  const api=client(),wait=deferred<QqReceipt>();vi.mocked(api.command).mockReturnValue(wait.promise);const {control,states}=controller(api);const operation=control.submit({type:"issue_pairing",ttl_seconds:300});await Promise.resolve();control.dispose();const count=states.length;wait.resolve({command_id:"request-1",status:"completed",pairing:{id:ID,code:"a".repeat(32),expires_at_ms:Date.now()+10000}});await operation;expect(states).toHaveLength(count);expect(control.getState().pairing).toBeNull();
 });
 it("外部快照更新不会让旧GET覆盖它，也不触发写入",async()=>{
  const api=client(),wait=deferred<QqSnapshot>();vi.mocked(api.snapshot).mockReturnValue(wait.promise);const {control}=controller(api);const read=control.refresh();const newer={...snapshot,bindings:[]};control.replaceSnapshot(newer);wait.resolve(snapshot);await read;expect(control.getState().snapshot.bindings).toEqual([]);expect(api.command).not.toHaveBeenCalled();control.dispose();
 });
});
