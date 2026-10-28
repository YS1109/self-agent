import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { LiveTaskMessage, ToolActivity, TaskUsage } from "./task-stream";
import { DESKTOP_SOURCE, type TaskRun, type RunEvent } from "../types";
const task:TaskRun={id:"t",session_id:"s",title:"任务",status:"running",execution_mode:"native",source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[],stream_output:"实际回复<script>不执行</script>"};
const events:RunEvent[]=[{id:"e",sequence:1,task_id:"t",kind:"tool_started",message:"开始",created_at:"now",data:{kind:"tool_started",call_id:"c",name:"read_file"}}];
describe("真实流式回复与工具展示",()=>{
 it("显示实际增量并转义正文，不添加推测的思考过程",()=>{
  const html=renderToStaticMarkup(<LiveTaskMessage task={task} events={events}/>);
  expect(html).toContain("实时回复");expect(html).toContain("实际回复&lt;script&gt;不执行&lt;/script&gt;");
  expect(html).toContain("真实工具执行");expect(html).toContain("读取文件");expect(html).not.toContain("<script>");
 });
 it("失败终态不能把只有开始记录的工具展示为成功",()=>{
  const html=renderToStaticMarkup(<ToolActivity task={{...task,status:"failed"}} events={events}/>);
  expect(html).toContain("未收到完成记录");expect(html).not.toContain("已完成");
  expect(renderToStaticMarkup(<ToolActivity task={task} events={[]}/>)).toBe("");
 });
 it("任务完成时使用最终结果，不重复中间回复",()=>{
  const html=renderToStaticMarkup(<LiveTaskMessage task={{...task,status:"completed",result:"最终结果"}} events={[]}/>);
  expect(html).toContain("最终结果");expect(html).not.toContain("实际回复");expect(html).not.toContain("执行中");
 });
 it("累计用量取最新服务报告，不相加或当作估算",()=>{
  const usage:RunEvent[]=[1,2].map(sequence=>({id:`u${sequence}`,sequence,task_id:"t",kind:"usage",message:"用量",created_at:"now",data:{kind:"usage",scope:"thread_total",input_tokens:sequence*100,output_tokens:null}}));
  const html=renderToStaticMarkup(<TaskUsage taskId="t" events={usage}/>);
  expect(html).toContain("会话累计用量（服务报告）");expect(html).toContain("输入 200");expect(html).toContain("输出 未知");expect(html).not.toContain("300");
 });

});
