import type { RunEvent, Snapshot } from "./types";
import { validCursor } from "./event-state";

export type StreamMessage =
  | { type: "event"; event: RunEvent; delivery_id?: number }
  | { type: "snapshot"; snapshot: Snapshot; reset?: boolean; delivery_id?: number }
  | { type: "status"; state: "connecting" | "connected" | "reconnecting" | "error"; message?: string; delivery_id?: number };
export interface SseFrame { event: string; id?: string; data: string }
const MAX_FRAME_BYTES = 512 * 1024;

export class SseDecoder {
  private decoder = new TextDecoder("utf-8", { fatal: true });
  private line = "";
  private event = "";
  private id: string | undefined;
  private data: string[] = [];
  private bytes = 0;
  private skipLf = false;
  push(bytes: Uint8Array): SseFrame[] {
    const frames: SseFrame[] = [];
    for (const character of this.decoder.decode(bytes, { stream: true })) {
      if (this.skipLf) { this.skipLf = false; if (character === "\n") continue; }
      if (character !== "\r" && character !== "\n") {
        this.line += character;
        if (this.line.length > MAX_FRAME_BYTES) throw new Error("事件行超过限制");
        continue;
      }
      this.skipLf = character === "\r";
      this.bytes += new TextEncoder().encode(this.line).length + 1;
      if (this.bytes > MAX_FRAME_BYTES) throw new Error("事件超过512 KiB限制");
      if (!this.line) {
        if (this.data.length || this.event) frames.push({ event: this.event, id: this.id, data: this.data.join("\n") });
        this.event = ""; this.id = undefined; this.data = []; this.bytes = 0;
      } else {
        const separator = this.line.indexOf(":");
        const field = separator < 0 ? this.line : this.line.slice(0, separator);
        let value = separator < 0 ? "" : this.line.slice(separator + 1);
        if (value.startsWith(" ")) value = value.slice(1);
        if (field === "event") this.event = value;
        if (field === "id" && !value.includes("\0")) this.id = value;
        if (field === "data") this.data.push(value);
        this.line = "";
      }
    }
    return frames;
  }
}

export function parseRunEvent(frame: SseFrame): RunEvent {
  if (!frame.id || !/^\d+$/.test(frame.id)) throw new Error("事件缺少有效游标");
  const sequence = Number(frame.id);
  const event = JSON.parse(frame.data) as RunEvent;
  if (!validCursor(sequence) || sequence === 0 || event.sequence !== sequence
    || typeof event.id !== "string" || typeof event.kind !== "string" || typeof event.message !== "string") {
    throw new Error("事件游标与正文无效或不一致");
  }
  const data = event.data;
  if (data?.kind === "assistant_delta" && typeof data.text !== "string") throw new Error("回复增量格式无效");
  if ((data?.kind === "tool_started" || data?.kind === "tool_finished")
    && (typeof data.call_id !== "string" || typeof data.name !== "string")) throw new Error("工具事件格式无效");
  if (data?.kind === "tool_finished" && (typeof data.success !== "boolean" || typeof data.output !== "string")) throw new Error("工具结果格式无效");
  if (data?.kind === "context_estimate" && (
    !validCursor(data.estimated_input_tokens) || !validCursor(data.context_window) || data.context_window === 0
    || !Number.isInteger(data.compression_threshold) || data.compression_threshold < 1 || data.compression_threshold > 100
    || typeof data.source !== "string" || typeof data.phase !== "string"
  )) throw new Error("上下文估算格式无效");
  return event;
}

async function retryDelay(signal: AbortSignal) {
  if (signal.aborted) return;
  await new Promise<void>((resolve) => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); resolve(); };
    const timer = setTimeout(finish, 500);
    signal.addEventListener("abort", finish, { once: true });
  });
}

/** The browser only talks to the same-origin controlled proxy. Credentials are
 * never supplied by JavaScript. A stopped subscription cannot update its owner. */
