import { describe, expect, it } from "vitest";
import { createPlanDraft, getPlanDraft, graphPositions, planEditState, removePlanNode, validatePlan } from "./plan-state";
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
  it("保存版本可启动，运行中的计划不可编辑，重新加载消除版本冲突", () => {
    const original = plan();
    expect(planEditState(original, createPlanDraft(original)).canStart).toBe(true);
    const running: Plan = { ...original, status: "running" };
    expect(planEditState(running, createPlanDraft(running))).toMatchObject({ editable: false, canStart: false });
    const newer = { ...original, revision: 3 };
    expect(planEditState(newer, createPlanDraft(newer))).toMatchObject({ stale: false, canStart: true });
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
