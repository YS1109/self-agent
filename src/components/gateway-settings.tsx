import {useEffect,useRef,useState} from "react";
import {createGatewayController,conservativeGatewayControls,EMPTY_GATEWAY_PERMISSIONS,gatewayCommandError} from "../gateway-client";
import type {GatewayBinding,GatewayClient,GatewayCommand,GatewayConfiguration,GatewayCredentialClient,GatewayPermissions,GatewaySnapshot,GatewayState} from "../gateway-client";
import type {TaskControls} from "../types";
import {ExecutionSettings} from "./execution-settings";
import {GatewayCredentials} from "./gateway-credentials";
import type {ManagedCapabilities} from "../capabilities-client";
import "./gateway-settings.css";
const PERMISSIONS:{key:keyof GatewayPermissions;label:string}[]=[{key:"view",label:"查看所选会话和任务状态"},{key:"read_results",label:"读取会话正文、任务结果与观察 diff"},{key:"read_artifacts",label:"读取已持久化的纯文本产物（还需读取结果权限）"},{key:"run",label:"发送、补充和继续任务（仍受权限交集限制）"},{key:"cancel",label:"取消所选会话中的任务，包括桌面发起的任务"},{key:"answer",label:"回答所选会话的问题，包括桌面任务；不是批准操作"}];
const stateLabel=(s:string)=>({disabled:"已停用",connecting:"连接中",ready:"认证连接就绪",disconnected:"已断开",credential_unavailable:"系统凭据不可用",pending:"等待本机确认",active:"已授权",revoked:"已撤销"})[s]??"状态未知";

export function GatewaySettings({initialSnapshot,client,credentialClient,sessions,executor,registry,disabled:externalDisabled=false}:{disabled?:boolean;initialSnapshot:GatewaySnapshot;client:GatewayClient;credentialClient:GatewayCredentialClient;sessions:{id:string;title:string}[];executor:"native"|"codex";registry?:ManagedCapabilities}) {
 const [state,setState]=useState<GatewayState>({snapshot:initialSnapshot,busy:false,loading:false,notice:"",uncertainId:null,pairing:null});const [credentialBusy,setCredentialBusy]=useState(false);
 const controller=useRef<ReturnType<typeof createGatewayController>|null>(null);
 const appliedSnapshot=useRef(initialSnapshot);
 useEffect(()=>{const value=createGatewayController(client,initialSnapshot,setState);controller.current=value;appliedSnapshot.current=initialSnapshot;void value.refresh();return()=>{value.dispose();controller.current=null;};},[client]);
 useEffect(()=>{if(appliedSnapshot.current!==initialSnapshot){appliedSnapshot.current=initialSnapshot;controller.current?.replaceSnapshot(initialSnapshot);}},[initialSnapshot]);
 useEffect(()=>{if(!state.pairing)return;const timeout=setTimeout(()=>controller.current?.hidePairing(),Math.max(0,state.pairing.expires_at_ms-Date.now()));return()=>clearTimeout(timeout);},[state.pairing]);
 const config=state.snapshot.configuration;const disabled=externalDisabled||state.busy||credentialBusy||!!state.uncertainId;
 const submit=(command:GatewayCommand)=>void controller.current?.submit(command);
 return <section className="gateway-settings" aria-label="自托管远程网关设置"><header><div><h2>自托管网关与 H5</h2><p>daemon 只向网关建立认证出站连接；本机管理端口仍为 loopback。浏览器不取得管理令牌。</p></div><button disabled={state.loading||disabled} onClick={()=>void controller.current?.refresh()}>刷新状态</button></header>
  <p className="help-text">这不会购买域名、申请证书或部署服务。请先在自己的服务器配置独立网关与 nginx HTTPS；配置成功不等于真实网络已验证。</p>
  <p role="status">连接：{stateLabel(state.snapshot.connection.state)}{state.snapshot.connection.updated_at_ms!==null&&` · ${new Date(state.snapshot.connection.updated_at_ms).toLocaleString("zh-CN")}`}</p>
  {config?.mode==="fixture"&&<p className="execution-warning">本地协议夹具，不是实际公网部署；无真实设备凭据或外部连接验证。</p>}
  <GatewayConfigurationForm key={`${config?.id}:${config?.revision}`} config={config} disabled={disabled} onSubmit={submit}/>
  {config&&<GatewayCredentials config={config} operations={state.snapshot.credential_operations} client={credentialClient} disabled={externalDisabled||state.busy||!!state.uncertainId} onBusyChange={setCredentialBusy} onRefresh={()=>controller.current?.refresh()}/ >}
  <section className="gateway-pairing"><h3>配对手机浏览器</h3><p>签发码只显示一次；在 H5 输入后，仍需在这里核对精确身份并授予会话范围。网关重启或浏览器会话失效后需新码重新配对。</p><button disabled={disabled||!config?.enabled} onClick={()=>submit({type:"issue_pairing",ttl_seconds:300})}>签发 5 分钟一次性配对码</button>
   {state.pairing&&<div role="status"><p>有效至 {new Date(state.pairing.expires_at_ms).toLocaleString("zh-CN")}</p>{state.pairing.code?<><code className="gateway-pair-code">{state.pairing.code}</code><button onClick={()=>controller.current?.hidePairing()}>隐藏配对码</button></>:<p>代码只显示一次，此回执不能重新取出旧码。</p>}</div>}
  </section>
  {state.notice&&<p role="status">{state.notice}</p>}{state.uncertainId&&<div className="execution-warning"><code>{state.uncertainId}</code><button disabled={state.busy} onClick={()=>void controller.current?.inspect()}>只读检查原管理收据</button></div>}
  <h3>浏览器身份与授权</h3>{state.snapshot.truncated&&<p className="execution-warning">当前仅返回有界审计预览，未显示的历史仍保留；可使用本机 CLI 按精确 principal ID 撤权，不会自动删除记录。</p>}{!state.snapshot.bindings.length&&<p>没有已配对身份。陌生浏览器没有任务权限。</p>}{state.snapshot.bindings.map(binding=><GatewayBindingCard key={`${binding.principal_id}:${binding.revision}`} binding={binding} disabled={disabled} sessions={sessions} executor={executor} registry={registry} onSubmit={submit}/>)}
 </section>;
}

