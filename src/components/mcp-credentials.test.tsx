import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { createMcpCredentialController, mcpSecretError, mcpSlotError, mcpSlotsError, validMcpOperation, type McpCredentialClient, type McpCredentialOperation, type McpCredentialReceipt, type McpCredentialState } from "../mcp-credentials-client";
import { capabilityCommandError, validCapabilityRegistry, type ManagedCapabilities, type McpServerView } from "../capabilities-client";
import { consumeCredentialInput } from "../providers-client";
import { McpCredentialOperationCard, McpCredentials, McpSlotCredentialForm } from "./mcp-credentials";
import { CapabilitiesSettings, McpServerEditor } from "./capabilities-settings";
const serverId = "10000000-0000-4000-8000-000000000001", operationId = "20000000-0000-4000-8000-000000000001";
const server: McpServerView = { id: serverId, revision: 2, name: "本地服务", enabled: true, program: "/tools/mcp", args: [], protocol: "modern2026", credential_slots: ["API_TOKEN"], credential_states: [{ slot: "API_TOKEN", configured: true, status: "bound_not_checked" }], status: "configured", reason: null, catalog_digest: null, tool_count: 0, updated_at: "now" };
const operation = (patch: Partial<McpCredentialOperation> = {}): McpCredentialOperation => ({ id: operationId, owner_kind: "mcp", owner_id: serverId, slot: "API_TOKEN", kind: "set", status: "completed", reason: null, can_cleanup: false, created_at: "now", updated_at: "now", ...patch });
const registry: ManagedCapabilities = { mcp_servers: [server], skills: [], credential_operations: [] };
const receipt = (op = operation()): McpCredentialReceipt => ({ operation: op, replayed: false, no_secret_applied: false });
function fixture(initial = registry) {
  const client: McpCredentialClient = { setCredential: vi.fn().mockResolvedValue(receipt()), deleteCredential: vi.fn().mockResolvedValue(receipt(operation({ kind: "delete" }))), operation: vi.fn().mockResolvedValue(operation()), cleanupCredential: vi.fn().mockResolvedValue(receipt(operation({ kind: "cleanup" }))) };
  const states: McpCredentialState[] = [], onRefresh = vi.fn();
  const controller = createMcpCredentialController(client, initial, next => states.push(next), { uuid: () => operationId, pollMs: 20, onRefresh });
  return { client, states, controller, onRefresh };
}
afterEach(() => vi.useRealTimers());
describe("MCP 槽声明和公开数据边界", () => {
  it("与后台完整危险变量表保持一致，限制名称、数量和重复", () => {
    expect(mcpSlotError("SERVICE_API_TOKEN")).toBeNull(); expect(mcpSlotError("A".repeat(64))).toBeNull();
    for (const name of ["", "_TOKEN", "api_token", "A".repeat(65), "TOKEN=abc", "AUTH/TOKEN", "中文", "A\nB", "AWS_EC2_METADATA_DISABLED"]) expect(mcpSlotError(name)).not.toBeNull();
    const backend = readFileSync("crates/harness-core/src/capabilities.rs", "utf8");
    for (const [section, transform] of [["exact", (x: string) => x], ["prefixes", (x: string) => `${x}TOKEN`], ["suffixes", (x: string) => `APP${x}`]] as const) {
      const values = backend.match(new RegExp(`let ${section} = \\[([\\s\\S]*?)\\];`))![1].matchAll(/"([A-Z0-9_]+)"/g);
      for (const value of values) expect(mcpSlotError(transform(value[1])), value[1]).not.toBeNull();
    }
    expect(mcpSlotsError(Array.from({ length: 8 }, (_, i) => `KEY_${i}`))).toBeNull();
    expect(mcpSlotsError(Array.from({ length: 9 }, (_, i) => `KEY_${i}`))).toContain("8"); expect(mcpSlotsError(["KEY", "KEY"])).toContain("重复");
    const definition = { name: "服务", program: "/tools/mcp", args: [], protocol: "modern2026" as const, enabled: false, credential_refs: {} };
    expect(capabilityCommandError({ type: "save_mcp_server", definition: { ...definition, credential_slots: ["TOKEN"] } })).toBeNull();
    expect(capabilityCommandError({ type: "save_mcp_server", definition: { ...definition, credential_slots: ["PATH"] } })).toContain("启动");
    expect(capabilityCommandError({ type: "save_mcp_server", definition: { ...definition, credential_refs: { TOKEN: "opaque" } } })).toContain("不支持凭据引用");
  });
  it("凭据允许UTF8与换行，2048字节边界，无空值或NUL", () => {
    expect(mcpSecretError('中文\n"\\')).toBeNull(); expect(mcpSecretError("中".repeat(682) + "ab")).toBeNull();
    for (const secret of ["", "中".repeat(682) + "abc", "token\0other"]) expect(mcpSecretError(secret)).not.toBeNull();
    const input = { value: "SECRET_FIXTURE" }; expect(consumeCredentialInput(input)).toBe("SECRET_FIXTURE"); expect(input.value).toBe("");
  });
  it("注册表兼容旧字段缺省，操作只接受MCP，数量1280封顶", () => {
    expect(validCapabilityRegistry({ mcp_servers: [{ ...server, credential_states: undefined }], skills: [] })).toBe(true);
    for (const count of [1025, 1280]) expect(validCapabilityRegistry({ ...registry, credential_operations: Array(count).fill(operation()) })).toBe(true);
    expect(validCapabilityRegistry({ ...registry, credential_operations: Array(1281).fill(operation()) })).toBe(false);
    expect(validCapabilityRegistry({ ...registry, credential_operations: [{ ...operation(), owner_kind: "provider" }] })).toBe(false);
    expect(validCapabilityRegistry({ ...registry, mcp_servers: [{ ...server, credential_states: [{ slot: "API_TOKEN", configured: "yes", status: "ready" }] }] })).toBe(false);
    expect(validMcpOperation(operation({ cleanup_status: "completed" }))).toBe(true);
    // Historical metadata remains readable for cleanup if a newer version tightens slot policy.
    expect(validMcpOperation(operation({ slot: "JAVA_OPTS", can_cleanup: true }))).toBe(true);
    expect(validMcpOperation({ ...operation(), cleanup_status: 42 })).toBe(false);
  });
});
describe("MCP 凭据一次性提交和只读恢复", () => {
  it("明确确认才提交；双击只有一次，修订与槽固定，状态不含密钥", async () => {
    const { controller, client, states, onRefresh } = fixture();
    await controller.setCredential(server, "API_TOKEN", "fixture", false); await controller.setCredential(server, "UNDECLARED", "fixture", true);
    await controller.setCredential({ ...server, revision: 1 }, "API_TOKEN", "fixture", true); expect(client.setCredential).not.toHaveBeenCalled();
    let captured: unknown;
    vi.mocked(client.setCredential).mockImplementation(async (id, slot, body) => { captured = { id, slot, body: { ...body } }; return receipt(); });
    const first = controller.setCredential(server, "API_TOKEN", "SECRET_FIXTURE\n中文", true);
    await controller.setCredential(server, "API_TOKEN", "SECOND_SECRET", true); await first;
    expect(client.setCredential).toHaveBeenCalledTimes(1); expect(captured).toEqual({ id: serverId, slot: "API_TOKEN", body: { id: operationId, expected_revision: 2, secret: "SECRET_FIXTURE\n中文" } });
    expect(vi.mocked(client.setCredential).mock.calls[0][2].secret).toBe("");
    expect(JSON.stringify(states)).not.toContain("SECRET"); expect(onRefresh).toHaveBeenCalledTimes(1); controller.dispose();
  });
  it("未知POST只GET恢复，找不到/404不重发，保留未知ID直到同owner同slot记录", async () => {
    vi.useFakeTimers(); const { controller, client } = fixture();
    vi.mocked(client.setCredential).mockRejectedValue(new Error("SECRET_IN_ERROR"));
    vi.mocked(client.operation).mockResolvedValueOnce(null).mockRejectedValueOnce(new Error("404"));
    await controller.setCredential(server, "API_TOKEN", "fixture", true);
    expect(controller.getState().uncertainId).toBe(operationId); expect(controller.getState().notice).not.toContain("SECRET");
    await controller.setCredential(server, "API_TOKEN", "fixture2", true); expect(client.setCredential).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(20); expect(controller.getState().uncertainId).toBe(operationId);
    await vi.advanceTimersByTimeAsync(20); expect(controller.getState().uncertainId).toBe(operationId);
    await vi.advanceTimersByTimeAsync(20); expect(controller.getState().uncertainId).toBeNull(); expect(controller.getState().operation?.status).toBe("completed");
    expect(client.setCredential).toHaveBeenCalledTimes(1); controller.dispose();
  });
  it("错owner/槽/ID或坏回执永不冒充成功，额外私密字段不进入状态", async () => {
    for (const patch of [{ owner_kind: "qq" }, { owner_id: "10000000-0000-4000-8000-000000000002" }, { slot: "OTHER" }, { id: "20000000-0000-4000-8000-000000000002" }]) {
      const { controller, client } = fixture(); vi.mocked(client.setCredential).mockResolvedValue({ ...receipt(), operation: { ...operation(), ...patch } } as McpCredentialReceipt);
      await controller.setCredential(server, "API_TOKEN", "fixture", true); expect(controller.getState().uncertainId).toBe(operationId); expect(controller.getState().operation).toBeNull(); controller.dispose();
    }
    const { controller, client, states } = fixture(); vi.mocked(client.setCredential).mockResolvedValue(receipt({ ...operation(), private_reference: "SECRET_FIXTURE" } as McpCredentialOperation));
    await controller.setCredential(server, "API_TOKEN", "fixture", true); expect(JSON.stringify(states)).not.toContain("SECRET_FIXTURE"); controller.dispose();
  });
  it("重放回执明确这次输入没有写入；刷新失败不把已确认结果改为未知", async () => {
    const { controller, client, onRefresh } = fixture(); vi.mocked(client.setCredential).mockResolvedValue({ ...receipt(), replayed: true, no_secret_applied: true }); onRefresh.mockRejectedValue(new Error("read failed"));
    await controller.setCredential(server, "API_TOKEN", "fixture", true);
    expect(controller.getState().notice).toContain("这次输入没有写入"); expect(controller.getState().uncertainId).toBeNull(); controller.dispose();
  });
  it("删除和清理也需要独立确认，已删除服务的孤立记录仍可清理", async () => {
    const cleanup = operation({ id: "30000000-0000-4000-8000-000000000001", status: "unknown", can_cleanup: true });
    const { controller, client } = fixture({ ...registry, credential_operations: [cleanup] });
    await controller.deleteCredential(server, "API_TOKEN", false); expect(client.deleteCredential).not.toHaveBeenCalled();
    await controller.deleteCredential(server, "API_TOKEN", true); expect(client.deleteCredential).toHaveBeenCalledWith(serverId, "API_TOKEN", { id: operationId, expected_revision: 2 }, expect.any(AbortSignal));
    controller.replaceRegistry({ mcp_servers: [], skills: [], credential_operations: [cleanup] });
    await controller.cleanupCredential(cleanup, false); expect(client.cleanupCredential).not.toHaveBeenCalled();
    await controller.cleanupCredential(cleanup, true); expect(client.cleanupCredential).toHaveBeenCalledWith(cleanup.id, { id: operationId }, expect.any(AbortSignal));
    controller.dispose();
  });
  it("重开观察pending只轮询；较新修订使旧表单不可写", async () => {
    vi.useFakeTimers(); const pending = operation({ status: "pending" });
    const { controller, client } = fixture({ ...registry, credential_operations: [pending] });
    controller.replaceRegistry({ ...registry, credential_operations: [pending] }); await vi.advanceTimersByTimeAsync(20);
    expect(client.operation).toHaveBeenCalledTimes(1); expect(client.setCredential).not.toHaveBeenCalled();
    controller.replaceRegistry({ ...registry, mcp_servers: [{ ...server, revision: 3 }] });
    await controller.setCredential(server, "API_TOKEN", "fixture", true); expect(client.setCredential).not.toHaveBeenCalled(); controller.dispose();
  });
  it("较新快照覆盖旧GET，较旧pending快照不覆盖已完成记录", async () => {
    const pending = operation({ status: "pending", updated_at: "2026-10-28T01:00:00Z" });
    const completed = operation({ updated_at: "2026-10-28T01:01:00Z" });
    const { controller, client } = fixture({ ...registry, credential_operations: [pending] });
    controller.replaceRegistry({ ...registry, credential_operations: [pending] });
    let finish!: (value: McpCredentialOperation) => void;
    vi.mocked(client.operation).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    const request = controller.inspect();
    controller.replaceRegistry({ ...registry, credential_operations: [completed] });
    expect(vi.mocked(client.operation).mock.calls[0][1].aborted).toBe(true);
    finish(pending); await request; expect(controller.getState().operation?.status).toBe("completed");
    controller.replaceRegistry({ ...registry, credential_operations: [pending] });
    expect(controller.getState().operation?.status).toBe("completed"); controller.dispose();
  });
  it("关闭观察中止信号，延迟回执不更新新页面；不会宣称撤销后台操作", async () => {
    vi.useFakeTimers(); const { controller, client, states, onRefresh } = fixture(); let finish!: (receipt: McpCredentialReceipt) => void;
    vi.mocked(client.setCredential).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    const request = controller.setCredential(server, "API_TOKEN", "fixture", true); await Promise.resolve();
    controller.dispose(); const count = states.length; expect(vi.mocked(client.setCredential).mock.calls[0][3].aborted).toBe(true);
    finish(receipt()); await request; await vi.advanceTimersByTimeAsync(200); expect(states).toHaveLength(count); expect(onRefresh).not.toHaveBeenCalled(); expect(client.operation).not.toHaveBeenCalled();
  });
});
describe("MCP 凭据界面", () => {
  it("专用密码槽默认空且须确认，声明表单不接受值或引用", () => {
    const html = renderToStaticMarkup(<McpSlotCredentialForm server={server} slot="API_TOKEN" disabled={false} onSet={vi.fn()} onDelete={vi.fn()} />);
    expect(html).toContain('type="password"'); expect(html).not.toContain('type="password" value='); expect(html).toContain("未验证系统存储可读");
    expect(html).toContain("多行明文编辑"); expect(html).toMatch(/<button[^>]*disabled[^>]*>提交 MCP 槽凭据/); expect(html).toMatch(/<button[^>]*disabled[^>]*>解绑并删除此槽凭据/);
    const editor = renderToStaticMarkup(<McpServerEditor server={server} disabled={false} onSubmit={vi.fn()} />);
    expect(editor).toContain('aria-label="MCP 声明凭据槽"'); expect(editor).toContain("API_TOKEN</textarea>"); expect(editor).toContain("解除所有旧凭据绑定"); expect(editor).not.toContain('type="password"');
  });
  it("未知原操作独立显示清理完成，清理不可用不提供按钮", () => {
    const html = renderToStaticMarkup(<McpCredentialOperationCard operation={operation({ status: "unknown", cleanup_status: "completed", reason: "SECRET_ERROR" })} disabled={false} onCleanup={vi.fn()} />);
    expect(html).toContain("清理已确认完成；原操作结果仍未知。"); expect(html).not.toContain("SECRET_ERROR"); expect(html).not.toContain("清理此 MCP 凭据</button>");
  });
  it("接线可选，无implicit mutation，含孤立项、重发现、未沙箱化风险", () => {
    const { client } = fixture();
    const html = renderToStaticMarkup(<McpCredentials registry={registry} client={client} />);
    expect(html).toContain("未沙箱化进程"); expect(html).toContain("重新明确发现工具"); expect(html).toContain("包含已删除服务"); expect(client.setCredential).not.toHaveBeenCalled();
    const full = renderToStaticMarkup(<CapabilitiesSettings registry={registry} credentialClient={client} client={{ getRegistry: vi.fn(), command: vi.fn() }} disabled={false} />);
    expect(full).toContain('aria-label="MCP 系统凭据槽"');
  });
});
