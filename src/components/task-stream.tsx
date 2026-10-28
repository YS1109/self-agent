import { Loader2, Terminal, Waypoints } from "lucide-react";
import { ExecutionCleanupNotice } from "./execution-settings";
import { TaskSupplements } from "./task-controls";
import { taskTools } from "../event-state";
import { activeTask, type RunEvent, type TaskRun } from "../types";

const TOOL_NAMES: Record<string,string> = { list_files:"列出文件",read_file:"读取文件",search_files:"搜索文件",write_file:"写入文件",run_command:"运行命令" };
export function ToolActivity({ task, events }: { task?: TaskRun; events: RunEvent[] }) {
  if (!task) return null;
  const tools = taskTools(events, task);
  if (!tools.length) return null;
  return <details className="tool-activity">
    <summary><Terminal size={13} />真实工具执行 · {tools.length} 条{tools.some(tool => tool.status === "running") ? " · 进行中" : ""}</summary>
    <p className="help-text">仅展示执行器已提交的记录，最多保留最近 30 条；开始不代表成功。</p>
    {tools.map(tool => <details className={`tool-activity-item tool-${tool.status}`} key={tool.id}>
      <summary><span>{TOOL_NAMES[tool.name] ?? tool.name}</span><span>{({running:"执行中",completed:"已完成",failed:"失败",interrupted:"未收到完成记录"})[tool.status]}</span></summary>
      {tool.missingStart && <p className="help-text">较早的开始事件不在当前保留窗口。</p>}
      <pre>{tool.output ?? (tool.status === "running" ? "等待真实工具结果…" : "任务已结束，不能将此工具标记为成功。")}</pre>
    </details>)}
  </details>;
}

export function TaskUsage({ taskId, events }: { taskId: string; events: RunEvent[] }) {
  const usage = [...events].reverse().find(event => event.task_id === taskId && event.data?.kind === "usage")?.data;
  if (usage?.kind !== "usage") return null;
  return <p className="task-usage">{usage.scope === "thread_total" ? "会话累计用量（服务报告）" : "最近一轮用量（服务报告）"}：输入 {usage.input_tokens ?? "未知"} · 输出 {usage.output_tokens ?? "未知"} token</p>;
}

export function TaskContext({ task, events }: { task: TaskRun; events: RunEvent[] }) {
  // Durable task fields outlive the global 300-event presentation window. The
  // reducer updates them for live events and replays newer ones over snapshots.
  // Event fallback only supports older stored tasks/daemons missing these fields.
  const own = [...events].reverse().filter(event => event.task_id === task.id);
  const estimateEvent = own.find(event => event.data?.kind === "context_estimate")?.data;
  const compactionEvent = own.find(event => event.data?.kind === "context_compaction")?.data;
  const estimate = task.context_estimate ?? (estimateEvent?.kind === "context_estimate" ? estimateEvent : undefined);
  const compaction = task.context_compaction ?? (compactionEvent?.kind === "context_compaction" ? compactionEvent : undefined);
  if (!estimate && !compaction) return null;
  return <section className="task-context" aria-label="上下文估算与压缩状态">
    {estimate && <>
      <strong>{estimate.source === "native_local" ? "本地上下文估算" : "本地输入估算"}：约 {estimate.estimated_input_tokens.toLocaleString("zh-CN")} token</strong>
      <p>配置窗口 {estimate.context_window.toLocaleString("zh-CN")} token · 压缩阈值 {estimate.compression_threshold}%</p>
      <p className="help-text">{estimate.source === "native_local" ? "仅为本地估算，不是服务计费或精确 token 数。" : "仅估算本次发送的文本，不含隐藏线程、系统或工具历史；不是会话实际占用。"}
        阶段：{({before_compaction:"压缩前",request:"请求前",before_turn:"回合前"} as Record<string,string>)[estimate.phase] ?? estimate.phase}。</p>
    </>}
    {compaction && <p>最近压缩状态：{({started:"压缩中",completed:"压缩完成",failed:"压缩失败",cancelled:"压缩取消",reused:"复用历史摘要",invalidated:"历史摘要已失效",unchanged:"未生成新摘要"} as Record<string,string>)[compaction.state] ?? compaction.state}{!activeTask(task.status) && compaction.state === "started" ? "（任务已结束，未收到压缩结束确认）" : ""}</p>}
  </section>;
}

export function LiveTaskMessage({ task, events }: { task: TaskRun; events: RunEvent[] }) {
  const running = activeTask(task.status);
  return <article className="message message-assistant message-stream" data-task-id={task.id}>
    <div className="message-avatar"><Waypoints size={17} /></div>
    <div className="message-body">
      <div className="message-author">主智能体<span className="inline-tag">{running ? "实时回复" : "任务结果"}</span>{running && <Loader2 size={13} className="spin" aria-label="执行中" />}</div>
      <div className="message-text">{!running && task.result ? task.result : task.stream_output || "任务正在执行，等待回复或工具结果…"}</div>
      {task.stream_output_truncated && <p className="help-text">过程回复已达到保留上限；任务终态与最终结果仍会更新。</p>}
      {!running && task.error && <p className="task-error">{task.error}</p>}
      <ExecutionCleanupNotice task={task} />
      <ToolActivity task={task} events={events} />
      <TaskUsage taskId={task.id} events={events} />
      <TaskContext task={task} events={events} />
      <TaskSupplements task={task} />
    </div>
  </article>;
}
