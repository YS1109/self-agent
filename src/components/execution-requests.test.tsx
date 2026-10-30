import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ExecutionRequestCard, ExecutionRequests } from "./execution-requests";
import { DESKTOP_SOURCE, type ExecutionRequest, type TaskRun } from "../types";
const task:TaskRun={id:"t",session_id:"s",title:"任务",status:"waiting_input",waiting_request_id:"r",execution_mode:"native",source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[]};
const question:ExecutionRequest={id:"r",task_id:"t",session_id:"s",kind:"question",prompt:"目录<script>？",action:null,action_digest:null,revision:2,status:"pending",expires_at_ms:Date.now()+600000,answer:null};
describe("请求卡片不推断授权",()=>{
 it("问题只有回答按钮且说明不授予权限，内容转义",()=>{
  const html=renderToStaticMarkup(<ExecutionRequestCard request={question} task={task} disabled={false} onCommand={async()=>undefined}/>);
  expect(html).toContain("提交回答");expect(html).toContain("不会授予写文件或运行命令权限");expect(html).not.toContain("仅批准此次操作");expect(html).toContain("&lt;script&gt;");expect(html).not.toContain("<script>");
 });
 it("批准先展示固定任务、修订、操作和摘要，未勾选不能批准",()=>{
  const approval={...question,kind:"approval" as const,action:{tool:"write_file",details:{path:"a.ts",content_preview:"safe code",content_preview_truncated:true}},action_digest:"a".repeat(64)};
  const html=renderToStaticMarkup(<ExecutionRequestCard request={approval} task={task} disabled={false} onCommand={async()=>undefined}/>);
  expect(html).toContain("修订 2");expect(html).toContain("a".repeat(64));expect(html).toContain("safe code");expect(html).toContain("写入内容预览已截断");
  expect(html).toMatch(/<button[^>]*disabled[^>]*>仅批准此次操作<\/button>/);
  expect(html).toContain("拒绝此次操作");expect(html).not.toContain("提交回答");
 });
 it("过期/错误任务禁用提交，历史批准不是操作执行成功",()=>{
  const html=renderToStaticMarkup(<ExecutionRequestCard request={{...question,expires_at_ms:1}} task={task} disabled={false} onCommand={async()=>undefined}/>);
  expect(html).toContain("按钮已禁用");expect(html).toMatch(/<textarea[^>]*disabled/);
  const history=renderToStaticMarkup(<ExecutionRequests requests={[{...question,status:"approved",kind:"approval",action:{},action_digest:"a".repeat(64)}]} tasks={[task]} disabled={false} onCommand={async()=>undefined}/>);
  expect(history).toContain("本次批准已记录（不是执行结果）");expect(history).not.toContain("仅批准此次操作");
 });
});
