import { useEffect, useRef, useState } from "react";
import { Button } from "./ui/button";
import { resolveTaskCleanup } from "../cleanup-resolution";
import type { PermissionMode, TaskControls, TaskRun } from "../types";
import { BUDGET_FIELDS, executionControlsError, permissionLabel, normalizeExecutionControls } from "../execution-controls";
import { CapabilitySelectionSettings, CapabilityScopeSummary } from "./capabilities-settings";
import type { ManagedCapabilities } from "../capabilities-client";
import { Select } from "./ui/select";

export function ExecutionSettings({ value: providedValue, onChange, executor, disabled, registry, scope = "global" }: {
  value: TaskControls; onChange: (value: TaskControls) => void; executor: "native" | "codex";
  disabled: boolean; registry?: ManagedCapabilities; scope?: "global" | "task" | "role";
}) {
  const value=normalizeExecutionControls(providedValue);
  const codex=executor === "codex";
  const error=executionControlsError(value,executor);
  return <section className="execution-settings" aria-label={scope === "global" ? "全局执行预算与权限" : scope === "role" ? "角色执行预算与权限" : "下一条任务预算与权限"}>
    <h3>{scope === "global" ? "执行默认预算与权限" : scope === "role" ? "角色额外执行限制" : "下一条任务的额外限制"}</h3>
    <p className="help-text">{scope === "global" ? "新任务入队时固化配置；不会改写旧任务预算。撤销权限可能阻止运行任务的后续工具调用。" : scope === "role" ? "仅影响新派发的角色任务，已有计划和任务保留快照。与全局设置取更严格值；不能扩大权限或移除全局验收要求。" : "仅作用于这次排队的新任务，不修改当前执行。有效策略与全局及角色限制取更严格值，不能借此扩大权限或移除已有验收要求。"}</p>
    {codex && <p className="execution-warning" role="note">Codex 仅支持本应用的运行时长和输出上限。内部模型请求、重试、修复轮次与逐工具权限不受本应用计量或管控，禁用字段里的默认数值不是已生效限制。需要这些控制或验收闭环时请使用 Native。</p>}
    <div className="execution-budget-fields">{BUDGET_FIELDS.map(field=><label key={field.key}>{field.label}
      <input aria-label={field.label} type="number" min={field.min} max={field.max} step={1} required value={value.budget[field.key]}
        disabled={disabled || (codex && !field.codex)} onChange={event=>onChange({...value,budget:{...value.budget,[field.key]:Number(event.target.value)}})} />
      {codex && !field.codex && <small>不支持，不代表实际限制</small>}
    </label>)}</div>
    <div className="execution-permissions">
      <label className="execution-checkbox"><input type="checkbox" checked={value.permissions.read_files} disabled={disabled || codex}
        onChange={event=>onChange({...value,permissions:{...value.permissions,read_files:event.target.checked}})} />允许读取项目文件</label>
      {(["write_files","run_commands","managed_tools"] as const).map(key=><label key={key}>{key === "write_files" ? "写入文件" : key === "managed_tools" ? "托管 MCP 工具（独立权限）" : "可信本机命令"}
        <Select aria-label={key === "write_files" ? "写入文件权限" : key === "managed_tools" ? "托管 MCP 工具权限" : "本机命令权限"} value={value.permissions[key]} disabled={disabled || codex}
          options={(["allow","ask","deny"] as const).map(mode=>({value:mode,label:permissionLabel(mode)}))}
          onValueChange={mode=>onChange({...value,permissions:{...value.permissions,[key]:mode as PermissionMode}})} />
      </label>)}
    </div>
    <p className="execution-warning">Native shell 是可信本机命令，不是项目目录、文件系统或网络沙箱；仍须后台显式设置 HARNESS_ALLOW_COMMANDS=1。禁止读取/写入时必须同时禁止命令；写入逐次询问时命令也必须询问或禁止。</p>
    <p className="help-text">托管 MCP 默认禁止，不能由服务启用状态或文件/命令权限推断许可。其未知作用可能超出项目目录；禁止读写时也须禁用 MCP，写入询问时 MCP 也须询问或禁止。</p>
    <CapabilitySelectionSettings registry={registry} value={value.capabilities} onChange={capabilities => onChange({...value,capabilities})} disabled={disabled} executor={executor} scope={scope} />
    <label>验收命令（每行一条，最多 8 条）<textarea rows={3} aria-label="真实验收命令" disabled={disabled || codex}
      value={value.acceptance_commands.join("\n")} placeholder="例如：pnpm test" onChange={event=>onChange({...value,acceptance_commands:event.target.value === "" ? [] : event.target.value.split("\n")})} /></label>
    <p className="help-text">命令原样保存，请勿留空行。必须有最终修改版本之后的真实成功证据，模型自述不算验收；声明命令并不绕过命令权限或自动授予批准。</p>
    {error && <p className="task-error" role="alert">{error}</p>}
  </section>;
}

