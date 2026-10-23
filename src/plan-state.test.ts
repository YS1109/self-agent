import { describe, expect, it } from "vitest";
import { canEditPlanNode, canRemovePlanNode, createPlanDraft, getPlanDraft, graphPositions, planEditState, planNodeBudgetUsed, removePlanNode, repairTargets, validatePlan } from "./plan-state";
import type { EditablePlanNode, Plan } from "./types";
const node = (id: string, depends_on: string[] = []): EditablePlanNode => ({
  id, title: `任务 ${id}`, objective: `独立执行 ${id}`, depends_on,
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
