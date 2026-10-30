import { EMPTY_SNAPSHOT, type RunEvent, type Snapshot, type TaskRun } from "./types";

const MAX_PENDING_EVENTS = 512;
const MAX_PENDING_BYTES = 2 * 1024 * 1024;
const MAX_TEXT_BYTES = 256 * 1024;
const MAX_EVENT_BYTES = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES = 12 * 1024 * 1024;
const MAX_LIVE_BYTES = 16 * 1024 * 1024;
const encoder = new TextEncoder();

export interface EventState {
  snapshot: Snapshot;
  cursor: number;
  baseCursor: number;
  replayFloor: number;
  pending: RunEvent[];
  pendingBytes: number;
  snapshotBytes: number;
  eventBytes: number;
  eventSizes: number[];
  needsSnapshot: boolean;
  recoveryFloor: number;
}

export function validCursor(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

export function createEventState(): EventState {
  return { snapshot: EMPTY_SNAPSHOT, cursor: 0, baseCursor: 0, replayFloor: 0, pending: [], pendingBytes: 0, snapshotBytes: 0, eventBytes: 0, eventSizes: [], needsSnapshot: false, recoveryFloor: 0 };
}

function appendText(task: TaskRun, text: string): TaskRun {
  if (task.stream_output_truncated) return task;
  const bytes = encoder.encode((task.stream_output ?? "") + text);
  if (bytes.length <= MAX_TEXT_BYTES) return { ...task, stream_output: new TextDecoder().decode(bytes) };
  let end = MAX_TEXT_BYTES;
  // Exclude a partially retained UTF-8 code point rather than adding U+FFFD.
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end--;
  return { ...task, stream_output: new TextDecoder().decode(bytes.subarray(0, end)), stream_output_truncated: true };
}

function reduceTask(task: TaskRun, event: RunEvent): TaskRun {
  if (event.task_id !== task.id) return task;
  const terminal = ["completed", "failed", "cancelled"].includes(task.status);
  if (event.data?.kind === "assistant_delta") {
    return terminal ? task : appendText(task, event.data.text);
  }
  if (event.data?.kind === "context_estimate") {
    if (terminal) return task;
    const { kind: _kind, ...estimate } = event.data;
    return { ...task, context_estimate: { ...estimate, updated_at: event.created_at } };
  }
  if (event.data?.kind === "context_compaction") {
    return terminal ? task : { ...task, context_compaction: { state: event.data.state, updated_at: event.created_at } };
  }
  if (event.kind === "task_started") return terminal ? task : { ...task, status: "running", updated_at: event.created_at };
  if (["task_completed", "task_failed", "task_cancelled"].includes(event.kind)) {
    if (terminal) return task;
    const status = event.kind.slice(5);
    return { ...task, status, updated_at: event.created_at, ...(status === "completed" ? { result: event.message } : { error: event.message }) };
  }
  return task;
}

const encodedBytes = (value: unknown) => encoder.encode(JSON.stringify(value)).length;
function requireSnapshot(state: EventState, sequence: number): EventState {
  return { ...state, needsSnapshot: true, recoveryFloor: Math.max(state.recoveryFloor, sequence) };
}
function reduceEvent(state: EventState, event: RunEvent): EventState {
  let changedBytes = 0;
  const tasks = state.snapshot.tasks.map(task => {
    const changed = reduceTask(task, event);
    if (changed !== task) changedBytes += encodedBytes(changed) - encodedBytes(task);
    return changed;
  });
  const events = [...state.snapshot.events, event], eventSizes = [...state.eventSizes, encodedBytes(event) + 1];
  let eventBytes = state.eventBytes + eventSizes[eventSizes.length - 1];
  while (events.length > 300 || eventBytes > MAX_EVENT_BYTES) { events.shift(); eventBytes -= eventSizes.shift()!; }
  // JSON cursor/framing growth is conservatively charged, reset by each snapshot.
  const snapshotBytes = state.snapshotBytes + changedBytes + eventBytes - state.eventBytes + 64;
  if (snapshotBytes > MAX_LIVE_BYTES) return requireSnapshot(state, event.sequence!);
  return { ...state, snapshot: { ...state.snapshot, event_cursor: event.sequence, tasks, events }, snapshotBytes, eventBytes, eventSizes };
}
export function receiveEvent(state: EventState, event: RunEvent): EventState {
  if (!validCursor(event.sequence) || event.sequence <= state.cursor) return state;
  if (state.needsSnapshot) return requireSnapshot(state, event.sequence);
  const bytes = encodedBytes(event);
  // Do not advance the applied cursor or silently forget a delta when replay
  // capacity is exhausted. The hook closes SSE and gets a fresh read-only base.
  if (state.pending.length >= MAX_PENDING_EVENTS || state.pendingBytes + bytes > MAX_PENDING_BYTES) return requireSnapshot(state, event.sequence);
  const reduced = reduceEvent(state, event);
  if (reduced.needsSnapshot) return reduced;
  return { ...reduced, cursor: event.sequence, pending: [...state.pending, event], pendingBytes: state.pendingBytes + bytes };
}

export function validateSnapshot(incoming: Snapshot): number {
  const bytes = encodedBytes(incoming);
  if (bytes > MAX_SNAPSHOT_BYTES) throw new Error("后台快照超过12 MiB限制，未采用不完整数据");
  for (const field of ["sessions", "tasks", "events", "deliveries", "selections", "capabilities", "plans", "plan_changes", "execution_requests", "context_states"] as const) {
    const values = incoming[field];
    if (values === undefined && ["plans", "plan_changes", "execution_requests", "context_states"].includes(field)) continue;
    if (!Array.isArray(values) || values.length > 300) throw new Error("后台窗口集合无效或超过数量限制");
  }
  const history = incoming.history;
  if (Object.hasOwn(incoming, "history") && !history) throw new Error("后台历史窗口元数据无效");
  if (history) {
    if (history.version !== 1 || !/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(history.anchor) || !history.collections || !history.session_messages || Array.isArray(history.collections) || Array.isArray(history.session_messages)) throw new Error("后台历史窗口元数据无效");
    for (const field of ["sessions", "tasks", "events", "deliveries", "selections", "plans", "plan_changes", "execution_requests", "context_states"]) {
      if (!Object.hasOwn(history.collections, field) || !Array.isArray((incoming as unknown as Record<string, unknown>)[field])) throw new Error("后台历史窗口缺少集合范围元数据");
    }
    for (const [field, meta] of Object.entries(history.collections)) {
      const values = (incoming as unknown as Record<string, unknown>)[field];
      if (!meta || !validCursor(meta.total) || !validCursor(meta.returned) || meta.total < meta.returned || typeof meta.has_more !== "boolean" || meta.has_more !== (meta.total > meta.returned)
        || Array.isArray(values) && meta.returned !== values.length
        || [meta.pending, meta.omitted, meta.pinned, meta.pinned_pending].some(value => value !== undefined && (!validCursor(value) || value > meta.total))) throw new Error("后台历史窗口计数不一致");
    }
    for (const session of incoming.sessions) {
      const meta = history.session_messages[session.id];
      if (!Object.hasOwn(history.session_messages, session.id) || !meta || !validCursor(meta.total) || !validCursor(meta.offset) || meta.offset > meta.total || meta.returned !== session.messages.length || typeof meta.has_more_before !== "boolean") throw new Error("后台会话消息窗口无效");
    }
  }
  return bytes;
}

/** Snapshot and cursor are atomic in Rust. Historical pages never enter here.
 * Replay newer committed events, or reject a base older than a recovery gap. */
export function receiveSnapshot(state: EventState, incoming: Snapshot): EventState {
  const bytes = validateSnapshot(incoming);
  if (incoming.history && state.snapshot.history && incoming.history.anchor !== state.snapshot.history.anchor) state = createEventState();
  const cursor = incoming.event_cursor;
  if (!validCursor(cursor)) {
    return state.cursor ? state : { ...state, snapshot: { ...incoming, plans: incoming.plans ?? [], plan_changes: incoming.plan_changes ?? [] }, snapshotBytes: bytes };
  }
  if (cursor < state.baseCursor || cursor < state.replayFloor || state.needsSnapshot && cursor < state.recoveryFloor) return state;
  const pending = state.pending.filter(event => event.sequence! > cursor);
  const events = [...incoming.events], eventSizes = events.map(event => encodedBytes(event) + 1);
  let eventBytes = eventSizes.reduce((sum, size) => sum + size, 0), removedBytes = 0;
  while (eventBytes > MAX_EVENT_BYTES) { events.shift(); const removed = eventSizes.shift()!; eventBytes -= removed; removedBytes += removed; }
  let next: EventState = { snapshot: { ...incoming, events, plans: incoming.plans ?? [], plan_changes: incoming.plan_changes ?? [] }, cursor, baseCursor: cursor, replayFloor: cursor, pending: [], pendingBytes: 0, snapshotBytes: bytes - removedBytes, eventBytes, eventSizes, needsSnapshot: false, recoveryFloor: 0 };
  for (const event of pending) next = receiveEvent(next, event);
  return next;
}

export interface ToolActivityItem {
  id: string;
  callId: string;
  name: string;
  status: "running" | "completed" | "failed" | "interrupted";
  output?: string;
  missingStart?: boolean;
}

/** Pair the latest unmatched call, not just a call ID: executors may reuse an
 * ID in later turns. Missing older records are explicit, never invented. */
export function taskTools(events: RunEvent[], task: TaskRun): ToolActivityItem[] {
  const tools: ToolActivityItem[] = [];
  for (const event of events) {
    if (event.task_id !== task.id) continue;
    const data = event.data;
    if (data?.kind === "tool_started") {
      tools.push({ id: event.id, callId: data.call_id, name: data.name, status: "running" });
    } else if (data?.kind === "tool_finished") {
      const current = [...tools].reverse().find((tool) => tool.callId === data.call_id && tool.name === data.name && tool.status === "running");
      if (current) {
        current.status = data.success ? "completed" : "failed";
        current.output = data.output;
      } else {
        tools.push({ id: event.id, callId: data.call_id, name: data.name, status: data.success ? "completed" : "failed", output: data.output, missingStart: true });
      }
    }
  }
  if (["completed", "failed", "cancelled"].includes(task.status)) {
    for (const tool of tools) if (tool.status === "running") tool.status = "interrupted";
  }
  return tools.slice(-30);
}
