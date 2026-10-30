import {useEffect,useRef,useState} from "react";
import {Button} from "./ui/button";
import {ExecutionSettings} from "./execution-settings";
import {QqConfigurationForm} from "./qq-configuration";
import {QqCredentials} from "./qq-credentials";
import {executionControlsError,normalizeExecutionControls} from "../execution-controls";
import type {TaskControls} from "../types";
import {createQqController,EMPTY_QQ_PERMISSIONS,qqCommandError,qqStateLabel} from "../qq-client";
import type {QqBinding,QqClient,QqCredentialClient,QqCommand,QqGrant,QqPermissions,QqSnapshot,QqState} from "../qq-client";
import "./qq-settings.css";

function clockLabel(ms:number):string {const date=new Date(ms);return Number.isSafeInteger(ms)&&Number.isFinite(date.getTime())?date.toLocaleString():"超出可显示范围，请检查时钟";}
const ACTIONS:Record<keyof QqPermissions,string>={view:"查看已选会话及状态",read_results:"读取有界结果摘要",run:"运行、补充和继续任务",cancel:"取消任务",answer:"回答问题（不能批准动作）",orchestrate:"显式编排及计划控制",create_session:"从已选会话的项目新建会话",share_group_results:"允许向群内受众展示结果和问题"};
function suggestedControls():TaskControls {const controls=normalizeExecutionControls(undefined);return {...controls,budget:{...controls.budget,max_runtime_seconds:300,max_output_bytes:32768},permissions:{...controls.permissions,write_files:"ask",run_commands:"deny",managed_tools:"deny"},capabilities:{mcp_servers:[],skills:[]}};}
export function QqBindingCard({binding,sessions,executor,disabled,onSubmit}:{binding:QqBinding;sessions:{id:string;title:string}[];executor:"native"|"codex";disabled:boolean;onSubmit:(command:QqCommand)=>Promise<unknown>}){
 const [selected,setSelected]=useState<string[]>([]),[permissions,setPermissions]=useState<QqPermissions>({...EMPTY_QQ_PERMISSIONS}),[controls,setControls]=useState<TaskControls>(suggestedControls),[days,setDays]=useState(1),[confirmed,setConfirmed]=useState(false),[revoke,setRevoke]=useState(false);
 useEffect(()=>{setSelected([]);setPermissions({...EMPTY_QQ_PERMISSIONS});setControls(suggestedControls());setConfirmed(false);setRevoke(false);},[binding.principal_id,binding.revision]);
 const missingSelected=selected.filter(id=>!sessions.some(session=>session.id===id));
 const selectionKey=selected.join("\0"),missingKey=missingSelected.join("\0");
 useEffect(()=>setConfirmed(false),[selectionKey,missingKey]);
 const now=Date.now(),expired=binding.expires_at_ms<=now,group=binding.identity.kind==="group";
 const inputAfter=binding.input_after_ms||binding.updated_at_ms+30000;
 const grant:QqGrant={session_ids:selected,permissions,controls,expires_at_ms:now+days*86400000};
 const command:QqCommand={type:"confirm_binding",principal_id:binding.principal_id,expected_revision:binding.revision,identity:binding.identity,grant};
 const error=qqCommandError(command,now)??executionControlsError(controls,executor);
 return <article className="qq-binding" aria-label="QQ 身份绑定"><h4>{group?"QQ群成员绑定":"QQ单聊绑定"} · {expired?"已过期":qqStateLabel(binding.status)}</h4>
 <dl><dt>应用 AppID</dt><dd>{binding.identity.app_id}</dd><dt>用户 OpenID</dt><dd>{binding.identity.sender_open_id}</dd><dt>{group?"群 OpenID":"单聊目标"}</dt><dd>{binding.identity.target_id}</dd><dt>修订</dt><dd>{binding.revision}</dd></dl>
 <p className="help-text">OpenID 仅在对应应用和对话范围内解释，不是 QQ 号；显示名或群管理员身份不会获得本地权限。</p>
 {binding.status==="pending"&&!expired&&<>
 {selected.length>0&&<section aria-label="已选固定会话范围"><strong>已选会话（固定 ID）</strong>{selected.map(id=><p key={id}><code>{id}</code> · {sessions.find(session=>session.id===id)?.title??"未载入当前窗口"}<button disabled={disabled} onClick={()=>setSelected(ids=>ids.filter(value=>value!==id))}>明确移除此选择</button></p>)}</section>}
 {missingSelected.length>0&&<p role="alert">已选会话有 {missingSelected.length} 项未载入当前窗口；未删除或改选。确认已清除，必须重新载入或显式移除这些选择后再核对授权。</p>}
 <fieldset disabled={disabled}><legend>明确选择会话（最多 32 个）</legend>{sessions.map(session=><label key={session.id} className="qq-check"><input type="checkbox" checked={selected.includes(session.id)} onChange={e=>setSelected(e.target.checked?[...selected,session.id]:selected.filter(id=>id!==session.id))}/>{session.title} <small>{session.id}</small></label>)}</fieldset>
 <fieldset disabled={disabled}><legend>远端允许的操作（默认全部拒绝）</legend>{(Object.keys(ACTIONS) as (keyof QqPermissions)[]).filter(key=>group||key!=="share_group_results").map(key=><label key={key} className="qq-check"><input type="checkbox" checked={permissions[key]} onChange={e=>setPermissions({...permissions,[key]:e.target.checked})}/>{ACTIONS[key]}</label>)}</fieldset>
 {group&&<p className="execution-warning">群内回复会被其他成员看到。即使只绑定一名成员，也不能把群聊当成该成员的私人收件箱。</p>}
 <label>授权有效期（1–30 天）<input type="number" min={1} max={30} step={1} value={days} disabled={disabled} onChange={e=>setDays(Number(e.target.value))}/></label>
 <ExecutionSettings value={controls} onChange={setControls} executor={executor} disabled={disabled} scope="role"/>
 <p className="help-text">这些限制只会收紧本机策略。Codex 不支持的逐工具限制不会被自动清空；动作审批仍必须在本机完成。</p>
 <label className="qq-check"><input type="checkbox" checked={confirmed} disabled={disabled||missingSelected.length>0} onChange={e=>setConfirmed(e.target.checked)}/>我已核对 AppID、用户、具体对话和以上范围</label>
 {error&&<p className="help-text">{error}</p>}
 <Button disabled={disabled||!confirmed||Boolean(error)||missingSelected.length>0} onClick={()=>{if(disabled||!confirmed||error||missingSelected.length)return;setConfirmed(false);void onSubmit(command);}}>确认此身份与范围</Button>
 </>}
 {binding.status==="active"&&!expired&&now<=inputAfter&&<p className="execution-warning">绑定已确认，正在等待安全接收窗口。仅接受平台时间晚于 {clockLabel(inputAfter)} 的新消息；对齐时钟下约等待 30 秒。等待期间发送的旧消息不会在之后自动执行，届时请发新消息。</p>}
 {binding.status==="active"&&!expired&&<p className="help-text">已授权 {binding.grant?.session_ids.length??0} 个初始会话，有效至 {new Date(binding.expires_at_ms).toLocaleString()}。修改范围请先撤销并重新配对。</p>}
 {binding.status!=="revoked"&&<><label className="qq-check"><input type="checkbox" checked={revoke} disabled={disabled} onChange={e=>setRevoke(e.target.checked)}/>确认撤销绑定；已在途消息可能无法召回</label><Button disabled={disabled||!revoke} onClick={()=>void onSubmit({type:"revoke_binding",principal_id:binding.principal_id,expected_revision:binding.revision})}>撤销此绑定</Button></>}
 </article>;
}
export function QqSettings({initialSnapshot,client,credentialClient,sessions,executor,disabled=false}:{initialSnapshot:QqSnapshot;client:QqClient;credentialClient?:QqCredentialClient;sessions:{id:string;title:string}[];executor:"native"|"codex";disabled?:boolean}){
 const [state,setState]=useState<QqState>({snapshot:initialSnapshot,busy:false,loading:false,notice:"",uncertainId:null,pairing:null});
 const controller=useRef<ReturnType<typeof createQqController>|null>(null);
 const [credentialBusy,setCredentialBusy]=useState(false);
 useEffect(()=>{const next=createQqController(client,initialSnapshot,setState);controller.current=next;void next.refresh();return()=>{next.dispose();if(controller.current===next)controller.current=null;};},[client]);
 useEffect(()=>{controller.current?.replaceSnapshot(initialSnapshot);},[initialSnapshot]);
 useEffect(()=>{if(!state.pairing)return;const timer=setTimeout(()=>controller.current?.hidePairing(),Math.max(0,state.pairing.expires_at_ms-Date.now()));return()=>clearTimeout(timer);},[state.pairing]);
 const busy=disabled||state.busy||credentialBusy||Boolean(state.uncertainId),config=state.snapshot.configuration;
 useEffect(()=>{if(!config?.enabled)return;const timer=setInterval(()=>{if(!state.busy&&!credentialBusy)void controller.current?.refresh();},3000);return()=>clearInterval(timer);},[config?.enabled,state.busy,credentialBusy]);
 const submit=(command:QqCommand)=>controller.current?.submit(command)??Promise.resolve(undefined);
 return <section className="qq-settings" aria-label="QQ 官方机器人"><h3>QQ 官方机器人</h3>
 <p className="help-text">通过官方出站连接接收消息；不公开本机控制端口，不把后台管理令牌交给 QQ。配置、连接、授权、任务成功和消息送达是不同状态。</p>
 {config?.mode==="fixture"&&<p className="execution-warning">当前是本地协议夹具，不代表真实 QQ 已通过验收。</p>}
 <p>连接：{qqStateLabel(state.snapshot.connection.state)}{config?` · AppID ${config.app_id}`:" · 尚未配置"}</p>
 {state.snapshot.connection.reason==="credential_unavailable"&&<p className="execution-warning">系统凭据暂不可读。请先在本机解锁或修复凭据，然后明确保存一次配置以重新连接；不会循环弹出凭据请求。</p>}
 {state.snapshot.connection.reason==="storage"&&<p className="execution-warning">本地存储暂不可用，未推进未提交的消息游标，请检查磁盘与数据库。</p>}
 <QqConfigurationForm key={config?`${config.id}:${config.revision}`:"unconfigured"} configuration={config} disabled={busy} onSubmit={submit}/>
 {config&&credentialClient&&<QqCredentials key={`${config.id}:${config.revision}`} config={config} operations={state.snapshot.credential_operations} client={credentialClient} disabled={disabled||state.busy||Boolean(state.uncertainId)} onRefresh={()=>controller.current?.refresh()} onBusyChange={setCredentialBusy}/>}
 <div className="qq-actions"><Button disabled={disabled||state.loading} onClick={()=>void controller.current?.refresh()}>刷新状态</Button><Button disabled={busy||!config?.enabled} onClick={()=>void submit({type:"issue_pairing",ttl_seconds:300})}>签发 5 分钟一次性配对码</Button></div>
 {state.pairing?.code&&<div className="qq-pairing" role="status"><p>配对码只显示此次，请在目标 QQ 对话发送：</p><code>/配对 {state.pairing.code}</code><p>有效至 {new Date(state.pairing.expires_at_ms).toLocaleString()}。提交后仍需回到本机确认，配对码本身不授予控制权。</p><Button onClick={()=>controller.current?.hidePairing()}>隐藏配对码</Button></div>}
 {state.notice&&<p role="status">{state.notice}</p>}
 {state.uncertainId&&<div className="execution-warning"><p>未知操作 ID：{state.uncertainId}。不能把停止观察当作撤销；不会自动重发或重新签发配对码。</p><Button disabled={disabled||state.busy} onClick={()=>void controller.current?.inspect()}>只读检查原命令收据</Button></div>}
 <p className="help-text">QQ群控制按具体成员与群分别配对；结果分享独立授权。动作批准、凭据、服务和项目设置只能在本机操作。</p>
 {state.snapshot.bindings.length===0?<p>尚无待确认或已绑定身份。</p>:state.snapshot.bindings.map(binding=><QqBindingCard key={`${binding.principal_id}:${binding.revision}`} binding={binding} sessions={sessions} executor={executor} disabled={busy} onSubmit={submit}/>)}
 {(state.snapshot.ignored_inputs?.clock_fault_ms??0)>Date.now()&&<p className="execution-warning">平台消息时间异常，当前安全截止点为 {clockLabel(state.snapshot.ignored_inputs!.clock_fault_ms!)}。未入队且不晚于该点的消息不会延后执行；请核对本机与平台时间，之后发送新消息，不重发旧指令。</p>}
 {state.snapshot.ignored_inputs&&<p className="help-text">已忽略 {state.snapshot.ignored_inputs.count} 次未获接收资格的输入（包括未配对、无效配对码或安全窗口内消息）。仅保存汇总，不占控制指令队列；不代表这些消息已执行。</p>}
 <h4>最近投递</h4><p className="help-text">默认被动回复窗口不超过 5 分钟，长任务可能无法自动通知。结果仍保存在本机，可在 QQ 发新消息查询；不会改用主动推送。</p>
 {state.snapshot.outbox.length===0?<p>暂无投递记录。</p>:<ul className="qq-outbox">{state.snapshot.outbox.map(item=><li key={item.id}><strong>{qqStateLabel(item.status)}</strong><span>{item.id}</span><small>原回复序号 {item.msg_seq} · 已尝试 {item.attempts} 次</small>{item.status==="unknown"&&<p>可能已经送达，不自动重试，不用新序号重发。</p>}</li>)}</ul>}
 </section>;
}
