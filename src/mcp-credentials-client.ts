/** MCP-only vault enrollment. Public state never retains credential request bodies. */
import type { ManagedCapabilities, McpServerView } from "./capabilities-client";
import { providerUuid, type CredentialOperation } from "./providers-client";
export interface McpCredentialOperation extends CredentialOperation { owner_kind: "mcp"; slot: string }
export interface McpCredentialReceipt { operation: McpCredentialOperation; replayed: boolean; no_secret_applied: boolean }
export interface McpCredentialClient {
  setCredential(serverId: string, slot: string, body: { id: string; expected_revision: number; secret: string }, signal: AbortSignal): Promise<McpCredentialReceipt>;
  deleteCredential(serverId: string, slot: string, body: { id: string; expected_revision: number }, signal: AbortSignal): Promise<McpCredentialReceipt>;
  operation(id: string, signal: AbortSignal): Promise<McpCredentialOperation | null>;
  cleanupCredential(operationId: string, body: { id: string }, signal: AbortSignal): Promise<McpCredentialReceipt>;
}
const deniedNames = new Set(["PATH", "HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "TMPDIR", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PSMODULEPATH", "GLIBC_TUNABLES", "LOCPATH", "NLSPATH", "BASH_XTRACEFD", "PROMPT_COMMAND", "PWD", "OLDPWD", "CWD", "SHLVL", "IFS", "ENV", "BASH_ENV", "BASHOPTS", "SHELLOPTS", "ZDOTDIR", "FPATH", "CDPATH", "CLASSPATH", "JAVA_HOME", "JAVA_TOOL_OPTIONS", "JDK_JAVA_OPTIONS", "JAVA_OPTS", "KUBECONFIG", "DOCKER_CONFIG", "PHPRC", "PHP_INI_SCAN_DIR", "LUA_PATH", "LUA_CPATH", "TCL_LIBRARY", "TCLLIBPATH", "R_PROFILE", "R_PROFILE_USER", "R_ENVIRON", "R_ENVIRON_USER", "R_LIBS", "R_LIBS_USER", "GIO_EXTRA_MODULES", "GI_TYPELIB_PATH", "YARN_RC_FILENAME", "NODE_OPTIONS", "NODE_PATH", "NODE_EXTRA_CA_CERTS", "NODE_TLS_REJECT_UNAUTHORIZED", "SSLKEYLOGFILE", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "GCONV_PATH", "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "SSH_AUTH_SOCK", "SSH_AGENT_PID", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "GOPATH", "GOROOT", "GOFLAGS", "GOTOOLCHAIN", "RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE", "AWS_PROFILE", "AWS_DEFAULT_PROFILE", "AWS_WEB_IDENTITY_TOKEN_FILE", "GH_CONFIG_DIR"]);
const deniedPrefixes = ["LD_", "DYLD_", "PYTHON", "RUBY", "PERL", "DOTNET_", "COMPLUS_", "CORECLR_", "COR_", "MONO_", "GIT_", "CARGO_", "NPM_CONFIG_", "RUSTUP_", "OPENSSL_", "XDG_", "GTK_", "QT_", "DBUS_", "HARNESS_", "CODEX_", "AWS_ENDPOINT_URL", "AWS_CONTAINER_CREDENTIALS_", "AWS_EC2_METADATA_"];
const deniedSuffixes = ["PROXY", "_CA_BUNDLE", "_CERT_FILE", "_CERT_DIR", "_CONFIG_FILE", "_TOKEN_FILE", "_PATH"];
export function mcpSlotError(slot: string): string | null {
  if (!/^[A-Z][A-Z0-9_]{0,63}$/.test(slot)) return "凭据槽须为 1–64 位大写 ASCII 变量名，首位为字母。";
  if (deniedNames.has(slot) || deniedPrefixes.some(prefix => slot.startsWith(prefix)) || deniedSuffixes.some(suffix => slot.endsWith(suffix))) return "凭据槽不能覆盖启动、代码加载、代理、证书或宿主配置变量。";
  return null;
}
export function mcpSlotsError(slots: string[]): string | null {
  if (slots.length > 8) return "每个 MCP 服务最多声明 8 个凭据槽。";
  if (new Set(slots).size !== slots.length) return "凭据槽不能重复。";
  return slots.map(mcpSlotError).find(Boolean) ?? null;
}
export function mcpSecretError(secret: string): string | null {
  return !secret || new TextEncoder().encode(secret).length > 2048 || secret.includes("\0") ? "凭据须为 1–2048 个 UTF-8 字节，不能包含空字符；允许换行。" : null;
}
function object(value: unknown): value is Record<string, unknown> { return !!value && typeof value === "object" && !Array.isArray(value); }
export function validMcpOperation(value: unknown): value is McpCredentialOperation {
  return object(value) && ["id", "owner_id", "kind", "status", "created_at", "updated_at", "slot"].every(key => typeof value[key] === "string")
    && providerUuid(value.id as string) && providerUuid(value.owner_id as string) && value.owner_kind === "mcp" && /^[A-Z][A-Z0-9_]{0,63}$/.test(value.slot as string)
    && ["set", "delete", "cleanup"].includes(value.kind as string) && typeof value.can_cleanup === "boolean"
    && (value.reason === null || typeof value.reason === "string") && (value.cleanup_status === undefined || value.cleanup_status === null || typeof value.cleanup_status === "string");
}
const project = (op: McpCredentialOperation): McpCredentialOperation => ({ id: op.id, owner_kind: "mcp", owner_id: op.owner_id, kind: op.kind, slot: op.slot, status: op.status, reason: op.reason, can_cleanup: op.can_cleanup, cleanup_status: op.cleanup_status, created_at: op.created_at, updated_at: op.updated_at });
function newerOperation(candidate: McpCredentialOperation, current: McpCredentialOperation): boolean {
  if (candidate.updated_at !== current.updated_at) return candidate.updated_at > current.updated_at;
  const terminal = (value: McpCredentialOperation) => !["pending", "inflight"].includes(value.status);
  if (terminal(candidate) !== terminal(current)) return terminal(candidate);
  const cleanupRank = (value: McpCredentialOperation) => !value.cleanup_status ? 0 : value.cleanup_status === "pending" ? 1 : 2;
  return cleanupRank(candidate) >= cleanupRank(current);
}
const pending = (op: McpCredentialOperation | null) => !!op && (["pending", "inflight"].includes(op.status) || op.cleanup_status === "pending");
export interface McpCredentialState { busy: boolean; notice: string; uncertainId: string | null; operation: McpCredentialOperation | null }
export function createMcpCredentialController(client: McpCredentialClient, initial: ManagedCapabilities, changed: (state: McpCredentialState) => void, options: { uuid?: () => string; pollMs?: number; onRefresh?: () => void | Promise<void> } = {}) {
  let registry = initial, state: McpCredentialState = { busy: false, notice: "", uncertainId: null, operation: null };
  let active = true, generation = 0, flight: Promise<unknown> | undefined, read: AbortController | undefined, write: AbortController | undefined, timer: ReturnType<typeof setTimeout> | undefined;
  let target: { owner: string; slot: string } | undefined;
  const publish = (patch: Partial<McpCredentialState>) => { if (active) { state = { ...state, ...patch }; changed(state); } };
  const refresh = async () => { if (active) { try { await options.onRefresh?.(); } catch { /* A failed GET never reclassifies a confirmed write. */ } } };
  function schedule() { clearTimeout(timer); if (active && (state.uncertainId || pending(state.operation))) timer = setTimeout(() => void inspect(), options.pollMs ?? 1500); }
  function accept(op: unknown, id: string) {
    if (!validMcpOperation(op) || op.id !== id || op.owner_id !== target?.owner || op.slot !== target?.slot) throw new Error("invalid_mcp_operation");
    const known = registry.credential_operations?.find(item => item.id === id);
    const observed = known && validMcpOperation(known) && known.owner_id === target.owner && known.slot === target.slot && newerOperation(known, op) ? known : op;
    const previous = state.operation?.id === id ? state.operation : null;
    publish({ operation: project(previous && !newerOperation(observed, previous) ? previous : observed), uncertainId: null });
  }
  async function inspect() {
    if (!active || state.busy || flight) return;
    const id = state.uncertainId ?? state.operation?.id; if (!id) return;
    const version = ++generation; read?.abort(); read = new AbortController(); publish({ busy: true });
    try {
      const op = await client.operation(id, read.signal); if (!active || version !== generation) return;
      if (op) { accept(op, id); publish({ notice: "已只读查询原 MCP 凭据操作；没有重新提交密码、删除或清理请求。" }); }
      else publish({ uncertainId: id, notice: "尚未找到原操作记录，结果仍未知；不代表没有写入，不会自动重发。" });
      await refresh();
    } catch { if (active && version === generation) publish({ uncertainId: id, notice: "原操作暂不可读或不匹配，结果仍未确认；不会回显底层错误或重发。" }); }
    finally { if (active && version === generation) { publish({ busy: false }); schedule(); } }
  }
  function mutate(owner: string, slot: string, send: (id: string, signal: AbortSignal) => Promise<McpCredentialReceipt>): Promise<unknown> {
    if (!active || state.busy || flight) return Promise.resolve(undefined);
    if (state.uncertainId || pending(state.operation)) { publish({ notice: "已有凭据操作尚未确认，请只读检查原记录；没有发送新操作。" }); return Promise.resolve(undefined); }
    const id = (options.uuid ?? (() => crypto.randomUUID()))(); target = { owner, slot }; ++generation; read?.abort(); clearTimeout(timer); write = new AbortController(); const signal = write.signal;
    publish({ busy: true, notice: "", uncertainId: id, operation: null });
    flight = (async () => {
      try {
        const receipt = await Promise.resolve().then(() => active ? send(id, signal) : undefined); if (!active || !receipt) return;
        if (typeof receipt.replayed !== "boolean" || typeof receipt.no_secret_applied !== "boolean") throw new Error("invalid_receipt");
        accept(receipt.operation, id);
        publish({ notice: receipt.replayed || receipt.no_secret_applied ? "返回的是已有操作记录；这次输入没有写入，不能视为新凭据已保存。" : "MCP 凭据操作已登记，请核对持久状态；绑定不证明系统存储可读、工具目录就绪或实际工具调用。" });
        await refresh(); return receipt;
      } catch { publish({ uncertainId: id, notice: "提交结果未知；输入已清空。只查询原操作，不自动重发；停止观察不代表后台撤销。" }); }
      finally { flight = undefined; publish({ busy: false }); schedule(); }
    })(); return flight;
  }
  function validTarget(server: McpServerView, slot: string) {
    const current = registry.mcp_servers.find(item => item.id === server.id);
    return providerUuid(server.id) && Number.isSafeInteger(server.revision) && server.revision > 0 && server.revision < Number.MAX_SAFE_INTEGER
      && current?.revision === server.revision && current.credential_slots.includes(slot) && !mcpSlotError(slot)
      && !(registry.credential_operations ?? []).some(op => op.owner_id === server.id && pending(op));
  }
  return { getState: () => state, inspect,
    replaceRegistry(next: ManagedCapabilities) {
      if (!active) return; registry = next;
      if (flight) return;
      // A newer snapshot supersedes an outstanding GET. It cannot cancel a write.
      if (state.busy) { ++generation; read?.abort(); publish({ busy: false }); }
      const id = state.uncertainId ?? state.operation?.id;
      const known = id ? next.credential_operations?.find(op => op.id === id) : next.credential_operations?.find(pending);
      if (known && validMcpOperation(known)) { if (!id) target = { owner: known.owner_id, slot: known.slot }; try { accept(known, known.id); } catch { /* Do not replace a mismatching owner/slot. */ } }
      schedule();
    },
    setCredential(server: McpServerView, slot: string, secret: string, confirmed: boolean) {
      const error = mcpSecretError(secret);
      if (!confirmed || error || !validTarget(server, slot)) { secret = ""; publish({ notice: error ?? "请刷新服务修订、核对声明槽并明确确认写入；已有操作未完成时不能覆盖。" }); return Promise.resolve(undefined); }
      return mutate(server.id, slot, async (id, signal) => { const body = { id, expected_revision: server.revision, secret }; secret = ""; try { return await client.setCredential(server.id, slot, body, signal); } finally { body.secret = ""; } });
    },
    deleteCredential(server: McpServerView, slot: string, confirmed: boolean) {
      if (!confirmed || !validTarget(server, slot)) { publish({ notice: "请刷新修订并明确确认解绑和删除该声明槽；已有操作未完成时不能覆盖。" }); return Promise.resolve(undefined); }
      return mutate(server.id, slot, (id, signal) => client.deleteCredential(server.id, slot, { id, expected_revision: server.revision }, signal));
    },
    cleanupCredential(op: McpCredentialOperation, confirmed: boolean) {
      const current = registry.credential_operations?.find(item => item.id === op.id);
      if (!confirmed || !validMcpOperation(op) || !op.can_cleanup || !current?.can_cleanup || current.owner_id !== op.owner_id || current.slot !== op.slot) { publish({ notice: "此 MCP 操作不可清理，或未明确确认；请刷新后核对。" }); return Promise.resolve(undefined); }
      return mutate(op.owner_id, op.slot, (id, signal) => client.cleanupCredential(op.id, { id }, signal));
    },
    dispose() { active = false; ++generation; read?.abort(); write?.abort(); clearTimeout(timer); },
  };
}
