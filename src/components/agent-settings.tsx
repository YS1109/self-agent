import { Plus, Trash2, Users } from "lucide-react";
import type { AgentProfile, Settings } from "../types";
import { Button } from "./ui/button";

export const DEFAULT_AGENT: AgentProfile = {
  id: "coder",
  name: "编码助手",
  instructions: "根据任务目标完成项目修改，执行必要验证并如实报告结果。",
  executor: "native",
  model: "",
};

export function normalizeSettings(settings: Settings): Settings {
  return {
    ...settings,
    agents: settings.agents?.length ? settings.agents : [{ ...DEFAULT_AGENT }],
    default_agent_id: settings.default_agent_id || "coder",
    codex_binary: settings.codex_binary || "codex",
  };
}

export function AgentSettings({
  settings, onChange, disabled,
}: {
  settings: Settings;
  onChange: (settings: Settings) => void;
  disabled: boolean;
}) {
  const agents = settings.agents;
  const update = (id: string, values: Partial<AgentProfile>) =>
    onChange({ ...settings, agents: agents.map((agent) => agent.id === id ? { ...agent, ...values } : agent) });
  const remove = (id: string) => {
    const next = agents.filter((agent) => agent.id !== id);
    onChange({
      ...settings, agents: next,
      default_agent_id: settings.default_agent_id === id ? next[0].id : settings.default_agent_id,
    });
  };
  return (
    <section className="agent-settings" aria-label="子智能体角色设置">
      <div className="agent-settings-heading">
        <div><Users size={16} /><h3>子智能体角色</h3></div>
        <span>{agents.length} 个角色</span>
      </div>
      <p className="help-text">主智能体负责规划与回顾，任务由这里的角色执行。新设置用于之后创建的计划，已有计划保留创建时的角色快照。</p>
      <label>默认执行角色
        <select disabled={disabled} value={settings.default_agent_id} onChange={(event) => onChange({ ...settings, default_agent_id: event.target.value })}>
          {agents.map((agent) => <option key={agent.id} value={agent.id}>{agent.name || "未命名角色"}</option>)}
        </select>
      </label>
      <div className="agent-settings-list">
        {agents.map((agent, index) => (
          <fieldset className="agent-editor" key={agent.id} disabled={disabled}>
            <div className="agent-editor-heading">
              <span>角色 {index + 1}{settings.default_agent_id === agent.id ? " · 默认" : ""}</span>
              <Button type="button" size="icon" variant="ghost" disabled={disabled || agents.length <= 1} aria-label={`删除角色 ${agent.name || index + 1}`} onClick={() => remove(agent.id)}><Trash2 size={14} /></Button>
            </div>
            <label>角色名称<input required value={agent.name} placeholder="例如：编码助手、验证助手" onChange={(event) => update(agent.id, { name: event.target.value })} /></label>
            <div className="agent-editor-columns">
              <label>执行器
                <select value={agent.executor} onChange={(event) => update(agent.id, { executor: event.target.value as AgentProfile["executor"] })}>
                  <option value="native">原生工具循环</option>
                  <option value="codex">Codex 本机 CLI</option>
                </select>
              </label>
              <label>执行模型<input value={agent.model} placeholder={agent.executor === "codex" ? "留空使用 Codex 本机配置" : "留空继承主智能体模型"} onChange={(event) => update(agent.id, { model: event.target.value })} /></label>
            </div>
            <label>角色指令<textarea value={agent.instructions} rows={3} placeholder="描述角色职责、执行约束和结果要求" onChange={(event) => update(agent.id, { instructions: event.target.value })} /></label>
          </fieldset>
        ))}
      </div>
      <Button type="button" variant="secondary" size="small" disabled={disabled || agents.length >= 16} onClick={() => onChange({
        ...settings,
        agents: [...agents, { id: `agent-${crypto.randomUUID()}`, name: "新角色", instructions: "", executor: "native", model: "" }],
      })}><Plus size={13} />添加角色</Button>
      <label className="codex-binary">Codex 可执行文件
        <input required value={settings.codex_binary} placeholder="codex 或本机绝对路径" onChange={(event) => onChange({ ...settings, codex_binary: event.target.value })} />
        <small>使用本机已安装 CLI 的登录状态和配置。请先在终端确认此命令可用并完成登录。当前版本不管理 MCP 服务和 Skills。</small>
      </label>
    </section>
  );
}
