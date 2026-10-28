/** Public capability observations only: never credential values or imported skill bodies. */
export type McpProtocol = "modern2026" | "legacy2025";
export interface McpServerView {
  id: string; revision: number; name: string; enabled: boolean; program: string; args: string[];
  protocol: McpProtocol; credential_slots: string[]; status: string; reason: string | null;
  catalog_digest: string | null; tool_count: number; updated_at: string;
}
export interface SkillView {
  id: string; revision: number; name: string; enabled: boolean; source: string;
  content_digest: string; content_bytes: number; updated_at: string;
}
export interface ManagedCapabilities { mcp_servers: McpServerView[]; skills: SkillView[] }
export interface CapabilitySelection { mcp_servers: string[]; skills: string[] }
export interface McpDefinition { name: string; program: string; args: string[]; protocol: McpProtocol; enabled: boolean; credential_refs: Record<string, string> }
export type CapabilityCommand =
  | { type: "save_mcp_server"; id?: string; expected_revision?: number; definition: McpDefinition }
  | { type: "set_capability_enabled"; kind: "mcp" | "skill"; id: string; expected_revision: number; enabled: boolean }
  | { type: "probe_mcp_server"; id: string; expected_revision: number; confirm_start: true }
  | { type: "save_skill"; id?: string; expected_revision?: number; name: string; content: string }
  | { type: "import_skill"; id?: string; expected_revision?: number; name: string; source_path: string }
  | { type: "delete_capability"; kind: "mcp" | "skill"; id: string; expected_revision: number };
