import { useEffect, useRef, useState } from "react";
import {
  EMPTY_CAPABILITIES, capabilityCommandError, capabilityReason, createCapabilityController,
  type CapabilityClient, type CapabilityCommand, type CapabilitySelection, type CapabilityState,
  type ManagedCapabilities, type McpProtocol, type McpServerView, type SkillView,
} from "../capabilities-client";
import type { McpCredentialClient } from "../mcp-credentials-client";
import { McpCredentials } from "./mcp-credentials";
import { Button } from "./ui/button";
import { Select } from "./ui/select";
import "./capabilities-settings.css";

const STATUS: Record<string, string> = { configured: "已配置 · 尚未发现", checking: "工具目录发现中", ready: "工具目录就绪 · 未证明实际调用", unavailable: "当前不可用" };
const PROTOCOLS = [{ value: "modern2026", label: "Modern · 2026-07-28" }, { value: "legacy2025", label: "Legacy · 2025-11-25" }];
type Submit = (command: CapabilityCommand) => Promise<unknown>;

export interface CapabilitiesSettingsProps {
  registry: ManagedCapabilities;
  client: CapabilityClient;
  credentialClient?: McpCredentialClient;
  onRegistryChange?: (registry: ManagedCapabilities) => void;
  disabled: boolean;
  selection?: CapabilitySelection;
  onSelectionChange?: (selection: CapabilitySelection) => void;
  executor?: "native" | "codex";
}
export function CapabilitiesSettings({ registry, client, credentialClient, onRegistryChange, disabled, selection, onSelectionChange, executor = "native" }: CapabilitiesSettingsProps) {
  const [state, setState] = useState<CapabilityState>({ registry, busy: false, loading: false, error: "", notice: "" });
  const [credentialBusy, setCredentialBusy] = useState(false);
  const controller = useRef<ReturnType<typeof createCapabilityController> | null>(null);
  const notify = useRef(onRegistryChange); notify.current = onRegistryChange;
  const currentRegistry = useRef(registry); currentRegistry.current = registry;
  useEffect(() => {
    const active = createCapabilityController(client, currentRegistry.current, next => {
      setState(next);
      if (next.registry !== currentRegistry.current) { currentRegistry.current = next.registry; notify.current?.(next.registry); }
    });
    controller.current = active;
    void active.refresh();
    return () => { active.dispose(); if (controller.current === active) controller.current = null; };
  }, [client]);
  useEffect(() => { if (controller.current?.getState().registry !== registry) controller.current?.replaceRegistry(registry); }, [registry]);
  const blocked = disabled || state.busy || credentialBusy;
  const submit: Submit = command => controller.current?.submit(command) ?? Promise.resolve(undefined);
  return <section className="capabilities-settings" aria-label="应用管理的 MCP 与 Skills">
    <div className="capability-heading"><h3>应用管理的 MCP 与 Skills</h3><Button type="button" variant="secondary" size="small" disabled={disabled || state.loading} onClick={() => void controller.current?.refresh()}>{state.loading ? "读取中…" : "刷新记录"}</Button></div>
    <p className="help-text">这里管理本应用的能力，不代表 Codex 自行加载的外部配置。已配置、启用、目录就绪与任务实际调用是不同状态；最终使用情况以任务执行记录为准。</p>
    <p className="execution-warning">MCP stdio 服务是可信、未沙箱化的本机进程，启动或发现工具也可能写文件、联网；不是只读操作。保存配置和启用开关不会自动启动服务，也不会授权任务调用。</p>
    <p className="help-text">不要把密钥、令牌或密码写入名称、路径、参数或技能正文。声明槽只填写环境变量名称；值仅通过下方专用系统凭据入口提交。</p>
    {state.error && <p role="alert" className="task-error">{state.error}</p>}
    {state.notice && <p role="status" className="capability-notice">{state.notice}</p>}
    {selection && onSelectionChange && <CapabilitySelectionSettings registry={state.registry} value={selection} onChange={onSelectionChange} disabled={blocked} executor={executor} />}
    <h4>MCP 本机服务 · {state.registry.mcp_servers.length}</h4>
    {!state.registry.mcp_servers.length && <p className="help-text">尚未配置服务。只支持 stdio，不自动连接远程服务。</p>}
    {state.registry.mcp_servers.map(server => <McpServerCard key={`${server.id}:${server.revision}`} server={server} disabled={blocked} onSubmit={submit} />)}
    <details className="capability-editor"><summary>添加 MCP 本机服务</summary><McpServerEditor disabled={blocked} onSubmit={submit} /></details>
    {credentialClient ? <McpCredentials registry={state.registry} client={credentialClient} disabled={disabled || state.busy} onRefresh={() => controller.current?.refresh()} onBusyChange={setCredentialBusy} /> : <p className="help-text">此入口尚未接入系统凭据操作；已声明槽不会自动获得值。</p>}
    <h4>Skills 指令包 · {state.registry.skills.length}</h4>
    <p className="help-text">显式选择且启用的技能将用于新任务的上下文，不会自动执行脚本；allowed-tools 等正文说明不授予权限。编辑仅影响新任务，禁用会撤销未来使用。</p>
    {state.registry.skills.map(skill => <SkillCard key={`${skill.id}:${skill.revision}`} skill={skill} disabled={blocked} onSubmit={submit} />)}
    <details className="capability-editor"><summary>添加或导入技能</summary><SkillEditor disabled={blocked} onSubmit={submit} /></details>
  </section>;
}

