import { describe, expect, it, vi } from "vitest";
import { createServer } from "node:http";
import { once } from "node:events";
import { createHistoryClient, HISTORY_PAGE_BYTES, HISTORY_TASK_BYTES, HistoryError, historyErrorText, historyPagePath, historyTaskPath, parseHistoryPage, parseHistoryTask, type HistoryPage } from "./history-client";
const anchor = "10000000-0000-4000-8000-000000000001";
const task = (id = "old-task") => ({ id, session_id: "session", title: "历史任务", status: "completed", execution_mode: "native", created_at: "then", updated_at: "now", source: { channel_id: "desktop", actor_id: "owner", conversation_id: "main" }, steps: [], result: "真实结果" });
const page = (current_cursor = "first"): HistoryPage => ({ current_cursor, items: [], total: 0, returned: 0, has_more: false, next_cursor: null, anchor, event_cursor: 10, unavailable: [] });
const detail = () => ({ task: task(), event_cursor: 10, events: [{ id: "event", task_id: "old-task", sequence: 9, kind: "task_completed", message: "真实结果", created_at: "now" }], execution_requests: [], history: { events: { total: 1, returned: 1, has_more: false }, execution_requests: {total:0,returned:0,has_more:false,omitted:0,pending:0,unavailable:[]} } });
const response = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });

