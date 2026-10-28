import { useEffect, useId, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { api } from "../api";
import { Button } from "./ui/button";

export interface ModelOption { id: string; name: string }
export interface ModelCatalog {
  models: ModelOption[];
  loading: boolean;
  error: string;
  refresh: () => void;
}

export function modelLabel(id: string) {
  return /^sk-[\w-]{20,}$/.test(id) ? "模型名称疑似访问密钥，请重新选择" : id;
}

export function modelOptions(models: ModelOption[], current: string): ModelOption[] {
  return current && !models.some((model) => model.id === current)
    ? [{ id: current, name: `${modelLabel(current)} · 当前设置` }, ...models]
    : models;
}

export function useModelCatalog(body: { base_url?: string; api_key?: string; executor?: string; provider_source?: string }, enabled: boolean): ModelCatalog {
  const key = JSON.stringify(body);
  const [reload, setReload] = useState(0);
  const [result, setResult] = useState<{ key: string; models: ModelOption[]; loading: boolean; error: string }>({
    key: "", models: [], loading: false, error: "",
  });
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    setResult({ key, models: [], loading: true, error: "" });
    const timer = setTimeout(() => {
      void api<{ models: ModelOption[] }>("POST", "/models", JSON.parse(key))
        .then(({ models }) => {
          if (active) setResult({ key, models, loading: false, error: "" });
        })
        .catch((error: unknown) => {
          if (active) setResult({ key, models: [], loading: false, error: error instanceof Error ? error.message : "模型列表获取失败" });
        });
    }, 350);
    return () => { active = false; clearTimeout(timer); };
  }, [key, enabled, reload]);
  const current = result.key === key && enabled ? result : { models: [], loading: enabled, error: "" };
  return { ...current, refresh: () => setReload((value) => value + 1) };
}

const CUSTOM = "__harness_custom_model__";

export function ModelSelect({ label, value, onChange, catalog, disabled = false, emptyLabel = "请选择模型", hint }: {
  label: string; value: string; onChange: (value: string) => void; catalog: ModelCatalog;
  disabled?: boolean; emptyLabel?: string; hint?: string;
}) {
  const id = useId();
  const [custom, setCustom] = useState(false);
  return (
    <div className="model-field">
      <label htmlFor={id}>{label}</label>
      <div className="model-select-row">
        <select id={id} value={custom ? CUSTOM : value} disabled={disabled} onChange={(event) => {
          const selected = event.target.value;
          setCustom(selected === CUSTOM);
          if (selected !== CUSTOM) onChange(selected);
        }}>
          <option value="">{emptyLabel}</option>
          {modelOptions(catalog.models, value).map((model) => <option key={model.id} value={model.id}>{modelLabel(model.name)}</option>)}
          <option value={CUSTOM}>自定义模型 ID…</option>
        </select>
        <Button type="button" variant="secondary" size="icon" disabled={disabled || catalog.loading} onClick={catalog.refresh} aria-label={`刷新${label}列表`} title="从当前服务重新获取模型列表">
          {catalog.loading ? <Loader2 size={14} className="spin" /> : <RefreshCw size={14} />}
        </Button>
      </div>
      {custom && <input aria-label={`${label} · 自定义 ID`} value={value} disabled={disabled} placeholder="输入服务支持的完整模型 ID" onChange={(event) => onChange(event.target.value)} />}
      <p className="model-select-hint" role="status">
        {catalog.loading ? "正在获取可用模型…" : catalog.error || (catalog.models.length ? `${catalog.models.length} 个可用模型` : "服务未返回可用模型，可刷新或选择自定义模型 ID。")}
        {hint && <span>{hint}</span>}
      </p>
    </div>
  );
}

export function QuickModelSelect({ value, catalog, disabled, onChange, onSettings }: {
  value: string; catalog: ModelCatalog; disabled: boolean;
  onChange: (value: string) => void; onSettings: () => void;
}) {
  return (
    <select className="model-picker" aria-label="当前主智能体模型" value={value} disabled={disabled} title={catalog.error || "选择主智能体模型；新设置用于后续请求"}
      onChange={(event) => event.target.value === CUSTOM ? onSettings() : onChange(event.target.value)}>
      <option value="" disabled>{catalog.loading ? "正在获取模型…" : "请选择模型"}</option>
      {modelOptions(catalog.models, value).map((model) => <option key={model.id} value={model.id}>{modelLabel(model.name)}</option>)}
      <option value={CUSTOM}>模型设置／刷新列表…</option>
    </select>
  );
}