export function McpServerCard({ server, disabled, onSubmit }: { server: McpServerView; disabled: boolean; onSubmit: Submit }) {
  const [confirmed, setConfirmed] = useState(false), [deleting, setDeleting] = useState(false);
  const checking = server.status === "checking";
  return <article className="capability-card" aria-label={`MCP 服务 ${server.name}`}>
    <div className="capability-heading"><strong>{server.name}</strong><span>{STATUS[server.status] ?? "后台状态待确认"}</span></div>
    <p className="help-text">修订 {server.revision} · {server.enabled ? "已启用（不是任务授权）" : "已禁用"} · stdio · {server.protocol === "modern2026" ? "2026-07-28" : "2025-11-25"}</p>
    <pre className="capability-code">{JSON.stringify({ program: server.program, args: server.args }, null, 2)}</pre>
    <p className="help-text">已发现工具 {server.tool_count} 个{server.catalog_digest && <> · 目录摘要 <code>{server.catalog_digest}</code></>}。这不证明任一工具已被任务调用。</p>
    {server.reason && <p className="task-error">{capabilityReason(server.reason)}</p>}
    {!!server.credential_slots.length && <p className="help-text">已有凭据槽名称：{server.credential_slots.join("、")}；值不在配置记录中展示。</p>}
    <div className="capability-actions"><Button type="button" size="small" variant="secondary" disabled={disabled} onClick={() => void onSubmit({ type: "set_capability_enabled", kind: "mcp", id: server.id, expected_revision: server.revision, enabled: !server.enabled })}>{server.enabled ? "禁用服务" : "启用配置（不启动）"}</Button></div>
    <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled || checking} onChange={event => setConfirmed(event.target.checked)} />我信任上方确切程序及参数，允许这次启动未沙箱化进程以发现工具（可能写文件、联网）。</label>
    <Button type="button" size="small" disabled={disabled || checking || !confirmed} onClick={() => { setConfirmed(false); void onSubmit({ type: "probe_mcp_server", id: server.id, expected_revision: server.revision, confirm_start: true }); }}>{checking ? "正在发现工具目录…" : "确认启动并发现工具"}</Button>
    <p className="help-text">Windows 使用后台提供的 Job 进程管理；是否可用以真实发现状态为准，不默认声称已验证。发现结果可刷新，不会自动重新启动。</p>
    <details className="capability-editor"><summary>编辑服务配置</summary>
      <McpServerEditor server={server} disabled={disabled || checking} onSubmit={onSubmit} />
    </details>
    <div className="capability-actions"><label className="execution-checkbox"><input type="checkbox" checked={deleting} disabled={disabled} onChange={event => setDeleting(event.target.checked)} />确认删除此服务配置</label><Button type="button" size="small" variant="secondary" disabled={disabled || !deleting} onClick={() => void onSubmit({ type: "delete_capability", kind: "mcp", id: server.id, expected_revision: server.revision })}>删除配置</Button></div>
  </article>;
}

