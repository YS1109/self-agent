import { describe, expect, it } from "vitest";
import { canEditPlanNode, canRemovePlanNode, connectPlanNodes, createPlanDraft, discardCleanPlanDraft, disconnectPlanNodes, filterPlanLayout, getPlanDraft, graphPositions, hasUnsavedPlanDraft, planEditState, planLayoutChanged, planNodeBudgetUsed, planNodeChanges, removePlanNode, repairTargets, selectedSessionPlan, sortedPlanChanges, updatePlanDrafts, updatePlanLayouts, validatePlan, type PlanDraft, type PlanLayoutDraft } from "./plan-state";
import type { EditablePlanNode, Plan, PlanChange, PlanLayout } from "./types";
const node = (id: string, depends_on: string[] = []): EditablePlanNode => ({
  id, title: `任务 ${id}`, objective: `独立执行 ${id}`, depends_on,
});
describe("计划编排画布与调整建议", () => {
  it("连线只修改目标依赖，拒绝循环、自连线与缺失目标，不受未填表单阻挡", () => {
    const original = plan();
    const nodes = [node("a"), { ...node("b"), title: " ", objective: "" }, node("c", ["b"])];
    const connected = connectPlanNodes(original, nodes, "a", "b")!;
    expect(connected[1].depends_on).toEqual(["a"]);
    expect(nodes[1].depends_on).toEqual([]);
    expect(connectPlanNodes(original, connected, "a", "b")![1].depends_on).toEqual(["a"]);
    expect(connectPlanNodes(original, connected, "c", "a")).toBeNull();
    expect(connectPlanNodes(original, connected, "a", "a")).toBeNull();
    expect(connectPlanNodes(original, connected, "missing", "b")).toBeNull();
  });
  it("已执行节点可作为前置来源，新增和删除依赖都不能改已锁定目标", () => {
    const original: Plan = { ...plan(), status: "running", nodes: [
      { ...node("a"), status: "completed" }, { ...node("b", ["a"]), status: "running" },
      { ...node("c", ["a"]), status: "queued" },
    ] };
    const nodes = createPlanDraft(original).nodes;
    expect(connectPlanNodes(original, nodes, "b", "c")![2].depends_on).toEqual(["a", "b"]);
    expect(connectPlanNodes(original, nodes, "c", "b")).toBeNull();
    expect(disconnectPlanNodes(original, nodes, "a", "b")).toBeNull();
    expect(disconnectPlanNodes(original, nodes, "a", "c")![2].depends_on).toEqual([]);
    expect(nodes[2].depends_on).toEqual(["a"]);
    expect(disconnectPlanNodes(original, nodes, "b", "c")).toBeNull();
  });
  it("布局过滤删除节点及非法坐标，只复制节点位置，不污染结构草稿", () => {
    const nodes = [node("a"), node("b"), node("bad"), node("too-far")];
    const before = JSON.stringify(nodes);
    const layout: PlanLayout = {
      a: { x: -10, y: 100_000 }, b: { x: 0, y: 40 }, removed: { x: 20, y: 30 },
      bad: { x: Number.NaN, y: 1 }, "too-far": { x: 100_001, y: 0 },
    };
    const filtered = filterPlanLayout(layout, nodes);
    expect(filtered).toEqual({ a: { x: -10, y: 100_000 }, b: { x: 0, y: 40 } });
    filtered.a.x = 50;
    expect(layout.a.x).toBe(-10);
    expect(JSON.stringify(nodes)).toBe(before);
  });
  it("建议差异包括角色、任务描述与修复验收关系，忽略可选空值和关系顺序", () => {
    const before = [node("a"), { ...node("b", ["a", "c"]), repairs: ["a", "c"] }, node("c"), node("removed")];
    const after = [
      { ...node("a"), objective: "更新描述", agent_id: "reviewer", verifies: ["c"], acceptance_command: "pnpm test" },
      { ...node("b", ["c", "a"]), repairs: ["c", "a"], verifies: [], agent_id: null, acceptance_command: null },
      node("c"), node("new"),
    ];
    const original = JSON.stringify(before);
    const diff = planNodeChanges(before, after);
    expect(diff.added.map((item) => item.id)).toEqual(["new"]);
    expect(diff.removed.map((item) => item.id)).toEqual(["removed"]);
    expect(diff.modified.map((item) => item.id)).toEqual(["a"]);
    expect(JSON.stringify(before)).toBe(original);
  });
  it("服务端建议的字段序列和默认角色表示不同，不会误判历史节点被修改", () => {
    const original: Plan = { ...plan(), status: "paused", default_agent_id: "coder", nodes: [
      { ...node("a"), agent_id: "coder", status: "completed" },
    ] };
    const remoteNode: EditablePlanNode = {
      acceptance_command: null, verifies: [], repairs: [], objective: "独立执行 a",
      title: "任务 a", depends_on: [], id: "a", agent_id: null,
    };
    const proposed = { revision: original.revision, nodes: [remoteNode] };
    expect(planEditState(original, proposed)).toMatchObject({ dirty: false, validation: null });
    expect(planNodeChanges(createPlanDraft(original).nodes, proposed.nodes, original.default_agent_id).modified).toEqual([]);
  });
  it("布局轮询不会改变结构编辑状态，远程应用后桌面未保存草稿仍保留并提示过期", () => {
    const original = plan();
    const draft = createPlanDraft(original);
    const layoutUpdated = { ...original, layout_revision: 2, layout: { a: { x: 20, y: 30 } } };
    expect(planEditState(layoutUpdated, draft)).toMatchObject({ dirty: false, stale: false, canStart: true });
    draft.nodes[0].objective = "桌面未保存要求";
    const cache = { [original.id]: draft };
    expect(hasUnsavedPlanDraft(original, cache)).toBe(true);
    const applied = { ...original, revision: 2, nodes: [{ ...original.nodes[0], objective: "远程已应用要求" }] };
    const preserved = getPlanDraft(applied, cache);
    expect(preserved.nodes[0].objective).toBe("桌面未保存要求");
    expect(planEditState(applied, preserved)).toMatchObject({ dirty: true, stale: true, canStart: false });
  });
  it("标题修改后恢复原值会移除草稿，建议应用更新后直接读取新版本并允许启动", () => {
    const original = plan();
    const changed = createPlanDraft(original);
    changed.nodes[0].title = "临时修改";
    let cache: Record<string, PlanDraft> = updatePlanDrafts(original, {}, changed);
    expect(hasUnsavedPlanDraft(original, cache)).toBe(true);
    const restored = { ...changed, nodes: changed.nodes.map((item) => ({ ...item, title: original.nodes[0].title })) };
    cache = updatePlanDrafts(original, cache, restored);
    expect(cache[original.id]).toBeUndefined();
    const applied: Plan = { ...original, revision: 2, nodes: [{ ...original.nodes[0], title: "建议更新的标题" }] };
    const current = getPlanDraft(applied, cache);
    expect(current.nodes[0].title).toBe("建议更新的标题");
    expect(planEditState(applied, current)).toMatchObject({ dirty: false, stale: false, canStart: true });
  });
  it("应用成功按应用前快照只清理目标计划的clean草稿，不丢弃并发或其他计划编辑", () => {
    const original = plan();
    const other = createPlanDraft(plan("other"));
    other.nodes[0].title = "其他计划未保存内容";
    const cleanCache = { one: createPlanDraft(original), other };
    const cleared = discardCleanPlanDraft(original, cleanCache);
    expect(cleared.one).toBeUndefined();
    expect(cleared.other).toBe(other);
    const dirty = createPlanDraft(original);
    dirty.nodes[0].title = "期间新增的未保存修改";
    const dirtyCache = { one: dirty, other };
    expect(discardCleanPlanDraft(original, dirtyCache)).toBe(dirtyCache);
    expect(hasUnsavedPlanDraft(original, dirtyCache)).toBe(true);
  });
  it("节点拖回默认或已保存位置会移除clean布局，后续布局版本更新不留下假冲突", () => {
    const original = plan();
    const nodes = createPlanDraft(original).nodes;
    let cache: Record<string, PlanLayoutDraft> = updatePlanLayouts(original, nodes, {}, { revision: 0, positions: { a: { x: 90, y: 100 } } });
    expect(planLayoutChanged(original, cache.one.positions, nodes)).toBe(true);
    cache = updatePlanLayouts(original, nodes, cache, { ...cache.one, positions: { a: { x: 0, y: 0 } } });
    expect(cache.one).toBeUndefined();
    const saved: Plan = { ...original, layout_revision: 2, layout: { a: { x: 120, y: 180 } } };
    const current = cache.one ?? { revision: saved.layout_revision!, positions: saved.layout! };
    expect(current.revision).toBe(2);
    expect(planLayoutChanged(saved, current.positions, nodes)).toBe(false);
    const moved = updatePlanLayouts(saved, nodes, {}, { revision: 2, positions: { a: { x: 300, y: 180 } } });
    expect(updatePlanLayouts(saved, nodes, moved, { revision: 2, positions: saved.layout! }).one).toBeUndefined();
  });
  it("计划选择按会话独立保存，缺失选择只回退当前会话的计划", () => {
    const plans: Plan[] = [
      { ...plan("a-first"), session_id: "a" }, { ...plan("a-second"), session_id: "a" },
      { ...plan("b-first"), session_id: "b" }, { ...plan("b-second"), session_id: "b" },
    ];
    const selections = { a: "a-second", b: "b-second" };
    expect(selectedSessionPlan(plans, "a", selections)?.id).toBe("a-second");
    expect(selectedSessionPlan(plans, "b", selections)?.id).toBe("b-second");
    expect(selectedSessionPlan(plans, "a", selections)?.id).toBe("a-second");
    expect(selectedSessionPlan(plans, "a", { a: "b-first" })?.id).toBe("a-first");
    expect(selectedSessionPlan(plans, undefined, selections)).toBeUndefined();
  });
  it("建议按照真实创建时间倒序，轮询更新和上游数组顺序不改变排序", () => {
    const change = (id: string, created_at: string, plan_id = "one"): PlanChange => ({
      id, plan_id, session_id: "session", base_revision: 1, source: plan().source,
      content: id, status: "ready", nodes: [node("a")], created_at, updated_at: "2026-10-25T18:00:00Z",
    });
    const changes = [
      change("older", "2026-10-25T10:00:00+08:00"),
      change("newer-a", "2026-10-25T03:00:00Z"),
      change("newer-b", "2026-10-25T03:00:00Z"),
      change("other", "2026-10-25T18:00:00Z", "another"),
    ];
    expect(sortedPlanChanges(changes, "one").map((item) => item.id)).toEqual(["newer-b", "newer-a", "older"]);
    expect(changes[0].id).toBe("older");
  });
});
const plan = (id = "one"): Plan => ({
  id, title: `计划 ${id}`, session_id: "session", status: "draft", revision: 1,
  round: 0, max_rounds: 3, created_at: "", updated_at: "", execution_mode: "demo",
  source: { channel_id: "desktop", actor_id: "owner", conversation_id: "main" },
  nodes: [{ ...node("a"), status: "pending" }],
});
describe("计划编辑状态", () => {
  it("角色分配进入保存草稿，后端轮询不会覆盖未保存的角色选择", () => {
    const original = plan();
    original.nodes[0].agent_id = "coder";
    const draft = createPlanDraft(original);
    expect(draft.nodes[0].agent_id).toBe("coder");
    draft.nodes[0].agent_id = "reviewer";
    expect(original.nodes[0].agent_id).toBe("coder");
    expect(planEditState(original, draft)).toMatchObject({ dirty: true, canStart: false });
    expect(getPlanDraft(original, { one: draft }).nodes[0].agent_id).toBe("reviewer");
  });
  it("后台轮询保留未保存编辑，版本变化阻止用旧版本启动", () => {
    const original = plan();
    const draft = createPlanDraft(original);
    draft.nodes[0].objective = "用户正在输入";
    const cache = { [original.id]: draft };
    const updated = { ...original, revision: 2 };
    const current = getPlanDraft(updated, cache);
    expect(current.nodes[0].objective).toBe("用户正在输入");
    expect(planEditState(updated, current)).toMatchObject({ dirty: true, stale: true, canStart: false });
    expect(original.nodes[0].objective).toBe("独立执行 a");
  });
  it("切换计划不会把旧计划的输入绑定给新计划，切回保留各自草稿", () => {
    const first = plan("one");
    const second = plan("two");
    const firstDraft = createPlanDraft(first);
    firstDraft.nodes[0].title = "第一个计划的编辑";
    const cache = { one: firstDraft };
    expect(getPlanDraft(second, cache).nodes[0].title).toBe("任务 a");
    expect(getPlanDraft(first, cache).nodes[0].title).toBe("第一个计划的编辑");
  });
  it("保存版本可启动，运行中的计划可调整后续任务但不能重复启动", () => {
    const original = plan();
    expect(planEditState(original, createPlanDraft(original)).canStart).toBe(true);
    const running: Plan = { ...original, status: "running" };
    expect(planEditState(running, createPlanDraft(running))).toMatchObject({ editable: true, canStart: false });
    const newer = { ...original, revision: 3 };
    expect(planEditState(newer, createPlanDraft(newer))).toMatchObject({ stale: false, canStart: true });
  });
  it("暂停计划允许编辑排队节点与追加任务，执行中和历史节点保持只读", () => {
    const original = plan();
    original.status = "paused";
    original.nodes = [
      { ...node("a"), status: "completed" },
      { ...node("b", ["a"]), status: "running" },
      { ...node("c", ["b"]), status: "queued" },
    ];
    const draft = createPlanDraft(original);
    draft.nodes[2].agent_id = "verifier";
    draft.nodes.push({ ...node("d", ["c"]), repairs: [], verifies: [] });
    expect(planEditState(original, draft)).toMatchObject({ editable: true, dirty: true, stale: false, validation: null, canStart: false });
    expect(canEditPlanNode(original, "a")).toBe(false);
    expect(canEditPlanNode(original, "b")).toBe(false);
    expect(canEditPlanNode(original, "c")).toBe(true);
    expect(canEditPlanNode(original, "d")).toBe(true);
    draft.nodes[0].objective = "覆盖历史";
    expect(planEditState(original, draft).validation).toContain("不能修改");
  });
  it("后台已领取正在编辑的节点时保留草稿并阻止覆盖，重新加载后锁定节点", () => {
    const original = { ...plan(), status: "running" as const };
    const draft = createPlanDraft(original);
    draft.nodes[0].title = "尚未保存的修改";
    const updated: Plan = { ...original, revision: 3, nodes: [{ ...original.nodes[0], status: "running" }] };
    const cached = getPlanDraft(updated, { one: draft });
    expect(cached.nodes[0].title).toBe("尚未保存的修改");
    expect(planEditState(updated, cached)).toMatchObject({ stale: true, dirty: true, canStart: false });
    expect(planEditState(updated, createPlanDraft(updated))).toMatchObject({ stale: false, dirty: false, validation: null });
    expect(canEditPlanNode(updated, "a")).toBe(false);
  });
  it("修复和验收关系独立复制，验收必须指定真实命令，删除节点清理关系", () => {
    const original = plan();
    original.nodes = [
      { ...node("failure"), status: "failed" },
      { ...node("repair"), status: "pending", repairs: ["failure"] },
      { ...node("verify", ["repair"]), status: "pending", verifies: ["failure"], acceptance_command: "pnpm test" },
    ];
    const draft = createPlanDraft(original);
    expect(validatePlan(draft.nodes)).toBeNull();
    draft.nodes[1].repairs!.push("extra");
    expect(original.nodes[1].repairs).toEqual(["failure"]);
    expect(validatePlan(draft.nodes)).toMatch("关系无效");
    draft.nodes[1].repairs = ["failure"];
    draft.nodes[2].acceptance_command = "";
    expect(validatePlan(draft.nodes)).toContain("验收命令");
    const removed = removePlanNode(createPlanDraft(original).nodes, "failure");
    expect(removed[0].repairs).toEqual([]);
    expect(removed[1].verifies).toEqual([]);
  });
  it("已开始任务引用的节点不可删除，完成或取消计划不再开放编辑", () => {
    const original: Plan = { ...plan(), status: "running", nodes: [
      { ...node("a"), status: "queued" },
      { ...node("b", ["a"]), status: "running" },
      { ...node("c"), status: "pending" },
    ] };
    const draft = createPlanDraft(original);
    expect(canRemovePlanNode(original, draft.nodes, "a")).toBe(false);
    expect(canRemovePlanNode(original, draft.nodes, "c")).toBe(true);
    for (const status of ["completed", "cancelled", "failed"] as const) {
      expect(planEditState({ ...original, status }, draft).editable).toBe(false);
      expect(canEditPlanNode({ ...original, status }, "c")).toBe(false);
    }
  });
  it("修复和验收允许指向失败、受阻与取消历史，不能指向自身和成功任务", () => {
    const original = plan();
    original.nodes = [
      { ...node("failure"), status: "failed" },
      { ...node("blocked"), status: "blocked" },
      { ...node("cancelled"), status: "cancelled" },
      { ...node("success"), status: "completed" },
      { ...node("pending"), status: "pending" },
    ];
    expect(repairTargets(original, "pending").map((item) => item.id)).toEqual(["failure", "blocked", "cancelled"]);
    expect(repairTargets(original, "failure").map((item) => item.id)).toEqual(["blocked", "cancelled"]);
  });
  it("运行中的节点额度累计已用标识，删除已保存节点不会恢复额度", () => {
    const original: Plan = { ...plan(), status: "paused", used_node_ids: ["a", ...Array.from({ length: 10 }, (_, index) => `old-${index}`)] };
    const draft = createPlanDraft(original);
    expect(planNodeBudgetUsed(original, draft.nodes)).toBe(11);
    draft.nodes.push(node("new"));
    expect(planNodeBudgetUsed(original, draft.nodes)).toBe(12);
    draft.nodes = draft.nodes.filter((item) => item.id !== "a");
    expect(planNodeBudgetUsed(original, draft.nodes)).toBe(12);
    draft.nodes.push(node("another"));
    expect(planEditState(original, draft).validation).toContain("累计创建");
    expect(planNodeBudgetUsed({ ...original, status: "draft" }, draft.nodes)).toBe(2);
    expect(planNodeBudgetUsed({ ...original, used_node_ids: undefined }, draft.nodes)).toBe(3);
  });
  it("拒绝循环、自依赖、缺失依赖、空内容和超出节点预算", () => {
    expect(validatePlan([node("a"), node("b", ["a"])])).toBeNull();
    expect(validatePlan([node("a", ["b"]), node("b", ["a"])])).toMatch("依赖无效");
    expect(validatePlan([node("a", ["a"])])).toMatch("依赖无效");
    expect(validatePlan([node("a", ["missing"])])).toMatch("依赖无效");
    expect(validatePlan([node("a"), node("a")])).toMatch("不能重复");
    expect(validatePlan([{ ...node("a"), objective: " " }])).toMatch("任务描述");
    expect(validatePlan([])).toMatch("1–12");
    expect(validatePlan(Array.from({ length: 13 }, (_, index) => node(String(index))))).toMatch("1–12");
  });
  it("删除任务时同时移除指向它的依赖，图层级只依据真实依赖", () => {
    const nodes = [node("a"), node("b"), node("c", ["a", "b"]), node("d", ["c"])];
    const removed = removePlanNode(nodes, "a");
    expect(removed.find((item) => item.id === "c")?.depends_on).toEqual(["b"]);
    expect(validatePlan(removed)).toBeNull();
    const positions = graphPositions(nodes);
    expect(positions.get("a")?.y).toBe(positions.get("b")?.y);
    expect(positions.get("a")?.x).not.toBe(positions.get("b")?.x);
    expect(positions.get("c")!.y).toBeGreaterThan(positions.get("b")!.y);
    expect(positions.get("d")!.y).toBeGreaterThan(positions.get("c")!.y);
    expect(nodes[2].depends_on).toEqual(["a", "b"]);
  });
});
