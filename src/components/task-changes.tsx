import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { ChangesReport, GitObservation, TaskRun } from "../types";
import { Button } from "./ui/button";
import { ExecutionEvidence } from "./execution-evidence";

const STATES: Record<string, string> = {
  captured: "已取得基线 · 等待执行结束", available: "报告可查看", partial: "部分报告", capturing: "正在记录基线", pending: "等待执行权或报告生成",
  running: "执行中 · 报告尚未完成", unavailable: "报告不可用", expired: "报告已过期", not_captured: "没有记录基线",
};
const FILE_STATES = { added: "新增", modified: "修改", deleted: "删除", renamed: "重命名" };
const SKIPPED: Record<string, string> = {
  symlink_not_followed: "符号链接不跟随", file_too_large: "文件超出读取预算", capture_budget_exceeded: "超出采集预算",
  unreadable_or_changed: "无法读取或文件已改变", not_regular_file: "不是普通文件", changed_during_capture: "采集期间文件发生变化",
  scan_incomplete: "扫描未完成，无法可靠比较", too_large: "文件超出读取预算", unreadable: "文件不可读取",
  symlink: "符号链接不跟随", unsupported: "不支持的文件类型", budget_exceeded: "超出扫描预算",
};
function GitState({ title, observation }: { title: string; observation?: GitObservation }) {
  if (!observation) return null;
  if (observation.state === "not_git") return <section className="changes-git"><strong>{title} · 未检测到 Git 标记</strong><p className="help-text">项目根目录未检测到 .git 标记；没有采集暂存区和工作树状态，也不能据此判断父目录仓库状态。</p></section>;
  if (observation.state === "unavailable") return <section className="changes-git" role="status">
    <strong>{title} · 未采集</strong>
    <p>Git 暂存区、工作树与未跟踪状态未采集，不能解释为没有改动或工作区干净。下方仅展示文件系统执行区间比较。</p>
    {observation.reason && <p className="help-text">原因：{observation.reason === "repository_filter_execution_disabled" ? "为避免触发仓库配置的 clean/process 过滤器，已禁用 Git 状态命令。" : observation.reason} <code>{observation.reason === "repository_filter_execution_disabled" ? observation.reason : ""}</code></p>}
  </section>;
  return <details className="changes-git">
    <summary>{title} · {({available:"已读取",partial:"部分可见",not_git:"非 Git 项目",unavailable:"不可读取"})[observation.state] ?? "状态未知"}</summary>
    <p className="help-text">Git 工作区状态不是本任务增量。索引列表示已暂存状态，工作区列表示未暂存状态；未跟踪文件单独标记。</p>
    {observation.entries.length > 0 ? <ul>{observation.entries.map((entry, index) => <li key={`${index}:${entry.path}`}>
      <code>{entry.path}</code><span>{entry.untracked ? "未跟踪" : `索引 ${entry.index.trim() || "—"} / 工作区 ${entry.worktree.trim() || "—"}`}</span>
    </li>)}</ul> : <p className="help-text">没有可展示的 Git 状态条目；不可读取或过滤时不等于工作区干净。</p>}
    {!!observation.omitted_count && <p>另有 {observation.omitted_count} 条已排除。</p>}
  </details>;
}

export function ChangesReportView({ report }: { report: ChangesReport }) {
  if (report.state && !report.files) return <p role="status">{STATES[report.state] ?? "报告暂不可用"}{report.notice || report.reason ? `：${report.notice ?? report.reason}` : ""}</p>;
  return <div className="changes-report">
    <p className="changes-notice">{report.notice || "这是执行区间内观察到的变化，不能独占归因于本任务。"}</p>
    <p className="help-text">仅比较取得项目执行权后至结束时的文件；外部并发写入可能混入。不会暂存、提交、重置或回滚任何文件。</p>
    {report.partial && <p className="changes-warning" role="status">此报告不完整：存在预算限制、未读取或跳过的文件；不能据此认定全部改动已覆盖。</p>}
    <GitState title="执行前 Git 概览" observation={report.baseline_git} />
    <GitState title="执行结束 Git 概览（非任务归因）" observation={report.current_git} />
    <h3>观察到的文件变化 · {report.files?.length ?? 0}</h3>
    {!report.files?.length && <p className="help-text">过滤和预算范围内未记录可展示变化，不保证工作区没有变化。</p>}
    {report.files?.map((file, index) => <details className="changes-file" key={`${index}:${file.path}`}>
      <summary><span>{FILE_STATES[file.status] ?? file.status}</span><code>{file.previous_path ? `${file.previous_path} → ` : ""}{file.path}</code></summary>
      <p className="help-text">大小 {file.before_bytes ?? "不存在"} → {file.after_bytes ?? "不存在"} 字节 · 只读 {file.before_readonly == null ? "不存在" : file.before_readonly ? "是" : "否"} → {file.after_readonly == null ? "不存在" : file.after_readonly ? "是" : "否"}
        {file.before_mode !== file.after_mode && ` · 权限 ${file.before_mode?.toString(8) ?? "—"} → ${file.after_mode?.toString(8) ?? "—"}`}</p>
      {file.diff_state === "available" ? <pre className="changes-diff">{file.diff || "没有文本差异（可能仅元数据改变）。"}</pre>
        : <p className="help-text">{file.diff_state === "binary_not_shown" ? "二进制文件：不展示内容。" : "文本差异超出预算，内容未展示。"}</p>}
    </details>)}
    {!!report.skipped?.length && <details className="changes-skipped"><summary>未能比较 · {report.skipped.length} 项</summary>
      <ul>{report.skipped.map((file, index) => <li key={`${index}:${file.path}`}><code>{file.path}</code> · {SKIPPED[file.reason] ?? file.reason}</li>)}</ul>
    </details>}
    <p className="help-text">排除项 {report.excluded_count ?? 0} · 敏感文件 {report.sensitive_file_count ?? 0}。{report.exclusions}</p>
    {report.limits && <details><summary>报告预算</summary><pre>{JSON.stringify(report.limits, null, 2)}</pre></details>}
  </div>;
}

/** Mount with key=task.id: an old task's response must not replace a new task. */
export function TaskChanges({ task, connected }: { task: TaskRun; connected: boolean }) {
  const [report, setReport] = useState<ChangesReport>();
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const alive = useRef(false);
  const pending = useRef(false);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  async function load() {
    if (pending.current) return;
    pending.current = true; setLoading(true); setError("");
    try {
      const value = await api<ChangesReport>("GET", `/tasks/${encodeURIComponent(task.id)}/changes`);
      if (alive.current) setReport(value);
    } catch (reason) {
      if (alive.current) { setReport(undefined); setError(reason instanceof Error ? reason.message : String(reason)); }
    } finally {
      pending.current = false;
      if (alive.current) setLoading(false);
    }
  }
  return <div className="task-detail task-changes">
    <h2>代码更改</h2><p>{task.title} · <code>{task.id.slice(0, 8)}</code></p>
    <p className="help-text">{STATES[task.changes_summary?.state ?? ""] ?? "尚无改动报告状态；旧任务可能未记录基线。"}
      {task.changes_summary?.visible_files !== undefined && ` · 可见 ${task.changes_summary.visible_files} 项`}</p>
    <Button variant="secondary" disabled={!connected || loading} onClick={() => void load()}>{loading ? "正在读取…" : report ? "重新读取只读报告" : "读取只读改动报告"}</Button>
    {error && <p className="task-error" role="alert">{error}</p>}
    {report && <ChangesReportView report={report} />}
    <ExecutionEvidence evidence={task.evidence} />
  </div>;
}
