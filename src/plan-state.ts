import type { EditablePlanNode, Plan, PlanChange, PlanLayout } from "./types";

export interface PlanDraft {
  revision: number;
  nodes: EditablePlanNode[];
}
export interface PlanLayoutDraft {
  revision: number;
  positions: PlanLayout;
}
export function selectedSessionPlan(plans: Plan[], sessionId: string | undefined, selections: Record<string, string>) {
  if (!sessionId) return undefined;
  const sessionPlans = plans.filter((plan) => plan.session_id === sessionId);
  return sessionPlans.find((plan) => plan.id === selections[sessionId]) ?? sessionPlans[0];
}
export function hasUnsavedPlanDraft(plan: Plan, drafts: Record<string, PlanDraft>) {
  return !!drafts[plan.id] && draftChanged(drafts[plan.id], plan);
}
export function updatePlanDrafts(plan: Plan, drafts: Record<string, PlanDraft>, draft?: PlanDraft): Record<string, PlanDraft> {
  if (draft && draftChanged(draft, plan)) return { ...drafts, [plan.id]: draft };
  const next = { ...drafts };
  delete next[plan.id];
  return next;
}
export function discardCleanPlanDraft(plan: Plan, drafts: Record<string, PlanDraft>) {
  return hasUnsavedPlanDraft(plan, drafts) ? drafts : updatePlanDrafts(plan, drafts);
}
export function planLayoutChanged(plan: Plan, positions: PlanLayout, nodes: EditablePlanNode[]) {
  const defaults = graphPositions(nodes);
  const current = filterPlanLayout(positions, nodes);
  const saved = filterPlanLayout(plan.layout ?? {}, nodes);
  return nodes.some((node) => {
    const actual = current[node.id] ?? defaults.get(node.id)!;
    const expected = saved[node.id] ?? defaults.get(node.id)!;
    return actual.x !== expected.x || actual.y !== expected.y;
  });
}
export function updatePlanLayouts(plan: Plan, nodes: EditablePlanNode[], layouts: Record<string, PlanLayoutDraft>, layout?: PlanLayoutDraft): Record<string, PlanLayoutDraft> {
  if (layout && planLayoutChanged(plan, layout.positions, nodes)) return { ...layouts, [plan.id]: layout };
  const next = { ...layouts };
  delete next[plan.id];
  return next;
}
export const createPlanDraft = (plan: Plan): PlanDraft => ({
  revision: plan.revision,
  nodes: plan.nodes.map(({ id, title, objective, depends_on, agent_id, repairs, verifies, acceptance_command }) => ({
    id, title, objective, depends_on: [...depends_on], ...(agent_id ? { agent_id } : {}),
    repairs: [...(repairs ?? [])], verifies: [...(verifies ?? [])],
    ...(acceptance_command ? { acceptance_command } : {}),
  })),
});
const nodeDefinition = (node: EditablePlanNode, defaultAgentId?: string | null) => ({
  id: node.id, title: node.title, objective: node.objective,
  depends_on: node.depends_on, agent_id: node.agent_id || defaultAgentId || null,
  repairs: node.repairs ?? [], verifies: node.verifies ?? [], acceptance_command: node.acceptance_command || null,
});
export const draftChanged = (draft: PlanDraft, plan: Plan) =>
  JSON.stringify(draft.nodes.map((node) => nodeDefinition(node, plan.default_agent_id))) !==
    JSON.stringify(plan.nodes.map((node) => nodeDefinition(node, plan.default_agent_id)));