async function browserStream(after: number, signal: AbortSignal, deliver: (message: StreamMessage) => void) {
  let cursor = after;
  let state = "";
  const status = (next: "connecting" | "connected" | "reconnecting" | "error", message?: string) => {
    if (!signal.aborted && state !== next) { state = next; deliver({ type: "status", state: next, message }); }
  };
  const restore = async () => {
    const response = await fetch("/api/v1/snapshot", { signal, cache: "no-store" });
    if (!response.ok) throw new Error("无法恢复后台快照");
    const snapshot = await response.json() as Snapshot;
    if (!validCursor(snapshot.event_cursor)) throw new Error("后台快照不支持事件游标");
    if (signal.aborted) return;
    deliver({ type: "snapshot", snapshot, reset: snapshot.event_cursor < cursor });
    cursor = snapshot.event_cursor;
  };
  status("connecting");
  while (!signal.aborted) {
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      const response = await fetch(`/api/v1/events?after=${cursor}`, { signal, cache: "no-store", headers: { Accept: "text/event-stream" } });
      if (response.status === 409) { status("reconnecting", "事件游标已过期，正在恢复权威快照。"); await restore(); await retryDelay(signal); continue; }
      if (!response.ok || !response.body || response.headers.get("content-type")?.split(";")[0].trim() !== "text/event-stream") {
        throw new Error("事件连接暂不可用");
      }
      status("connected");
      reader = response.body.getReader();
      const decoder = new SseDecoder();
      let resync = false;
      while (!signal.aborted && !resync) {
        const chunk = await reader.read();
        if (chunk.done) break;
        for (const frame of decoder.push(chunk.value)) {
          if (signal.aborted) break;
          if (frame.event === "resync") { resync = true; break; }
          if (frame.event !== "run_event") continue;
          const event = parseRunEvent(frame);
          if (event.sequence! <= cursor) continue;
          deliver({ type: "event", event });
          cursor = event.sequence!;
        }
      }
      await reader.cancel().catch(() => {});
      reader.releaseLock(); reader = undefined;
      if (resync && !signal.aborted) await restore();
    } catch {
      if (!signal.aborted) status("reconnecting", "事件连接已断开，正在从已提交游标恢复；快照查询仍可用。");
    } finally {
      if (reader) { await reader.cancel().catch(() => {}); reader.releaseLock(); }
    }
    if (!signal.aborted) { status("reconnecting", "正在恢复实时事件，后台任务不会重新提交。"); await retryDelay(signal); }
  }
}

export function subscribeEvents(after: number, onMessage: (message: StreamMessage) => void): () => void {
  let alive = true;
  const controller = new AbortController();
  const subscriptionId = crypto.randomUUID();
  const deliver = (message: StreamMessage) => { if (alive) onMessage(message); };
  let stopNative: (() => Promise<void>) | undefined;
  if ("__TAURI_INTERNALS__" in window) {
    void (async () => {
      const { Channel, invoke } = await import("@tauri-apps/api/core");
      if (!alive) return;
      const channel = new Channel<StreamMessage>();
      channel.onmessage = (message) => {
        if (!alive) return;
        deliver(message);
        if (validCursor(message.delivery_id)) {
          void invoke("ack_event_stream", { subscriptionId, deliveryId: message.delivery_id }).catch(() => {});
        }
      };
      stopNative = () => invoke<void>("stop_event_stream", { subscriptionId });
      await invoke("start_event_stream", { subscriptionId, after, channel });
      // StrictMode/unmount can stop while invoke is still registering in Rust.
      if (!alive) await stopNative();
    })().catch(() => deliver({ type: "status", state: "error", message: "桌面事件桥暂不可用，正在使用快照查询。" }));
  } else {
    void browserStream(after, controller.signal, deliver);
  }
  return () => {
    if (!alive) return;
    alive = false;
    controller.abort();
    void stopNative?.().catch(() => {});
  };
}
