// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { HistoryBrowser, type HistoryBrowserProps } from "./history-browser";
import { HistoryError, type HistoryClient, type HistoryPage, type HistoryTaskDetail } from "../history-client";
import type { TaskRun } from "../types";
const epoch = "10000000-0000-4000-8000-000000000001";
const task = (id = "task", sessionId = "session"): TaskRun => ({ id, session_id: sessionId, task_kind: "main", title: `任务-${id}`, status: "completed", execution_mode: "native", created_at: "then", updated_at: "now", steps: [], source: { channel_id: "desktop", actor_id: "owner", conversation_id: "main" }, result: "真实结果", execution_cleanup: "unknown" });
const page = (items: TaskRun[] = [task()], next: string | null = null, current_cursor = "first"): HistoryPage => ({ current_cursor, items, total: 100, returned: items.length, has_more: next !== null, next_cursor: next, anchor: epoch, event_cursor: 10, unavailable: [] });
const detail = (value = task()): HistoryTaskDetail => ({ task: value, event_cursor: 10, events: [], execution_requests: [], history: { events: { total: 0, returned: 0, has_more: false }, execution_requests: {total:0,returned:0,has_more:false,omitted:0,pending:0,unavailable:[]} } });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
let container: HTMLDivElement, root: Root;
beforeEach(() => { (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true; container = document.createElement("div"); document.body.append(container); root = createRoot(container); });
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); });
const buttons = () => [...container.querySelectorAll("button")];
function button(text: string) { const found = buttons().find(b => b.textContent === text || b.getAttribute("aria-label") === text); if (!found) throw new Error(`missing button ${text}`); return found; }
async function click(text: string) { await act(async () => button(text).click()); }
function fixture() {
  const client: HistoryClient = { page: vi.fn(async () => page()), task: vi.fn(async () => detail()) };
  const props: HistoryBrowserProps = { open: true, client, onClose: vi.fn(), sessionId: "session", epoch, onCommand: vi.fn(async () => ({ command_id: "command", status: "accepted", task_id: "new-task" })) };
  return { client, props };
}
async function render(props: HistoryBrowserProps) { await act(async () => root.render(<HistoryBrowser {...props} />)); }

