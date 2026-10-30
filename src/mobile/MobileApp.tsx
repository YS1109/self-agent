import {useEffect,useRef,useState} from "react";
import {createHttpMobileApi,createMobileController,codeMessage,inputError} from "./client";
import type {ArtifactSummary,MobileApi,MobileState,MobileTask,Question,TextArtifact} from "./client";
import "./mobile.css";
const api=createHttpMobileApi();
const EMPTY:MobileState={session:null,sessions:[],selected:null,snapshot:null,busy:false,notice:"",unknown:null,pairing:null,connected:false};
type Controller=ReturnType<typeof createMobileController>;
const status=(value:string)=>({queued:"排队中",running:"执行中",waiting_input:"等待输入",completed:"已完成",failed:"已停止",cancelled:"已取消"})[value]??value;

export function MobileApp({client=api}:{client?:MobileApi}) {
 const [state,setState]=useState<MobileState>(EMPTY),[draft,setDraft]=useState(""),[zoom,setZoom]=useState(100),[dark,setDark]=useState(false),[steerTarget,setSteerTarget]=useState<string|null>(null);
 const ref=useRef<Controller|null>(null);
 const [detail,setDetail]=useState<{title:string;content:string;artifacts:ArtifactSummary[]}|null>(null),[detailBusy,setDetailBusy]=useState(false);
 const detailEpoch=useRef(0);
 useEffect(()=>{let storage:Storage|undefined;try{storage=window.sessionStorage;}catch{}const controller=createMobileController(client,setState,{storage});ref.current=controller;void controller.open();return()=>{detailEpoch.current++;controller.dispose();if(ref.current===controller)ref.current=null;};},[client]);
 useEffect(()=>{detailEpoch.current++;setDetail(null);setDetailBusy(false);setSteerTarget(null);},[state.selected]);
 function closeDetail(){detailEpoch.current++;setDetail(null);setDetailBusy(false);}
 async function inspect(task:MobileTask){
  const c=ref.current;if(!c)return;const version=++detailEpoch.current;setDetail({title:task.title,content:"正在只读获取…",artifacts:[]});setDetailBusy(true);
  try {const result=state.session?.permissions?.read_results?await c.read<unknown>({type:"changes",task_id:task.id}):null;
   const artifacts=state.session?.permissions?.read_artifacts?await c.read<{artifacts:ArtifactSummary[]}>({type:"artifacts",task_id:task.id}):null;
   if(version!==detailEpoch.current)return;setDetail({title:task.title,content:result?result.code==="ok"?JSON.stringify(result.data,null,2):codeMessage(result.code):"未授权查看 diff。",artifacts:artifacts?.code==="ok"?artifacts.data?.artifacts??[]:[]});
  }catch{if(version===detailEpoch.current)setDetail({title:task.title,content:"读取中断；没有提交执行命令。",artifacts:[]});}finally{if(version===detailEpoch.current)setDetailBusy(false);}
 }
 async function artifact(item:ArtifactSummary){const c=ref.current;if(!c)return;const version=++detailEpoch.current;setDetailBusy(true);try{const result=await c.read<{artifact:TextArtifact}>({type:"artifact",resource_id:item.id});if(version!==detailEpoch.current)return;setDetail({title:item.name,content:result.code==="ok"&&result.data?`${result.data.artifact.truncated?"[有界内容，已截断]\n":""}${result.data.artifact.content}`:codeMessage(result.code),artifacts:[]});}catch{if(version===detailEpoch.current)setDetail({title:item.name,content:"产物读取未完成；权限或连接可能已改变。",artifacts:[]});}finally{if(version===detailEpoch.current)setDetailBusy(false);}}
 return <div className={`mobile-workbench ${dark?"mobile-dark":""}`} style={{fontSize:`${zoom}%`}}>
  <header className="mobile-header"><div><span className="mobile-eyebrow">SELF AGENT · 自托管</span><h1>同一个会话，随时继续</h1></div><div className="mobile-header-actions"><button onClick={()=>setDark(value=>!value)} aria-label="切换明暗主题">{dark?"浅色":"深色"}</button><select aria-label="界面缩放" value={zoom} onChange={e=>setZoom(Number(e.target.value))}><option value={90}>90%</option><option value={100}>100%</option><option value={115}>115%</option><option value={130}>130%</option></select></div></header>
  <main className="mobile-main">
   <aside className="mobile-status" role="status"><span className={`mobile-dot ${state.connected?"online":""}`}/><span>{state.session?.paired?state.connected?"设备在线 · 事件已连接":"会话已配对 · 正在读取本机状态":"尚未配对"}</span></aside>
   {state.notice&&<p className="mobile-notice" aria-live="polite">{state.notice}</p>}
   {state.session?.mode==="fixture"&&<p className="mobile-notice">本地协议夹具 · 不是已部署的公网网关，不使用真实凭据或外部服务。</p>}
   {!state.session?.paired?<><PairForm disabled={state.busy||!state.session} pairing={state.pairing?.status} onPair={body=>void ref.current?.pair(body)} onCheck={()=>void ref.current?.pairStatus()}/>{state.session&&<button disabled={state.busy} onClick={()=>void ref.current?.logout()}>清除浏览器配对状态并重新开始</button>}</>:<>
    <div className="mobile-session-bar"><label>当前会话<select value={state.selected??""} disabled={state.busy} onChange={e=>{closeDetail();void ref.current?.select(e.target.value);}}>{state.sessions.map(session=><option key={session.id} value={session.id}>{session.title}</option>)}</select></label><button onClick={()=>void ref.current?.refresh(undefined,true)} disabled={state.busy}>刷新</button><button onClick={()=>void ref.current?.logout()} disabled={state.busy}>退出</button></div>
    <p className="mobile-scope">仅控制本机确认的会话与动作。退出不取消已提交任务；网关重启后需重新配对，任务仍保存在设备中。</p>
    {state.unknown&&<section className="mobile-unknown" aria-label="未知命令"><h2>这次提交尚未确认</h2><p>不会自动重发，也不会换一个标识再次执行。</p><code>{state.unknown.command_id}</code><button disabled={state.busy} onClick={()=>void ref.current?.reconcile()}>只读检查原回执</button><p>暂未找到回执不表示一定未执行；可在本机用此标识核对。</p></section>}
    <section className="mobile-conversation" aria-label="会话内容">
     {state.snapshot?.truncated&&<p className="mobile-scope">仅显示最近的有界历史，完整记录保留在本机。</p>}
     {state.snapshot?.session.messages.map(message=><article key={message.id} className={`mobile-message ${message.role==="user"?"from-user":""}`}><h2>{message.role==="user"?"你":message.role==="assistant"?"Self Agent":"系统"}</h2><pre>{message.content}</pre></article>)}
     {!state.snapshot&&<p>正在只读获取会话…</p>}
    </section>
    <section className="mobile-task-list" aria-label="任务进度">{state.snapshot?.tasks.map(task=><TaskCard key={task.id} task={task} disabled={state.busy||!!state.unknown} canRun={!!state.session?.permissions?.run} canCancel={!!state.session?.permissions?.cancel} canInspect={!!(state.session?.permissions?.read_results||state.session?.permissions?.read_artifacts)} onCancel={()=>void ref.current?.submit({type:"cancel",task_id:task.id})} onContinue={()=>void ref.current?.submit({type:"continue",task_id:task.id,content:null})} onInspect={()=>void inspect(task)} onSteer={()=>setSteerTarget(task.id)}/>)}</section>
    <section aria-label="等待回答">{state.snapshot?.questions.map(question=><QuestionCard key={`${question.id}:${question.revision}`} question={question} task={state.snapshot?.tasks.find(t=>t.id===question.task_id)} disabled={state.busy||!!state.unknown||!state.session?.permissions?.answer} onAnswer={answer=>void ref.current?.submit({type:"answer",request_id:question.id,expected_revision:question.revision,answer})}/>)}</section>
    <form className="mobile-composer" onSubmit={event=>{event.preventDefault();if(inputError(draft)||state.busy||state.unknown||!state.session?.permissions?.run)return;if(steerTarget&&!state.snapshot?.tasks.some(t=>t.id===steerTarget&&["queued","running","waiting_input"].includes(t.status)))return;const content=draft;setDraft("");void ref.current?.submit(steerTarget?{type:"steer",task_id:steerTarget,content}:{type:"run",content});}}><label htmlFor="mobile-objective">{steerTarget?"明确补充所选任务":"在此会话发起新任务"}</label>{steerTarget&&<p className="mobile-scope">目标任务：<code>{steerTarget}</code>。旧任务权限超出远程上限时拒绝，不会静默切换为新任务。<button type="button" onClick={()=>setSteerTarget(null)}>取消补充模式</button></p>}<textarea id="mobile-objective" value={draft} onChange={e=>setDraft(e.target.value)} rows={3} placeholder="描述下一步目标…" disabled={state.busy||!!state.unknown||!state.session?.permissions?.run}/><div><small>命令只提交一次；执行权限以本机限制为准。</small><button type="submit" className="mobile-primary" disabled={state.busy||!!state.unknown||!state.selected||!state.session?.permissions?.run||!!inputError(draft)||(!!steerTarget&&!state.snapshot?.tasks.some(t=>t.id===steerTarget&&["queued","running","waiting_input"].includes(t.status)))}>{steerTarget?"提交补充":"发送新任务"}</button></div></form>
   </>}
   {detail&&<section className="mobile-detail" role="dialog" aria-modal="true" aria-label={detail.title}><header><h2>{detail.title}</h2><button onClick={closeDetail}>关闭</button></header><p>只读文本产物 / 观察 diff，不提供任意路径下载。</p><pre>{detail.content}</pre>{detail.artifacts.map(item=><button key={item.id} disabled={detailBusy} onClick={()=>void artifact(item)}>{item.name} · {item.bytes} 字节{item.truncated?" · 截断":""}</button>)}</section>}
  </main><footer className="mobile-footer">设备执行 · 本机记录 · 显式授权</footer>
 </div>;
}

