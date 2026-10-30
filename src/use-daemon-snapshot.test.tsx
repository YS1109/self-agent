// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { EMPTY_SNAPSHOT, DESKTOP_SOURCE, type Snapshot, type TaskRun } from "./types";
import type { StreamMessage } from "./event-stream";
const mocks = vi.hoisted(() => ({ api: vi.fn(), subscribe: vi.fn(), stop: vi.fn() }));
vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./event-stream", () => ({ subscribeEvents: mocks.subscribe }));
import { useDaemonSnapshot } from "./use-daemon-snapshot";
const task: TaskRun = { id: "t", session_id: "s", title: "task", status: "running", execution_mode: "native", created_at: "now", updated_at: "now", source: DESKTOP_SOURCE, steps: [] };
const snapshot = (cursor: number, text = ""): Snapshot => ({ ...EMPTY_SNAPSHOT, event_cursor: cursor, tasks: [{ ...task, stream_output: text }] });
let container: HTMLDivElement, root: Root, deliver: (message: StreamMessage) => void;
let current: ReturnType<typeof useDaemonSnapshot>;
function Probe() { current = useDaemonSnapshot(); return <div>{current.snapshot.tasks[0]?.stream_output}{current.recovering ? "恢复中" : "正常"}</div>; }
beforeEach(() => {
  vi.useFakeTimers(); vi.clearAllMocks(); (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
  mocks.subscribe.mockImplementation((_cursor, callback) => { deliver = callback; return mocks.stop; });
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.useRealTimers(); });
const overflow = () => { for (let n = 1; n <= 513; n++) deliver({ type: "event", event: { id: `e${n}`, sequence: n, task_id: "t", kind: "assistant_delta", message: "回复", created_at: "now", data: { kind: "assistant_delta", text: "x" } } }); };
it("预算溢出停止SSE并只读恢复，之后从权威cursor重新订阅", async () => {
  mocks.api.mockResolvedValueOnce(snapshot(0)).mockResolvedValue(snapshot(513, "权威完整回复"));
  await act(async () => root.render(<Probe />)); await act(async () => overflow());
  expect(mocks.stop).toHaveBeenCalledOnce(); expect(container.textContent).toBe("权威完整回复正常");
  expect(mocks.api.mock.calls.every(([method, path]) => method === "GET" && path === "/snapshot?window=1")).toBe(true);
  await act(async () => vi.advanceTimersByTimeAsync(1500)); expect(mocks.subscribe).toHaveBeenLastCalledWith(513, expect.any(Function));
});
it("溢出时已有旧GET在途，其迟到结果不能覆盖缺口；下轮GET恢复", async () => {
  let resolve!: (value: Snapshot) => void; const pending = new Promise<Snapshot>(r => { resolve = r; });
  mocks.api.mockResolvedValueOnce(snapshot(0)).mockReturnValueOnce(pending).mockResolvedValue(snapshot(513, "重新读取"));
  await act(async () => root.render(<Probe />));
  let refresh!: Promise<void>; await act(async () => { refresh = current.refresh(); }); await act(async () => overflow());
  await act(async () => { resolve(snapshot(0, "迟到旧内容")); await refresh; });
  expect(container.textContent).toContain("恢复中"); expect(container.textContent).not.toContain("迟到旧内容"); expect(mocks.subscribe).toHaveBeenCalledTimes(1);
  await act(async () => vi.advanceTimersByTimeAsync(1500)); expect(container.textContent).toBe("重新读取正常"); expect(mocks.subscribe).toHaveBeenLastCalledWith(513, expect.any(Function));
});
it("无效stream快照不会抛出到IPC回调，关闭观察后旧事件无效", async () => {
  mocks.api.mockResolvedValue(snapshot(0)); await act(async () => root.render(<Probe />));
  await act(async () => deliver({ type: "snapshot", snapshot: { ...snapshot(1), tasks: "bad" as unknown as TaskRun[] } }));
  expect(mocks.stop).toHaveBeenCalled(); expect(mocks.api).toHaveBeenCalledTimes(2);
  await act(async () => root.unmount()); root = createRoot(container);
  await act(async () => deliver({ type: "event", event: { id: "late", sequence: 1, kind: "task_completed", message: "late", created_at: "now" } }));
  expect(container.textContent).toBe("");
});