export function McpServerEditor({ server, disabled, onSubmit }: { server?: McpServerView; disabled: boolean; onSubmit: Submit }) {
  const [name, setName] = useState(server?.name ?? ""), [program, setProgram] = useState(server?.program ?? "");
  const [args, setArgs] = useState(JSON.stringify(server?.args ?? [], null, 2)), [protocol, setProtocol] = useState<McpProtocol>(server?.protocol ?? "modern2026");
  const [slots, setSlots] = useState(server?.credential_slots.join("\n") ?? "");
  const [enabled, setEnabled] = useState(server?.enabled ?? false), [localError, setLocalError] = useState("");
  return <form className="capability-fields" onSubmit={event => {
    event.preventDefault();
    let parsed: unknown;
    try { parsed = JSON.parse(args); } catch { setLocalError("参数须为 JSON 字符串数组，例如 [\"--mode\", \"stdio\"]。"); return; }
    if (!Array.isArray(parsed) || !parsed.every(arg => typeof arg === "string")) { setLocalError("参数须为 JSON 字符串数组。"); return; }
    const command: CapabilityCommand = { type: "save_mcp_server", ...(server ? { id: server.id, expected_revision: server.revision } : {}), definition: { name, program, args: parsed, protocol, enabled, credential_refs: {}, credential_slots: slots.split(/\r?\n/).map(slot => slot.trim()).filter(Boolean) } };
    const error = capabilityCommandError(command); setLocalError(error ?? ""); if (!error) void onSubmit(command);
  }}>
    <label>服务名称<input aria-label="MCP 服务名称" required maxLength={80} value={name} disabled={disabled} onChange={event => setName(event.target.value)} /></label>
    <label>程序绝对路径<input aria-label="MCP 程序绝对路径" required maxLength={4096} placeholder="/absolute/path/to/server" value={program} disabled={disabled} onChange={event => setProgram(event.target.value)} /></label>
    <label>参数（JSON 字符串数组，不经 shell 拼接）<textarea aria-label="MCP 程序参数" rows={3} maxLength={270000} value={args} disabled={disabled} onChange={event => setArgs(event.target.value)} /></label>
    <label>声明凭据槽（每行一个环境变量名，最多 8 个）<textarea aria-label="MCP 声明凭据槽" value={slots} rows={3} maxLength={600} disabled={disabled} onChange={event => setSlots(event.target.value)} placeholder="SERVICE_API_TOKEN" /></label>
    <p className="help-text">只声明名称，不填写值或 opaque 引用。禁止启动、代码加载、代理、证书和宿主配置变量。修改程序、参数、协议或声明槽会解除所有旧凭据绑定，保存后须重新明确发现工具。</p>
    <label>显式协议版本<Select aria-label="MCP 协议版本" value={protocol} options={PROTOCOLS} disabled={disabled} onValueChange={value => setProtocol(value as McpProtocol)} /></label>
    <label className="execution-checkbox"><input type="checkbox" checked={enabled} disabled={disabled} onChange={event => setEnabled(event.target.checked)} />启用配置（不会启动服务或授予工具调用权限）</label>
    {localError && <p role="alert" className="task-error">{localError}</p>}
    <Button type="submit" size="small" disabled={disabled}>保存配置（不启动）</Button>
  </form>;
}