export function PairForm({disabled,pairing,onPair,onCheck}:{disabled:boolean;pairing?:string;onPair:(body:{device_id:string;code:string;label:string})=>void;onCheck:()=>void}){
 const [device,setDevice]=useState(""),[label,setLabel]=useState("我的手机");const secret=useRef<HTMLInputElement>(null);
 return <section className="mobile-pair"><span className="mobile-eyebrow">连接自己的设备</span><h2>先在本机确认，再远程操作</h2><p>从本机网关设置获取一次性配对码。它不会直接授予任务权限；本机还需要核对浏览器身份、会话和动作。</p><form onSubmit={event=>{event.preventDefault();const code=secret.current?.value.trim()??"";if(secret.current)secret.current.value="";onPair({device_id:device.trim(),label:label.trim(),code});}}><label>设备标识<input value={device} onChange={e=>setDevice(e.target.value)} autoComplete="off" spellCheck={false} disabled={disabled||!!pairing}/></label><label>浏览器名称<input value={label} onChange={e=>setLabel(e.target.value)} maxLength={80} disabled={disabled||!!pairing}/></label><label>一次性配对码<input ref={secret} type="password" autoComplete="off" spellCheck={false} disabled={disabled||!!pairing}/></label><button className="mobile-primary" disabled={disabled||!!pairing}>申请配对</button><button type="button" disabled={disabled} onClick={onCheck}>只读检查配对状态</button></form><p className="mobile-scope">不会保存配对码。网关重启后需要重新配对，不承诺持续登录。</p></section>;
}
export function TaskCard({task,disabled,canRun,canCancel,canInspect,onCancel,onContinue,onInspect,onSteer}:{task:MobileTask;disabled:boolean;canRun:boolean;canCancel:boolean;canInspect:boolean;onCancel:()=>void;onContinue:()=>void;onInspect:()=>void;onSteer?:()=>void}){
 const active=["queued","running","waiting_input"].includes(task.status);
 return <article className="mobile-task"><header><h2>{task.title}</h2><span>{status(task.status)}</span></header>{task.stream_output&&<pre>{task.stream_output}</pre>}{task.result&&<pre>{task.result}</pre>}{task.error&&<p>{task.error}</p>}{task.status==="waiting_input"&&<p className="mobile-scope">请核对下方问题；未显示的请求或执行授权须回本机处理。回答不是批准。</p>}{task.execution_cleanup==="pending"&&<p className="mobile-notice">清理仍在进行；任务终态不代表副作用已停止。</p>}{task.execution_cleanup==="unknown"&&<p className="mobile-notice">清理结果未知，请在本机检查，不自动重放。</p>}<div className="mobile-task-actions">{active&&canRun&&onSteer&&<button disabled={disabled} onClick={onSteer}>补充此任务</button>}{active&&canCancel&&<button disabled={disabled} onClick={onCancel}>取消任务</button>}{!active&&canRun&&<button disabled={disabled} onClick={onContinue}>基于已有结果继续</button>}{canInspect&&<button disabled={disabled} onClick={onInspect}>查看 diff / 文本产物</button>}</div></article>;
}
export function QuestionCard({question,task,disabled,onAnswer}:{question:Question;task?:MobileTask;disabled:boolean;onAnswer:(answer:string)=>void}){
 const [answer,setAnswer]=useState("");const active=question.status==="pending"&&question.expires_at_ms>Date.now()&&task?.status==="waiting_input"&&task.waiting_request_id===question.id;
 return <article className="mobile-question"><h2>{question.kind==="approval"?"需要本机授权":"需要你的回答"}</h2><p>{question.prompt}</p>{question.kind==="approval"?<p>远程回答不是执行授权。请回到本机核对具体操作；此处没有批准按钮。</p>:<form onSubmit={event=>{event.preventDefault();const value=answer;setAnswer("");onAnswer(value);}}><textarea aria-label="问题回答" value={answer} onChange={e=>setAnswer(e.target.value)} disabled={disabled||!active}/><button disabled={disabled||!active||!!inputError(answer)}>提交这次回答</button></form>}</article>;
}
