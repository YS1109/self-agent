import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { agentContext, modelContext } from "../model-context";
import type { AgentProfile, Settings } from "../types";
import { AgentSettings, normalizeSettings } from "./agent-settings";
import { ModelContextSettings } from "./model-context-settings";
import { ProviderSettings } from "./provider-settings";
import type { ModelCatalog } from "./model-select";

const settings: Settings = {
  base_url: "https://example.com/v1", model: "model/main", configured: true,
  execution_mode: "native", agents: [], default_agent_id: "coder", codex_binary: "codex",
  models_endpoint: "/custom/models",
  model_contexts: {
    "model/main": { context_window: 256_000, compression_threshold: 75 },
    "model/worker": { context_window: 64_000, compression_threshold: 70 },
  },
};
const catalog: ModelCatalog = { models: [], loading: false, error: "", refresh: vi.fn() };
const agent: AgentProfile = { id: "coder", name: "编码助手", executor: "native", instructions: "", model: "model/worker" };

describe("模型服务与上下文配置", () => {
  it("角色按自己的模型继承，且窗口与阈值可分别覆盖", () => {
    expect(agentContext(settings, agent)).toEqual({ context_window: 64_000, compression_threshold: 70 });
    expect(agentContext(settings, { ...agent, context_window: 32_000 })).toEqual({ context_window: 32_000, compression_threshold: 70 });
    expect(agentContext(settings, { ...agent, compression_threshold: 50 })).toEqual({ context_window: 64_000, compression_threshold: 50 });
    expect(agentContext(settings, { ...agent, model: "" })).toEqual({ context_window: 256_000, compression_threshold: 75 });
    expect(agentContext(settings, { ...agent, model: "", executor: "codex" })).toEqual({ context_window: 128_000, compression_threshold: 80 });
    expect(modelContext(settings, "unknown")).toEqual({ context_window: 128_000, compression_threshold: 80 });
  });

  it("模型统一设置显示当前策略、合法边界和继承规则", () => {
    const html = renderToStaticMarkup(<ModelContextSettings settings={settings} onChange={vi.fn()} disabled={false} catalogs={[catalog]} />);
    expect(html).toContain('aria-label="上下文配置模型"');
    expect(html).toContain('value="256000"');
    expect(html).toContain('value="75"');
    expect(html).toContain('min="1024"');
    expect(html).toContain('max="2000000"');
    expect(html).toContain("主智能体和使用同一模型的子智能体共享");
    expect(html).toContain("恢复此模型默认值");
  });

  it("角色覆盖明确继承值，并保持旧设置和现有角色预设", () => {
    const normalized = normalizeSettings({ ...settings, agents: [agent] });
    expect(normalized.main_executor).toBe("native");
    expect(normalized.models_endpoint).toBe("/custom/models");
    expect(normalized.agents[0]).toEqual(agent);
    const html = renderToStaticMarkup(<AgentSettings settings={normalized} onChange={vi.fn()} disabled={false} nativeModels={catalog} codexModels={catalog} codexPathChanged={false} />);
    expect(html).toContain('aria-label="角色 1 · 上下文窗口覆盖"');
    expect(html).toContain('placeholder="继承：64000"');
    expect(html).toContain('placeholder="继承：70%"');
    expect(html).toContain("只覆盖其中一项，另一项继续继承");
    expect(html).toContain("桌面对话由主智能体直接处理");
  });

  it("自定义 Endpoint 和模型列表地址可见，已有密钥不回填输入", () => {
    const html = renderToStaticMarkup(<ProviderSettings settings={settings} saved={{ ...settings, api_key_configured: true }} onChange={vi.fn()}
      apiKey="" onKeyChange={vi.fn()} catalog={catalog} disabled={false} />);
    expect(html).toContain("自定义 Endpoint");
    expect(html).toContain('value="/custom/models"');
    expect(html).toContain("与服务同源的完整 URL");
    expect(html).toContain("已配置 · 留空保留同一服务的密钥");
    expect(html).toContain('type="password"');
    expect(html).not.toContain("sk-");
  });
});
