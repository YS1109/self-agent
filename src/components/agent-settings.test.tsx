import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { agentModelLabel } from "../agent-model";
import type { Settings } from "../types";
import { AgentSettings, normalizeSettings } from "./agent-settings";
import type { ModelCatalog } from "./model-select";

const settings: Settings = {
  base_url: "https://example.com/v1",
  model: "coordinator/model",
  configured: true,
  execution_mode: "native",
  agents: [
    { id: "coder", name: "编码助手", executor: "native", instructions: "", model: "worker/model" },
    { id: "verifier", name: "验证助手", executor: "codex", instructions: "", model: "" },
  ],
  default_agent_id: "coder",
  codex_binary: "codex",
};
const catalog: ModelCatalog = { models: [], loading: false, error: "", refresh: vi.fn() };

describe("子智能体模型预设", () => {
  it("角色的固定模型独立于主模型，空值明确展示对应执行器的默认来源", () => {
    expect(agentModelLabel(settings.agents[0])).toBe("worker/model");
    expect(agentModelLabel(settings.agents[1])).toBe("Codex 本机默认模型");
    expect(agentModelLabel({ ...settings.agents[0], model: "" })).toBe("跟随主智能体模型");
    const normalized = normalizeSettings({ ...settings, model: "new/coordinator" });
    expect(normalized.agents[0].model).toBe("worker/model");
    expect(normalized.agents[1].model).toBe("");
  });

  it("设置中可辨识每个角色的模型，并说明旧计划保留配置", () => {
    const html = renderToStaticMarkup(<AgentSettings settings={settings} onChange={vi.fn()} disabled={false}
      nativeModels={catalog} codexModels={catalog} codexPathChanged={false} />);
    expect(html).toContain("子智能体模型预设");
    expect(html).toContain('aria-label="角色 1 · 预设模型"');
    expect(html).toContain('aria-label="角色 2 · 预设模型"');
    expect(html).toContain("worker/model");
    expect(html).toContain("Codex 本机默认模型");
    expect(html).toContain("已有计划保留原配置");
    expect(html).toContain("添加子智能体预设");
  });
});