export function GatewayConfigurationForm({config,disabled,onSubmit}:{config:GatewayConfiguration|null;disabled:boolean;onSubmit:(command:GatewayCommand)=>void}){
 const [origin,setOrigin]=useState(config?.origin??""),[device,setDevice]=useState(config?.device_id??""),[enabled,setEnabled]=useState(config?.enabled??false),[confirmed,setConfirmed]=useState(false);
 const command:GatewayCommand={type:"configure",config_id:config?.id??null,expected_revision:config?.revision??null,origin,device_id:device,enabled};const error=gatewayCommandError(command);
 return <form className="gateway-config" onSubmit={event=>{event.preventDefault();if(disabled||!confirmed||error)return;setConfirmed(false);onSubmit(command);}}><h3>固定设备与网关来源</h3><label>公开 HTTPS origin<input value={origin} placeholder="https://agent.example.invalid" disabled={disabled||config?.credential_state==="configured"} onChange={e=>setOrigin(e.target.value)} autoComplete="off"/></label><label>网关登记的设备 UUID（不是 token）<input value={device} disabled={disabled||config?.credential_state==="configured"} onChange={e=>setDevice(e.target.value)} autoComplete="off" spellCheck={false}/></label><label><input type="checkbox" checked={enabled} disabled={disabled||!config} onChange={e=>setEnabled(e.target.checked)}/>明确启用认证出站连接</label><p>首次只能停用创建。绑定凭据后，修改来源/设备必须先解绑；任何新配置修订撤销旧浏览器授权，重新启用不会复活旧授权。</p><label><input type="checkbox" checked={confirmed} disabled={disabled} onChange={e=>setConfirmed(e.target.checked)}/>已核对来源、设备和停用/撤权影响</label>{error&&<p className="task-error">{error}</p>}<button disabled={disabled||!confirmed||!!error}>{config?"保存新配置修订":"停用创建网关配置"}</button></form>;
}

