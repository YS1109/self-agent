import {describe,it,expect,vi,afterEach} from "vitest";
import {renderToStaticMarkup} from "react-dom/server";
import {QqCredentials} from "./qq-credentials";
import {createQqCredentialController,type QqCredentialState} from "../qq-credentials-client";
import {consumeCredentialInput,type CredentialOperation,type CredentialReceipt} from "../providers-client";
import type {QqConfiguration,QqCredentialClient} from "../qq-client";
const CONFIG="11111111-1111-4111-8111-111111111111",OP="22222222-2222-4222-8222-222222222222",TARGET="33333333-3333-4333-8333-333333333333";
const config:QqConfiguration={id:CONFIG,revision:0,app_id:"app_1",enabled:false,mode:"official",reply_policy:{window_ms:300000,max_user:4,max_group:5},credential_state:"missing"};
function operation(status="completed",id=OP):CredentialOperation{return{id,owner_kind:"qq",owner_id:CONFIG,kind:"set",status,reason:null,can_cleanup:false,created_at:"now",updated_at:"now"};}
function receipt(status="completed"):CredentialReceipt{return{operation:operation(status),replayed:false,no_secret_applied:false};}
function client():QqCredentialClient{return{setCredential:vi.fn(async()=>receipt()),deleteCredential:vi.fn(async()=>receipt()),cleanupCredential:vi.fn(async()=>receipt()),operation:vi.fn(async()=>operation())};}
function deferred<T>(){let resolve!:(value:T)=>void,reject!:(value?:unknown)=>void;const promise=new Promise<T>((yes,no)=>{resolve=yes;reject=no;});return{promise,resolve,reject};}
afterEach(()=>vi.useRealTimers());
describe("QQ credential controller",()=>{
 it("clears uncontrolled input synchronously; duplicate clicks send one scoped POST with fixed CAS",async()=>{
  const api=client(),wait=deferred<CredentialReceipt>(),states:QqCredentialState[]=[];let observed="";api.setCredential=vi.fn(async(owner,body)=>{expect(owner).toBe(CONFIG);observed=body.secret;expect(body.expected_revision).toBe(0);return wait.promise;});
  const mutable={...config};const ctl=createQqCredentialController(api,mutable,s=>states.push(s),{uuid:()=>OP});const field={value:"fixture-secret-canary"};const pending=ctl.setCredential(consumeCredentialInput(field),true);expect(field.value).toBe("");mutable.revision=99;void ctl.setCredential("another",true);await Promise.resolve();expect(api.setCredential).toHaveBeenCalledTimes(1);expect(observed).toBe("fixture-secret-canary");expect(JSON.stringify(states)).not.toContain("fixture-secret");wait.resolve(receipt());await pending;expect(api.setCredential).toHaveBeenCalledTimes(1);ctl.dispose();
 });
 it("unknown response only GETs original ID; missing receipt never proves not applied",async()=>{
  vi.useFakeTimers();const api=client();api.setCredential=vi.fn(async()=>{throw new Error("SECRET_CANARY")});api.operation=vi.fn(async()=>null);const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP,pollMs:10});await ctl.setCredential("fixture-secret",true);expect(ctl.getState().uncertainId).toBe(OP);await ctl.setCredential("different-secret",true);await vi.advanceTimersByTimeAsync(10);expect(api.setCredential).toHaveBeenCalledTimes(1);expect(api.operation).toHaveBeenCalledWith(OP,expect.any(AbortSignal));expect(ctl.getState().uncertainId).toBe(OP);expect(ctl.getState().notice).not.toContain("SECRET_CANARY");ctl.dispose();
 });
 it("pending/inflight receipt polls GET and refreshes without repeating mutation",async()=>{
  vi.useFakeTimers();const api=client();api.setCredential=vi.fn(async()=>receipt("pending"));const refresh=vi.fn();const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP,pollMs:10,onRefresh:refresh});await ctl.setCredential("fixture-secret",true);expect(ctl.getState().operation?.status).toBe("pending");await vi.advanceTimersByTimeAsync(10);expect(ctl.getState().operation?.status).toBe("completed");expect(api.setCredential).toHaveBeenCalledTimes(1);expect(refresh).toHaveBeenCalledTimes(2);await vi.advanceTimersByTimeAsync(50);expect(api.operation).toHaveBeenCalledTimes(1);ctl.dispose();
 });
 it("rejects cross-owner, cross-config and wrong operation receipts",async()=>{
  for(const bad of [{owner_kind:"provider"},{owner_id:TARGET},{id:TARGET}]){const api=client();api.setCredential=vi.fn(async()=>({...receipt(),operation:{...operation(),...bad}}));const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP});await ctl.setCredential("fixture-secret",true);expect(ctl.getState().uncertainId).toBe(OP);expect(ctl.getState().operation).toBeNull();ctl.dispose();}
 });
 it("replay/no-secret-applied notice never claims new write",async()=>{
  const api=client();api.setCredential=vi.fn(async()=>({...receipt(),replayed:true,no_secret_applied:true}));const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP});await ctl.setCredential("fixture-secret",true);expect(ctl.getState().notice).toContain("这次输入没有写入");ctl.dispose();
 });
 it("requires explicit confirmations and ASCII limits without any POST",async()=>{
  const api=client();const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP});for(const value of ["","x".repeat(2049),"with space","中文","a\nb"]){await ctl.setCredential(value,true);}await ctl.setCredential("valid",false);await ctl.deleteCredential(false);await ctl.cleanupCredential({...operation("unknown",TARGET),can_cleanup:true},false);await ctl.cleanupCredential({...operation("unknown",TARGET),owner_kind:"provider",can_cleanup:true},true);expect(api.setCredential).not.toHaveBeenCalled();expect(api.deleteCredential).not.toHaveBeenCalled();expect(api.cleanupCredential).not.toHaveBeenCalled();ctl.dispose();
 });
 it("delete sends revision, cleanup targets explicit QQ original and preserves historical Unknown",async()=>{
  const api=client();const ctl=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP});await ctl.deleteCredential(true);expect(api.deleteCredential).toHaveBeenCalledWith(CONFIG,{id:OP,expected_revision:0},expect.any(AbortSignal));ctl.dispose();
  const original={...operation("unknown",TARGET),can_cleanup:true,cleanup_status:"unknown"};const cleanup=createQqCredentialController(api,config,()=>{}, {uuid:()=>OP});await cleanup.cleanupCredential(original,true);expect(api.cleanupCredential).toHaveBeenCalledWith(TARGET,{id:OP},expect.any(AbortSignal));expect(original.status).toBe("unknown");expect(original.cleanup_status).toBe("unknown");cleanup.dispose();
 });
 it("dispose repeatedly ignores late POST and GET and aborts observation",async()=>{
  const api=client(),wait=deferred<CredentialReceipt>(),changed=vi.fn(),refresh=vi.fn();api.setCredential=vi.fn(()=>wait.promise);const ctl=createQqCredentialController(api,config,changed,{uuid:()=>OP,onRefresh:refresh});const post=ctl.setCredential("fixture-secret",true);await Promise.resolve();ctl.dispose();ctl.dispose();const count=changed.mock.calls.length;wait.resolve(receipt());await post;expect(changed).toHaveBeenCalledTimes(count);expect(refresh).not.toHaveBeenCalled();
  const read=deferred<CredentialOperation|null>();api.operation=vi.fn(()=>read.promise);const second=createQqCredentialController(api,config,changed,{onRefresh:refresh});second.observe([operation("pending")]);const inspect=second.inspect();second.dispose();const before=changed.mock.calls.length;read.resolve(operation());await inspect;expect(changed).toHaveBeenCalledTimes(before);expect(refresh).not.toHaveBeenCalled();
 });
 it("duplicate inspect while GET pending is singleflight",async()=>{
  const api=client(),read=deferred<CredentialOperation|null>();api.operation=vi.fn(()=>read.promise);const ctl=createQqCredentialController(api,config,()=>{});ctl.observe([operation("pending")]);const first=ctl.inspect();await ctl.inspect();expect(api.operation).toHaveBeenCalledTimes(1);read.resolve(operation());await first;ctl.dispose();
 });
 it("SSR has no secret field value or automatic operations and labels fixture accurately",()=>{
  const api=client();const html=renderToStaticMarkup(<QqCredentials config={{...config,mode:"fixture",credential_state:"configured"}} operations={[{...operation("unknown"),cleanup_status:"completed"}]} client={api} onRefresh={()=>{}}/>);expect(html).toContain("本地夹具");expect(html).toContain("未据此验证系统存储可读");expect(html).toContain("结果未知");expect(html).toContain("清理状态");expect(api.setCredential).not.toHaveBeenCalled();expect(api.operation).not.toHaveBeenCalled();expect(html).not.toContain("type=\"password\"");
 });
});
