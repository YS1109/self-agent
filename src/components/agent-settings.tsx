import { ExecutionSettings } from "./execution-settings";
import { defaultExecutionControls, executionControlsError, intersectExecutionControls } from "../execution-controls";
import { Bot, Plus, Trash2, Users } from "lucide-react";
import { agentModelLabel } from "../agent-model";
import { agentModelContext } from "../model-context";
import type { AgentProfile, Settings } from "../types";
import { Button } from "./ui/button";
import { ModelSelect, type ModelCatalog } from "./model-select";
import { Select } from "./ui/select";

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
    main_executor: settings.main_executor || "native",
    models_endpoint: settings.models_endpoint || "",
    model_contexts: settings.model_contexts || {},
  };
}

export function AgentSettings({
  settings, onChange, disabled, nativeModels, codexModels, codexPathChanged,
}: {
  settings: Settings;
  onChange: (settings: Settings) => void;
  disabled: boolean;
  nativeModels: ModelCatalog;
  codexModels: ModelCatalog;
  codexPathChanged: boolean;
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
    <section className="agent-settings" aria-label="子智能体模型预设">
      <div className="agent-settings-heading">
        <div><Users size={18} /><h3>子智能体模型预设</h3></div>
        <span>{agents.length} 个角色</span>
      </div>
      <p className="help-text">用于远程派发与编排，为每个角色预设模型、执行器和职责。桌面对话由主智能体直接处理。固定模型后，切换主智能体模型不会改变该角色的预设。新派发保存角色预设，已有计划保留原配置。</p>
      <label>远程派发默认角色
        <Select aria-label="远程派发默认角色" disabled={disabled} value={settings.default_agent_id}
          onValueChange={(value) => onChange({ ...settings, default_agent_id: value })}
          options={agents.map((agent) => ({ value: agent.id, label: `${agent.name || "未命名角色"} · ${agentModelLabel(agent)}` }))} />
      </label>
      <div className="agent-settings-list">
        {agents.map((agent, index) => (
          <fieldset className="agent-editor" key={agent.id} disabled={disabled}>
            <div className="agent-editor-heading">
              <span>角色 {index + 1}{settings.default_agent_id === agent.id ? " · 默认" : ""}</span>
              <Button type="button" size="icon" variant="ghost" disabled={disabled || agents.length <= 1} aria-label={`删除角色 ${agent.name || index + 1}`} onClick={() => remove(agent.id)}><Trash2 size={14} /></Button>
            </div>
            <div className="agent-model-summary"><Bot size={17} /><span>{agentModelLabel(agent)}</span><small>{agent.model ? "固定模型" : "使用默认模型"}</small></div>
            <label>角色名称<input required value={agent.name} placeholder="例如：编码助手、验证助手" onChange={(event) => update(agent.id, { name: event.target.value })} /></label>
            <ModelSelect key={agent.executor} label={`角色 ${index + 1} · 预设模型`} value={agent.model} onChange={(model) => update(agent.id, { model })}
                catalog={agent.executor === "codex" ? codexModels : nativeModels} disabled={disabled || (agent.executor === "codex" && codexPathChanged)}
                emptyLabel={agent.executor === "codex" ? "使用 Codex 本机默认模型" : "跟随主智能体模型（不固定）"}
                hint={agent.executor === "codex" && codexPathChanged ? "CLI 路径已修改，请先保存设置再获取列表。" : undefined} />
            {agent.executor === "native" && !agent.model && settings.model && <Button type="button" variant="secondary" size="small" disabled={disabled}
              onClick={() => update(agent.id, { model: settings.model })}>固定为当前主模型</Button>}
            <label>执行器
              <Select aria-label={`角色 ${index + 1} · 执行器`} value={agent.executor} disabled={disabled}
                onValueChange={(value) => update(agent.id, { executor: value as AgentProfile["executor"], model: value === "native" ? settings.model : "" })}
                options={[{ value: "native", label: "原生工具循环" }, { value: "codex", label: "Codex 本机 CLI" }]} />
            </label>
            <div className="context-fields">
              <label>上下文窗口覆盖（tokens）
                <input aria-label={`角色 ${index + 1} · 上下文窗口覆盖`} type="number" min={1024} max={2_000_000} step={1}
                  placeholder={`继承：${agentModelContext(settings, agent).context_window}`}
                  value={agent.context_window ?? ""} onChange={(event) => update(agent.id, { context_window: event.target.value === "" ? null : Number(event.target.value) })} />
              </label>
              <label>压缩阈值覆盖（%）
                <input aria-label={`角色 ${index + 1} · 压缩阈值覆盖`} type="number" min={10} max={95} step={1}
                  placeholder={`继承：${agentModelContext(settings, agent).compression_threshold}%`}
                  value={agent.compression_threshold ?? ""} onChange={(event) => update(agent.id, { compression_threshold: event.target.value === "" ? null : Number(event.target.value) })} />
              </label>
            </div>
            <p className="help-text">留空分别继承所用模型的统一配置。只覆盖其中一项，另一项继续继承。</p>
            {agent.executor === "codex" && !agent.model && <p className="help-text">Codex 模型未固定时使用应用默认上下文策略；选择具体模型后可继承该模型的统一配置。</p>}
            <details className="role-execution-controls">
              <summary>角色执行预算与权限 · {agent.controls ? "已设额外限制" : "继承全局设置"}</summary>
              <label className="execution-checkbox"><input type="checkbox" aria-label={`角色 ${index + 1} · 自定义执行限制`} checked={!!agent.controls} disabled={disabled}
                onChange={event => update(agent.id, { controls: event.target.checked ? structuredClone(settings.execution_controls ?? defaultExecutionControls()) : null })} />为此角色指定额外限制</label>
              <p className="help-text">切换模型或执行器不会清除这里的限制。取消勾选只移除角色额外限制，全局策略仍然生效。</p>
              {agent.controls && <ExecutionSettings value={agent.controls} executor={agent.executor} scope="role" disabled={disabled} onChange={controls => update(agent.id, { controls })} />}
              {executionControlsError(intersectExecutionControls(settings.execution_controls ?? defaultExecutionControls(), agent.controls), agent.executor) && <p className="execution-warning" role="alert">此角色继承全局限制后当前无法执行：{executionControlsError(intersectExecutionControls(settings.execution_controls ?? defaultExecutionControls(), agent.controls), agent.executor)} 不会自动放宽；请核对全局策略或选用 Native。</p>}
            </details>
            <label>角色指令<textarea value={agent.instructions} rows={3} placeholder="描述角色职责、执行约束和结果要求" onChange={(event) => update(agent.id, { instructions: event.target.value })} /></label>
          </fieldset>
        ))}
      </div>
      <Button type="button" variant="secondary" size="small" disabled={disabled || agents.length >= 16} onClick={() => onChange({
        ...settings,
        agents: [...agents, { id: `agent-${crypto.randomUUID()}`, name: "新角色", instructions: "", executor: "native", model: settings.model }],
      })}><Plus size={15} />添加子智能体预设</Button>
      <label className="codex-binary">Codex 可执行文件
        <input required value={settings.codex_binary} placeholder="codex 或本机绝对路径" onChange={(event) => onChange({ ...settings, codex_binary: event.target.value })} />
        <small>使用本机已安装 CLI 的登录状态和配置。请先在终端确认此命令可用并完成登录。当前版本不管理 MCP 服务和 Skills。</small>
      </label>
    </section>
  );
}
