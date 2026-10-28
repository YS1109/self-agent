import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";
import { createEventState, receiveEvent, receiveSnapshot, validCursor } from "./event-state";
import { subscribeEvents, type StreamMessage } from "./event-stream";
import type { Snapshot } from "./types";

export function useDaemonSnapshot() {
  const state = useRef(createEventState());
  const [snapshot, setSnapshot] = useState(state.current.snapshot);
  const [connected, setConnected] = useState(false);
  const [checked, setChecked] = useState(false);
  const [connectionError, setConnectionError] = useState("");
  const [eventStatus, setEventStatus] = useState("connecting");
  const generation = useRef(0);
  const epoch = useRef(0);
  const request = useRef<Promise<void> | undefined>(undefined);
  const refresh = useCallback(async () => {
    if (request.current) return request.current;
    const owner = generation.current, requestEpoch = epoch.current;
    const pending = (async () => {
      try {
        const value = await api<Snapshot>("GET", "/snapshot");
        if (owner !== generation.current || requestEpoch !== epoch.current) return;
        state.current = receiveSnapshot(state.current, value);
        setSnapshot(state.current.snapshot); setConnected(true); setConnectionError("");
      } catch (error) {
        if (owner !== generation.current || requestEpoch !== epoch.current) return;
        setConnected(false); setConnectionError(error instanceof Error ? error.message : String(error));
      } finally {
        if (owner === generation.current && requestEpoch === epoch.current) setChecked(true);
      }
    })();
    request.current = pending;
    await pending;
    if (request.current === pending) request.current = undefined;
  }, []);

  useEffect(() => {
    generation.current++;
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let eventRefresh: ReturnType<typeof setTimeout> | undefined;
    let unsubscribe: (() => void) | undefined;
    request.current = undefined;
    const onMessage = (message: StreamMessage) => {
      if (!alive) return;
      if (message.type === "status") { setEventStatus(message.state); return; }
      if (message.type === "snapshot") {
        if (message.reset) { epoch.current++; request.current = undefined; state.current = createEventState(); }
        state.current = receiveSnapshot(state.current, message.snapshot);
      } else {
        const missingTask = !!message.event.task_id && !state.current.snapshot.tasks.some(task => task.id === message.event.task_id);
        state.current = receiveEvent(state.current, message.event);
        if (missingTask || !["assistant_delta", "tool_started", "tool_finished", "usage", "context_compaction", "command_output"].includes(message.event.kind)) {
          if (!eventRefresh) eventRefresh = setTimeout(() => { eventRefresh = undefined; if (alive) void refresh(); }, 150);
        }
      }
      setSnapshot(state.current.snapshot);
      setConnected(true); setChecked(true); setConnectionError("");
    };
    const poll = async () => {
      if (!alive) return;
      await refresh();
      if (!alive) return;
      if (!unsubscribe && validCursor(state.current.snapshot.event_cursor)) {
        unsubscribe = subscribeEvents(state.current.cursor, onMessage);
      }
      timer = setTimeout(poll, 1500);
    };
    void poll();
    return () => {
      alive = false; generation.current++;
      clearTimeout(timer); clearTimeout(eventRefresh); unsubscribe?.();
    };
  }, [refresh]);
  return { snapshot, connected, checked, connectionError, eventStatus, refresh };
}
