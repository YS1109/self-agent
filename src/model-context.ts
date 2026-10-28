import type { NamedProvider } from "./providers-client";
import type { AgentProfile, ModelContextConfig, Settings } from "./types";

export const DEFAULT_MODEL_CONTEXT: ModelContextConfig = {
  context_window: 128_000,
  compression_threshold: 80,
};

export function modelContext(settings: Settings, model: string): ModelContextConfig {
  return settings.model_contexts?.[model] ?? DEFAULT_MODEL_CONTEXT;
}

export function mainEffectiveModel(settings: Settings, providers: NamedProvider[] = []): string {
  if (settings.main_executor === "codex" || settings.provider_source !== "named") return settings.model;
  return providers.find(provider => provider.id === settings.provider_id)?.definition.model ?? settings.model;
}
export function agentEffectiveModel(settings: Settings, agent: AgentProfile, providers: NamedProvider[] = []): string {
  if (agent.model || agent.executor === "codex") return agent.model;
  if (agent.provider_id) return providers.find(provider => provider.id === agent.provider_id)?.definition.model ?? "";
  if (settings.provider_source === "named") return providers.find(provider => provider.id === settings.provider_id)?.definition.model ?? settings.model;
  return settings.model;
}
export function agentModelContext(settings: Settings, agent: AgentProfile, providers: NamedProvider[] = []): ModelContextConfig {
  return modelContext(settings, agentEffectiveModel(settings, agent, providers));
}

export function agentContext(settings: Settings, agent: AgentProfile, providers: NamedProvider[] = []): ModelContextConfig {
  const inherited = agentModelContext(settings, agent, providers);
  return {
    context_window: agent.context_window ?? inherited.context_window,
    compression_threshold: agent.compression_threshold ?? inherited.compression_threshold,
  };
}
