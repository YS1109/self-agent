import { EMPTY_SNAPSHOT, type RunEvent, type Snapshot, type TaskRun } from "./types";

const MAX_PENDING_EVENTS = 512;
const MAX_PENDING_BYTES = 2 * 1024 * 1024;
const MAX_TEXT_BYTES = 256 * 1024;
const encoder = new TextEncoder();

export interface EventState {
  snapshot: Snapshot;
  cursor: number;
  baseCursor: number;
  replayFloor: number;
  pending: RunEvent[];
  pendingBytes: number;
}

export function validCursor(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

export function createEventState(): EventState {
  return { snapshot: EMPTY_SNAPSHOT, cursor: 0, baseCursor: 0, replayFloor: 0, pending: [], pendingBytes: 0 };
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
  if (event.kind === "task_started") return terminal ? task : { ...task, status: "running", updated_at: event.created_at };
  if (["task_completed", "task_failed", "task_cancelled"].includes(event.kind)) {
    if (terminal) return task;
    const status = event.kind.slice(5);
    return { ...task, status, updated_at: event.created_at, ...(status === "completed" ? { result: event.message } : { error: event.message }) };
  }
  return task;
}

function reduceEvent(snapshot: Snapshot, event: RunEvent): Snapshot {
  return {
    ...snapshot,
    event_cursor: event.sequence,
    tasks: snapshot.tasks.map((task) => reduceTask(task, event)),
    events: [...snapshot.events, event].slice(-300),
  };
}

export function receiveEvent(state: EventState, event: RunEvent): EventState {
  if (!validCursor(event.sequence) || event.sequence <= state.cursor) return state;
  const pending = [...state.pending, event];
  let pendingBytes = state.pendingBytes + encoder.encode(JSON.stringify(event)).length;
  let replayFloor = state.replayFloor;
  while (pending.length > MAX_PENDING_EVENTS || pendingBytes > MAX_PENDING_BYTES) {
    const removed = pending.shift()!;
    pendingBytes -= encoder.encode(JSON.stringify(removed)).length;
    replayFloor = removed.sequence!;
  }
  return {
    ...state,
    snapshot: reduceEvent(state.snapshot, event),
    cursor: event.sequence,
    replayFloor,
    pending,
    pendingBytes,
  };
}

/** Snapshot and its cursor are atomic in Rust. Replay only newer committed
 * events received while this HTTP snapshot was in flight, never append snapshot
 * text as if it were a delta. Out-of-order/too-old snapshots cannot roll back UI. */
export function receiveSnapshot(state: EventState, incoming: Snapshot): EventState {
  const cursor = incoming.event_cursor;
  if (!validCursor(cursor)) {
    // Legacy daemons still support polling; never overwrite a sequenced stream.
    return state.cursor ? state : { ...state, snapshot: { ...incoming, plans: incoming.plans ?? [], plan_changes: incoming.plan_changes ?? [] } };
  }
  if (cursor < state.baseCursor || cursor < state.replayFloor) return state;
  let snapshot: Snapshot = { ...incoming, plans: incoming.plans ?? [], plan_changes: incoming.plan_changes ?? [] };
  const pending = state.pending.filter((event) => event.sequence! > cursor);
  for (const event of pending) snapshot = reduceEvent(snapshot, event);
  return {
    snapshot,
    cursor: Math.max(cursor, state.cursor),
    baseCursor: cursor,
    replayFloor: cursor,
    pending,
    pendingBytes: pending.reduce((size, event) => size + encoder.encode(JSON.stringify(event)).length, 0),
  };
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
