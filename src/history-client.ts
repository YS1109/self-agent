import type { Delivery, ExecutionRequest, Message, Plan, PlanChange, RunEvent, Session, TaskRun } from "./types";

export const HISTORY_PAGE_BYTES = 2 * 1024 * 1024;
export const HISTORY_TASK_BYTES = 8 * 1024 * 1024;
export const HISTORY_KINDS = ["sessions", "tasks", "plans", "plan_changes", "deliveries", "execution_requests", "messages"] as const;
export type HistoryKind = typeof HISTORY_KINDS[number];
export type HistoryMessage = Message & { history_offset: number };
export type HistoryItem = Session | TaskRun | Plan | PlanChange | Delivery | ExecutionRequest | HistoryMessage;
export interface HistoryMessageWindow {
  offset: number; total: number; returned: number; has_more_before: boolean;
  unavailable?: { id: string; offset: number; reason: "record_too_large" }[];
}
export interface HistoryQuery { kind: HistoryKind; sessionId?: string; cursor?: string }
export interface HistoryPage {
  items: HistoryItem[]; total: number; returned: number; has_more: boolean;
  current_cursor: string; next_cursor: string | null; anchor: string; event_cursor: number; offset?: number;
  unavailable: { id: string; reason: "record_too_large"; offset?: number }[];
  session_messages?: Record<string, HistoryMessageWindow>;
}
export interface HistoryTaskDetail {
  task: TaskRun; event_cursor: number; events: RunEvent[]; execution_requests: ExecutionRequest[];
  history: {
    events: { total: number; returned: number; has_more: boolean };
    execution_requests: { total: number; returned: number; has_more: boolean; omitted: number; pending: number; unavailable: { id: string; reason: "record_too_large" }[] };
  };
}
export interface HistoryClient {
  page(query: HistoryQuery, signal: AbortSignal): Promise<HistoryPage>;
  task(id: string, signal: AbortSignal): Promise<HistoryTaskDetail>;
}
export type HistoryErrorCode = "history_reset" | "record_too_large" | "not_found" | "invalid" | "unavailable" | "aborted";
const ERRORS: Record<HistoryErrorCode, string> = {
  history_reset: "后台历史已重置，旧分页位置已清除。请重新读取首页；没有重放任何指令。",
  record_too_large: "记录超过读取上限，未伪造或截断内容。请通过所有者离线备份检查完整记录。",
  not_found: "后台确认该记录不存在。",
  invalid: "历史数据或分页位置无效，未采用不完整响应。请重新读取首页。",
  unavailable: "历史读取未确认，请稍后只读刷新；未显示底层错误。",
  aborted: "已结束本次历史读取。",
};
export class HistoryError extends Error {
  constructor(readonly code: HistoryErrorCode) { super(ERRORS[code]); this.name = "HistoryError"; }
}
export const historyErrorText = (error: unknown) => error instanceof HistoryError ? error.message : ERRORS.unavailable;
export const safeHistoryId = (value: string) => /^[A-Za-z0-9_-]{1,128}$/.test(value);
export const validHistoryCursor = (value: unknown): value is string => typeof value === "string" && /^[A-Za-z0-9_-]{1,2048}$/.test(value);
const epoch = (v: unknown) => typeof v === "string" && /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(v);
const object = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
const integer = (v: unknown): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
const text = (v: unknown): v is string => typeof v === "string";
const id = (v: unknown): v is string => text(v) && v.length > 0 && Array.from(v).length <= 128;
const strings = (v: Record<string, unknown>, keys: string[]) => keys.every(key => text(v[key]));
const optionalText = (v: unknown) => v == null || text(v);
const source = (v: unknown) => object(v) && strings(v, ["channel_id", "actor_id", "conversation_id"]);
const message = (v: unknown) => object(v) && id(v.id) && strings(v, ["content", "created_at"]) && ["user", "assistant", "system"].includes(v.role as string) && optionalText(v.task_id);
const node = (v: unknown) => object(v) && id(v.id) && strings(v, ["title", "objective"]) && Array.isArray(v.depends_on) && v.depends_on.every(text);
function messageOffsets(items: unknown[], unavailable: unknown[], offset: number, total: number): boolean {
  let previous = -1;
  const positions = new Set<number>();
  for (const value of items) {
    if (!object(value) || !integer(value.history_offset) || value.history_offset < offset || value.history_offset >= total || value.history_offset <= previous) return false;
    previous = value.history_offset; positions.add(value.history_offset);
  }
  for (const value of unavailable) {
    if (!object(value) || !id(value.id) || value.reason !== "record_too_large" || !integer(value.offset) || value.offset < offset || value.offset >= total || positions.has(value.offset)) return false;
    positions.add(value.offset);
  }
  for (let position = offset; position < offset + positions.size; position++) if (!positions.has(position)) return false;
  return true;
}
function task(v: unknown): v is TaskRun {
  return object(v) && id(v.id) && id(v.session_id) && strings(v, ["title", "status", "created_at", "updated_at"])
    && ["native", "demo"].includes(v.execution_mode as string) && source(v.source)
    && Array.isArray(v.steps) && v.steps.every(s => object(s) && id(s.id) && strings(s, ["title", "status"]) && optionalText(s.detail))
    && [v.result, v.error, v.stream_output, v.plan_id, v.task_kind, v.execution_cleanup].every(optionalText)
    && (v.stream_output_truncated === undefined || typeof v.stream_output_truncated === "boolean")
    && (v.evidence === undefined || Array.isArray(v.evidence) && v.evidence.every(e => object(e) && strings(e, ["command", "output"]) && Number.isSafeInteger(e.exit_code)));
}
function request(v: unknown): v is ExecutionRequest {
  return object(v) && id(v.id) && id(v.task_id) && id(v.session_id) && text(v.prompt)
    && ["question", "approval"].includes(v.kind as string) && ["pending", "answered", "approved", "denied", "expired", "cancelled"].includes(v.status as string)
    && integer(v.revision) && integer(v.expires_at_ms)
    && (v.answer === null || text(v.answer))
    && (v.kind === "question" ? v.action === null && v.action_digest === null : object(v.action) && text(v.action_digest) && v.action_digest.length > 0);
}
function item(v: unknown, kind: HistoryKind): v is HistoryItem {
  if (!object(v) || !id(v.id)) return false;
  if (kind === "messages") return message(v);
  if (kind === "tasks") return task(v);
  if (kind === "execution_requests") return request(v);
  if (kind === "sessions") return strings(v, ["title", "project_path", "created_at", "updated_at"]) && Array.isArray(v.messages) && v.messages.every(message);
  if (!id(v.session_id) || !strings(v, ["status", "created_at", "updated_at"])) return false;
  if (kind === "deliveries") return id(v.task_id) && source(v.target) && integer(v.attempts) && Object.hasOwn(v, "payload");
  if (!source(v.source) || !Array.isArray(v.nodes) || !v.nodes.every(node)) return false;
  return kind === "plans" ? text(v.title) && integer(v.revision) && v.revision > 0 && integer(v.round) && integer(v.max_rounds) && ["native", "demo"].includes(v.execution_mode as string)
    : id(v.plan_id) && text(v.content) && integer(v.base_revision);
}
export function historyPagePath(query: HistoryQuery): string {
  if (!HISTORY_KINDS.includes(query.kind) || query.sessionId !== undefined && !safeHistoryId(query.sessionId)
    || query.cursor !== undefined && !validHistoryCursor(query.cursor) || query.kind === "messages" && !query.sessionId) throw new HistoryError("invalid");
  const parameters = new URLSearchParams();
  if (query.kind !== "messages" && query.sessionId) parameters.set("session_id", query.sessionId);
  if (query.cursor) parameters.set("cursor", query.cursor);
  return `/api/v1/history/${query.kind === "messages" ? `sessions/${query.sessionId}/messages` : query.kind}${parameters.size ? `?${parameters}` : ""}`;
}
export function historyTaskPath(value: string): string {
  if (!safeHistoryId(value)) throw new HistoryError("invalid");
  return `/api/v1/tasks/${value}`;
}
export function parseHistoryPage(value: unknown, query: HistoryQuery): HistoryPage {
  historyPagePath(query);
  if (!object(value) || !Array.isArray(value.items) || !Array.isArray(value.unavailable) || !integer(value.total)
    || value.returned !== value.items.length || typeof value.has_more !== "boolean" || !epoch(value.anchor) || !integer(value.event_cursor)
    || !validHistoryCursor(value.current_cursor) || query.cursor !== undefined && value.current_cursor !== query.cursor
    || (value.next_cursor !== null && !validHistoryCursor(value.next_cursor)) || value.has_more !== (value.next_cursor !== null)
    || value.next_cursor !== null && value.next_cursor === value.current_cursor
    || value.items.length + value.unavailable.length > 80 || value.total < value.items.length + value.unavailable.length
    || value.has_more && value.items.length + value.unavailable.length === 0
    || value.has_more && value.total <= value.items.length + value.unavailable.length
    || query.kind === "messages" && (!integer(value.offset) || value.offset + value.items.length + value.unavailable.length > value.total)
    || !value.items.every(v => item(v, query.kind))
    || !value.unavailable.every(v => object(v) && id(v.id) && v.reason === "record_too_large")) throw new HistoryError("invalid");
  const all = [...value.items, ...value.unavailable] as { id: string; session_id?: string }[];
  if (new Set(all.map(v => v.id)).size !== all.length
    || query.sessionId && value.items.some(v => query.kind === "sessions" ? v.id !== query.sessionId : query.kind !== "messages" && (!("session_id" in v) || v.session_id !== query.sessionId))) throw new HistoryError("invalid");
  if (query.kind === "messages" && (value.has_more !== ((value.offset as number) > 0) || !messageOffsets(value.items, value.unavailable, value.offset as number, value.total))) throw new HistoryError("invalid");
  if (query.kind === "sessions") {
    if (!object(value.session_messages)) throw new HistoryError("invalid");
    for (const session of value.items as Session[]) {
      const meta = value.session_messages[session.id];
      if (!Object.hasOwn(value.session_messages, session.id) || !object(meta) || !integer(meta.offset) || !integer(meta.total) || meta.offset > meta.total
        || meta.returned !== session.messages.length || typeof meta.has_more_before !== "boolean" || !Array.isArray(meta.unavailable ?? [])
        || meta.offset + session.messages.length + (meta.unavailable as unknown[] | undefined ?? []).length > meta.total
        || !messageOffsets(session.messages, meta.unavailable as unknown[] | undefined ?? [], meta.offset, meta.total)) throw new HistoryError("invalid");
    }
  }
  return value as unknown as HistoryPage;
}
export function parseHistoryTask(value: unknown, expectedId: string): HistoryTaskDetail {
  historyTaskPath(expectedId);
  if (!object(value) || !task(value.task) || value.task.id !== expectedId || !integer(value.event_cursor)
    || !Array.isArray(value.events) || !Array.isArray(value.execution_requests) || !object(value.history) || !object(value.history.events) || !object(value.history.execution_requests)) throw new HistoryError("invalid");
  const events = value.events, requests = value.execution_requests, meta = value.history.events, requestMeta = value.history.execution_requests, taskSession = value.task.session_id;
  if (!integer(meta.total) || meta.returned !== events.length || meta.total < events.length || meta.has_more !== (meta.total > events.length)
    || !requests.every(v => request(v) && v.task_id === expectedId && v.session_id === taskSession)
    || new Set(requests.map(v => v.id)).size !== requests.length) throw new HistoryError("invalid");
  if (!integer(requestMeta.total) || requestMeta.returned !== requests.length || requestMeta.total < requests.length
    || requestMeta.has_more !== (requestMeta.total > requests.length) || requestMeta.omitted !== requestMeta.total - requests.length
    || !integer(requestMeta.pending) || requestMeta.pending > (requestMeta.omitted as number)
    || !Array.isArray(requestMeta.unavailable) || requestMeta.unavailable.length > 80 || requestMeta.unavailable.length > (requestMeta.omitted as number)
    || !requestMeta.unavailable.every(v => object(v) && id(v.id) && v.reason === "record_too_large")
    || new Set([...requests.map(v => v.id), ...requestMeta.unavailable.map(v => v.id)]).size !== requests.length + requestMeta.unavailable.length) throw new HistoryError("invalid");
  let last = 0;
  for (const event of events) {
    if (!object(event) || !id(event.id) || !strings(event, ["kind", "message", "created_at"]) || event.task_id !== expectedId
      || !integer(event.sequence) || event.sequence <= last || event.sequence > value.event_cursor) throw new HistoryError("invalid");
    last = event.sequence;
  }
  return value as unknown as HistoryTaskDetail;
}