const STOP_REASONS: Record<string,string>={
  user_cancelled:"用户已取消任务",daemon_interrupted:"后台中断执行，未自动重放",interrupted_wait:"后台中断等待，原决定未重放",permission_revoked:"运行权限已撤销",evidence_persistence_failed:"命令证据保存失败",
  model_budget_exhausted:"模型请求预算耗尽",runtime_exhausted:"运行时长预算耗尽",retry_budget_exhausted:"安全重试预算耗尽",
  repair_budget_exhausted:"修复轮次预算耗尽",output_budget_exhausted:"输出字节预算耗尽",permission_denied:"工具权限拒绝",
  approval_denied:"用户拒绝此次操作",approval_expired:"逐次授权已过期",input_expired:"问题等待已过期",verification_failed:"验收尚未通过",
};
export function ExecutionCleanupNotice({ task, onResolved, disabled = false }: { task: TaskRun; onResolved?: () => void; disabled?: boolean }) {
  const [confirmed, setConfirmed] = useState(false), [sending, setSending] = useState(false), [notice, setNotice] = useState("");
  const pending = useRef(false), observer = useRef<AbortController | null>(null);
  useEffect(() => { setConfirmed(false); setNotice(""); pending.current = false; setSending(false); return () => observer.current?.abort(); }, [task.id, task.execution_cleanup_updated_at]);
  async function resolve() {
    if (pending.current || disabled || !confirmed) return;
    pending.current = true; setSending(true); setNotice(""); const request = new AbortController(); observer.current = request;
    try {
      await resolveTaskCleanup(task, confirmed, request.signal);
      if (!request.signal.aborted) { setConfirmed(false); setNotice("所有者确认已记录，请等待状态刷新；原任务终态与未知调用结果没有改写。"); onResolved?.(); }
    } catch { if (!request.signal.aborted) setNotice("确认结果尚未核实或记录已改变，请刷新核对；不会自动重发或声称进程已被后台验证停止。"); }
    finally { if (!request.signal.aborted) { pending.current = false; setSending(false); } }
  }
  if (!task.execution_cleanup) return null;
  const message = task.execution_cleanup === "pending"
    ? "在途操作清理中：任务终态不表示副作用已经停止。项目执行权仍由后台保留，请等待清理并检查实际改动。"
    : task.execution_cleanup === "settled"
      ? task.execution_cleanup_owner_ack ? "所有者确认解除隔离：已记录本机所有者对进程停止的确认，不是后台验证成功。原任务、未知调用及验收结论保持不变。" : "后台确认在途操作清理已结束；这不表示任务成功、改动已撤销或验收通过。"
      : "清理结果未知：无法确认原写入是否完成或相关进程是否全部停止，同项目后续任务已隔离。请在操作系统核对进程并检查实际改动；不会自动重放。";
  return <div className="execution-cleanup" role="status"><strong>执行清理 · {({pending:"进行中",settled:"已结束",unknown:"结果未知"})[task.execution_cleanup]}</strong><p>{message}</p>
    {task.execution_cleanup === "unknown" && onResolved && <>
      <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled || sending || !["completed","failed","cancelled"].includes(task.status) || !task.execution_cleanup_updated_at} onChange={event => setConfirmed(event.target.checked)} />已在操作系统确认相关进程全部停止，并检查项目改动</label>
      <Button type="button" size="small" disabled={disabled || sending || !confirmed} onClick={() => void resolve()}>{sending ? "记录确认中…" : "以本机所有者身份确认解除隔离"}</Button>
      <p className="help-text">只记录你的核对结果，不终止进程、不撤销改动、不重放工具，也不把原失败或未知结果改为成功。</p>
    </>}
    {task.execution_cleanup === "unknown" && !onResolved && <p className="help-text">请在任务详情完成本机所有者核对后显式解除隔离。</p>}
    {notice && <p role="status">{notice}</p>}
    {task.execution_cleanup_owner_ack && <small>所有者确认于 {new Date(task.execution_cleanup_owner_ack.confirmed_at).toLocaleString("zh-CN")}</small>}
    {task.execution_cleanup_updated_at && <small>更新于 {new Date(task.execution_cleanup_updated_at).toLocaleString("zh-CN")}</small>}
  </div>;
}

