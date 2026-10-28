import type { Command, ExecutionBudget, ExecutionRequest, TaskControls, TaskControlsInput, TaskRun, Settings } from "./types";

export function defaultExecutionControls(): TaskControls {
  return { budget: { max_model_requests: 32, max_runtime_seconds: 900, max_retries: 0, max_repair_attempts: 2, max_output_bytes: 262144 },
    permissions: { read_files: true, write_files: "allow", run_commands: "allow" }, acceptance_commands: [] };
}
export const MAX_ACCEPTANCE_COMMAND_BYTES = 16000;
export const BUDGET_FIELDS: { key: keyof ExecutionBudget; label: string; min: number; max: number; codex: boolean }[] = [
  {key:"max_model_requests",label:"模型请求上限（含压缩）",min:1,max:128,codex:false},
  {key:"max_runtime_seconds",label:"运行时长上限（秒）",min:1,max:900,codex:true},
  {key:"max_retries",label:"安全重试上限",min:0,max:3,codex:false},
  {key:"max_repair_attempts",label:"修复轮次上限",min:0,max:8,codex:false},
  {key:"max_output_bytes",label:"输出上限（字节）",min:1024,max:262144,codex:true},
];
export const permissionLabel = (mode: string) => ({allow:"允许（仍受后台能力上限限制）",ask:"每次询问",deny:"禁止"})[mode] ?? "未知";
const utf8Length = (text: string) => new TextEncoder().encode(text).length;
export function executionControlsError(value: TaskControls, executor: "native" | "codex"): string | undefined {
  for (const field of BUDGET_FIELDS) if (!Number.isInteger(value.budget[field.key]) || value.budget[field.key] < field.min || value.budget[field.key] > field.max)
    return `${field.label}必须为 ${field.min}–${field.max} 的整数。`;
  const permissions=value.permissions;
  if ((!permissions.read_files || permissions.write_files === "deny") && permissions.run_commands !== "deny") return "禁止读取或写入时也必须禁止本机命令；shell 不是项目沙箱。";
  if (permissions.write_files === "ask" && permissions.run_commands === "allow") return "写入需要授权时，本机命令也必须逐次询问或禁止。";
  if (value.acceptance_commands.length > 8 || new Set(value.acceptance_commands).size !== value.acceptance_commands.length
    || value.acceptance_commands.some(command => !command.trim() || command.includes("\0") || utf8Length(command) > MAX_ACCEPTANCE_COMMAND_BYTES)) return "验收命令最多 8 条，必须非空、不重复且每条最多 16000 UTF-8 字节。";
  if (value.acceptance_commands.length && permissions.run_commands === "deny") return "验收需要执行真实命令；命令被禁止时无法完成验收，请调整策略或移除新增验收要求。";
  if (executor === "codex") {
    const defaults=defaultExecutionControls();
    if (BUDGET_FIELDS.some(field=>!field.codex && value.budget[field.key] !== defaults.budget[field.key])
      || !permissions.read_files || permissions.write_files !== "allow" || permissions.run_commands !== "allow" || value.acceptance_commands.length)
      return "Codex 不支持本应用的内部请求/修复计量、逐工具权限或验收闭环。请改用 Native；不会静默放宽现有限制。";
  }
  return undefined;
}
/** Preview only: the Rust enqueue transaction remains authoritative. */
export function intersectExecutionControls(base: TaskControls, restriction?: TaskControls | null): TaskControls {
  if (!restriction) return structuredClone(base);
  const value=structuredClone(base);
  for(const field of BUDGET_FIELDS) value.budget[field.key]=Math.min(base.budget[field.key],restriction.budget[field.key]);
  const rank={allow:0,ask:1,deny:2} as const;
  value.permissions.read_files=base.permissions.read_files && restriction.permissions.read_files;
  for(const key of ["write_files","run_commands"] as const)
    value.permissions[key]=rank[base.permissions[key]] >= rank[restriction.permissions[key]] ? base.permissions[key] : restriction.permissions[key];
  value.acceptance_commands=[...new Set([...base.acceptance_commands,...restriction.acceptance_commands])];
  return value;
}
export function settingsExecutionError(settings: Settings): string | undefined {
  const global=executionControlsError(settings.execution_controls ?? defaultExecutionControls(),settings.main_executor ?? "native");
  if(global)return global;
  for(const agent of settings.agents) if(agent.controls){
    const error=executionControlsError(agent.controls,agent.executor);
    if(error)return `角色「${agent.name}」：${error}`;
  }
  return undefined;
}

export function controlsForTask(value: TaskControls, executor: "native" | "codex"): TaskControlsInput {
  const error=executionControlsError(value,executor);if(error)throw new Error(error);
  // Codex refuses even explicit default values for unsupported dimensions.
  return executor === "codex" ? {budget:{max_runtime_seconds:value.budget.max_runtime_seconds,max_output_bytes:value.budget.max_output_bytes}}
    : {budget:{...value.budget},permissions:{...value.permissions},acceptance_commands:[...value.acceptance_commands]};
}
export function requestPending(request: ExecutionRequest, task: TaskRun | undefined, now = Date.now()): boolean {
  return request.status === "pending" && Number.isSafeInteger(request.revision) && request.revision >= 0
    && request.expires_at_ms > now && task?.id === request.task_id && task.session_id === request.session_id
    && task.status === "waiting_input" && task.waiting_request_id === request.id;
}
export function requestCommand(request: ExecutionRequest, task: TaskRun | undefined, decision: { answer: string } | { approve: boolean }, now = Date.now()): Command {
  if (!requestPending(request, task, now)) throw new Error("请求已改变、过期或不再属于等待任务，请刷新后核对；没有提交决定。");
  if (request.kind === "question" && "answer" in decision) {
    if (!decision.answer.trim() || decision.answer.includes("\0") || utf8Length(decision.answer) > 8192) throw new Error("回答须为非空文本，最多 8192 UTF-8 字节。");
    return {type:"answer_request",request_id:request.id,expected_revision:request.revision,answer:decision.answer};
  }
  if (request.kind === "approval" && "approve" in decision && request.action && request.action_digest && /^[a-f0-9]{64}$/.test(request.action_digest))
    return {type:"decide_request",request_id:request.id,expected_revision:request.revision,approve:decision.approve,action_digest:request.action_digest};
  throw new Error("问题回答不是授权；授权必须匹配当前具体操作及其摘要。");
}