export function getPlanDraft(plan: Plan, drafts: Record<string, PlanDraft>): PlanDraft {
  return drafts[plan.id] ?? createPlanDraft(plan);
}
export function planEditState(plan: Plan, draft: PlanDraft) {
  const editable = ["draft", "running", "paused", "reviewing", "waiting_input"].includes(plan.status);
  const dirty = editable && draftChanged(draft, plan);
  const stale = editable && draft.revision !== plan.revision;
  const validation = editable ? validatePlan(draft.nodes) ?? validateLockedNodes(plan, draft.nodes) ??
    (planNodeBudgetUsed(plan, draft.nodes) > 12 ? "本计划累计创建任务不能超过 12 个，删除历史任务不会恢复额度。" : null) : null;
  return {
    editable, dirty, stale, validation,
    canStart: plan.status === "draft" && !dirty && !stale && !validation,
  };
}
export function planNodeBudgetUsed(plan: Plan, nodes: EditablePlanNode[]): number {
  if (plan.status === "draft") return nodes.length;
  return new Set([...(plan.used_node_ids ?? plan.nodes.map((node) => node.id)), ...nodes.map((node) => node.id)]).size;
}
export function repairTargets(plan: Plan, nodeId: string) {
  return plan.nodes.filter((node) => node.id !== nodeId && ["failed", "blocked", "cancelled"].includes(node.status));
}
export function canEditPlanNode(plan: Plan, id: string): boolean {
  if (!["draft", "running", "paused", "reviewing", "waiting_input"].includes(plan.status)) return false;
  const node = plan.nodes.find((item) => item.id === id);
  return plan.status === "draft" || !node || ["pending", "queued"].includes(node.status);
}
export function canRemovePlanNode(plan: Plan, nodes: EditablePlanNode[], id: string) {
  return canEditPlanNode(plan, id) && !nodes.some((node) =>
    !canEditPlanNode(plan, node.id) &&
    [...node.depends_on, ...(node.repairs ?? []), ...(node.verifies ?? [])].includes(id));
}
function validateLockedNodes(plan: Plan, nodes: EditablePlanNode[]): string | null {
  const saved = createPlanDraft(plan);
  for (const original of saved.nodes) {
    if (canEditPlanNode(plan, original.id)) continue;
    const current = nodes.find((node) => node.id === original.id);
    if (!current || JSON.stringify(nodeDefinition(original, plan.default_agent_id)) !== JSON.stringify(nodeDefinition(current, plan.default_agent_id)))
      return "已有任务开始执行，不能修改或删除历史节点。请重新加载后台版本，再调整后续任务。";
  }
  return null;
}

