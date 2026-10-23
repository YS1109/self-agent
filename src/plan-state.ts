import type { EditablePlanNode, Plan } from "./types";

export interface PlanDraft {
  revision: number;
  nodes: EditablePlanNode[];
}
export const createPlanDraft = (plan: Plan): PlanDraft => ({
  revision: plan.revision,
  nodes: plan.nodes.map(({ id, title, objective, depends_on, agent_id }) => ({
    id, title, objective, depends_on: [...depends_on], ...(agent_id ? { agent_id } : {}),
  })),
});
export const draftChanged = (draft: PlanDraft, plan: Plan) =>
  JSON.stringify(draft.nodes) !== JSON.stringify(createPlanDraft(plan).nodes);
export function getPlanDraft(plan: Plan, drafts: Record<string, PlanDraft>): PlanDraft {
  return drafts[plan.id] ?? createPlanDraft(plan);
}
export function planEditState(plan: Plan, draft: PlanDraft) {
  const editable = plan.status === "draft";
  const dirty = editable && draftChanged(draft, plan);
  const stale = editable && draft.revision !== plan.revision;
  const validation = editable ? validatePlan(draft.nodes) : null;
  return {
    editable, dirty, stale, validation,
    canStart: editable && !dirty && !stale && !validation,
  };
}

// Validate the full dependency graph before sending a revisioned update.
export function validatePlan(nodes: EditablePlanNode[]): string | null {
  if (nodes.length < 1 || nodes.length > 12) return "计划需要 1–12 个任务。";
  const ids = new Set(nodes.map((node) => node.id));
  if (ids.size !== nodes.length) return "任务标识不能重复。";
  if (nodes.some((node) => !node.title.trim() || !node.objective.trim()))
    return "请填写每个任务的标题和完整任务描述。";
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
  }));
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