describe("独立有界历史浏览，不改实时快照", () => {
  it("显示查询时刻/总数/只读边界，换页仅保留当前页", async () => {
    const { client, props } = fixture();
    vi.mocked(client.page).mockImplementation(async query => query.cursor === "next" ? page([task("second")], null, "next") : page([task("first")], "next"));
    await render(props); expect(container.textContent).toContain("已展示 1 条 / 总计 100 条"); expect(container.textContent).toContain("读取时刻");
    await click("下一页"); expect(container.textContent).toContain("任务-second"); expect(container.textContent).not.toContain("任务-first");
    await click("上一页"); expect(container.textContent).toContain("任务-first");
    expect(client.page).toHaveBeenCalledTimes(3); expect(props.onCommand).not.toHaveBeenCalled();
  });
  it("只保留最近32个返回位置，仍可向前浏览并显式回首页", async () => {
    const { client, props } = fixture();
    vi.mocked(client.page).mockImplementation(async query => { const n = query.cursor ? Number(query.cursor.slice(1)) : 0; return page([task(`page-${n}`)], `c${n + 1}`, `c${n}`); });
    await render(props);
    for (let i = 0; i < 35; i++) await click("下一页");
    expect(container.textContent).toContain("任务-page-35"); expect(container.querySelectorAll(".history-record")).toHaveLength(1);
    for (let i = 0; i < 32; i++) await click("上一页");
    expect(container.textContent).toContain("任务-page-3"); expect(button("上一页").disabled).toBe(true);
    await click("重新读取首页"); expect(container.textContent).toContain("任务-page-0");
  });
  it("首面固定current_cursor，插入后返回/刷新不混入新upper；只有显式首页重读采用新记录", async () => {
    const { client, props } = fixture(); let inserted = false;
    vi.mocked(client.page).mockImplementation(async query => query.cursor === "second" ? page([task("older")], null, "second") : query.cursor === "first" ? page([task("original")], "second", "first") : page([task(inserted ? "inserted" : "original")], "second", inserted ? "fresh" : "first"));
    await render(props); await click("下一页"); inserted = true;
    await click("上一页"); expect(container.textContent).toContain("任务-original"); expect(container.textContent).not.toContain("任务-inserted");
    await click("刷新当前页"); expect(container.textContent).not.toContain("任务-inserted");
    await click("重新读取首页"); expect(container.textContent).toContain("任务-inserted");
    expect(vi.mocked(client.page).mock.calls.map(([query]) => query.cursor)).toEqual([undefined, "second", "first", "first", undefined]);
  });
  it("关闭/重开丢弃游标并中止观察，迟到响应不能覆写新页", async () => {
    const { client, props } = fixture(); const old = deferred<HistoryPage>();
    vi.mocked(client.page).mockImplementationOnce(() => old.promise).mockResolvedValueOnce(page([task("new")]));
    await render(props); const signal = vi.mocked(client.page).mock.calls[0][1];
    await click("关闭历史浏览器"); expect(props.onClose).toHaveBeenCalledOnce();
    await render({ ...props, open: false }); expect(signal.aborted).toBe(true);
    await render(props); await act(async () => old.resolve(page([task("old")])));
    expect(container.textContent).toContain("任务-new"); expect(container.textContent).not.toContain("任务-old");
    expect(vi.mocked(client.page).mock.calls[1][0].cursor).toBeUndefined();
  });
  it("切换会话后忽略旧scope，epoch变化时清除旧历史", async () => {
    const { client, props } = fixture(); const old = deferred<HistoryPage>();
    vi.mocked(client.page).mockImplementationOnce(() => old.promise).mockResolvedValueOnce(page([task("new", "new-session")]));
    await render(props); await render({ ...props, sessionId: "new-session" });
    await act(async () => old.resolve(page([task("old")])));
    expect(container.textContent).toContain("任务-new"); expect(container.textContent).not.toContain("任务-old");
    vi.mocked(client.page).mockResolvedValue(page([task("wrong-epoch", "new-session")]));
    await render({ ...props, sessionId: "new-session", epoch: "20000000-0000-4000-8000-000000000002" });
    expect(container.textContent).toContain("历史已重置"); expect(container.textContent).not.toContain("任务-wrong-epoch");
  });
  it("409清除当前页与返回栈，只读手动恢复不自动重发", async () => {
    const { client, props } = fixture();
    vi.mocked(client.page).mockResolvedValueOnce(page([task("old")], "next")).mockRejectedValueOnce(new HistoryError("history_reset")).mockResolvedValueOnce(page([task("recovered")]));
    await render(props); await click("下一页");
    expect(container.textContent).toContain("旧分页位置已清除"); expect(container.textContent).not.toContain("任务-old");
    expect(button("上一页").disabled).toBe(true); expect(client.page).toHaveBeenCalledTimes(2);
    await click("重新读取首页"); expect(vi.mocked(client.page).mock.calls[2][0].cursor).toBeUndefined(); expect(container.textContent).toContain("任务-recovered"); expect(props.onCommand).not.toHaveBeenCalled();
  });
  it("旧不安全ID仍展示但不能读详情，超大项显示明确不可用且不是不存在", async () => {
    const { client, props } = fixture();
    vi.mocked(client.page).mockResolvedValue({ ...page([task("old/unsafe")]), unavailable: [{ id: "huge", reason: "record_too_large" }] });
    vi.mocked(client.task).mockRejectedValue(new HistoryError("record_too_large"));
    await render(props); expect(container.textContent).toContain("old/unsafe"); expect(container.textContent).toContain("仅展示");
    expect(buttons().filter(b => b.textContent === "读取任务详情")).toHaveLength(1);
    await click("读取任务详情"); expect(client.task).toHaveBeenCalledWith("huge", expect.any(AbortSignal)); expect(container.textContent).toContain("离线备份检查完整记录"); expect(container.textContent).not.toContain("该记录不存在");
  });
  it("会话选择固定安全ID并通过回调，问题/批准历史只读", async () => {
    const { client, props } = fixture(); const select = vi.fn();
    vi.mocked(client.page).mockResolvedValue({ ...page([]), returned: 1, items: [{ id: "session", title: "旧会话", project_path: "/fixture", created_at: "then", updated_at: "now", messages: [] }], session_messages: { session: { total: 50, offset: 50, returned: 0, has_more_before: true, unavailable: [] } } });
    await render({ ...props, initialKind: "sessions", onSelectSession: select }); await click("选择此会话"); expect(select).toHaveBeenCalledWith("session");
    expect(container.textContent).toContain("消息总计 50 条 · 本页载入 0 条"); expect(container.textContent).toContain("未载入不等于空历史");
    vi.mocked(client.page).mockResolvedValue({ ...page([]), returned: 1, items: [{ id: "approval", session_id: "session", task_id: "task", kind: "approval", prompt: "批准精确写入？", action: { path: "test.txt" }, action_digest: "abc123", revision: 2, status: "pending", expires_at_ms: 100, answer: null }] });
    await click("问题与授权记录"); expect(container.textContent).toContain("精确操作摘要"); expect(container.textContent).toContain("abc123");
    expect(buttons().some(b => /^(批准|拒绝|回答)/.test(b.textContent ?? ""))).toBe(false); expect(props.onCommand).not.toHaveBeenCalled();
  });
  it("捕获任务详情与实时流分离，双击只提交一次固定ID继续，未知结果不重试", async () => {
    const { client, props } = fixture(); const pending = deferred<never>();
    const command = vi.fn(() => pending.promise);
    await render({ ...props, onCommand: command }); await click("读取任务详情");
    expect(container.textContent).toContain("任务详情（捕获记录）"); expect(container.textContent).toContain("真实结果"); expect(container.textContent).toContain("未知，需检查项目隔离");
    await act(async () => { button("继续此任务 · 创建新任务").click(); button("继续此任务 · 创建新任务").click(); });
    expect(command).toHaveBeenCalledOnce(); expect(command).toHaveBeenCalledWith({ type: "continue_task", task_id: "task" });
    await render({ ...props, open: false }); pending.resolve(undefined as never); await act(async () => {});
    const rejected = vi.fn(async () => { throw new Error("PRIVATE_CANARY"); });
    await render({ ...props, onCommand: rejected }); await click("读取任务详情"); await click("继续此任务 · 创建新任务");
    expect(container.textContent).toContain("提交结果未确认"); expect(container.textContent).not.toContain("PRIVATE_CANARY");
    expect(button("继续此任务 · 创建新任务").disabled).toBe(true); await click("只读刷新任务详情"); expect(rejected).toHaveBeenCalledOnce(); expect(client.task).toHaveBeenCalledTimes(3);
  });
  it("取消仍走owner回调且不将receipt当终态，详情409清除外层缓存", async () => {
    const { client, props } = fixture(); vi.mocked(client.task).mockResolvedValue(detail({ ...task(), status: "running" }));
    await render(props); await click("读取任务详情"); await click("取消此任务");
    expect(props.onCommand).toHaveBeenCalledWith({ type: "cancel_task", task_id: "task" }); expect(container.textContent).toContain("受理不代表任务已完成"); expect(container.textContent).toContain("执行中");
    vi.mocked(client.task).mockRejectedValue(new HistoryError("history_reset")); await click("只读刷新任务详情");
    expect(container.textContent).toContain("旧分页位置已清除"); expect(container.querySelector("[aria-label='当前历史页']")).toBeNull();
  });
  it("切类型期间不把任务投射成计划；消息显示真实绝对位置与更早分页", async () => {
    const { client, props } = fixture(); await render(props);
    const pending = deferred<HistoryPage>(); vi.mocked(client.page).mockImplementationOnce(() => pending.promise);
    await click("计划"); expect(container.textContent).not.toContain("任务-task");
    await act(async () => pending.resolve({ ...page([]), total: 0 }));
    vi.mocked(client.page).mockResolvedValue({ ...page([]), total: 12, returned: 2, offset: 9, has_more: true, next_cursor: "older", items: [{ id: "m9", role: "user", content: "第九条", created_at: "then", history_offset: 9 }, { id: "m11", role: "assistant", content: "第十一条", created_at: "now", history_offset: 11 }], unavailable: [{ id: "m10", offset: 10, reason: "record_too_large" }] });
    await click("会话消息"); expect(container.textContent).toContain("消息绝对位置 9"); expect(container.textContent).toContain("消息绝对位置 10"); expect(container.textContent).toContain("消息绝对位置 11"); expect(button("更早一页").disabled).toBe(false);
  });
  it("静态所有权与样式：无共享快照合并、无secret/localStorage、无绿色", () => {
    const source = readFileSync("src/components/history-browser.tsx", "utf8");
    expect(source).not.toMatch(/receiveSnapshot|receiveEvent|localStorage|dangerouslySetInnerHTML/);
    const css = readFileSync("src/components/history-browser.css", "utf8"); expect(css).not.toMatch(/green|#(?:00ff00|0f0)\b/i);
  });
  it("详情请求数组为空但预算省略pending时明确提示，不称没有待批准", async () => {
    const { client, props } = fixture();
    vi.mocked(client.task).mockResolvedValue({ ...detail({ ...task(), result: "大正文".repeat(500_000) }), history: { events: { total: 0, returned: 0, has_more: false }, execution_requests: { total: 2, returned: 0, has_more: true, omitted: 2, pending: 1, unavailable: [{ id: "approval-large", reason: "record_too_large" }] } } });
    await render(props); await click("读取任务详情");
    const section = container.querySelector('[aria-label="任务问题与授权范围"]')!;
    expect(section.textContent).toContain("已载入 0 / 2"); expect(section.textContent).toContain("待处理 1 条"); expect(section.textContent).toContain("不能认定没有待批准"); expect(section.textContent).toContain("approval-large"); expect(section.textContent).not.toContain("后台确认此任务没有");
  });
});
