import type { NamedProvider } from "../providers-client";
import { useEffect, useState } from "react";
import { Layers3, RotateCcw } from "lucide-react";
import type { Settings } from "../types";
import { modelContext, mainEffectiveModel, agentEffectiveModel } from "../model-context";
import { Button } from "./ui/button";
import { Select } from "./ui/select";
import type { ModelCatalog } from "./model-select";

export function ModelContextSettings({ settings, onChange, disabled, catalogs, providers = [] }: {
  settings: Settings; onChange: (settings: Settings) => void; disabled: boolean; catalogs: ModelCatalog[]; providers?: NamedProvider[];
}) {
  const mainModel = mainEffectiveModel(settings, providers);
  const [target, setTarget] = useState(mainModel);
  useEffect(() => setTarget(mainModel), [mainModel]);
  const models = [...new Set([
    mainModel,
    ...settings.agents.map((agent) => agentEffectiveModel(settings, agent, providers)),
    ...Object.keys(settings.model_contexts ?? {}),
    ...catalogs.flatMap((catalog) => catalog.models.map((model) => model.id)),
  ].filter(Boolean))];
  const model = models.includes(target) ? target : models[0] ?? "";
  const policy = modelContext(settings, model);
  const update = (values: Partial<typeof policy>) => {
    if (!model) return;
    onChange({ ...settings, model_contexts: { ...settings.model_contexts, [model]: { ...policy, ...values } } });
  };
  return <section className="model-context-settings" aria-label="模型上下文配置">
    <div className="settings-section-heading"><Layers3 size={18} /><h3>模型上下文</h3></div>
    <p className="help-text">按模型 ID 统一保存，主智能体和使用同一模型的子智能体共享。子智能体可分别覆盖窗口和阈值。未单独配置时使用 128,000 tokens / 80%。</p>
    <label>配置模型
      <Select aria-label="上下文配置模型" disabled={disabled || !models.length} value={model} onValueChange={setTarget}
        options={models.length ? models.map((id) => ({ value: id, label: `${id}${id === mainModel ? " · 主模型" : ""}` })) : [{ value: "", label: "请先选择模型" }]} />
    </label>
    <div className="context-fields">
      <label>上下文窗口（tokens）
        <input aria-label="模型上下文窗口" type="number" min={1024} max={2_000_000} step={1} required
          disabled={disabled || !model} value={policy.context_window}
          onChange={(event) => update({ context_window: Number(event.target.value) })} />
      </label>
      <label>自动压缩阈值（%）
        <input aria-label="模型自动压缩阈值" type="number" min={10} max={95} step={1} required
          disabled={disabled || !model} value={policy.compression_threshold}
          onChange={(event) => update({ compression_threshold: Number(event.target.value) })} />
      </label>
    </div>
    <p className="help-text">约 {(Math.floor(policy.context_window * policy.compression_threshold / 100)).toLocaleString("zh-CN")} tokens 时压缩较早上下文。原生执行器使用估算用量，Codex 执行器传递自动压缩配置。请按模型服务的实际限制填写。</p>
    {model && settings.model_contexts?.[model] && <Button type="button" size="small" variant="ghost" disabled={disabled} onClick={() => {
      const next = { ...settings.model_contexts };
      delete next[model];
      onChange({ ...settings, model_contexts: next });
    }}><RotateCcw size={13} />恢复此模型默认值</Button>}
  </section>;
}
