// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { DESKTOP_SOURCE, REMOTE_SOURCE, EMPTY_SNAPSHOT, type Snapshot, type TaskRun, type Plan } from "./types";
const mocks = vi.hoisted(() => ({ snapshot: undefined as Snapshot | undefined, refresh: vi.fn(async () => {}), api: vi.fn(), dispatch: vi.fn(), page: vi.fn(), task: vi.fn() }));
vi.mock("./use-daemon-snapshot", () => ({ useDaemonSnapshot: () => ({ snapshot: mocks.snapshot, connected: true, checked: true, connectionError: "", eventStatus: "connected", recovering: false, refresh: mocks.refresh }) }));
vi.mock("./api", () => ({ api: mocks.api, dispatch: mocks.dispatch, historyClient: { page: mocks.page, task: mocks.task }, capabilityClient: {}, providersClient: {}, mcpCredentialClient: {}, qqClient: {}, qqCredentialClient: {}, gatewayClient: {}, gatewayCredentialClient: {}, reliabilityClient: {} }));
vi.mock("./components/model-select", () => ({ QuickModelSelect: () => null, useModelCatalog: () => ({ models: [], loading: false }) }));
vi.mock("./components/ui/select", () => ({ Select: ({ value, options, onValueChange, disabled, "aria-label": label }: { value: string; options: { value: string; label: string; disabled?: boolean }[]; onValueChange: (value: string) => void; disabled?: boolean; "aria-label"?: string }) => <select value={value} disabled={disabled} aria-label={label} onChange={event => onValueChange(event.target.value)}>{options.map(item => <option key={item.value} value={item.value} disabled={item.disabled}>{item.label}</option>)}</select> }));
import App from "./App";
const anchor = "10000000-0000-4000-8000-000000000001";
const task = (id: string): TaskRun => ({ id, session_id: "s", task_kind: "main", title: `任务-${id}`, status: "completed", execution_mode: "native", created_at: "2026-01-01", updated_at: "2026-01-01", source: DESKTOP_SOURCE, steps: [], result: `结果-${id}` });
function snapshot(tasks = [task("chosen"), task("other")]): Snapshot {
  return { ...EMPTY_SNAPSHOT, tasks, sessions: [{ id: "s", title: "会话", project_path: "/fixture", created_at: "2026-01-01", updated_at: "2026-01-01", messages: [] }], selections: [{ source: DESKTOP_SOURCE, session_id: "s" }], history: { version: 1, anchor, collections: { sessions: { total: 100, returned: 1, has_more: true }, tasks: { total: 5000, returned: tasks.length, has_more: true, pending: 2 }, execution_requests: { total: 100, returned: 0, has_more: true, pending: 3 } }, session_messages: { s: { offset: 200, total: 200, returned: 0, has_more_before: true } } } };
}
let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.clearAllMocks(); (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() }) });
  Object.defineProperty(Element.prototype, "scrollTo", { configurable: true, value: vi.fn() });
  mocks.api.mockRejectedValue(new Error("fixture settings unavailable")); mocks.snapshot = snapshot();
  mocks.page.mockResolvedValue({ items: [], total: 0, returned: 0, has_more: false, current_cursor: "abc", next_cursor: null, anchor, event_cursor: 0, unavailable: [], session_messages: {} });
  mocks.task.mockResolvedValue({ task: task("chosen"), event_cursor: 10, events: [], execution_requests: [], history: { events: { total: 0, returned: 0, has_more: false }, execution_requests: {total:0,returned:0,has_more:false,omitted:0,pending:0,unavailable:[]} } });
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); });
it("实际App展示总量/未载入消息与审批提示，不把历史窗口假装完整", async () => {
  await act(async () => root.render(<App />));
  expect(container.textContent).toContain("已载入任务 2 / 5000"); expect(container.textContent).toContain("3 条待处理问题或授权未载入");
  expect(container.textContent).toContain("当前载入 0 / 200 条消息"); expect(container.textContent).not.toContain("准备好了，告诉我你的目标");
  const open = [...container.querySelectorAll("button")].find(button => button.textContent === "浏览完整历史（分页）")!;
  await act(async () => open.click()); expect(mocks.page).toHaveBeenCalledWith({ kind: "sessions", sessionId: undefined, cursor: undefined }, expect.any(AbortSignal));
  expect(mocks.dispatch).not.toHaveBeenCalled();
});
it("已选session不在window时明确未载入，不显示新会话welcome或切其他session", async () => {
  mocks.snapshot = { ...snapshot(), sessions: [] };
  await act(async () => root.render(<App />));
  expect(container.textContent).toContain("所选会话未载入当前窗口"); expect(container.textContent).not.toContain("想法在前"); expect(mocks.dispatch).not.toHaveBeenCalled();
});
it("已选task离窗后按原ID查询detail，不静默回退到第一条其他task", async () => {
  await act(async () => root.render(<App />));
  const choose = [...container.querySelectorAll<HTMLButtonElement>("button.task-card")].find(button => button.textContent?.includes("任务-chosen"))!;
  await act(async () => choose.click());
  mocks.snapshot = snapshot([task("other")]); await act(async () => root.render(<App />));
  expect(mocks.task).toHaveBeenCalledWith("chosen", expect.any(AbortSignal)); expect(container.textContent).toContain("不会静默切换其他任务");
  expect(container.querySelector(".work-panel")?.textContent).toContain("结果-chosen"); expect(container.querySelector(".work-panel")?.textContent).not.toContain("结果-other");
});
it("远程计划A已有草稿后离窗，不能静默把草稿或暂停目标转向B；缺页选择保持未知", async () => {
  const plan = (id: string): Plan => ({ id, session_id: "s", title: `计划-${id}`, source: REMOTE_SOURCE, execution_mode: "native", status: "running", revision: 1, round: 1, max_rounds: 2, created_at: "2026-01-01", updated_at: "2026-01-01", nodes: [] });
  mocks.snapshot = { ...snapshot(), plans: [plan("A"), plan("B")], selections: [{ source: DESKTOP_SOURCE, session_id: "s" }, { source: REMOTE_SOURCE, session_id: "s" }] };
  await act(async () => root.render(<App />));
  const open = [...container.querySelectorAll("button")].find(button => button.textContent?.startsWith("远程入口"))!; await act(async () => open.click());
  const mode = document.querySelector<HTMLSelectElement>('[aria-label="远程指令用途"]')!;
  await act(async () => { mode.value = "change"; mode.dispatchEvent(new Event("change", { bubbles: true })); });
  const input = document.querySelector<HTMLTextAreaElement>('[aria-label="远程指令内容"]')!;
  await act(async () => { Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input, "只补充计划A"); input.dispatchEvent(new Event("input", { bubbles: true })); });
  mocks.snapshot = { ...mocks.snapshot!, plans: [plan("B")] }; await act(async () => root.render(<App />));
  expect(document.body.textContent).toContain("所选计划 A 未载入当前窗口"); expect(input.value).toBe("只补充计划A");
  const send = [...document.querySelectorAll("button")].find(button => button.textContent?.includes("生成调整建议"))!;
  expect(send.disabled).toBe(true); expect([...document.querySelectorAll("button")].some(button => button.textContent === "暂停派发")).toBe(false);
  await act(async () => send.click()); expect(mocks.dispatch).not.toHaveBeenCalled();
  mocks.snapshot = { ...mocks.snapshot!, selections: [{ source: DESKTOP_SOURCE, session_id: "s" }], history: { ...mocks.snapshot!.history!, collections: { ...mocks.snapshot!.history!.collections, selections: { total: 2, returned: 1, has_more: true } } } };
  await act(async () => root.render(<App />)); expect(document.body.textContent).toContain("此远程入口的会话选择尚未载入"); expect(input.value).toBe("只补充计划A"); expect(mocks.dispatch).not.toHaveBeenCalled();
});
