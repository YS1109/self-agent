import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { Plan } from "../types";
import { PlanPanel } from "./plan-panel";

function plan(): Plan {
  return {
    id: "plan-1",
    session_id: "session-1",
    title: "测试计划",
    source: { channel_id: "desktop", actor_id: "owner", conversation_id: "main" },
    execution_mode: "native",
    status: "draft",
    revision: 1,
    round: 0,
    max_rounds: 3,
    created_at: "2026-10-28T00:00:00Z",
    updated_at: "2026-10-28T00:00:00Z",
    agents: [
      { id: "coder", name: "编码角色", instructions: "", executor: "native", model: "provider/coder" },
      { id: "reviewer", name: "审阅角色", instructions: "", executor: "codex", model: "" },
    ],
    default_agent_id: "coder",
    nodes: [
      { id: "step-1", title: "编码", objective: "完成修改", depends_on: [], status: "pending" },
      { id: "step-2", title: "审阅", objective: "检查结果", depends_on: ["step-1"], status: "pending", agent_id: "reviewer" },
    ],
  };
}

function render(planValue: Plan, busy = false) {
  return renderToStaticMarkup(<PlanPanel
    plans={[planValue]} selectedId={planValue.id} onSelect={vi.fn()} connected busy={busy}
    onCommand={vi.fn()} drafts={{}} setDrafts={vi.fn()} layouts={{}} setLayouts={vi.fn()}
  />);
}

function roleTrigger(html: string, index: number) {
  return html.match(new RegExp(`<button[^>]*aria-label="任务 ${index} · 执行角色"[^>]*>`))?.[0] ?? "";
}

describe("计划角色与模型预设", () => {
  it("默认角色和指定角色分别显示计划保存的模型预设", () => {
    const html = render(plan());
    expect(html).toContain("计划默认 · 编码角色 · provider/coder");
    expect(html).toContain("模型预设：provider/coder");
    expect(html).toContain("模型预设：Codex 本机默认模型");
    expect(html).not.toContain("实际模型");
    expect(roleTrigger(html, 1)).toContain('role="combobox"');
    expect(roleTrigger(html, 1)).not.toContain('disabled=""');
  });

  it("运行中只开放尚未开始的角色选择，已执行角色保持锁定", () => {
    const value = plan();
    value.status = "running";
    value.nodes[0].status = "running";
    const html = render(value);
    expect(roleTrigger(html, 1)).toContain('disabled=""');
    expect(roleTrigger(html, 2)).not.toContain('disabled=""');
    expect(roleTrigger(render(value, true), 2)).toContain('disabled=""');
  });
});
