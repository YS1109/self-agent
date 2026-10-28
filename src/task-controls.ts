import type { Command, TaskRun } from "./types";

export type InputTarget = { mode: "queue" } | { mode: "steer"; taskId: string };
export const canSteer = (task?: TaskRun) => !!task && task.task_kind === "main" && task.status === "running" && task.execution_mode !== "demo";
export const canContinue = (task?: TaskRun) => !!task && task.task_kind === "main" && ["completed", "failed", "cancelled"].includes(task.status);

/** A steering target is an explicit, stable ID. Never silently switch to the
 * next running task when the selected one ends, or to a different session. */
export function inputCommand(sessionId: string, target: InputTarget, content: string, tasks: TaskRun[]): Command {
  if (!content.trim()) throw new Error("请输入指令内容。");
  if (target.mode === "queue") return { type: "run_agent", session_id: sessionId, content: content.trim() };
  const task = tasks.find(task => task.id === target.taskId && task.session_id === sessionId);
  if (!canSteer(task)) throw new Error("所选补充目标已结束或不可用。请重新选择任务，或明确改为排队下一条。");
  return { type: "steer_task", task_id: target.taskId, content: content.trim() };
}

export function continuationCommand(task: TaskRun, content: string): Command {
  if (!canContinue(task)) throw new Error("只有已结束的主任务可以继续；不会重放原任务。");
  return { type: "continue_task", task_id: task.id, ...(content.trim() ? { content: content.trim() } : {}) };
}

/** Ref-like guard, independent of React's deferred rendering. */
export function createCommandGate() {
  let pending = false;
  return {
    async run<T>(action: () => Promise<T>): Promise<T | undefined> {
      if (pending) return undefined;
      pending = true;
      try { return await action(); } finally { pending = false; }
    },
  };
}
