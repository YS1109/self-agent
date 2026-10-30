import { describe, expect, it } from "vitest";
import { BUDGET_FIELDS, MAX_ACCEPTANCE_COMMAND_BYTES, controlsForTask, intersectExecutionControls, defaultExecutionControls, executionControlsError, requestCommand, requestPending } from "./execution-controls";
import { DESKTOP_SOURCE, type ExecutionRequest, type TaskRun } from "./types";
const task:TaskRun={id:"t",session_id:"s",title:"任务",status:"waiting_input",waiting_request_id:"r",execution_mode:"native",source:DESKTOP_SOURCE,created_at:"now",updated_at:"now",steps:[]};
const question:ExecutionRequest={id:"r",task_id:"t",session_id:"s",kind:"question",prompt:"需要哪个目录？",action:null,action_digest:null,revision:2,status:"pending",expires_at_ms:2000,answer:null};
const approval:ExecutionRequest={...question,kind:"approval",action:{tool:"write_file",details:{path:"a.ts",content_preview:"内容"}},action_digest:"a".repeat(64)};
describe("预算与权限输入",()=>{
 it("逐项校验数值边界和整数，保持独立默认对象",()=>{
  expect(executionControlsError(defaultExecutionControls(),"native")).toBeUndefined();
  for(const field of BUDGET_FIELDS) for(const invalid of [field.min-1,field.max+1,1.5,NaN]){
    const value=defaultExecutionControls();value.budget[field.key]=invalid;
    expect(executionControlsError(value,"native")).toBeDefined();
  }
  const one=defaultExecutionControls();one.permissions.write_files="deny";
  expect(defaultExecutionControls().permissions.write_files).toBe("allow");
 });
 it("不能用不受沙箱限制的 shell 绕过文件限制或逐次批准",()=>{
  for(const permissions of [{read_files:false,write_files:"allow" as const,run_commands:"ask" as const},{read_files:true,write_files:"deny" as const,run_commands:"allow" as const},{read_files:true,write_files:"ask" as const,run_commands:"allow" as const}])
    expect(executionControlsError({...defaultExecutionControls(),permissions:{...permissions,managed_tools:"deny"}},"native")).toBeDefined();
  expect(executionControlsError({...defaultExecutionControls(),permissions:{read_files:false,write_files:"deny",run_commands:"deny",managed_tools:"deny"}},"native")).toBeUndefined();
 });
 it("Codex 只发支持的两维，不发送看似默认但无法执行的模型预算",()=>{
  const value=defaultExecutionControls();value.budget.max_runtime_seconds=30;value.budget.max_output_bytes=1024;
  expect(controlsForTask(value,"codex")).toEqual({budget:{max_runtime_seconds:30,max_output_bytes:1024},capabilities:{mcp_servers:[],skills:[]}});
  const restricted=defaultExecutionControls();restricted.permissions.write_files="ask";restricted.permissions.run_commands="ask";
  expect(()=>controlsForTask(restricted,"codex")).toThrow("不会静默放宽");
  expect(restricted.permissions.write_files).toBe("ask");
  const lower=defaultExecutionControls();lower.budget.max_model_requests=2;
  expect(()=>controlsForTask(lower,"codex")).toThrow();
 });
 it("托管能力默认拒绝、调用预算有界，角色/任务范围只能取交集",()=>{
  const first="00000000-0000-0000-0000-000000000001", second="00000000-0000-0000-0000-000000000002";
  const base=defaultExecutionControls();base.capabilities={mcp_servers:[first],skills:[first,second]};base.permissions.managed_tools="ask";
  const role=defaultExecutionControls();role.capabilities={mcp_servers:[second],skills:[second]};role.budget.max_external_calls=3;
  const result=intersectExecutionControls(base,role);
  expect(result.capabilities).toEqual({mcp_servers:[],skills:[second]});expect(result.permissions.managed_tools).toBe("deny");expect(result.budget.max_external_calls).toBe(3);
  expect(base.capabilities.skills).toEqual([first,second]);
  expect(()=>controlsForTask(base,"codex")).toThrow("MCP 仅支持 Native");
  role.capabilities.mcp_servers=[];
  expect(controlsForTask(role,"codex")).toMatchObject({capabilities:{mcp_servers:[],skills:[second]}});
  base.permissions.write_files="deny";base.permissions.run_commands="deny";
  expect(executionControlsError(base,"native")).toContain("托管工具");
  const invalid=defaultExecutionControls();invalid.capabilities.skills=[first,first];expect(executionControlsError(invalid,"native")).toContain("注册标识");
 });
 it("验收命令 UTF-8 上限与后台16000字节一致",()=>{
  expect(MAX_ACCEPTANCE_COMMAND_BYTES).toBe(16000);
  const controls=defaultExecutionControls();
  const boundary="中".repeat(5333)+"a";
  expect(new TextEncoder().encode(boundary)).toHaveLength(16000);
  controls.acceptance_commands=[boundary];
  expect(executionControlsError(controls,"native")).toBeUndefined();
  controls.acceptance_commands=[boundary+"b"];
  expect(new TextEncoder().encode(controls.acceptance_commands[0])).toHaveLength(16001);
  expect(executionControlsError(controls,"native")).toContain("16000 UTF-8 字节");
 });
 it("角色预览只收紧全局限制，不能移除既有验收命令",()=>{
  const global=defaultExecutionControls();global.budget.max_model_requests=5;global.permissions.run_commands="ask";global.acceptance_commands=["owner-test"];
  const role=defaultExecutionControls();role.budget.max_runtime_seconds=30;role.acceptance_commands=["role-test"];
  const effective=intersectExecutionControls(global,role);
  expect(effective.budget.max_model_requests).toBe(5);expect(effective.budget.max_runtime_seconds).toBe(30);
  expect(effective.permissions.run_commands).toBe("ask");expect(effective.acceptance_commands).toEqual(["owner-test","role-test"]);
  expect(executionControlsError(effective,"codex")).toBeDefined();
  expect(global.budget.max_runtime_seconds).toBe(900);
 });
 it("验收原样保存，拒绝空行、重复、超限或无法执行的要求",()=>{
  const value=defaultExecutionControls();value.acceptance_commands=["pnpm test -- --runInBand"];
  expect(controlsForTask(value,"native").acceptance_commands).toEqual(value.acceptance_commands);
  for(const commands of [[""],["same","same"],["中".repeat(5500)],Array.from({length:9},(_,i)=>`test${i}`)]){
    expect(executionControlsError({...value,acceptance_commands:commands},"native")).toBeDefined();
  }
  expect(executionControlsError({...value,permissions:{...value.permissions,run_commands:"deny"}},"native")).toBeDefined();
 });
});
describe("问题与逐次授权绑定",()=>{
 it("回答不能变成授权，批准和拒绝必须携带完整具体操作摘要",()=>{
  expect(requestCommand(question,task,{answer:"项目 A"},1000)).toEqual({type:"answer_request",request_id:"r",expected_revision:2,answer:"项目 A"});
  for(const approve of [true,false]) expect(requestCommand(approval,task,{approve},1000)).toEqual({type:"decide_request",request_id:"r",expected_revision:2,approve,action_digest:"a".repeat(64)});
  expect(()=>requestCommand(question,task,{approve:true},1000)).toThrow("回答不是授权");
  expect(()=>requestCommand(approval,task,{answer:"yes"},1000)).toThrow();
  expect(()=>requestCommand({...approval,action_digest:null},task,{approve:true},1000)).toThrow();
 });
 it("过期、重复决定、取消、错误会话和新等待请求均不发送命令",()=>{
  expect(requestPending(question,task,1999)).toBe(true);
  for(const now of [2000,9999]) expect(()=>requestCommand(question,task,{answer:"A"},now)).toThrow("请求已改变");
  for(const changed of [{...task,status:"cancelled"},{...task,waiting_request_id:"new"},{...task,session_id:"other"},undefined])
    expect(()=>requestCommand(question,changed,{answer:"A"},1000)).toThrow();
  for(const status of ["answered","approved","denied","cancelled","expired"] as const)
    expect(()=>requestCommand({...question,status},task,{answer:"A"},1000)).toThrow();
 });
 it("拒绝空回答和超大 UTF-8 回答，不改变用户正文",()=>{
  expect(()=>requestCommand(question,task,{answer:" "},1000)).toThrow();
  expect(()=>requestCommand(question,task,{answer:"中".repeat(2731)},1000)).toThrow();
  expect(requestCommand(question,task,{answer:"  保留格式\n继续"},1000)).toMatchObject({answer:"  保留格式\n继续"});
 });
});
