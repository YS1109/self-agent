import type { AgentProfile, ModelContextConfig, Settings } from "./types";

export const DEFAULT_MODEL_CONTEXT: ModelContextConfig = {
  context_window: 128_000,
  compression_threshold: 80,
};

export function modelContext(settings: Settings, model: string): ModelContextConfig {
  return settings.model_contexts?.[model] ?? DEFAULT_MODEL_CONTEXT;
}

export function agentModelContext(settings: Settings, agent: AgentProfile): ModelContextConfig {
  return modelContext(settings, agent.model || (agent.executor === "native" ? settings.model : ""));
}

export function agentContext(settings: Settings, agent: AgentProfile): ModelContextConfig {
  const inherited = agentModelContext(settings, agent);
  return {
    context_window: agent.context_window ?? inherited.context_window,
    compression_threshold: agent.compression_threshold ?? inherited.compression_threshold,
  };
}
