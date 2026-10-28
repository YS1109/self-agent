import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ExecutionCleanupNotice, ExecutionSettings, TaskExecutionDetails } from "./execution-settings";
import { defaultExecutionControls } from "../execution-controls";
import { DESKTOP_SOURCE, type TaskRun } from "../types";
const task:TaskRun={id:"t",session_id:"s",title:"任务",status:"failed",execution_mode:"native",source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[],controls:defaultExecutionControls(),budget_usage:{model_requests:3,retries:0,repair_attempts:1,output_bytes:50,mutation_generation:2,started_at:"2026-10-28T00:00:00Z"},stop_reason:"repair_budget_exhausted"};
describe("执行控制诚实展示",()=>{
 it("取消和重启显示稳定停止原因且不声称已清理",()=>{
  const cancelled=renderToStaticMarkup(<TaskExecutionDetails task={{...task,stop_reason:"user_cancelled"}}/>);
  expect(cancelled).toContain("用户已取消任务");
  const interrupted=renderToStaticMarkup(<TaskExecutionDetails task={{...task,stop_reason:"daemon_interrupted"}}/>);
  expect(interrupted).toContain("后台中断执行，未自动重放");
 });
 it("Native 权限不被描述为 shell 沙箱，任务覆盖只能收紧",()=>{
  const html=renderToStaticMarkup(<ExecutionSettings value={defaultExecutionControls()} onChange={()=>{}} executor="native" disabled={false} scope="task"/>);
  expect(html).toContain("不能借此扩大权限");expect(html).toContain("不是项目目录、文件系统或网络沙箱");expect(html).toContain("HARNESS_ALLOW_COMMANDS=1");
 });
 it("Codex 禁用不支持的维度，默认数值不伪装成已执行的限制",()=>{
  const html=renderToStaticMarkup(<ExecutionSettings value={defaultExecutionControls()} onChange={()=>{}} executor="codex" disabled={false}/>);
  expect(html).toMatch(/aria-label="模型请求上限（含压缩）"[^>]*disabled/);
  expect(html).not.toMatch(/aria-label="运行时长上限（秒）"[^>]*disabled/);
  expect(html).toContain("默认数值不是已生效限制");
  const details=renderToStaticMarkup(<TaskExecutionDetails task={{...task,agent:{id:"c",name:"Codex",executor:"codex",model:"",instructions:""}}}/>);
  expect(details).toContain("Codex 不支持计量");expect(details).not.toContain("3 / 32");
 });
 it("保留失败、退出码未知与旧修改版本证据，不靠文字宣布验收成功",()=>{
  const html=renderToStaticMarkup(<TaskExecutionDetails task={{...task,verification_attempts:[{id:"a",command:"pnpm test",exit_code:1,output:"真实失败<script>",mutation_generation:1,created_at:"now"},{id:"b",command:"pnpm test",exit_code:null,output:"结果未知",mutation_generation:2,created_at:"now"}]}}/>);
  expect(html).toContain("修复轮次预算耗尽");expect(html).toContain("退出码 1");expect(html).toContain("退出码 未知");expect(html).toContain("旧版本证据");expect(html).toContain("真实失败&lt;script&gt;");expect(html).not.toContain("<script>");
 });
 it("终态和执行清理分开，pending/unknown均不宣称副作用停止",()=>{
  const pending=renderToStaticMarkup(<TaskExecutionDetails task={{...task,status:"cancelled",execution_cleanup:"pending"}}/>);
  expect(pending).toContain("任务终态不表示副作用已经停止");expect(pending).toContain("项目执行权仍由后台保留");
  const unknown=renderToStaticMarkup(<ExecutionCleanupNotice task={{...task,status:"failed",execution_cleanup:"unknown"}}/>);
  expect(unknown).toContain("无法确认原写入是否完成");expect(unknown).toContain("不会自动重放");
  const settled=renderToStaticMarkup(<ExecutionCleanupNotice task={{...task,status:"cancelled",execution_cleanup:"settled"}}/>);
  expect(settled).toContain("不表示任务成功、改动已撤销或验收通过");
  expect(renderToStaticMarkup(<ExecutionCleanupNotice task={task}/>)).toBe("");
 });

});
