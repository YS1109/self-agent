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
  const [recovering, setRecovering] = useState(false);
  const stream = useRef<(() => void) | undefined>(undefined);
  const generation = useRef(0);
  const epoch = useRef(0);
  const request = useRef<Promise<void> | undefined>(undefined);
  const refresh = useCallback(async () => {
    if (request.current) return request.current;
    const owner = generation.current, requestEpoch = epoch.current;
    const pending = (async () => {
      try {
        const value = await api<Snapshot>("GET", "/snapshot?window=1");
        if (owner !== generation.current || requestEpoch !== epoch.current) return;
        if (value.history && state.current.snapshot.history && value.history.anchor !== state.current.snapshot.history.anchor) { epoch.current++; stream.current?.(); stream.current = undefined; }
        state.current = receiveSnapshot(state.current, value);
        setRecovering(state.current.needsSnapshot);
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
    request.current = undefined;
    const applyMessage = (message: StreamMessage) => {
      if (!alive) return;
      if (message.type === "status") { setEventStatus(message.state); return; }
      if (message.type === "snapshot") {
        const reset = message.reset || message.snapshot.history && state.current.snapshot.history && message.snapshot.history.anchor !== state.current.snapshot.history.anchor;
        if (reset) { epoch.current++; request.current = undefined; state.current = createEventState(); }
        state.current = receiveSnapshot(state.current, message.snapshot);
      } else {
        const missingTask = !!message.event.task_id && !state.current.snapshot.tasks.some(task => task.id === message.event.task_id);
        state.current = receiveEvent(state.current, message.event);
        if (state.current.needsSnapshot) {
          stream.current?.(); stream.current = undefined; setRecovering(true); setEventStatus("reconnecting");
          void refresh();
        }
        if (missingTask || !["assistant_delta", "tool_started", "tool_finished", "usage", "context_estimate", "context_compaction", "command_output"].includes(message.event.kind)) {
          if (!eventRefresh) eventRefresh = setTimeout(() => { eventRefresh = undefined; if (alive) void refresh(); }, 150);
        }
      }
      setSnapshot(state.current.snapshot);
      setRecovering(state.current.needsSnapshot);
      setConnected(true); setChecked(true); setConnectionError("");
    };
    const onMessage = (message: StreamMessage) => {
      try { applyMessage(message); }
      catch {
        if (!alive) return;
        stream.current?.(); stream.current = undefined;
        state.current = { ...state.current, needsSnapshot: true, recoveryFloor: state.current.cursor };
        setRecovering(true); setEventStatus("reconnecting");
        setConnectionError("后台事件窗口无效，正在只读恢复权威快照。");
        void refresh();
      }
    };
    const poll = async () => {
      if (!alive) return;
      await refresh();
      if (!alive) return;
      if (!stream.current && !state.current.needsSnapshot && validCursor(state.current.snapshot.event_cursor)) {
        stream.current = subscribeEvents(state.current.cursor, onMessage);
      }
      timer = setTimeout(poll, 1500);
    };
    void poll();
    return () => {
      alive = false; generation.current++;
      clearTimeout(timer); clearTimeout(eventRefresh); stream.current?.(); stream.current = undefined;
    };
  }, [refresh]);
  return { snapshot, connected, checked, connectionError, eventStatus, recovering, refresh };
}