export function SkillCard({ skill, disabled, onSubmit }: { skill: SkillView; disabled: boolean; onSubmit: Submit }) {
  const [deleting, setDeleting] = useState(false);
  return <article className="capability-card" aria-label={`技能 ${skill.name}`}><div className="capability-heading"><strong>{skill.name}</strong><span>{skill.enabled ? "已启用" : "已禁用"}</span></div>
    <p className="help-text">修订 {skill.revision} · 来源 <code>{skill.source}</code> · {skill.content_bytes} 字节</p><p className="help-text">正文摘要 <code>{skill.content_digest}</code></p>
    <Button type="button" size="small" variant="secondary" disabled={disabled} onClick={() => void onSubmit({ type: "set_capability_enabled", kind: "skill", id: skill.id, expected_revision: skill.revision, enabled: !skill.enabled })}>{skill.enabled ? "禁用技能" : "启用技能"}</Button>
    <details className="capability-editor"><summary>替换技能正文 / 重新导入</summary><p className="help-text">公开记录不返回原正文。请显式提供完整替换文本或文件；不会用空白表单覆盖已有技能。</p><SkillEditor skill={skill} disabled={disabled} onSubmit={onSubmit} /></details>
    <div className="capability-actions"><label className="execution-checkbox"><input type="checkbox" checked={deleting} disabled={disabled} onChange={event => setDeleting(event.target.checked)} />确认删除此技能</label><Button type="button" size="small" variant="secondary" disabled={disabled || !deleting} onClick={() => void onSubmit({ type: "delete_capability", kind: "skill", id: skill.id, expected_revision: skill.revision })}>删除技能</Button></div>
  </article>;
}
export function SkillEditor({ skill, disabled, onSubmit }: { skill?: SkillView; disabled: boolean; onSubmit: Submit }) {
  const [name, setName] = useState(skill?.name ?? ""), [content, setContent] = useState(""), [path, setPath] = useState("");
  const [mode, setMode] = useState("manual"), [localError, setLocalError] = useState("");
  return <form className="capability-fields" onSubmit={event => {
    event.preventDefault(); const revision = skill ? { id: skill.id, expected_revision: skill.revision } : {};
    const command: CapabilityCommand = mode === "manual" ? { type: "save_skill", ...revision, name, content } : { type: "import_skill", ...revision, name, source_path: path };
    const error = capabilityCommandError(command); setLocalError(error ?? ""); if (!error) void onSubmit(command);
  }}>
    <label>技能名称<input aria-label="技能名称" value={name} maxLength={80} required disabled={disabled} onChange={event => setName(event.target.value)} /></label>
    <label>内容来源<Select aria-label="技能内容来源" value={mode} disabled={disabled} options={[{ value: "manual", label: "手动输入完整文本" }, { value: "import", label: "导入本机 SKILL.md" }]} onValueChange={setMode} /></label>
    {mode === "manual" ? <label>完整技能正文<textarea aria-label="完整技能正文" rows={6} maxLength={65536} value={content} required disabled={disabled} onChange={event => setContent(event.target.value)} /></label>
      : <label>后台机器上的 SKILL.md 绝对路径<input aria-label="SKILL.md 绝对路径" value={path} maxLength={4096} required disabled={disabled} onChange={event => setPath(event.target.value)} /></label>}
    <p className="help-text">只导入指令文本，不自动运行脚本、安装依赖或放宽工具权限。外部正文需要你审阅，不应包含密钥。</p>
    {localError && <p role="alert" className="task-error">{localError}</p>}
    <Button type="submit" size="small" disabled={disabled}>{skill ? "确认替换技能" : "保存技能"}</Button>
  </form>;
}
export function toggleCapability(value: CapabilitySelection, kind: keyof CapabilitySelection, id: string, enabled: boolean): CapabilitySelection {
  return { ...value, [kind]: enabled ? [...new Set([...value[kind], id])] : value[kind].filter(item => item !== id) };
}
export function CapabilitySelectionSettings({ registry = EMPTY_CAPABILITIES, value, onChange, disabled, executor = "native", scope = "global" }: {
  registry?: ManagedCapabilities; value: CapabilitySelection; onChange: (value: CapabilitySelection) => void; disabled: boolean;
  executor?: "native" | "codex"; scope?: "global" | "role" | "task";
}) {
  const codex = executor === "codex";
  return <section className="capability-selection" aria-label="任务能力范围"><h4>{scope === "global" ? "新任务允许选择的能力" : "额外收窄能力范围"}</h4>
    <p className="help-text">{scope === "global" ? "选中只进入能力允许列表，MCP 调用还受独立的托管工具权限和调用预算约束。" : "与全局及角色允许列表取交集，不能扩大范围；清空表示不使用这些能力。"} MCP 不是内置 read_file，不能用文件/命令权限推断其作用域；服务自述只读也不构成授权。每类最多选择 8 项。</p>
    {codex && <p className="execution-warning">应用管理的 MCP 暂仅支持 Native；Skills 可用于两种执行器的上下文。不会假称管控 Codex 自行加载的能力，切换执行器不会静默清除已有选择。</p>}
    {(["mcp_servers", "skills"] as const).map(kind => <fieldset key={kind} className="capability-selection-group"><legend>{kind === "mcp_servers" ? "MCP 服务范围" : "技能上下文"}</legend>
      {registry[kind].map(item => <label className="execution-checkbox" key={item.id}><input type="checkbox" checked={value[kind].includes(item.id)} disabled={disabled || (codex && kind === "mcp_servers" && !value[kind].includes(item.id)) || (!item.enabled && !value[kind].includes(item.id)) || (!value[kind].includes(item.id) && value[kind].length >= 8)} onChange={event => onChange(toggleCapability(value, kind, item.id, event.target.checked))} />{item.name}{!item.enabled && "（配置已禁用）"}</label>)}
      {value[kind].filter(id => !registry[kind].some(item => item.id === id)).map(id => <label className="execution-checkbox" key={id}><input type="checkbox" checked disabled={disabled} onChange={() => onChange(toggleCapability(value, kind, id, false))} />不可用的记录 <code>{id}</code>（可取消选择）</label>)}
      {!registry[kind].length && !value[kind].length && <p className="help-text">没有可选记录</p>}
    </fieldset>)}
  </section>;
}

export function CapabilityScopeSummary({ value, registry, label = "继承的能力范围" }: { value: CapabilitySelection; registry?: ManagedCapabilities; label?: string }) {
  return <div className="help-text" aria-label={label}><strong>{label}</strong>{(["mcp_servers","skills"] as const).map(kind => <p key={kind}>{kind === "mcp_servers" ? "MCP" : "Skills"}：{value[kind].length ? value[kind].map(id => registry?.[kind].find(item => item.id === id)?.name ?? id).join("、") : "未选择"}</p>)}<span>允许列表不证明实际调用或技能已进入上下文，以任务持久化记录为准。</span></div>;
}