describe("历史只读路径与必要字段", () => {
  it("规范路径保留旧安全 ID，opaque cursor 有界，不提供任意路径", () => {
    expect(historyPagePath({ kind: "tasks", sessionId: "old-session_1", cursor: "a-B_9" })).toBe("/api/v1/history/tasks?session_id=old-session_1&cursor=a-B_9");
    expect(historyPagePath({ kind: "messages", sessionId: "session", cursor: "f0" })).toBe("/api/v1/history/sessions/session/messages?cursor=f0");
    expect(historyTaskPath("old-task")).toBe("/api/v1/tasks/old-task");
    for (const id of ["", "../x", "a/b", "a%2fb", "a b", "a?x=1", "a#x", "a\n", "a".repeat(129)]) {
      expect(() => historyTaskPath(id)).toThrow(HistoryError);
      expect(() => historyPagePath({ kind: "tasks", sessionId: id })).toThrow(HistoryError);
    }
    for (const cursor of ["", "a/b", "a%26x", "a=b", "a b", "中", "a".repeat(2049)]) expect(() => historyPagePath({ kind: "tasks", cursor })).toThrow(HistoryError);
    expect(historyPagePath({ kind: "tasks", cursor: "a".repeat(2048) })).toHaveLength("/api/v1/history/tasks?cursor=".length + 2048);
    expect(() => historyPagePath({ kind: "messages" })).toThrow();
  });
  it("页计数含不可载入项，旧不安全记录ID可展示但不能生成路径", () => {
    const value = { ...page("c1"), items: [task("old/record")], returned: 1, total: 3, unavailable: [{ id: "huge", reason: "record_too_large" }], has_more: true, next_cursor: "c2", future_field: true };
    expect(parseHistoryPage(value, { kind: "tasks", sessionId: "session", cursor: "c1" }).items[0].id).toBe("old/record");
    expect(() => historyTaskPath("old/record")).toThrow();
  });
  it("拒绝计数矛盾、跨scope、重复ID、空页不推进、游标复用与超量", () => {
    const base = { ...page("current"), items: [task()], returned: 1, total: 2, has_more: true, next_cursor: "next" };
    const invalid = [
      { ...base, returned: 0 }, { ...base, total: 0 }, { ...base, event_cursor: Number.MAX_SAFE_INTEGER + 1 },
      { ...base, current_cursor: undefined }, { ...base, current_cursor: "different" },
      { ...base, next_cursor: null }, { ...base, has_more: false }, { ...base, next_cursor: "current" }, { ...base, anchor: "bad" },
      { ...base, items: [], returned: 0 }, { ...base, items: [task(), task()], returned: 2, total: 3 },
      { ...base, unavailable: [{ id: "old-task", reason: "record_too_large" }] },
      { ...base, items: [task("x")], unavailable: [{ id: "u", reason: "secret_error" }] },
      { ...base, items: [{ ...task(), session_id: "other" }] },
      { ...base, items: Array.from({ length: 81 }, (_, i) => task(`t${i}`)), returned: 81, total: 100 },
      { ...base, items: [{ ...task(), steps: "bad" }] },
    ];
    for (const value of invalid) expect(() => parseHistoryPage(value, { kind: "tasks", sessionId: "session", cursor: "current" })).toThrow(HistoryError);
    expect(parseHistoryPage({ ...page(), total: 2, has_more: true, next_cursor: "next", unavailable: [{ id: "huge", reason: "record_too_large" }] }, { kind: "tasks" }).returned).toBe(0);
  });
  it("消息必须有绝对offset；问题和授权结构不同，不能伪造批准入口", () => {
    const value = { ...page(), total: 6, returned: 1, offset: 5, has_more: true, next_cursor: "older", items: [{ id: "m", role: "user", content: "正文", created_at: "now", history_offset: 5 }] };
    expect(parseHistoryPage(value, { kind: "messages", sessionId: "session" }).offset).toBe(5);
    for (const offset of [undefined, -1, 6, 1.5]) expect(() => parseHistoryPage({ ...value, offset }, { kind: "messages", sessionId: "session" })).toThrow();
    const question = { id: "q", task_id: "old-task", session_id: "session", kind: "question", prompt: "问题", status: "pending", revision: 0, expires_at_ms: 100, answer: null, action: null, action_digest: null };
    expect(parseHistoryPage({ ...page(), total: 1, returned: 1, items: [question] }, { kind: "execution_requests" }).returned).toBe(1);
    expect(() => parseHistoryPage({ ...page(), total: 1, returned: 1, items: [{ ...question, kind: "approval" }] }, { kind: "execution_requests" })).toThrow();
  });
  it("会话头必须明确消息未载入，消息绝对位置与不可载入缺口不混淆", () => {
    const session = { id: "session", title: "会话头", project_path: "/fixture", created_at: "then", updated_at: "now", messages: [] };
    const value = { ...page(), total: 1, returned: 1, items: [session], session_messages: { session: { total: 5000, offset: 5000, returned: 0, has_more_before: true, unavailable: [] } } };
    expect(parseHistoryPage(value, { kind: "sessions" }).session_messages?.session.total).toBe(5000);
    expect(() => parseHistoryPage({ ...value, session_messages: undefined }, { kind: "sessions" })).toThrow();
    const message = (id: string, offset: number) => ({ id, role: "assistant", content: "正文", created_at: "now", history_offset: offset });
    const messages = { ...page(), total: 10, offset: 7, returned: 2, has_more: true, next_cursor: "older", items: [message("m7", 7), message("m9", 9)], unavailable: [{ id: "m8", offset: 8, reason: "record_too_large" }] };
    expect(parseHistoryPage(messages, { kind: "messages", sessionId: "session" }).items).toHaveLength(2);
    for (const invalid of [{ ...messages, unavailable: [{ id: "m8", offset: 7, reason: "record_too_large" }] }, { ...messages, items: [message("m9", 9), message("m7", 7)] }, { ...messages, items: [message("m7", 7), { ...message("m9", 9), history_offset: undefined }] }]) expect(() => parseHistoryPage(invalid, { kind: "messages", sessionId: "session" })).toThrow();
  });
  it("详情校验任务身份、事件顺序/上界、任务授权关联，不将缺失解释为终态", () => {
    expect(parseHistoryTask(detail(), "old-task").task.result).toBe("真实结果");
    const value = detail();
    for (const invalid of [
      { ...value, task: task("other") }, { ...value, task: { ...task(), result: {} } },
      { ...value, events: [{ ...value.events[0], task_id: "other" }] },
      { ...value, events: [{ ...value.events[0], sequence: 11 }] },
      { ...value, events: [value.events[0], value.events[0]], history: { events: { total: 2, returned: 2, has_more: false } } },
      { ...value, history: { events: { total: 2, returned: 1, has_more: false } } },
    ]) expect(() => parseHistoryTask(invalid, "old-task")).toThrow(HistoryError);
  });
  it("计划、调整与回传保留结构，不使用通用JSON伪装缺失必需字段", () => {
    const common = { id: "record", session_id: "session", status: "completed", created_at: "then", updated_at: "now", source: task().source };
    const values = [
      ["plans", { ...common, title: "计划", execution_mode: "native", revision: 1, round: 1, max_rounds: 1, nodes: [{ id: "node", title: "节点", objective: "目标", depends_on: [] }] }],
      ["plan_changes", { ...common, plan_id: "plan", content: "调整", base_revision: 1, nodes: [] }],
      ["deliveries", { ...common, task_id: "task", target: task().source, attempts: 1, payload: { result: "回传" } }],
    ] as const;
    for (const [kind, value] of values) {
      expect(parseHistoryPage({ ...page(), items: [value], total: 1, returned: 1 }, { kind, sessionId: "session" }).returned).toBe(1);
      expect(() => parseHistoryPage({ ...page(), items: [{ ...value, session_id: undefined }], total: 1, returned: 1 }, { kind })).toThrow();
    }
  });
  it("详情请求范围不可省略或伪造零数，预算省略pending与正文独立", () => {
    const value = { ...detail(), history: { ...detail().history, execution_requests: { total: 2, returned: 0, has_more: true, omitted: 2, pending: 1, unavailable: [{ id: "large", reason: "record_too_large" }] } } };
    expect(parseHistoryTask(value, "old-task").history.execution_requests.pending).toBe(1);
    for (const meta of [undefined, { ...value.history.execution_requests, returned: 1 }, { ...value.history.execution_requests, omitted: 0 }, { ...value.history.execution_requests, pending: 3 }, { ...value.history.execution_requests, unavailable: [{ id: "large", reason: "secret" }] }]) expect(() => parseHistoryTask({ ...value, history: { ...value.history, execution_requests: meta } }, "old-task")).toThrow();
  });
});

