import type { PermissionMode, TaskControls, TaskRun } from "../types";
import { BUDGET_FIELDS, executionControlsError, permissionLabel } from "../execution-controls";
import { Select } from "./ui/select";

export function ExecutionSettings({ value, onChange, executor, disabled, scope = "global" }: {
  value: TaskControls; onChange: (value: TaskControls) => void; executor: "native" | "codex";
  disabled: boolean; scope?: "global" | "task" | "role";
}) {
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
      {(["write_files","run_commands"] as const).map(key=><label key={key}>{key === "write_files" ? "写入文件" : "可信本机命令"}
        <Select aria-label={key === "write_files" ? "写入文件权限" : "本机命令权限"} value={value.permissions[key]} disabled={disabled || codex}
          options={(["allow","ask","deny"] as const).map(mode=>({value:mode,label:permissionLabel(mode)}))}
          onValueChange={mode=>onChange({...value,permissions:{...value.permissions,[key]:mode as PermissionMode}})} />
      </label>)}
    </div>
    <p className="execution-warning">Native shell 是可信本机命令，不是项目目录、文件系统或网络沙箱；仍须后台显式设置 HARNESS_ALLOW_COMMANDS=1。禁止读取/写入时必须同时禁止命令；写入逐次询问时命令也必须询问或禁止。</p>
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
export function ExecutionCleanupNotice({ task }: { task: TaskRun }) {
  if (!task.execution_cleanup) return null;
  const message = task.execution_cleanup === "pending"
    ? "文件操作清理中：任务终态不表示副作用已经停止。项目执行权仍由后台保留，请等待清理并检查实际改动。"
    : task.execution_cleanup === "settled"
      ? "在途文件操作清理已确认结束；这不表示任务成功、改动已撤销或验收通过。"
      : "文件操作结果未知：后台中断后无法确认原写入是否完成。请检查实际改动；不会自动重放。";
  return <div className="execution-cleanup" role="status"><strong>执行清理 · {({pending:"进行中",settled:"已结束",unknown:"结果未知"})[task.execution_cleanup]}</strong><p>{message}</p>
    {task.execution_cleanup_updated_at && <small>更新于 {new Date(task.execution_cleanup_updated_at).toLocaleString("zh-CN")}</small>}
  </div>;
}

export function TaskExecutionDetails({ task }: { task: TaskRun }) {
  const controls=task.controls, usage=task.budget_usage, codex=task.agent?.executor === "codex";
  if (!controls && !usage && !task.stop_reason && !task.execution_cleanup && !task.verification_attempts?.length) return null;
  return <section className="task-execution-details" aria-label="固化执行控制与验收证据">
    <h3>执行预算与权限（任务快照）</h3>
    <ExecutionCleanupNotice task={task} />
    {task.stop_reason && <p className="task-error">停止原因：{STOP_REASONS[task.stop_reason] ?? "后台已停止任务"} <code>{task.stop_reason}</code></p>}
    {controls && <>
      <dl className="task-budget-list">
        <dt>运行时长上限</dt><dd>{controls.budget.max_runtime_seconds} 秒</dd>
        <dt>输出字节</dt><dd>{usage?.output_bytes ?? "尚无记录"} / {controls.budget.max_output_bytes}</dd>
        <dt>模型请求（含压缩）</dt><dd>{codex ? "Codex 不支持计量" : `${usage?.model_requests ?? "尚无记录"} / ${controls.budget.max_model_requests}`}</dd>
        <dt>安全重试 / 修复轮次</dt><dd>{codex ? "Codex 不支持本应用闭环" : `${usage?.retries ?? "—"} / ${controls.budget.max_retries}；${usage?.repair_attempts ?? "—"} / ${controls.budget.max_repair_attempts}`}</dd>
      </dl>
      {!codex && <p className="help-text">读取：{controls.permissions.read_files ? "允许" : "禁止"} · 写入：{permissionLabel(controls.permissions.write_files)} · 命令：{permissionLabel(controls.permissions.run_commands)}。shell 非沙箱，权限值不表示后台已经启用命令。</p>}
      {codex && <p className="help-text">本应用只控制时长与输出；不宣称限制 Codex 内部请求、工具权限或完成应用管理的验收闭环。</p>}
      {!!controls.acceptance_commands.length && <details><summary>固化验收命令 · {controls.acceptance_commands.length} 条</summary><ul>{controls.acceptance_commands.map(command=><li key={command}><code>{command}</code></li>)}</ul></details>}
    </>}
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
