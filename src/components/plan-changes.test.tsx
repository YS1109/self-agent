import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { PlanChanges } from "./plan-changes";
import type { Plan, PlanChange } from "../types";
import { createPlanDraft } from "../plan-state";

const plan: Plan = {
  id: "plan", session_id: "session", title: "计划", revision: 3,
  status: "paused", round: 1, max_rounds: 3, execution_mode: "native",
  created_at: "", updated_at: "",
  source: { channel_id: "desktop", actor_id: "owner", conversation_id: "main" },
  nodes: [{ id: "a", title: "保留的历史任务", objective: "真实结果不可重写", depends_on: [], status: "completed" }],
};
const suggestion = (overrides: Partial<PlanChange> = {}): PlanChange => ({
  id: "change", plan_id: plan.id, session_id: plan.session_id, base_revision: plan.revision,
  source: { channel_id: "simulator", actor_id: "owner", conversation_id: "remote" },
  content: "补充要求", status: "ready", nodes: [
    ...createPlanDraft(plan).nodes,
    { id: "b", title: "新任务", objective: "补充检查", depends_on: ["a"] },
  ], created_at: "", updated_at: "", ...overrides,
});
const render = (change: PlanChange, localDirty = false) => renderToStaticMarkup(
  <PlanChanges plan={plan} changes={[change]} busy={false} connected localDirty={localDirty} onCommand={vi.fn()} />,
);
const applyButton = (html: string) => html.match(/<button[^>]*>应用调整<\/button>/)?.[0] ?? "";

describe("调整建议审阅", () => {
  it("有效建议显示来源和增删改预览，仍需显式应用", () => {
    const html = render(suggestion());
    expect(html).toContain("远程 · remote");
    expect(html).toContain("新增 1");
    expect(html).toContain("修改 0");
    expect(applyButton(html)).toContain("应用调整");
    expect(applyButton(html)).not.toContain("disabled");
  });
  it("远程入口感知同计划桌面草稿，禁止应用以保留未保存编辑", () => {
    const html = render(suggestion(), true);
    expect(html).toContain("还有未保存的结构修改");
    expect(applyButton(html)).toContain("disabled");
  });
  it("过期建议保留预览但禁止应用，提供基于当前版本重新生成入口", () => {
    const html = render(suggestion({ base_revision: 2 }));
    expect(html).toContain("计划已更新到版本 3");
    expect(html).toContain("新增 1");
    expect(html).toContain("按当前版本重新生成");
    expect(applyButton(html)).toContain("disabled");
  });
  it("重写已执行节点的建议显示错误并禁止应用", () => {
    const change = suggestion();
    change.nodes[0].objective = "覆写历史任务";
    const html = render(change);
    expect(html).toContain("不能修改或删除历史节点");
    expect(applyButton(html)).toContain("disabled");
  });
});
