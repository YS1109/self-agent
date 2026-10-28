import type { AgentProfile } from "./types";

export function agentModelLabel(agent: AgentProfile): string {
  return agent.model || (agent.executor === "codex" ? "Codex 本机默认模型" : "跟随主智能体模型");
}