export interface CapabilityEnvelope { id: string; command: CapabilityCommand }
export interface CapabilityReceipt { command_id: string; status: "accepted" | "checking" | "completed"; capability_id?: string; revision?: number }
export interface CapabilityClient {
  getRegistry(signal: AbortSignal): Promise<ManagedCapabilities>;
  command(envelope: CapabilityEnvelope, signal: AbortSignal): Promise<CapabilityReceipt>;
}
export const EMPTY_CAPABILITIES: ManagedCapabilities = { mcp_servers: [], skills: [] };
const bytes = (value: string) => new TextEncoder().encode(value).length;
export const absoluteCapabilityPath = (path: string) => /^(?:\/|[A-Za-z]:[\\/]|\\\\[^\\]+\\[^\\]+)/.test(path) && !path.includes("\0");
export function capabilityCommandError(command: CapabilityCommand): string | null {
  if (bytes(JSON.stringify({id:"0".repeat(36),command})) > 262144) return "管理请求最多 256 KiB，请缩短参数或技能正文。";
  if ("id" in command && command.id !== undefined && (!command.id || !Number.isSafeInteger(command.expected_revision) || command.expected_revision! < 0)) return "记录修订无效，请刷新后重新编辑。";
  if (command.type === "save_mcp_server") {
    const value = command.definition;
    if (!value.name.trim() || Array.from(value.name).length > 80 || value.name.includes("\0")) return "服务名称须为 1–80 个字符。";
    if (!absoluteCapabilityPath(value.program) || bytes(value.program) > 4096) return "程序须为有效绝对路径（最多 4096 个 UTF-8 字节）。";
    if (value.args.length > 64 || value.args.some(arg => bytes(arg) > 8192 || arg.includes("\0")) || value.args.reduce((total, arg) => total + bytes(arg), 0) > 65536) return "参数最多 64 项，每项最多 8192 个 UTF-8 字节，合计最多 65536 字节，不能包含空字符。";
    if (value.protocol !== "modern2026" && value.protocol !== "legacy2025") return "请显式选择支持的 MCP 协议版本。";
    if (Object.keys(value.credential_refs).length) return "当前表单不支持凭据配置，请勿在参数里填写密钥。";
  }
  if (command.type === "save_skill" || command.type === "import_skill") {
    if (!command.name.trim() || Array.from(command.name).length > 80 || command.name.includes("\0")) return "技能名称须为 1–80 个字符。";
    if (command.type === "save_skill" && (!command.content.trim() || bytes(command.content) > 65536 || command.content.includes("\0"))) return "技能正文须非空，最多 65536 个 UTF-8 字节，不能包含空字符。";
    if (command.type === "import_skill" && (!absoluteCapabilityPath(command.source_path) || !/(?:^|[\\/])SKILL\.md$/.test(command.source_path) || bytes(command.source_path) > 4096)) return "导入路径须是 SKILL.md 的绝对路径。";
  }
  return null;
}
const ERROR_MESSAGES: Record<string, string> = {
  conflict: "记录已被其他入口修改，请刷新后重新核对；原操作没有自动重发。",
  invalid: "配置不符合后台约束，请检查字段、绝对路径和修订后重试。",
  unavailable: "服务或当前平台不可用，请检查本机配置；没有自动启动或重发。",
  stale_revision: "记录已被其他入口修改，请刷新后重新核对；原操作没有自动重发。",
  revision_conflict: "记录已被其他入口修改，请刷新后重新核对；原操作没有自动重发。",
  invalid_path: "路径无效或不可访问，请核对后台所在机器的绝对路径。",
  unsupported_platform: "当前平台不支持此 stdio 传输，后台没有启动服务。",
  windows_unsupported: "Windows 暂不支持此 stdio 传输，后台没有启动服务。",
  server_unavailable: "服务当前不可用，请检查配置后显式重新验证。",
  invalid_command: "配置不符合后台约束，请检查字段并刷新记录。",
  not_found: "记录不存在或已经删除，请刷新。",
};
export function capabilityErrorMessage(error: unknown): string {
  // Never reflect transport/child-process errors, which may contain arguments or secrets.
  const code = typeof error === "object" && error !== null && "code" in error ? error.code : error instanceof Error ? error.message : null;
  return typeof code === "string" && ERROR_MESSAGES[code] ? ERROR_MESSAGES[code] : "请求未能确认结果，请先刷新查看；不会自动重发管理命令。";
}
export function capabilityReason(reason: string | null): string {
  return reason && ERROR_MESSAGES[reason] ? ERROR_MESSAGES[reason] : reason ? "后台报告服务不可用；详情已隐藏，请核对本机配置后重新验证。" : "";
}
function record(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null && !Array.isArray(value); }
export function validCapabilityRegistry(value: unknown): value is ManagedCapabilities {
  if (!record(value) || !Array.isArray(value.mcp_servers) || !Array.isArray(value.skills) || value.mcp_servers.length > 32 || value.skills.length > 64) return false;
  const base = (item: unknown) => record(item) && typeof item.id === "string" && typeof item.name === "string" && Number.isSafeInteger(item.revision) && (item.revision as number) >= 0 && typeof item.enabled === "boolean" && typeof item.updated_at === "string";
  return value.mcp_servers.every(item => base(item) && typeof item.program === "string" && Array.isArray(item.args) && item.args.every((arg: unknown) => typeof arg === "string") && ["modern2026", "legacy2025"].includes(item.protocol) && Array.isArray(item.credential_slots) && item.credential_slots.every((slot: unknown) => typeof slot === "string") && typeof item.status === "string" && (item.reason === null || typeof item.reason === "string") && (item.catalog_digest === null || typeof item.catalog_digest === "string") && Number.isSafeInteger(item.tool_count) && item.tool_count >= 0)
    && value.skills.every(item => base(item) && typeof item.source === "string" && typeof item.content_digest === "string" && Number.isSafeInteger(item.content_bytes) && item.content_bytes >= 0);
}
export interface CapabilityState { registry: ManagedCapabilities; busy: boolean; loading: boolean; error: string; notice: string }
/** One mutation at a time, GET-only discovery polling, abortable observer lifecycle. */
export function createCapabilityController(client: CapabilityClient, initial: ManagedCapabilities, onChange: (state: CapabilityState) => void, options: { uuid?: () => string; pollMs?: number } = {}) {
  let state: CapabilityState = { registry: initial, busy: false, loading: false, error: "", notice: "" };
  let active = true, timer: ReturnType<typeof setTimeout> | undefined, read: AbortController | undefined, write: AbortController | undefined;
  let generation = 0;
  let last: { fingerprint: string; envelope: CapabilityEnvelope; receipt?: CapabilityReceipt } | undefined;
  let submitting: Promise<CapabilityReceipt | undefined> | undefined;
  const publish = (patch: Partial<CapabilityState>) => { if (active) { state = { ...state, ...patch }; onChange(state); } };
  const poll = () => {
    clearTimeout(timer);
    if (active && state.registry.mcp_servers.some(server => server.status === "checking")) timer = setTimeout(() => { void refresh(); }, options.pollMs ?? 1500);
  };
  async function refresh() {
    if (!active) return;
    const current = ++generation; read?.abort(); read = new AbortController();
    publish({ loading: true });
    try {
      const registry = await client.getRegistry(read.signal);
      if (!active || current !== generation) return;
      if (!validCapabilityRegistry(registry)) throw new Error("malformed_response");
      publish({ registry, error: "" });
    } catch (error) { if (active && current === generation) publish({ error: capabilityErrorMessage(error) }); }
    finally { if (active && current === generation) { publish({ loading: false }); poll(); } }
  }
  function submit(command: CapabilityCommand): Promise<CapabilityReceipt | undefined> {
    if (!active) return Promise.resolve(undefined);
    if (submitting) {
      if (last?.fingerprint === JSON.stringify(command)) return submitting;
      publish({error:"另一项管理操作尚未确认，请等待结果后再提交；没有发送新操作。"});
      return Promise.resolve(undefined);
    }
    const validation = capabilityCommandError(command);
    if (validation) { publish({ error: validation }); return Promise.resolve(undefined); }
    const fingerprint = JSON.stringify(command);
    if (last?.fingerprint === fingerprint && last.receipt) return Promise.resolve(last.receipt);
    if (last?.fingerprint !== fingerprint) last = { fingerprint, envelope: { id: (options.uuid ?? (() => crypto.randomUUID()))(), command: structuredClone(command) } };
    const request = last!; write = new AbortController(); const writeSignal = write.signal;
    publish({ busy: true, error: "", notice: "" });
    submitting = (async () => {
      try {
        const receipt = await Promise.resolve().then(() => client.command(request.envelope, writeSignal));
        if (!active) return undefined;
        if (!record(receipt) || receipt.command_id !== request.envelope.id || !["accepted", "checking", "completed"].includes(receipt.status)) throw new Error("malformed_response");
        request.receipt = receipt;
        publish({ notice: receipt.status === "checking" ? "发现请求已登记，正在获取工具目录；这不是工具调用验证。" : "管理操作已确认；任务是否实际使用能力仍以执行记录为准。" });
        await refresh();
        return receipt;
      } catch (error) { publish({ error: capabilityErrorMessage(error) }); return undefined; }
      finally { submitting = undefined; publish({ busy: false }); }
    })();
    return submitting;
  }
  return { refresh, submit, getState: () => state,
    replaceRegistry(registry: ManagedCapabilities) { if (active) { ++generation; read?.abort(); publish({ registry, loading: false }); poll(); } },
    dispose() { active = false; ++generation; clearTimeout(timer); read?.abort(); write?.abort(); },
  };
}