export function GatewayBindingCard({binding,disabled,sessions,executor,registry,onSubmit}:{binding:GatewayBinding;disabled:boolean;sessions:{id:string;title:string}[];executor:"native"|"codex";registry?:ManagedCapabilities;onSubmit:(command:GatewayCommand)=>void}){
 const [selected,setSelected]=useState<string[]>([]),[permissions,setPermissions]=useState<GatewayPermissions>({...EMPTY_GATEWAY_PERMISSIONS}),[controls,setControls]=useState<TaskControls>(conservativeGatewayControls),[hours,setHours]=useState(24),[confirmed,setConfirmed]=useState(false),[revoke,setRevoke]=useState(false);
 const missingSelected=selected.filter(id=>!sessions.some(session=>session.id===id));
 const selectionKey=selected.join("\0"),missingKey=missingSelected.join("\0");
 useEffect(()=>setConfirmed(false),[selectionKey,missingKey]);
 const command:GatewayCommand={type:"confirm_binding",principal_id:binding.principal_id,expected_revision:binding.revision,expected_label:binding.label,expected_pairing_id:binding.pairing_id,grant:{session_ids:selected,permissions,controls,expires_at_ms:Date.now()+hours*3600000}};
 const error=gatewayCommandError(command);const pending=binding.status==="pending"&&binding.expires_at_ms>Date.now();
 return <article className="gateway-binding"><header><h4>{binding.label}</h4><span>{stateLabel(binding.status)}</span></header><dl><dt>浏览器 principal</dt><dd><code>{binding.principal_id}</code></dd><dt>设备 / 配置修订</dt><dd><code>{binding.device_id}</code> / {binding.config_revision}</dd><dt>配对请求 / 身份修订</dt><dd><code>{binding.pairing_id}</code> / {binding.revision}</dd></dl>
  {pending&&<><p className="execution-warning">确认前请在浏览器与本机核对身份。以下权限只作用于明确选择的会话，但取消/回答可影响这些会话中的桌面任务；回答不能批准执行动作。</p>{selected.length>0&&<section aria-label="已选固定会话范围"><strong>已选会话（固定 ID）</strong>{selected.map(id=><p key={id}><code>{id}</code> · {sessions.find(session=>session.id===id)?.title??"未载入当前窗口"}<button disabled={disabled} onClick={()=>setSelected(ids=>ids.filter(value=>value!==id))}>明确移除此选择</button></p>)}</section>}{missingSelected.length>0&&<p role="alert">已选会话有 {missingSelected.length} 项未载入当前窗口；未删除或改选。确认已清除，必须重新载入或显式移除这些选择后再核对授权。</p>}<fieldset disabled={disabled}><legend>明确选择会话</legend>{sessions.map(session=><label key={session.id}><input type="checkbox" checked={selected.includes(session.id)} onChange={e=>setSelected(ids=>e.target.checked?[...ids,session.id]:ids.filter(id=>id!==session.id))}/>{session.title}</label>)}</fieldset><fieldset disabled={disabled}><legend>动作权限（默认全部拒绝）</legend>{PERMISSIONS.map(({key,label})=><label key={key}><input type="checkbox" checked={permissions[key]} onChange={e=>setPermissions(value=>({...value,[key]:e.target.checked}))}/>{label}</label>)}</fieldset><label>授权时长（小时，最多 720）<input type="number" min={1} max={720} value={hours} disabled={disabled} onChange={e=>setHours(Number(e.target.value))}/></label>
   <ExecutionSettings value={controls} onChange={setControls} executor="native" disabled={disabled} registry={registry} scope="role"/>
   {executor==="codex"&&permissions.run&&<p className="execution-warning">当前主执行器是 Codex，不能兑现这些逐工具限制时后台会拒绝；不会为远端静默放宽。请明确调整本机执行器或权限配置。</p>}
   <p>远端新建任务随授权撤销停止后续执行；已被桌面任务接受的补充/回答不会回滚或转移所有权，撤权不自动取消桌面任务。新任务与本机、角色及继续目标权限取交集；补充或回答旧任务时，若原任务权限高于此上限将拒绝，不改写旧任务固化配置。</p><label><input type="checkbox" checked={confirmed} disabled={disabled||missingSelected.length>0} onChange={e=>setConfirmed(e.target.checked)}/>我已核对上述精确浏览器身份、会话、动作、预算与期限</label>{error&&<p className="task-error">{error}</p>}<button disabled={disabled||!confirmed||!!error||missingSelected.length>0} onClick={()=>{if(disabled||!confirmed||error||missingSelected.length)return;setConfirmed(false);onSubmit(command);}}>确认这次浏览器授权</button></>}
  {binding.status==="active"&&binding.grant&&<><p>到期：{new Date(binding.expires_at_ms).toLocaleString("zh-CN")}；会话数：{binding.grant.session_ids.length}</p><ul>{PERMISSIONS.filter(p=>binding.grant!.permissions[p.key]).map(p=><li key={p.key}>{p.label}</li>)}</ul></>}
  {binding.status!=="revoked"&&<><label><input type="checkbox" checked={revoke} disabled={disabled} onChange={e=>setRevoke(e.target.checked)}/>确认撤销此身份并关闭后续远程操作/事件流</label><button disabled={disabled||!revoke} onClick={()=>{setRevoke(false);onSubmit({type:"revoke_binding",principal_id:binding.principal_id,expected_revision:binding.revision});}}>撤销浏览器授权</button></>}
 </article>;
}
