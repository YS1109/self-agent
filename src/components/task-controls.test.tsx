import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { TaskSupplements, ContinueTask } from "./task-controls";
import { TaskContext } from "./task-stream";
import { DESKTOP_SOURCE, type TaskRun, type RunEvent, type TaskSupplement } from "../types";
const task:TaskRun={id:"t",session_id:"s",task_kind:"main",title:"任务",status:"failed",execution_mode:"native",source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[]};
describe("补充与上下文语义",()=>{
 it("逐一展示后台确认状态，不把收据称作模型遵从",()=>{
  const supplements:TaskSupplement[]=["pending","delivering","applied","not_applied","unknown"].map(status=>({id:status,task_id:"t",content:"<script>用户补充</script>",status:status as TaskSupplement["status"],created_at:"now",updated_at:"now",reason:"实际原因"}));
  const html=renderToStaticMarkup(<TaskSupplements task={{...task,supplements}}/>);
  for(const label of ["等待安全边界","尚未确认","已纳入执行器输入","未应用","结果未知","不保证模型遵从","实际原因"]) expect(html).toContain(label);
  expect(html).toContain("&lt;script&gt;");expect(html).not.toContain("<script>");
 });
 it("继续说明新任务和不重放；无后台记录不编造补充",()=>{
  expect(renderToStaticMarkup(<TaskSupplements task={task}/>)).toBe("");
  const html=renderToStaticMarkup(<ContinueTask task={task} disabled={false} onCommand={async()=>undefined} onCreated={()=>{}}/>);
  expect(html).toContain("创建新任务");expect(html).toContain("不会自动重放");
 });
 it("Codex估算不是累计线程占用或服务实际用量，使用最新事件",()=>{
  const events:RunEvent[]=[100,200].map((tokens,index)=>({id:`e${index}`,sequence:index+1,task_id:"t",kind:"context_estimate",message:"估算",created_at:"now",data:{kind:"context_estimate",estimated_input_tokens:tokens,context_window:128000,compression_threshold:80,source:"codex_current_input_only",phase:"before_turn"}}));
  const html=renderToStaticMarkup(<TaskContext task={task} events={events}/>);
  expect(html).toContain("本地输入估算");expect(html).toContain("约 200 token");expect(html).toContain("压缩阈值 80%");expect(html).toContain("不含隐藏线程");expect(html).not.toContain("约 300");expect(html).not.toContain("服务报告");
 });
 it("重开任务使用持久上下文，不依赖保留事件也不被较早事件覆盖",()=>{
  const durable={...task,context_estimate:{estimated_input_tokens:2500,context_window:128000,compression_threshold:80,source:"native_local",phase:"request",updated_at:"now"},context_compaction:{state:"completed",updated_at:"now"}};
  const events:RunEvent[]=[{id:"old",sequence:1,task_id:"t",kind:"context_compaction",message:"压缩",created_at:"old",data:{kind:"context_compaction",state:"started"}}];
  for(const retained of [events,[]]){
    const html=renderToStaticMarkup(<TaskContext task={durable} events={retained}/>);
    expect(html).toContain("约 2,500 token");expect(html).toContain("压缩完成");expect(html).not.toContain("压缩中");
  }
 });

});
