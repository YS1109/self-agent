import { useEffect, useRef, useState } from "react";
import { Loader2, ShieldCheck } from "lucide-react";
import { api } from "../api";
import type { Settings } from "../types";
import { ModelSelect, type ModelCatalog } from "./model-select";
import { Button } from "./ui/button";
import { Select } from "./ui/select";

export function ProviderSettings({ settings, saved, onChange, apiKey, onKeyChange, catalog, disabled }: {
  settings: Settings; saved: Settings; onChange: (settings: Settings) => void;
  apiKey: string; onKeyChange: (key: string) => void; catalog: ModelCatalog; disabled: boolean;
}) {
  const local = settings.provider_source === "local_codex";
  const codex = settings.main_executor === "codex";
  const [sourceError, setSourceError] = useState("");
  const [reading, setReading] = useState(false);
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<{ key: string; message: string }>();
  const draft = {
    provider_source: settings.provider_source ?? "manual",
    base_url: settings.base_url,
    models_endpoint: settings.models_endpoint ?? "",
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
        onChangeRef.current({ ...current, base_url: provider.base_url, model: current.model || (current.main_executor === "codex" ? "" : provider.model) });
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
    <label>主智能体 · 执行器
      <Select aria-label="主智能体 · 执行器" value={settings.main_executor ?? "native"} disabled={disabled} onValueChange={(value) =>
        onChange({ ...settings, main_executor: value as "native" | "codex", model: "" })} options={[
        { value: "native", label: "原生工具循环 · 自定义模型服务" },
        { value: "codex", label: "Codex 本机 CLI" },
      ]} />
    </label>
    <p className="help-text">{codex ? "主智能体通过本机 Codex CLI 直接工作，使用其登录与服务配置。下面的模型服务配置仍供原生子智能体与远程编排使用。" : "主智能体直接调用模型与项目工具，多轮处理会话目标。远程入口可额外派发子智能体。"}</p>
    <ModelSelect key={settings.main_executor ?? "native"} label="主智能体 · 模型" value={settings.model} catalog={catalog}
      disabled={disabled || (!codex && reading) || (codex && settings.codex_binary !== saved.codex_binary)}
      hint={codex && settings.codex_binary !== saved.codex_binary ? "CLI 路径已修改，请先保存设置再获取列表。" : undefined}
      emptyLabel={codex ? "使用 Codex 本机默认模型" : "请选择模型"}
      onChange={(model) => onChange({ ...settings, model })} />
    {codex && !settings.model && <p className="help-text">未指定模型时由 Codex 选择本机默认模型，应用使用 128,000 tokens / 80% 的默认上下文策略。需要按模型设置策略时，请先选择具体模型。</p>}
    <div className="settings-section-heading"><h3>模型服务</h3></div>
    <label>服务配置来源
      <Select aria-label="服务配置来源" value={settings.provider_source ?? "manual"} disabled={disabled} onValueChange={(value) => {
        onKeyChange("");
        setSourceError("");
        onChange({ ...settings, provider_source: value as "manual" | "local_codex", base_url: "", models_endpoint: "" });
      }} options={[
        { value: "manual", label: "手动配置模型服务" },
        { value: "local_codex", label: "复用本机 Codex 服务配置" },
      ]} />
    </label>
    {local && <div className="form-info"><ShieldCheck size={16} /><p>
      从本机 Codex 的顶层自定义服务配置读取地址、凭据和请求头；重启后自动重新读取，不复制凭据到本应用。
      当前仍使用 Chat Completions，连接检测会验证 JSON 回复。专属登录与配置档案暂不支持。
    </p></div>}
    {reading && <p role="status">正在读取本机服务配置…</p>}
    {sourceError && <p className="codex-check-result" role="alert">{sourceError}</p>}
    <label>自定义 Endpoint
      <input type="url" placeholder="https://api.example.com/v1" value={settings.base_url} readOnly={local}
        onChange={(event) => onChange({ ...settings, base_url: event.target.value })} />
      <small>{local ? "地址由本机 Codex 配置提供，切换为手动配置后可以修改。" : "填写模型服务的 API 基础地址；模型请求使用此地址下的 /chat/completions。"}</small>
    </label>
    <label>模型列表 Endpoint <span className="field-note">可选</span>
      <input aria-label="模型列表 Endpoint" placeholder="留空使用 /models，例如 /v1/models" value={settings.models_endpoint ?? ""}
        onChange={(event) => onChange({ ...settings, models_endpoint: event.target.value })} />
      <small>可填写路径或与服务同源的完整 URL，使用当前服务凭据拉取。修改地址后会自动刷新模型下拉列表。</small>
    </label>
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
    {saved.configuration_error && <p className="codex-check-result">{codex ? "原生执行与远程编排的模型服务配置不可用" : "当前保存的配置不可用"}：{saved.configuration_error}</p>}
    <div className="codex-check">
      <Button type="button" size="small" variant="secondary" disabled={disabled || codex || checking || reading || !!sourceError || !settings.model}
        onClick={() => void check()}>
        {checking ? <Loader2 size={13} className="spin" /> : <ShieldCheck size={13} />}检测模型连接
      </Button>
      <span>{codex ? "主智能体使用 Codex CLI，请在下方检测其可用状态。" : "检测当前填写的配置，会请求一次简短模型回复并产生用量；不保存设置。"}</span>
    </div>
    {result?.key === key && <p className="codex-check-result" role="status">{result.message}</p>}
  </>;
}
