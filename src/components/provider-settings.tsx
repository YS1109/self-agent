import { useEffect, useRef, useState } from "react";
import { Loader2, ShieldCheck } from "lucide-react";
import { api } from "../api";
import type { Settings } from "../types";
import { ModelSelect, type ModelCatalog } from "./model-select";
import { Button } from "./ui/button";

export function ProviderSettings({ settings, saved, onChange, apiKey, onKeyChange, catalog, disabled }: {
  settings: Settings; saved: Settings; onChange: (settings: Settings) => void;
  apiKey: string; onKeyChange: (key: string) => void; catalog: ModelCatalog; disabled: boolean;
}) {
  const local = settings.provider_source === "local_codex";
  const [sourceError, setSourceError] = useState("");
  const [reading, setReading] = useState(false);
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<{ key: string; message: string }>();
  const draft = {
    provider_source: settings.provider_source ?? "manual",
    base_url: settings.base_url,
    model: settings.model,
    ...(apiKey ? { api_key: apiKey } : {}),
  };
  const key = JSON.stringify(draft);
  const currentKey = useRef(key);
  currentKey.current = key;
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  useEffect(() => {
    if (!local) {
      setReading(false);
      return;
    }
    let active = true;
    setReading(true);
    setSourceError("");
    void api<Settings>("GET", "/provider/local-codex")
      .then((provider) => {
        if (!active) return;
        const current = settingsRef.current;
        onChangeRef.current({ ...current, base_url: provider.base_url, model: current.model || provider.model });
      })
      .catch((error: unknown) => {
        if (active) setSourceError(error instanceof Error ? error.message : "读取本机服务配置失败");
      })
      .finally(() => { if (active) setReading(false); });
    return () => { active = false; };
  }, [local]);

  async function check() {
    if (checking) return;
    const requestKey = key;
    setChecking(true);
    setResult(undefined);
    try {
      const value = await api<{ message: string }>("POST", "/settings/check", draft);
      if (currentKey.current === requestKey) setResult({ key: requestKey, message: value.message });
    } catch (error) {
      if (currentKey.current === requestKey) setResult({ key: requestKey, message: error instanceof Error ? error.message : "连接检测失败" });
    } finally {
      setChecking(false);
    }
  }

  return <>
    <label>主智能体 · 配置来源
      <select value={settings.provider_source ?? "manual"} disabled={disabled} onChange={(event) => {
        onKeyChange("");
        setSourceError("");
        onChange({ ...settings, provider_source: event.target.value as "manual" | "local_codex", base_url: "", model: "" });
      }}>
        <option value="manual">手动配置模型服务</option>
        <option value="local_codex">复用本机 Codex 服务配置</option>
      </select>
    </label>
    {local && <div className="form-info"><ShieldCheck size={16} /><p>
      从本机 Codex 的顶层自定义服务配置读取地址、凭据和请求头；重启后自动重新读取，不复制凭据到本应用。
      当前仍使用 Chat Completions，连接检测会验证 JSON 回复。专属登录与配置档案暂不支持。
    </p></div>}
    {reading && <p role="status">正在读取本机服务配置…</p>}
    {sourceError && <p className="codex-check-result" role="alert">{sourceError}</p>}
    <label>主智能体 · 模型服务地址
      <input type="url" placeholder="https://api.example.com/v1" value={settings.base_url} readOnly={local}
        onChange={(event) => onChange({ ...settings, base_url: event.target.value })} />
    </label>
    <ModelSelect label="主智能体 · 模型" value={settings.model} catalog={catalog} disabled={disabled || reading}
      onChange={(model) => onChange({ ...settings, model })} />
    {!local && <>
      <label>访问密钥 <span className="field-note">{saved.provider_source !== "local_codex" && saved.api_key_configured ? "已配置 · 留空保留同一服务的密钥" : "未配置"}</span>
        <input type="password" autoComplete="off" placeholder="输入访问密钥，留空不会复制其他服务的密钥"
          value={apiKey} onChange={(event) => onKeyChange(event.target.value)} />
      </label>
      <div className="form-info"><ShieldCheck size={16} /><p>
        手动填写的密钥仅保存在后台内存中，重启后需重新填写，也可通过环境变量配置。修改服务地址会清除旧密钥。
        模型协议为 Chat Completions。
      </p></div>
    </>}
    {saved.configuration_error && <p className="codex-check-result">当前保存的配置不可用：{saved.configuration_error}</p>}
    <div className="codex-check">
      <Button type="button" size="small" variant="secondary" disabled={disabled || checking || reading || !!sourceError || !settings.model}
        onClick={() => void check()}>
        {checking ? <Loader2 size={13} className="spin" /> : <ShieldCheck size={13} />}检测模型连接
      </Button>
      <span>检测当前填写的配置，会请求一次简短模型回复并产生用量；不保存设置。</span>
    </div>
    {result?.key === key && <p className="codex-check-result" role="status">{result.message}</p>}
  </>;
}