// Validate the full dependency graph before sending a revisioned update.
export function validatePlan(nodes: EditablePlanNode[]): string | null {
  if (nodes.length < 1 || nodes.length > 12) return "计划需要 1–12 个任务。";
  const ids = new Set(nodes.map((node) => node.id));
  if (ids.size !== nodes.length) return "任务标识不能重复。";
  if (nodes.some((node) => !node.title.trim() || !node.objective.trim()))
    return "请填写每个任务的标题和完整任务描述。";
  for (const node of nodes) {
    for (const relation of [node.depends_on, node.repairs ?? [], node.verifies ?? []]) {
      if (new Set(relation).size !== relation.length || relation.some((id) => !ids.has(id) || id === node.id))
        return "依赖无效或修复、验收关系无效：目标不能缺失、重复或指向自身。";
    }
    if ((node.verifies?.length ?? 0) > 0 && !node.acceptance_command?.trim())
      return "验收节点需要填写验收命令，后台将根据真实命令与退出码判定是否通过。";
  }
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const byId = new Map(nodes.map((node) => [node.id, node]));
  function visit(id: string): boolean {
    if (visiting.has(id)) return false;
    if (visited.has(id)) return true;
    visiting.add(id);
    for (const dependency of byId.get(id)!.depends_on) {
      if (!ids.has(dependency) || dependency === id || !visit(dependency)) return false;
    }
    visiting.delete(id);
    visited.add(id);
    return true;
  }
  if (nodes.some((node) => !visit(node.id)))
    return "依赖无效：请检查缺失任务、自依赖或循环依赖。";
  return null;
}
export function removePlanNode(nodes: EditablePlanNode[], id: string) {
  return nodes.filter((node) => node.id !== id).map((node) => ({
    ...node, depends_on: node.depends_on.filter((dependency) => dependency !== id),
    repairs: (node.repairs ?? []).filter((target) => target !== id),
    verifies: (node.verifies ?? []).filter((target) => target !== id),
  }));
}
export function connectPlanNodes(plan: Plan, nodes: EditablePlanNode[], source: string, target: string): EditablePlanNode[] | null {
  if (!canEditPlanNode(plan, target) || source === target ||
    !nodes.some((node) => node.id === source) || !nodes.some((node) => node.id === target)) return null;
  const next = nodes.map((node) => node.id === target
    ? { ...node, depends_on: [...new Set([...node.depends_on, source])] } : node);
  // Graph validation is independent of incomplete fields in a newly added task.
  const check = next.map((node) => ({ ...node, title: node.title.trim() || "任务", objective: node.objective.trim() || "任务描述",
    ...((node.verifies?.length ?? 0) > 0 && !node.acceptance_command?.trim() ? { acceptance_command: "待填写" } : {}),
  }));
  return validatePlan(check) ? null : next;
}
export function disconnectPlanNodes(plan: Plan, nodes: EditablePlanNode[], source: string, target: string): EditablePlanNode[] | null {
  if (!canEditPlanNode(plan, target) || !nodes.some((node) => node.id === target && node.depends_on.includes(source))) return null;
  return nodes.map((node) => node.id === target ? { ...node, depends_on: node.depends_on.filter((id) => id !== source) } : node);
}
export function filterPlanLayout(layout: PlanLayout, nodes: EditablePlanNode[]): PlanLayout {
  return Object.fromEntries(nodes.filter((node) => {
    const position = layout[node.id];
    return position && [position.x, position.y].every((coordinate) => Number.isFinite(coordinate) && Math.abs(coordinate) <= 100_000);
  }).map((node) => [node.id, { x: layout[node.id].x, y: layout[node.id].y }]));
}
export function sortedPlanChanges(changes: PlanChange[], planId: string) {
  const createdTime = (change: PlanChange) => Date.parse(change.created_at) || 0;
  return changes.filter((change) => change.plan_id === planId).slice().sort((a, b) =>
    createdTime(b) - createdTime(a) || b.id.localeCompare(a.id));
}
export function planNodeChanges(before: EditablePlanNode[], after: EditablePlanNode[], defaultAgentId?: string | null) {
  const comparable = (node: EditablePlanNode) => JSON.stringify({
    ...nodeDefinition(node, defaultAgentId), depends_on: [...node.depends_on].sort(),
    repairs: [...(node.repairs ?? [])].sort(), verifies: [...(node.verifies ?? [])].sort(),
  });
  return {
    added: after.filter((node) => !before.some((old) => old.id === node.id)),
    removed: before.filter((node) => !after.some((next) => next.id === node.id)),
    modified: after.filter((node) => {
      const old = before.find((item) => item.id === node.id);
      return old && comparable(old) !== comparable(node);
    }),
  };
}
export function graphPositions(nodes: EditablePlanNode[]) {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const depth = new Map<string, number>();
  const visiting = new Set<string>();
  function level(id: string): number {
    if (depth.has(id)) return depth.get(id)!;
    if (visiting.has(id)) return 0;
    visiting.add(id);
    const dependencies = byId.get(id)?.depends_on.filter((dependency) => byId.has(dependency)) ?? [];
    const value = dependencies.length ? Math.max(...dependencies.map(level)) + 1 : 0;
    visiting.delete(id);
    depth.set(id, value);
    return value;
  }
  const rows = new Map<number, number>();
  return new Map(nodes.map((node) => {
    const column = level(node.id);
    const row = rows.get(column) ?? 0;
    rows.set(column, row + 1);
    return [node.id, { x: row * 260, y: column * 144 }];
  }));
}
