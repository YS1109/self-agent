import { api } from "./api";
import type { TaskRun } from "./types";
export function cleanupResolutionBody(task: TaskRun, confirmed: boolean) {
  if (!confirmed || task.execution_cleanup !== "unknown" || !["completed", "failed", "cancelled"].includes(task.status)
    || !task.execution_cleanup_updated_at || !/^[A-Za-z0-9_-]{1,128}$/.test(task.id))
    throw new Error("必须核对当前终态任务、未知清理记录，并显式确认本机进程已全部停止。");
  return { expected_updated_at: task.execution_cleanup_updated_at, confirm_processes_stopped: true as const };
}
export async function resolveTaskCleanup(task: TaskRun, confirmed: boolean, signal?: AbortSignal) {
  const body = cleanupResolutionBody(task, confirmed);
  const result = await api<{task_id:string;resolved:boolean}>("POST", `/tasks/${task.id}/cleanup-resolution`, body, signal);
  if (result.task_id !== task.id || result.resolved !== true) throw new Error("所有者确认回执无效，请刷新核对；没有自动重发。");
  return result;
}
