import { Loader2, RefreshCw, WandSparkles } from "lucide-react";
import type { Command, EditablePlanNode, Plan, PlanChange, Receipt } from "../types";
import { createPlanDraft, planEditState, planNodeChanges, sortedPlanChanges } from "../plan-state";
import { Button } from "./ui/button";

const statuses: Record<PlanChange["status"], string> = {
  queued: "等待生成", running: "正在生成调整建议", ready: "待审阅", applied: "已应用", failed: "生成失败", discarded: "已丢弃",
};
const describe = (node: EditablePlanNode, nodes: EditablePlanNode[], plan: Plan) => {
  const titles = (ids: string[] = []) => ids.map((id) => nodes.find((item) => item.id === id)?.title || id).join("、") || "无";
  return `任务描述：${node.objective}\n执行角色：${plan.agents?.find((agent) => agent.id === (node.agent_id || plan.default_agent_id))?.name || node.agent_id || "计划默认"}\n依赖：${titles(node.depends_on)}\n修复：${titles(node.repairs)}\n验收：${titles(node.verifies)}\n验收命令：${node.acceptance_command || "无"}`;
};
export function PlanChanges({ plan, changes, busy, connected, localDirty = false, onCommand }: {
  plan: Plan; changes: PlanChange[]; busy: boolean; connected: boolean; localDirty?: boolean;
  onCommand: (command: Command) => Promise<Receipt | undefined>;
}) {
  const relevant = sortedPlanChanges(changes, plan.id);
  if (!relevant.length) return null;
  const currentNodes = createPlanDraft(plan).nodes;
  return <section className="plan-changes" aria-label="对话调整建议">
    <div className="section-label"><WandSparkles size={13} />对话调整建议 · 最新创建优先</div>
    {relevant.map((change) => {
      const pending = ["queued", "running"].includes(change.status);
      const stale = change.base_revision !== plan.revision;
      const diff = planNodeChanges(currentNodes, change.nodes, plan.default_agent_id);
      const validation = change.status === "ready" ? planEditState(plan, { revision: change.base_revision, nodes: change.nodes }).validation : null;
      const editable = ["draft", "running", "paused", "reviewing", "waiting_input"].includes(plan.status);
      return <article className="plan-change" key={change.id}>
        <div className="plan-change-heading"><strong>{pending && <Loader2 size={12} className="spin" />}{statuses[change.status]}</strong><span>基于版本 {change.base_revision}</span></div>
        <p className="plan-change-prompt">{change.content}</p>
        <span className="help-text">来源：{change.source.channel_id === "desktop" ? "桌面" : `远程 · ${change.source.conversation_id}`}</span>
        {change.message && <p>{change.message}</p>}
        {change.error && <p className="error-text" role="alert">{change.error}</p>}
        {change.status === "ready" && <>
          <div className="plan-change-counts"><span>新增 {diff.added.length}</span><span>修改 {diff.modified.length}</span><span>删除 {diff.removed.length}</span></div>
          {([ ["新增", diff.added], ["修改", diff.modified], ["删除", diff.removed] ] as const).map(([label, nodes]) => nodes.map((node) => {
            const original = currentNodes.find((item) => item.id === node.id);
            return <details className="plan-change-detail" key={`${label}-${node.id}`}><summary>{label} · {node.title}</summary>
              {label === "修改" && original && <><span className="help-text">当前：{original.title}</span><pre>{describe(original, currentNodes, plan)}</pre><span className="help-text">建议：{node.title}</span></>}
              <pre>{describe(node, label === "删除" ? currentNodes : change.nodes, plan)}</pre>
            </details>;
          }))}
          {stale && <p className="plan-attention" role="status">计划已更新到版本 {plan.revision}。建议内容已保留，请基于当前版本重新生成。</p>}
          {validation && !stale && <p className="error-text" role="alert">{validation}</p>}
          {localDirty && <p className="help-text">画布或表单还有未保存的结构修改，请先保存或重新加载后再应用建议。</p>}
          <Button size="small" disabled={busy || !connected || stale || localDirty || !editable || !!validation} onClick={() => void onCommand({ type: "apply_plan_change", change_id: change.id, expected_revision: change.base_revision })}>应用调整</Button>
        </>}
        {editable && (change.status === "failed" || (change.status === "ready" && stale)) && <Button size="small" variant="secondary" disabled={busy || !connected || localDirty} onClick={() => void onCommand({ type: "request_plan_change", plan_id: plan.id, expected_revision: plan.revision, content: change.content })}><RefreshCw size={12} />{stale ? "按当前版本重新生成" : "重试生成"}</Button>}
        {["queued", "running", "ready", "failed"].includes(change.status) && <Button size="small" variant="ghost" disabled={busy || !connected} onClick={() => void onCommand({ type: "discard_plan_change", change_id: change.id })}>丢弃建议</Button>}
      </article>;
    })}
  </section>;
}
