import { useEffect, useRef, useState } from "react";
import { requestCommand, requestPending } from "../execution-controls";
import type { Command, ExecutionRequest, Receipt, TaskRun } from "../types";
import { Button } from "./ui/button";

const LABELS:Record<string,string>={pending:"等待明确决定",answered:"回答已记录（不产生授权）",approved:"本次批准已记录（不是执行结果）",denied:"已拒绝",expired:"已过期，未授权",cancelled:"已取消"};
export function ExecutionRequestCard({ request, task, disabled, onCommand }: {
  request: ExecutionRequest; task?: TaskRun; disabled: boolean; onCommand: (command: Command) => Promise<Receipt | undefined>;
}) {
  const [answer,setAnswer]=useState("");const [confirmed,setConfirmed]=useState(false);
  const [pending,setPending]=useState(false);const [result,setResult]=useState("");const [now,setNow]=useState(Date.now());
  const sending=useRef(false), alive=useRef(false);
  useEffect(()=>{alive.current=true;const timer=setInterval(()=>setNow(Date.now()),1000);return()=>{alive.current=false;clearInterval(timer);};},[]);
  const allowed=requestPending(request,task,now);
  const actionValid=!!request.action && !!request.action_digest && /^[a-f0-9]{64}$/.test(request.action_digest);
  const details=request.action?.details;
  const previewTruncated=!!details && typeof details === "object" && "content_preview_truncated" in details && details.content_preview_truncated === true;
  async function decide(decision:{answer:string}|{approve:boolean}) {
    if(sending.current || disabled)return;
    sending.current=true;setPending(true);setResult("");
    try {
      const receipt=await onCommand(requestCommand(request,task,decision));
      if(alive.current && receipt)setResult("决定已接收，等待后台状态；没有自动重送，也不代表操作已完成。");
    } catch(error){if(alive.current)setResult(error instanceof Error?error.message:String(error));}
    finally{sending.current=false;if(alive.current)setPending(false);}
  }
  return <section className="execution-request" aria-label={request.kind === "question" ? "任务问题" : "逐次操作授权"}>
    <h3>{request.kind === "question" ? "任务需要你的回答" : "请核对本次具体操作"} · {LABELS[request.status] ?? request.status}</h3>
    <p className="help-text">任务：{task?.title ?? request.task_id} <code>{request.task_id}</code><br/>请求 <code>{request.id}</code> · 修订 {request.revision} · 截止 {new Date(request.expires_at_ms).toLocaleString("zh-CN")}</p>
    <p className="execution-request-prompt">{request.prompt}</p>
    {request.reason && <p className="help-text">{request.reason}</p>}
    {request.kind === "approval" && <>
      <pre className="execution-action">{JSON.stringify(request.action,null,2)}</pre>
      {previewTruncated && <p className="execution-warning">写入内容预览已截断，不是完整文件内容；操作摘要绑定后台待执行的完整参数。无法确认时请拒绝。</p>}
      <p className="execution-digest">操作摘要：<code>{request.action_digest ?? "缺失，不能批准"}</code></p>
      <p className="execution-warning">批准只适用于此任务、当前修订与具体操作。命令可能在本机 shell 中执行，非项目沙箱；拒绝会停止依赖此授权的执行。</p>
    </>}
    {request.status === "pending" && !allowed && <p role="status">请求已过期或不再属于当前等待任务；按钮已禁用，请刷新核对。</p>}
    {request.status === "pending" && request.kind === "question" && <>
      <textarea aria-label="回答任务问题" value={answer} onChange={event=>setAnswer(event.target.value)} disabled={disabled || pending || !allowed} rows={3} />
      <p className="help-text">这是问题回答，不会授予写文件或运行命令权限。</p>
      <Button type="button" disabled={disabled || pending || !allowed || !answer.trim()} onClick={()=>void decide({answer})}>提交回答</Button>
    </>}
    {request.status === "pending" && request.kind === "approval" && <>
      <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled || pending || !allowed} onChange={event=>setConfirmed(event.target.checked)} />我已核对上方任务、具体操作及摘要</label>
      <div className="execution-request-actions">
        <Button type="button" disabled={disabled || pending || !allowed || !confirmed || !actionValid} onClick={()=>void decide({approve:true})}>仅批准此次操作</Button>
        <Button type="button" variant="secondary" disabled={disabled || pending || !allowed || !actionValid} onClick={()=>void decide({approve:false})}>拒绝此次操作</Button>
      </div>
    </>}
    {request.answer && <p className="execution-answer">已记录回答：{request.answer}</p>}
    {result && <p role="status">{result}</p>}
  </section>;
}

export function ExecutionRequests({ requests, tasks, disabled, onCommand }: {
  requests: ExecutionRequest[]; tasks: TaskRun[]; disabled: boolean; onCommand: (command: Command) => Promise<Receipt | undefined>;
}) {
  const pending=requests.filter(request=>request.status === "pending");
  const recent=requests.filter(request=>request.status !== "pending").slice(-8);
  if(!requests.length)return null;
  const card=(request:ExecutionRequest)=><ExecutionRequestCard key={`${request.id}:${request.revision}:${request.action_digest ?? "question"}`} request={request} task={tasks.find(task=>task.id === request.task_id)} disabled={disabled} onCommand={onCommand}/>;
  return <div className="execution-requests">{pending.map(card)}{!!recent.length && <details><summary>最近交互记录 · {recent.length} 条</summary>{recent.map(card)}</details>}</div>;
}
