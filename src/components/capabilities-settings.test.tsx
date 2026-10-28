import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CapabilitiesSettings, CapabilitySelectionSettings, McpServerCard, SkillCard, SkillEditor, toggleCapability } from "./capabilities-settings";
import {
  capabilityCommandError, capabilityErrorMessage, createCapabilityController, validCapabilityRegistry,
  type CapabilityClient, type CapabilityCommand, type CapabilityEnvelope, type CapabilityState, type ManagedCapabilities,
} from "../capabilities-client";
const registry: ManagedCapabilities = {
  mcp_servers: [{ id: "m1", revision: 2, name: "本机服务<script>", program: "/tools/server", args: ["--stdio"], protocol: "modern2026", enabled: true, credential_slots: [], status: "configured", reason: null, catalog_digest: null, tool_count: 0, updated_at: "now" }],
  skills: [{ id: "s1", revision: 3, name: "编码约定", enabled: true, source: "/repo/SKILL.md", content_digest: "a".repeat(64), content_bytes: 90, updated_at: "now" }],
};
const save: CapabilityCommand = { type: "save_mcp_server", definition: { name: "本机", program: "/tools/server", args: ["--stdio"], protocol: "modern2026", enabled: false, credential_refs: {} } };
const probe: CapabilityCommand = { type: "probe_mcp_server", id: "m1", expected_revision: 2, confirm_start: true };
const receipt = (envelope: CapabilityEnvelope) => ({ command_id: envelope.id, status: "completed" as const, capability_id: "m1", revision: 3 });
function fixture() {
  const client: CapabilityClient = { getRegistry: vi.fn().mockResolvedValue(registry), command: vi.fn().mockImplementation(async (envelope: CapabilityEnvelope) => receipt(envelope)) };
  const states: CapabilityState[] = [];
  const controller = createCapabilityController(client, registry, state => states.push(state), { uuid: () => "command-id", pollMs: 50 });
  return { client, states, controller };
}
afterEach(() => { vi.useRealTimers(); });
describe("能力表单与安全边界", () => {
  it("明确区分配置、发现和调用，无隐式启动或凭据表单", () => {
    const { client } = fixture();
    const html = renderToStaticMarkup(<CapabilitiesSettings registry={registry} client={client} disabled={false} />);
    expect(html).toContain("已配置 · 尚未发现"); expect(html).toContain("可能写文件、联网"); expect(html).toContain("保存配置（不启动）");
    expect(html).toContain("Modern · 2026-07-28"); expect(html).toContain("是否可用以真实发现状态为准");
    expect(html).not.toContain('type="password"'); expect(html).not.toContain('name="token"'); expect(html).not.toContain("<script>"); expect(html).toContain("&lt;script&gt;");
    expect(client.command).not.toHaveBeenCalled(); expect(client.getRegistry).not.toHaveBeenCalled();
    expect(html).toMatch(/<button[^>]*disabled[^>]*>确认启动并发现工具<\/button>/);
    expect(html).toMatch(/<button[^>]*disabled[^>]*>删除配置<\/button>/);
  });
  it("发现就绪也不声称真实工具使用；发现中和已绑定凭据配置不能重复启动/覆盖", () => {
    const onSubmit = vi.fn();
    const ready = renderToStaticMarkup(<McpServerCard server={{ ...registry.mcp_servers[0], status: "ready", tool_count: 2, catalog_digest: "b".repeat(64) }} disabled={false} onSubmit={onSubmit} />);
    expect(ready).toContain("工具目录就绪 · 未证明实际调用"); expect(ready).toContain("这不证明任一工具已被任务调用");
    const checking = renderToStaticMarkup(<McpServerCard server={{ ...registry.mcp_servers[0], status: "checking", credential_slots: ["AUTH_TOKEN"] }} disabled={false} onSubmit={onSubmit} />);
    expect(checking).toContain("当前表单不支持编辑"); expect(checking).toContain("正在发现工具目录…"); expect(checking).not.toContain('aria-label="MCP 程序绝对路径"');
  });
  it("技能展示来源与摘要，替换无正文必须由用户补齐，不自动运行脚本", () => {
    const html = renderToStaticMarkup(<SkillCard skill={registry.skills[0]} disabled={false} onSubmit={vi.fn()} />);
    expect(html).toContain("/repo/SKILL.md"); expect(html).toContain("a".repeat(64)); expect(html).toContain("不会用空白表单覆盖已有技能"); expect(html).toContain("不自动运行脚本");
    const blank = renderToStaticMarkup(<SkillEditor skill={registry.skills[0]} disabled={false} onSubmit={vi.fn()} />);
    expect(blank).toMatch(/<textarea[^>]*required[^>]*><\/textarea>/);
  });
  it("任务选择独立于启用，禁用/丢失选项可移除且不能扩大 Codex 能力", () => {
    const value = { mcp_servers: ["missing", "m1"], skills: [] };
    const html = renderToStaticMarkup(<CapabilitySelectionSettings registry={{ ...registry, mcp_servers: [{ ...registry.mcp_servers[0], enabled: false }] }} value={value} onChange={vi.fn()} disabled={false} executor="codex" scope="task" />);
    expect(html).toContain("取交集"); expect(html).toContain("不是内置 read_file"); expect(html).toContain("暂仅支持 Native"); expect(html).toContain("不可用的记录");
    expect(toggleCapability(value, "mcp_servers", "missing", false)).toEqual({ mcp_servers: ["m1"], skills: [] });
    expect(toggleCapability(value, "mcp_servers", "m1", true)).toEqual(value);
  });
  it("检查绝对路径、UTF8边界、完整技能、参数、无凭据绑定", () => {
    expect(capabilityCommandError(save)).toBeNull();
    expect(capabilityCommandError({ ...save, definition: { ...save.definition, program: "server" } })).toContain("绝对路径");
    expect(capabilityCommandError({ ...save, definition: { ...save.definition, program: "C:\\bin\\server.exe" } })).toBeNull();
    expect(capabilityCommandError({ ...save, definition: { ...save.definition, credential_refs: { SECRET: "slot" } } })).toContain("不支持凭据");
    expect(capabilityCommandError({ ...save, definition: { ...save.definition, name: "中".repeat(81) } })).toContain("80");
    expect(capabilityCommandError({ type: "save_skill", name: "技能", content: " " })).not.toBeNull();
    expect(capabilityCommandError({ type: "save_skill", name: "技能", content: "中".repeat(21845) + "a" })).toBeNull();
    expect(capabilityCommandError({ type: "save_skill", name: "技能", content: "中".repeat(21845) + "ab" })).not.toBeNull();
    expect(capabilityCommandError({ type: "import_skill", name: "技能", source_path: "/repo/OTHER.md" })).not.toBeNull();
    expect(capabilityCommandError({ type: "import_skill", name: "技能", source_path: "/repo/SKILL.md" })).toBeNull();
    expect(capabilityCommandError({ ...probe, expected_revision: -1 })).not.toBeNull();
  });
});
describe("管理请求生命周期与幂等", () => {
  it("GET刷新不启动进程；并发与重复提交复用同一已确认命令", async () => {
    const { controller, client } = fixture();
    await controller.refresh(); expect(client.command).not.toHaveBeenCalled();
    const first = controller.submit(save), second = controller.submit(save);
    expect(first).toBe(second); await first;
    expect(client.command).toHaveBeenCalledTimes(1); expect(client.command).toHaveBeenCalledWith({ id: "command-id", command: save }, expect.any(AbortSignal));
    await controller.submit(save); expect(client.command).toHaveBeenCalledTimes(1); controller.dispose();
  });
  it("不同操作不能搭便车获得前一项回执，同步失败也不锁死后续请求", async () => {
    const { controller, client } = fixture();
    const first = controller.submit(save); expect(await controller.submit(probe)).toBeUndefined(); await first;
    expect(client.command).toHaveBeenCalledTimes(1);
    vi.mocked(client.command).mockImplementationOnce(() => { throw new Error("offline"); });
    await controller.submit(probe); expect(controller.getState().busy).toBe(false);
    await controller.submit(probe); expect(client.command).toHaveBeenCalledTimes(3); controller.dispose();
  });
  it("未知POST结果没有自动重发；手动同意重试保留命令ID，修改后生成新ID", async () => {
    let id = 0; const client: CapabilityClient = { getRegistry: vi.fn().mockResolvedValue(registry), command: vi.fn().mockRejectedValueOnce(new Error("SECRET_TRANSPORT_DATA")).mockImplementation(async envelope => receipt(envelope)) };
    const controller = createCapabilityController(client, registry, vi.fn(), { uuid: () => `id-${++id}` });
    await controller.submit(save); expect(client.command).toHaveBeenCalledTimes(1); expect(controller.getState().error).not.toContain("SECRET");
    await controller.refresh(); expect(client.command).toHaveBeenCalledTimes(1);
    await controller.submit(save); expect(vi.mocked(client.command).mock.calls[1][0].id).toBe("id-1");
    await controller.submit(probe); expect(vi.mocked(client.command).mock.calls[2][0].id).toBe("id-2"); controller.dispose();
  });
  it("过时修订、非法路径和后台离线只给静态错误，不回显私密值", async () => {
    expect(capabilityErrorMessage({ code: "stale_revision", message: "secret" })).toContain("其他入口修改");
    expect(capabilityErrorMessage({ code: "invalid_path" })).toContain("绝对路径");
    const { controller, client } = fixture(); vi.mocked(client.command).mockRejectedValue({ code: "stale_revision" });
    await controller.submit({ type: "set_capability_enabled", kind: "mcp", id: "m1", expected_revision: 2, enabled: false });
    expect(controller.getState().error).toContain("没有自动重发"); expect(client.command).toHaveBeenCalledTimes(1); controller.dispose();
  });
  it("发现仅GET轮询直到就绪，停止后不再轮询", async () => {
    vi.useFakeTimers(); const { controller, client } = fixture();
    vi.mocked(client.command).mockImplementation(async envelope => ({ ...receipt(envelope), status: "checking" }));
    vi.mocked(client.getRegistry).mockResolvedValueOnce({ ...registry, mcp_servers: [{ ...registry.mcp_servers[0], status: "checking" }] }).mockResolvedValue({ ...registry, mcp_servers: [{ ...registry.mcp_servers[0], status: "ready" }] });
    await controller.submit(probe); expect(client.getRegistry).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(50); expect(client.getRegistry).toHaveBeenCalledTimes(2); expect(client.command).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(500); expect(client.getRegistry).toHaveBeenCalledTimes(2); controller.dispose();
  });
  it("卸载中止GET与POST观察，晚到结果不修改新页面，也不宣称后台进程已取消", async () => {
    const { controller, client, states } = fixture(); let finish!: (value: ManagedCapabilities) => void;
    vi.mocked(client.getRegistry).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    const pending = controller.refresh(); const signal = vi.mocked(client.getRegistry).mock.calls[0][0]; controller.dispose(); const count = states.length;
    expect(signal.aborted).toBe(true); finish(registry); await pending; expect(states).toHaveLength(count);
    await controller.submit(probe); expect(client.command).not.toHaveBeenCalled();
    const next = fixture(); let complete!: (value: ReturnType<typeof receipt>) => void;
    vi.mocked(next.client.command).mockImplementation(() => new Promise(resolve => { complete = resolve; }));
    const mutation = next.controller.submit(probe); await Promise.resolve(); next.controller.dispose();
    expect(vi.mocked(next.client.command).mock.calls[0][1].aborted).toBe(true); const size = next.states.length;
    complete(receipt({ id: "command-id", command: probe })); await mutation; expect(next.states).toHaveLength(size); expect(next.client.getRegistry).not.toHaveBeenCalled();
  });
  it("较旧GET不能覆盖新记录；畸形回应不被当作成功", async () => {
    const { controller, client } = fixture(); let old!: (value: ManagedCapabilities) => void;
    vi.mocked(client.getRegistry).mockImplementationOnce(() => new Promise(resolve => { old = resolve; }));
    const request = controller.refresh(); controller.replaceRegistry({ ...registry, skills: [] }); old(registry); await request;
    expect(controller.getState().registry.skills).toEqual([]);
    vi.mocked(client.getRegistry).mockResolvedValue({ mcp_servers: [{}], skills: [] } as unknown as ManagedCapabilities); await controller.refresh();
    expect(controller.getState().error).toContain("未能确认"); expect(controller.getState().registry.skills).toEqual([]);
    vi.mocked(client.command).mockResolvedValue({ command_id: "wrong", status: "completed" }); await controller.submit(probe);
    expect(controller.getState().notice).toBe(""); expect(controller.getState().error).toContain("未能确认"); controller.dispose();
  });
  it("启停/导入绑定确切修订，空正文不发命令", async () => {
    const { controller, client } = fixture();
    await controller.submit({ type: "save_skill", id: "s1", expected_revision: 3, name: "技能", content: "" }); expect(client.command).not.toHaveBeenCalled();
    const command: CapabilityCommand = { type: "set_capability_enabled", kind: "skill", id: "s1", expected_revision: 3, enabled: false };
    await controller.submit(command); expect(vi.mocked(client.command).mock.calls[0][0].command).toEqual(command);
    await controller.submit({ type: "import_skill", id: "s1", expected_revision: 3, name: "技能", source_path: "/repo/SKILL.md" }); expect(client.command).toHaveBeenCalledTimes(2); controller.dispose();
  });
  it("验证公开记录结构与未知服务状态，不把坏数据当成无记录", () => {
    expect(validCapabilityRegistry(registry)).toBe(true); expect(validCapabilityRegistry({ mcp_servers: [], skills: [{}] })).toBe(false);
    expect(validCapabilityRegistry({ mcp_servers: [{ ...registry.mcp_servers[0], tool_count: -1 }], skills: [] })).toBe(false);
  });
});
