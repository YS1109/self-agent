import { useEffect, useId, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { api } from "../api";
import { Button } from "./ui/button";
import { Select } from "./ui/select";

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

export function useModelCatalog(body: { base_url?: string; models_endpoint?: string; api_key?: string; executor?: string; provider_source?: string; provider_id?: string }, enabled: boolean): ModelCatalog {
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
        <Select id={id} aria-label={label} value={custom ? CUSTOM : value} disabled={disabled} onValueChange={(selected) => {
          setCustom(selected === CUSTOM);
          if (selected !== CUSTOM) onChange(selected);
        }} options={[
          { value: "", label: emptyLabel },
          ...modelOptions(catalog.models, value).map((model) => ({ value: model.id, label: modelLabel(model.name) })),
          { value: CUSTOM, label: "自定义模型 ID…" },
        ]} />
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

export function QuickModelSelect({ value, catalog, disabled, onChange, onSettings, emptyLabel = "请选择模型" }: {
  value: string; catalog: ModelCatalog; disabled: boolean;
  onChange: (value: string) => void; onSettings: () => void;
  emptyLabel?: string;
}) {
  return (
    <Select className="model-picker" aria-label="当前主智能体模型" value={value} disabled={disabled} title={catalog.error || "选择主智能体模型；新设置用于后续请求"}
      onValueChange={(next) => next === CUSTOM ? onSettings() : onChange(next)}
      options={[
        { value: "", disabled: true, label: catalog.loading ? "正在获取模型…" : emptyLabel },
        ...modelOptions(catalog.models, value).map((model) => ({ value: model.id, label: modelLabel(model.name) })),
        { value: CUSTOM, label: "模型设置／刷新列表…" },
      ]} />
  );
}
