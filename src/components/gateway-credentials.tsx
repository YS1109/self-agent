import {useCallback,useEffect,useRef,useState} from "react";
import type {GatewayConfiguration,GatewayCredentialClient} from "../gateway-client";
import {consumeCredentialInput,credentialStateLabel,credentialReason,type CredentialOperation} from "../providers-client";
import {createGatewayCredentialController,validGatewayOperation,type GatewayCredentialState} from "../gateway-credentials-client";
export interface GatewayCredentialsProps {config:GatewayConfiguration;operations:CredentialOperation[];client:GatewayCredentialClient;disabled?:boolean;onRefresh:()=>void|Promise<void>;onBusyChange?:(busy:boolean)=>void}
export function GatewayCredentials({config,operations,client,disabled=false,onRefresh,onBusyChange}:GatewayCredentialsProps){
 const [state,setState]=useState<GatewayCredentialState>({busy:false,notice:"",uncertainId:null,operation:null});const [open,setOpen]=useState(false);const [confirmed,setConfirmed]=useState(false);const [deleteConfirmed,setDeleteConfirmed]=useState(false);const [cleanupId,setCleanupId]=useState<string|null>(null);
 const input=useRef<HTMLInputElement>(null),submitting=useRef(false),callbacks=useRef({onRefresh,onBusyChange}),controller=useRef<ReturnType<typeof createGatewayCredentialController>|null>(null);callbacks.current={onRefresh,onBusyChange};
 useEffect(()=>{const c=createGatewayCredentialController(client,config,value=>{setState(value);callbacks.current.onBusyChange?.(value.busy);},{onRefresh:()=>callbacks.current.onRefresh()});controller.current=c;setState(c.getState());setOpen(false);setConfirmed(false);setDeleteConfirmed(false);setCleanupId(null);return()=>{consumeCredentialInput(input.current);c.dispose();controller.current=null;callbacks.current.onBusyChange?.(false);};},[client,config.id,config.revision]);
 useEffect(()=>controller.current?.observe(operations),[operations,client,config.id,config.revision]);
 const secretInput=useCallback((node:HTMLInputElement|null)=>{if(input.current&&input.current!==node)consumeCredentialInput(input.current);input.current=node;},[]);
 const blocked=disabled||state.busy||!!state.uncertainId||!!state.operation&&["pending","inflight"].includes(state.operation.status);
 const visible=operations.filter(op=>validGatewayOperation(op,config.id));if(state.operation&&!visible.some(op=>op.id===state.operation!.id))visible.unshift(state.operation);
 const close=()=>{consumeCredentialInput(input.current);setOpen(false);setConfirmed(false);};
 return <section className="provider-fields" aria-label="网关 专用凭据">
  <h3>网关 专用系统凭据</h3><p>{config.credential_state==="configured"?"存在凭据绑定；未据此验证系统存储可读或 网关 平台连接。":"尚未绑定 网关 凭据。"}</p>
  {config.mode==="fixture"&&<p role="note">当前为本地夹具；模拟结果不证明真实系统凭据保存或 网关 连接。</p>}
  <button type="button" disabled={blocked} onClick={()=>setOpen(true)}>输入或轮换 网关 凭据</button>
  {open&&<form aria-label="网关 凭据写入" onSubmit={event=>{event.preventDefault();event.stopPropagation();if(blocked||!confirmed||submitting.current)return;submitting.current=true;const secret=consumeCredentialInput(input.current);setConfirmed(false);void Promise.resolve(controller.current?.setCredential(secret,true)).finally(()=>{submitting.current=false;});}}>
   <label>设备连接凭据<input ref={secretInput} type="password" autoComplete="new-password" maxLength={64} disabled={blocked} aria-label="网关 设备连接凭据" /></label>
   <label><input type="checkbox" checked={confirmed} disabled={blocked} onChange={e=>setConfirmed(e.target.checked)}/>我确认按当前配置修订写入 网关 专用系统凭据，不用于模型服务。</label>
   <button type="submit" disabled={blocked||!confirmed}>写入凭据</button><button type="button" onClick={close}>关闭并清空输入</button>
  </form>}
  <p>删除会先停用 网关、撤销已有及待确认绑定，再尝试删除系统凭据；重新启用不会恢复旧授权。</p>
  <label><input type="checkbox" checked={deleteConfirmed} disabled={blocked||config.credential_state!=="configured"} onChange={e=>setDeleteConfirmed(e.target.checked)}/>我确认停用、撤权并删除</label>
  <button type="button" disabled={blocked||!deleteConfirmed||config.credential_state!=="configured"} onClick={()=>{setDeleteConfirmed(false);void controller.current?.deleteCredential(true);}}>解绑并删除</button>
  {state.notice&&<p role="status">{state.notice}</p>}
  {(state.uncertainId||state.operation)&&<button type="button" disabled={state.busy} onClick={()=>void controller.current?.inspect()}>只读检查原操作</button>}
  <ul>{visible.map(op=><li key={op.id}><code>{op.id}</code>：{credentialStateLabel(op.status)}{op.cleanup_status&&<span>；清理状态：{credentialStateLabel(op.cleanup_status)}</span>}<p>{credentialReason(op.reason)}</p>{op.can_cleanup&&<><label><input type="checkbox" disabled={blocked} checked={cleanupId===op.id} onChange={e=>setCleanupId(e.target.checked?op.id:null)}/>确认删除此记录对应的未绑定凭据</label><button type="button" disabled={blocked||cleanupId!==op.id} onClick={()=>{setCleanupId(null);void controller.current?.cleanupCredential(op,true);}}>清理未绑定凭据</button></>}</li>)}</ul>
 </section>;
}