export function TaskExecutionDetails({ task, onCleanupResolved, disabled }: { task: TaskRun; onCleanupResolved?: () => void; disabled?: boolean }) {
  const controls=task.controls ? normalizeExecutionControls(task.controls) : undefined, usage=task.budget_usage, codex=task.agent?.executor === "codex";
  if (!controls && !usage && !task.stop_reason && !task.execution_cleanup && !task.verification_attempts?.length && !task.managed_capabilities && !task.capability_calls?.length) return null;
  return <section className="task-execution-details" aria-label="固化执行控制与验收证据">
    <h3>执行预算与权限（任务快照）</h3>
    <ExecutionCleanupNotice task={task} onResolved={onCleanupResolved} disabled={disabled} />
    {task.stop_reason && <p className="task-error">停止原因：{STOP_REASONS[task.stop_reason] ?? "后台已停止任务"} <code>{task.stop_reason}</code></p>}
    {controls && <>
      <dl className="task-budget-list">
        <dt>运行时长上限</dt><dd>{controls.budget.max_runtime_seconds} 秒</dd>
        <dt>输出字节</dt><dd>{usage?.output_bytes ?? "尚无记录"} / {controls.budget.max_output_bytes}</dd>
        <dt>托管 MCP 调用</dt><dd>{codex ? "Codex 不支持应用管理的 MCP" : `${usage?.external_calls ?? "尚无记录"} / ${controls.budget.max_external_calls}`}</dd>
        <dt>模型请求（含压缩）</dt><dd>{codex ? "Codex 不支持计量" : `${usage?.model_requests ?? "尚无记录"} / ${controls.budget.max_model_requests}`}</dd>
        <dt>安全重试 / 修复轮次</dt><dd>{codex ? "Codex 不支持本应用闭环" : `${usage?.retries ?? "—"} / ${controls.budget.max_retries}；${usage?.repair_attempts ?? "—"} / ${controls.budget.max_repair_attempts}`}</dd>
      </dl>
      <CapabilityScopeSummary value={controls.capabilities} label="任务固化能力范围（不是调用证据）" />
      {!codex && <p className="help-text">托管 MCP：{permissionLabel(controls.permissions.managed_tools)} · 读取：{controls.permissions.read_files ? "允许" : "禁止"} · 写入：{permissionLabel(controls.permissions.write_files)} · 命令：{permissionLabel(controls.permissions.run_commands)}。shell 非沙箱，权限值不表示后台已经启用命令。</p>}
      {codex && <p className="help-text">本应用只控制时长与输出；不宣称限制 Codex 内部请求、工具权限或完成应用管理的验收闭环。</p>}
      {!!controls.acceptance_commands.length && <details><summary>固化验收命令 · {controls.acceptance_commands.length} 条</summary><ul>{controls.acceptance_commands.map(command=><li key={command}><code>{command}</code></li>)}</ul></details>}
    </>}
    <TaskCapabilityDetails task={task} />
    {usage?.started_at && <p className="help-text">计量开始：{new Date(usage.started_at).toLocaleString("zh-CN")} · 当前修改版本 {usage.mutation_generation}。字节计数不是 token 或账单费用。</p>}
    {!!task.verification_attempts?.length && <details className="verification-evidence" open><summary>真实验收尝试 · {task.verification_attempts.length} 次</summary>
      <p className="help-text">保留失败和复验；旧修改版本的成功不能证明后续修改通过。以后台任务终态为准，不根据模型文字宣布成功。</p>
      {task.verification_attempts.map(attempt=><div className="verification-attempt" key={attempt.id}>
        <code>{attempt.command}</code><p>退出码 {attempt.exit_code ?? "未知"} · 修改版本 {attempt.mutation_generation}
          {usage && attempt.mutation_generation !== usage.mutation_generation ? " · 旧版本证据" : ""}</p><pre>{attempt.output || "无命令输出"}</pre>
      </div>)}
    </details>}
  </section>;
}

export function TaskCapabilityDetails({ task }: { task: TaskRun }) {
  const pinned = task.managed_capabilities, calls = task.capability_calls ?? [];
  if (!pinned && !calls.length) return null;
  const statuses: Record<string,string> = {sending:"发送中（结果未确定）",completed:"工具返回完成",tool_error:"工具返回错误",not_sent:"工具请求未发送",unknown:"调用结果未知（不会自动重放）"};
  return <section className="task-capability-details" aria-label="固定能力与真实调用记录"><h4>固定能力与真实调用记录</h4>
    <p className="help-text">固定引用不是实际调用证据；调用预算计数包括启动/发现尝试，不等于成功调用次数。未发送工具请求也不保证进程启动没有副作用。</p>
    {pinned && (["mcp_servers","skills"] as const).map(kind => <details key={kind}><summary>{kind === "skills" ? "固定技能上下文" : "固定 MCP 目录"} · {pinned[kind].length}</summary><ul>{pinned[kind].map(item => <li key={item.id}><strong>{item.name}</strong> · 修订 {item.revision}<p><code>{item.id}</code></p><p>摘要 <code>{item.digest}</code></p></li>)}</ul></details>)}
    {calls.length ? calls.map(call => <div className="verification-attempt" key={call.id}><strong>{call.tool}</strong><p>{statuses[call.status] ?? "后台调用状态待核对"}</p><p>服务 <code>{call.server_id}</code> · 修订 {call.revision}</p><p>操作摘要 <code>{call.digest}</code></p>{call.reason && <p className="help-text">后台原因：{call.reason}</p>}<small>{new Date(call.updated_at).toLocaleString("zh-CN")}</small></div>) : <p className="help-text">没有持久化 MCP 工具调用记录；不宣称已使用配置中的工具。</p>}
  </section>;
}
