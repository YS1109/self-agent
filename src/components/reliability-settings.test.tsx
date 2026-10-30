import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "node:http";
import { once } from "node:events";
import { readFileSync } from "node:fs";
import { backupRequestError, createReliabilityController, diagnosticJson, DIAGNOSTIC_COUNTS, DIAGNOSTIC_EXCLUDES, localBackupRecovery, parseBackupReceipt, parseReliabilityStatus, parseSafeDiagnostics, type BackupReceipt, type BackupRecovery, type ReliabilityClient, type ReliabilityState, type ReliabilityStatus } from "../reliability-client";
import { reliabilityClient } from "../api";
import { BackupReceiptCard, downloadDiagnostic, ReliabilitySettings } from "./reliability-settings";
const id = "10000000-0000-4000-8000-000000000001";
const job = (state: BackupReceipt["state"] = "pending"): BackupReceipt => ({ id, state, reason: state === "failed" ? "export_failed" : state === "unknown" ? "outcome_uncertain" : null, manifest: state === "completed" ? { format: 1, schema: 1, bytes: 4096, sha256: "a".repeat(64) } : null });
const status = (jobs: BackupReceipt[] = []): ReliabilityStatus => ({ schema_version: 1, backup_available: true, restore_mode: "offline_only", updates: "disabled_no_trusted_release_configuration", backup_jobs: jobs });
const diagnostic = () => ({ diagnostic_schema: 1, application: "self-agent", application_version: "0.6.0", platform: "linux", database_schema: 1, connection_check_history: "not_recorded", storage: "compatible", vault: "not_checked", updates: "disabled_no_trusted_release_configuration", counts: Object.fromEntries(DIAGNOSTIC_COUNTS.map(key => [key, 0])), excludes: [...DIAGNOSTIC_EXCLUDES] });
function fixture(saved: string | null = null) {
  let stored = saved; const history: string[] = [];
  const recovery: BackupRecovery = { load: vi.fn(() => stored), save: vi.fn(value => { stored = value; }), clear: vi.fn(value => { if (stored === value) stored = null; }), history: vi.fn(() => [...history]), archive: vi.fn(value => { if (!history.includes(value)) history.push(value); if (stored === value) stored = null; }) };
  const client: ReliabilityClient = { status: vi.fn().mockResolvedValue(status()), backup: vi.fn().mockResolvedValue(job()), receipt: vi.fn().mockResolvedValue(job("completed")), diagnostics: vi.fn().mockResolvedValue(diagnostic()) };
  const states: ReliabilityState[] = [];
  const controller = createReliabilityController(client, value => states.push(value), recovery, { uuid: () => id, pollMs: 20 });
  return { controller, client, recovery, states, stored: () => stored };
}
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });
describe("可靠性类型、限额和诊断隐私", () => {
  it("备份请求只接受绝对父目录和有界新名称，不接受路径跳转", () => {
    for (const path of ["/private/backups", "C:\\private\\backups", "\\\\server\\share\\backups"]) expect(backupRequestError(path, "backup-1_2")).toBeNull();
    for (const path of ["relative", "/a/../b", "/a/./b", "C:\\a\\..\\b", "/a\0b", "/" + "中".repeat(1366)]) expect(backupRequestError(path, "safe")).not.toBeNull();
    for (const name of ["", ".", "..", "a/b", "中文", "a".repeat(81), "https://example"]) expect(backupRequestError("/private", name)).not.toBeNull();
    expect(backupRequestError("/private", "a".repeat(80))).toBeNull();
  });
  it("接受不等于完成，清单只出现在合法completed回执", () => {
    for (const state of ["pending", "running", "completed", "failed", "unknown"] as const) expect(parseBackupReceipt(job(state)).state).toBe(state);
    for (const value of [{ ...job(), state: ["pending"] }, { ...job(), manifest: job("completed").manifest }, { ...job("completed"), manifest: null }, { ...job("completed"), manifest: { ...job("completed").manifest, sha256: "A".repeat(64) } }, { ...job("completed"), manifest: { ...job("completed").manifest, bytes: 128 * 1024 * 1024 + 1 } }, { ...job("unknown"), reason: "raw private error" }, { ...job(), path: "/PRIVATE" }]) expect(() => parseBackupReceipt(value)).toThrow();
    expect(parseBackupReceipt({ ...job("completed"), manifest: { ...job("completed").manifest, bytes: 128 * 1024 * 1024 } }).state).toBe("completed");
    const jobs = Array.from({ length: 256 }, (_, i) => ({ ...job(), id: `10000000-0000-4000-8000-${String(i).padStart(12, "0")}` }));
    expect(parseReliabilityStatus(status(jobs)).backup_jobs).toHaveLength(256); expect(() => parseReliabilityStatus(status([...jobs, job()]))).toThrow();
    expect(() => parseReliabilityStatus(status([job(), job()]))).toThrow();
  });
  it("诊断固定字段、枚举、计数和16KiB边界，拒绝任何额外原始数据", () => {
    expect(parseSafeDiagnostics(diagnostic()).vault).toBe("not_checked");
    const raw = { token: "PRIVATE_SECRET", path: "/private/project", url: "https://private.invalid", logs: ["PRIVATE_LOG"], prompt: "PRIVATE_PROMPT", snapshot: { tasks: [] } };
    for (const [key, value] of Object.entries(raw)) { expect(() => diagnosticJson({ ...diagnostic(), [key]: value })).toThrow(); expect(() => diagnosticJson({ ...diagnostic(), counts: { ...diagnostic().counts, [key]: value } })).toThrow(); }
    for (const value of [{ ...diagnostic(), application_version: "SECRET" }, { ...diagnostic(), storage: "PRIVATE_PATH" }, { ...diagnostic(), platform: ["linux"] }, { ...diagnostic(), counts: { ...diagnostic().counts, sessions: -1 } }, { ...diagnostic(), counts: { ...diagnostic().counts, sessions: Number.MAX_SAFE_INTEGER + 1 } }, { ...diagnostic(), excludes: [] }, { ...diagnostic(), application_version: "x".repeat(16385) }]) expect(() => parseSafeDiagnostics(value)).toThrow();
    const text = diagnosticJson(diagnostic()); expect(text.length).toBeLessThan(16384); expect(text).toContain('"connection_check_history": "not_recorded"'); expect(text).not.toContain("PRIVATE");
  });
  it("恢复存储只保存UUID，不接受秘密/目录，清理不能删除别的窗口新操作", () => {
    const values = new Map<string, string>(); const storage = { getItem: (key: string) => values.get(key) ?? null, setItem: vi.fn((key: string, value: string) => { values.set(key, value); }), removeItem: (key: string) => values.delete(key) }; vi.stubGlobal("localStorage", storage);
    localBackupRecovery.save(id); expect(localBackupRecovery.load()).toBe(id); expect(storage.setItem.mock.calls[0][1]).toBe(id);
    expect(()=>localBackupRecovery.save("20000000-0000-4000-8000-000000000002")).toThrow(); expect(localBackupRecovery.load()).toBe(id);
    expect(() => localBackupRecovery.save("/private/path")).toThrow();
    localBackupRecovery.archive(id); expect(localBackupRecovery.load()).toBeNull(); expect(localBackupRecovery.history()).toEqual([id]);
    for (let i=1;i<64;i++) localBackupRecovery.archive(`10000000-0000-4000-8000-${String(i+1).padStart(12,"0")}`);
    expect(()=>localBackupRecovery.save(id)).toThrow(); expect(()=>localBackupRecovery.archive("30000000-0000-4000-8000-000000000001")).toThrow(); expect(localBackupRecovery.history()).toHaveLength(64);
    values.delete("self-agent.backup-observation-history.v1");
    const other = "20000000-0000-4000-8000-000000000002"; localBackupRecovery.save(other); localBackupRecovery.clear(id); expect(localBackupRecovery.load()).toBe(other);
  });
});
describe("在线备份一次提交和GET恢复", () => {
  it("显式确认、保存ID先于一次POST，双击不重发，状态/持久恢复不带路径", async () => {
    const { controller, client, recovery, states } = fixture(); await controller.refresh();
    await controller.backup("/private/backup", "once", false); expect(client.backup).not.toHaveBeenCalled();
    vi.mocked(client.backup).mockImplementation(async body => { expect(recovery.save).toHaveBeenCalledWith(id); expect(body).toEqual({ id, parent: "/private/backup", name: "once" }); return job(); });
    const first = controller.backup("/private/backup", "once", true); await controller.backup("/private/backup", "twice", true); await first;
    expect(client.backup).toHaveBeenCalledTimes(1); expect(controller.getState().selected?.state).toBe("pending"); expect(JSON.stringify(states)).not.toContain("/private"); controller.dispose();
  });
  it("结果未知保留ID，404仅只读查询；重开不需要路径也不自动POST", async () => {
    const first = fixture(); await first.controller.refresh(); vi.mocked(first.client.backup).mockRejectedValue(new Error("SECRET_PATH")); await first.controller.backup("/private", "once", true);
    expect(first.stored()).toBe(id); expect(first.controller.getState().notice).not.toContain("SECRET_PATH"); await first.controller.backup("/private", "again", true); expect(first.client.backup).toHaveBeenCalledTimes(1); first.controller.dispose();
    const second = fixture(id); vi.mocked(second.client.receipt).mockRejectedValueOnce(new Error("404"));
    await second.controller.inspect(id); expect(second.controller.getState().recoveryId).toBe(id);
    await second.controller.inspect(id); expect(second.controller.getState().selected?.state).toBe("completed"); expect(second.stored()).toBeNull(); expect(second.client.backup).not.toHaveBeenCalled(); second.controller.dispose();
  });
  it("结束观察必须确认，保留UUID后只读查询，不删除后台或重发；历史满时阻止新观察", async () => {
    vi.useFakeTimers();
    const f = fixture(id); expect(f.controller.finishObservation(false)).toBe(false); expect(f.recovery.archive).not.toHaveBeenCalled(); expect(f.stored()).toBe(id);
    expect(f.controller.finishObservation(true)).toBe(true); expect(f.controller.getState().observationHistory).toEqual([id]); expect(f.stored()).toBeNull();
    await vi.advanceTimersByTimeAsync(100); expect(f.client.receipt).not.toHaveBeenCalled();
    await f.controller.inspect(id); expect(f.client.receipt).toHaveBeenCalledTimes(1); expect(f.client.backup).not.toHaveBeenCalled(); f.controller.dispose();
    const next = createReliabilityController(f.client, vi.fn(), f.recovery); expect(next.getState().observationHistory).toEqual([id]); next.dispose();
    const g = fixture(id); vi.mocked(g.recovery.archive).mockImplementation(() => { throw new Error("full"); }); expect(g.controller.finishObservation(true)).toBe(false); expect(g.stored()).toBe(id); expect(g.client.backup).not.toHaveBeenCalled(); g.controller.dispose();
    const h = fixture(); vi.mocked(h.recovery.history).mockReturnValue(Array.from({length:64}, (_,i)=>`10000000-0000-4000-8000-${String(i).padStart(12,"0")}`)); const full = createReliabilityController(h.client,vi.fn(),h.recovery); await full.refresh(); await full.backup("/private","new",true); expect(h.client.backup).not.toHaveBeenCalled(); full.dispose(); h.controller.dispose();
  });
  it("pending只GET轮询，较旧快照不覆盖terminal；坏ID/回执不能称完成", async () => {
    vi.useFakeTimers(); const { controller, client } = fixture(); await controller.refresh(); await controller.backup("/private", "once", true);
    await vi.advanceTimersByTimeAsync(20); expect(client.backup).toHaveBeenCalledTimes(1); expect(controller.getState().selected?.state).toBe("completed");
    vi.mocked(client.status).mockResolvedValue(status([job()])); await controller.refresh(); expect(controller.getState().status?.backup_jobs[0].state).toBe("completed");
    vi.mocked(client.receipt).mockResolvedValue({ ...job("completed"), id: "20000000-0000-4000-8000-000000000002" }); await controller.inspect(id); expect(controller.getState().selected?.id).toBe(id); controller.dispose();
  });
  it("容量、后台不可用或ID持久失败时不提交；原unknown终态不称未写入", async () => {
    const f = fixture(); vi.mocked(f.client.status).mockResolvedValue({ ...status(), backup_available: false }); await f.controller.refresh(); await f.controller.backup("/private", "once", true); expect(f.client.backup).not.toHaveBeenCalled();
    vi.mocked(f.client.status).mockResolvedValue(status()); await f.controller.refresh(); vi.mocked(f.recovery.save).mockImplementation(() => { throw new Error("storage disabled"); }); await f.controller.backup("/private", "once", true); expect(f.client.backup).not.toHaveBeenCalled(); f.controller.dispose();
    const g = fixture(id); vi.mocked(g.client.receipt).mockResolvedValue(job("unknown")); await g.controller.inspect(id); expect(g.controller.getState().selected?.state).toBe("unknown"); expect(g.stored()).toBeNull(); g.controller.dispose();
  });
  it("慢GET不会被轮询不断中止，历史到256时拒绝新备份", async () => {
    vi.useFakeTimers(); const f = fixture(id); let finish!: (value: unknown) => void;
    vi.mocked(f.client.receipt).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    f.controller.start(); await vi.advanceTimersByTimeAsync(100);
    expect(f.client.receipt).toHaveBeenCalledTimes(1); expect(vi.mocked(f.client.receipt).mock.calls[0][1].aborted).toBe(false);
    finish(job("completed")); await vi.advanceTimersByTimeAsync(20); f.controller.dispose();
    const g = fixture(); const history = Array.from({ length: 256 }, (_, i) => ({ ...job("completed"), id: `10000000-0000-4000-8000-${String(i).padStart(12, "0")}` }));
    vi.mocked(g.client.status).mockResolvedValue(status(history)); await g.controller.refresh(); await g.controller.backup("/private", "once", true); expect(g.client.backup).not.toHaveBeenCalled(); g.controller.dispose();
  });
  it("关闭/后退中止观察，晚到POST和诊断不更新UI；已提交的操作ID保留", async () => {
    const { controller, client, states, stored } = fixture(); await controller.refresh(); let done!: (value: unknown) => void;
    vi.mocked(client.backup).mockImplementation(() => new Promise(resolve => { done = resolve; })); const request = controller.backup("/private", "once", true); await Promise.resolve();
    controller.dispose(); const count = states.length; expect(vi.mocked(client.backup).mock.calls[0][1].aborted).toBe(true); done(job("completed")); await request; expect(states).toHaveLength(count); expect(stored()).toBe(id);
    const other = fixture(); let exported!: (value: unknown) => void; vi.mocked(other.client.diagnostics).mockImplementation(() => new Promise(resolve => { exported = resolve; })); const read = other.controller.diagnostics(); other.controller.dispose(); const n = other.states.length; exported(diagnostic()); await read; expect(other.states).toHaveLength(n);
  });
  it("真实loopback TCP丢失POST响应后仅GET恢复，不回显路径或token", async () => {
    const requests: string[] = []; let body: unknown;
    const server = createServer((request, response) => { requests.push(`${request.method} ${request.url}`); if (request.method === "POST") { let raw = ""; request.on("data", chunk => { raw += String(chunk); }); request.on("end", () => { body = JSON.parse(raw); request.socket.destroy(); }); return; } response.setHeader("Content-Type", "application/json"); response.end(JSON.stringify(request.url === "/api/v1/reliability" ? status() : job("completed"))); });
    server.listen(0, "127.0.0.1"); await once(server, "listening"); const address = server.address(); if (!address || typeof address === "string") throw new Error("fixture_address");
    const fetcher = globalThis.fetch; vi.stubGlobal("window", {}); vi.stubGlobal("fetch", (path: string, init?: RequestInit) => fetcher(`http://127.0.0.1:${address.port}${path}`, init));
    const memory = fixture(); const controller = createReliabilityController(reliabilityClient, vi.fn(), memory.recovery, { uuid: () => id });
    try { await controller.refresh(); await controller.backup("/fixture/private", "once", true); await controller.inspect(id); expect(body).toEqual({ id, parent: "/fixture/private", name: "once" }); expect(requests.filter(value => value.startsWith("POST"))).toHaveLength(1); expect(controller.getState().selected?.state).toBe("completed"); expect(JSON.stringify(controller.getState())).not.toContain("/fixture/private"); }
    finally { controller.dispose(); memory.controller.dispose(); server.closeAllConnections(); await new Promise<void>(resolve => server.close(() => resolve())); }
  });
});
describe("本机可靠性界面", () => {
  it("入口接线到真实client，私有备份/固定诊断/离线恢复边界清楚，无隐式导出", () => {
    const { client, recovery } = fixture(); const html = renderToStaticMarkup(<ReliabilitySettings client={client} recovery={recovery} />);
    for (const text of ["不是脱敏诊断", "128 MiB", "256 条", "恢复仅限离线 CLI", "生产自动更新与安装仍禁用", "连接检查历史未记录", "最多 16 KiB"]) expect(html).toContain(text);
    expect(html).toMatch(/<button[^>]*disabled[^>]*>创建一次备份/); expect(client.backup).not.toHaveBeenCalled(); expect(client.diagnostics).not.toHaveBeenCalled();
    const app = readFileSync("src/App.tsx", "utf8"); expect(app).toContain('<ReliabilitySettings client={reliabilityClient}'); expect(app).toContain('setModal("reliability")');
  });
  it("completed才显示摘要，unknown绝不称没写入；下载边界再次拒绝原始snapshot", () => {
    const pending = renderToStaticMarkup(<BackupReceiptCard receipt={job()} />); expect(pending).toContain("未完成"); expect(pending).not.toContain("SHA-256");
    const completed = renderToStaticMarkup(<BackupReceiptCard receipt={job("completed")} />); expect(completed).toContain("a".repeat(64)); expect(completed).toContain("不认证备份来源");
    const unknown = renderToStaticMarkup(<BackupReceiptCard receipt={job("unknown")} />); expect(unknown).toContain("可能已有材料"); expect(unknown).toContain("不要自动重发");
    const create = vi.spyOn(URL, "createObjectURL"); expect(() => downloadDiagnostic({ tasks: [], secrets: "PRIVATE" })).toThrow(); expect(create).not.toHaveBeenCalled();
  });
  it("诊断读取只在显式操作，畸形返回不回退snapshot或保留旧下载", async () => {
    const { controller, client } = fixture(); await controller.refresh(); expect(client.diagnostics).not.toHaveBeenCalled(); await controller.diagnostics(); expect(controller.getState().diagnostic?.application).toBe("self-agent");
    vi.mocked(client.diagnostics).mockResolvedValue({ ...diagnostic(), logs: "PRIVATE" }); await controller.diagnostics(); expect(controller.getState().diagnostic).toBeNull(); expect(controller.getState().diagnosticNotice).not.toContain("PRIVATE"); controller.dispose();
  });
});
