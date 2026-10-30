import { useState } from "react";
import { continuationCommand } from "../task-controls";
import type { Command, Receipt, TaskRun } from "../types";
import { Button } from "./ui/button";

const STATES: Record<string, string> = {
  pending: "已接收 · 等待安全边界", delivering: "交付中 · 尚未确认", applied: "已纳入执行器输入",
  not_applied: "未应用", unknown: "结果未知 · 不自动重送",
};
export function TaskSupplements({ task }: { task: TaskRun }) {
  if (!task.supplements?.length) return null;
  return <section className="task-supplements" aria-label="当前任务补充记录">
    <h3>任务补充 · {task.supplements.length} 条</h3>
    <p className="help-text">目标任务 {task.id}。接收不代表应用；“已纳入执行器输入”只确认传输或请求包含，不保证模型遵从。</p>
    {task.supplements.map(item => <div className="supplement-item" key={item.id}>
      <strong>{STATES[item.status] ?? "状态待确认"}</strong>
      <p>{item.content}</p>
      {item.reason && <p className="help-text">{item.reason}</p>}
      {item.applied_via && <p className="help-text">应用通道：{item.applied_via}</p>}
      <small>补充 {item.id.slice(0, 8)} · {new Date(item.updated_at).toLocaleString("zh-CN")}</small>
    </div>)}
  </section>;
}

export function ContinueTask({ task, disabled, onCommand, onCreated }: {
  task: TaskRun; disabled: boolean; onCommand: (command: Command) => Promise<Receipt | undefined>;
  onCreated: (id: string) => void;
}) {
  const [content, setContent] = useState("");
  const [pending, setPending] = useState(false);
  return <section className="continue-task">
    <h3>继续此任务</h3>
    <p className="help-text">从已结束的任务 {task.id.slice(0, 8)} 创建新的主任务，保留历史和已完成结果；不会自动重放原工具操作。</p>
    <textarea aria-label="继续任务的补充要求（可选）" placeholder="补充继续要求（可选）" value={content} onChange={event => setContent(event.target.value)} disabled={pending} rows={2} />
    <Button disabled={disabled || pending} onClick={async () => {
      if (pending) return;
      setPending(true);
      try {
        const receipt = await onCommand(continuationCommand(task, content));
        if (receipt?.task_id) { setContent(""); onCreated(receipt.task_id); }
      } finally { setPending(false); }
    }}>{pending ? "正在创建…" : "继续 · 创建新任务"}</Button>
  </section>;
}