/** This read-only transport never receives a daemon bearer token. Desktop
 * integration must inject a bounded Rust bridge client, not fetch around it. */
export function createHistoryClient(fetcher: (path: string, init: RequestInit) => Promise<Response> = fetch): HistoryClient {
  async function get(path: string, maximum: number, signal: AbortSignal): Promise<unknown> {
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      signal.throwIfAborted();
      const response = await fetcher(path, { method: "GET", signal: AbortSignal.any([signal, AbortSignal.timeout(20_000)]), cache: "no-store", credentials: "same-origin", headers: { Accept: "application/json" } });
      const limit = response.ok ? maximum : 4096;
      const length = response.headers.get("content-length");
      if (!response.body) throw new HistoryError(response.ok ? "invalid" : "unavailable");
      reader = response.body.getReader();
      if (response.headers.get("content-type")?.split(";")[0].trim() !== "application/json") throw new HistoryError(response.ok ? "invalid" : "unavailable");
      if (length && /^\d+$/.test(length) && Number(length) > limit) throw new HistoryError(response.ok ? "record_too_large" : "unavailable");
      const chunks: Uint8Array[] = []; let bytes = 0;
      while (true) {
        const chunk = await reader.read(); signal.throwIfAborted();
        if (chunk.done) break;
        bytes += chunk.value.byteLength;
        if (bytes > limit) throw new HistoryError(response.ok ? "record_too_large" : "unavailable");
        chunks.push(chunk.value);
      }
      const joined = new Uint8Array(bytes); let offset = 0;
      for (const chunk of chunks) { joined.set(chunk, offset); offset += chunk.byteLength; }
      const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(joined));
      if (!response.ok) {
        if (response.status === 409 && object(value) && value.code === "history_reset") throw new HistoryError("history_reset");
        if (response.status === 413 && object(value) && value.code === "record_too_large") throw new HistoryError("record_too_large");
        throw new HistoryError(response.status === 404 && object(value) && value.code === "not_found" ? "not_found" : "unavailable");
      }
      return value;
    } catch (error) {
      if (signal.aborted) throw new HistoryError("aborted");
      throw error instanceof HistoryError ? error : new HistoryError("unavailable");
    } finally {
      if (reader) { await reader.cancel().catch(() => {}); reader.releaseLock(); }
    }
  }
  return {
    async page(query, signal) { return parseHistoryPage(await get(historyPagePath(query), HISTORY_PAGE_BYTES, signal), query); },
    async task(id, signal) { return parseHistoryTask(await get(historyTaskPath(id), HISTORY_TASK_BYTES, signal), id); },
  };
}