describe("真实有界流式读取，不使用response.json或提升上限", () => {
  it("按字节消费分块UTF8，用只读GET且不发送token", async () => {
    const value = { ...page(), items: [task()], total: 1, returned: 1 };
    const bytes = new TextEncoder().encode(JSON.stringify(value));
    const result = new Response(new ReadableStream({ start(controller) { for (let i = 0; i < bytes.length; i += 3) controller.enqueue(bytes.subarray(i, i + 3)); controller.close(); } }), { headers: { "Content-Type": "application/json" } });
    const json = vi.spyOn(result, "json").mockRejectedValue(new Error("must not call"));
    const fetcher = vi.fn(async () => result);
    expect((await createHistoryClient(fetcher).page({ kind: "tasks" }, new AbortController().signal)).returned).toBe(1);
    expect(json).not.toHaveBeenCalled();
    expect(fetcher.mock.calls[0]).toEqual(["/api/v1/history/tasks", expect.objectContaining({ method: "GET", cache: "no-store", credentials: "same-origin", headers: { Accept: "application/json" } })]);
  });
  it.each([HISTORY_PAGE_BYTES, HISTORY_TASK_BYTES])("无content-length的%d字节上限在读取时强制执行并取消余流", async maximum => {
    const cancel = vi.fn(); let count = 0;
    const fetcher = vi.fn(async () => new Response(new ReadableStream({ pull(controller) { if (count++ === 0) controller.enqueue(new Uint8Array(maximum)); else controller.enqueue(new Uint8Array(1)); }, cancel }), { headers: { "Content-Type": "application/json" } }));
    const client = createHistoryClient(fetcher), signal = new AbortController().signal;
    await expect(maximum === HISTORY_PAGE_BYTES ? client.page({ kind: "tasks" }, signal) : client.task("old-task", signal)).rejects.toMatchObject({ code: "record_too_large" });
    expect(cancel).toHaveBeenCalledOnce(); expect(fetcher).toHaveBeenCalledOnce();
  });
  it("已知超限长度提前拒绝，错误内容不进入静态提示", async () => {
    const cancel = vi.fn();
    const fetcher = vi.fn(async () => new Response(new ReadableStream({ cancel }), { headers: { "Content-Type": "application/json", "Content-Length": `${HISTORY_PAGE_BYTES + 1}` } }));
    await expect(createHistoryClient(fetcher).page({ kind: "tasks" }, new AbortController().signal)).rejects.toMatchObject({ code: "record_too_large" });
    expect(cancel).toHaveBeenCalled();
    expect(historyErrorText(new Error("PRIVATE_CANARY"))).not.toContain("PRIVATE_CANARY");
  });
  it("恰好2 MiB合法页可以读取，超出即拒绝；错误媒体类型也释放流", async () => {
    const value = { ...page(), future_padding: "" };
    value.future_padding = "x".repeat(HISTORY_PAGE_BYTES - new TextEncoder().encode(JSON.stringify(value)).length);
    expect(new TextEncoder().encode(JSON.stringify(value))).toHaveLength(HISTORY_PAGE_BYTES);
    expect((await createHistoryClient(async () => response(value)).page({ kind: "tasks" }, new AbortController().signal)).total).toBe(0);
    const cancel = vi.fn();
    await expect(createHistoryClient(async () => new Response(new ReadableStream({ cancel }), { headers: { "Content-Type": "text/html" } })).page({ kind: "tasks" }, new AbortController().signal)).rejects.toMatchObject({ code: "invalid" });
    expect(cancel).toHaveBeenCalledOnce();
  });
  it.each([[409, "history_reset"], [413, "record_too_large"], [404, "not_found"]] as const)("HTTP%d区分静态错误%s，不重发请求", async (status, code) => {
    const fetcher = vi.fn(async () => response({ code, error: "PRIVATE_CANARY" }, status));
    const result = createHistoryClient(fetcher).task("old-task", new AbortController().signal);
    await expect(result).rejects.toMatchObject({ code });
    await result.catch(error => expect(String(error)).not.toContain("PRIVATE_CANARY"));
    expect(fetcher).toHaveBeenCalledOnce();
  });
  it("错误JSON仅在4 KiB以内采纳code，超限流立即取消且不回显正文", async () => {
    const value = { code: "not_found", error: "PRIVATE_CANARY", padding: "" };
    value.padding = "x".repeat(4096 - new TextEncoder().encode(JSON.stringify(value)).length);
    await expect(createHistoryClient(async () => response(value, 404)).task("old-task", new AbortController().signal)).rejects.toMatchObject({ code: "not_found" });
    for (const declared of [false, true]) {
      const cancel = vi.fn();
      const bytes = new TextEncoder().encode(JSON.stringify({ ...value, padding: value.padding + "x" }));
      const fetcher = vi.fn(async () => new Response(new ReadableStream({ start(controller) { controller.enqueue(bytes); }, cancel }), { status: 404, headers: { "Content-Type": "application/json", ...(declared ? { "Content-Length": `${bytes.length}` } : {}) } }));
      const result = createHistoryClient(fetcher).task("old-task", new AbortController().signal);
      await expect(result).rejects.toMatchObject({ code: "unavailable" });
      await result.catch(error => expect(String(error)).not.toContain("PRIVATE_CANARY"));
      expect(cancel).toHaveBeenCalledOnce(); expect(fetcher).toHaveBeenCalledOnce();
    }
  });
  it("取消的延迟响应不返回页面，无效UTF8/JSON返回安全错误", async () => {
    const controller = new AbortController();
    const client = createHistoryClient(async () => { controller.abort(); return response(page()); });
    await expect(client.page({ kind: "tasks" }, controller.signal)).rejects.toMatchObject({ code: "aborted" });
    for (const body of [new Uint8Array([0xff]), "{PRIVATE_CANARY"]) {
      const invalid = createHistoryClient(async () => new Response(body, { headers: { "Content-Type": "application/json" } }));
      await expect(invalid.page({ kind: "tasks" }, new AbortController().signal)).rejects.toThrow("历史读取未确认");
    }
  });
  it("404路由缺失/旧daemon/HTML不等于任务不存在，409/413也必须匹配可信code", async () => {
    for (const result of [response({}, 404), new Response("", { status: 404 }), new Response("PRIVATE_CANARY", { status: 404, headers: { "Content-Type": "text/html" } }), response({ code: "history_reset" }, 404), response({}, 409), response({}, 413)]) {
      await expect(createHistoryClient(async () => result).task("old-task", new AbortController().signal)).rejects.toMatchObject({ code: "unavailable" });
    }
  });
  it("真实本机TCP夹具只收到一次GET，保持规范分页参数和响应解析", async () => {
    const requests: string[] = [];
    const server = createServer((req, res) => { requests.push(`${req.method} ${req.url}`); res.writeHead(200, { "Content-Type": "application/json" }); const text = JSON.stringify(page("c1")); res.write(text.slice(0, 40)); res.end(text.slice(40)); });
    server.listen(0, "127.0.0.1"); await once(server, "listening");
    const address = server.address(); if (!address || typeof address === "string") throw new Error("fixture");
    try {
      const client = createHistoryClient((path, init) => fetch(`http://127.0.0.1:${address.port}${path}`, init));
      expect((await client.page({ kind: "tasks", sessionId: "session", cursor: "c1" }, new AbortController().signal)).anchor).toBe(anchor);
      expect(requests).toEqual(["GET /api/v1/history/tasks?session_id=session&cursor=c1"]);
    } finally { await new Promise<void>(resolve => server.close(() => resolve())); }
  });
});
