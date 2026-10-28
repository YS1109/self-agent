import type { Command, Receipt, SourceContext } from "./types";
import type { CapabilityClient } from "./capabilities-client";

export async function api<T>(
  method: string,
  path: string,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  signal?.throwIfAborted();
  if ("__TAURI_INTERNALS__" in window) {
    const { invoke } = await import("@tauri-apps/api/core");
    signal?.throwIfAborted();
    const result = await invoke<T>("api_request", {
      method,
      path: `/api/v1${path}`,
      body: body ?? null,
    });
    // Cancelling this observer cannot retract an accepted backend command.
    signal?.throwIfAborted();
    return result;
  }
  const response = await fetch(`/api/v1${path}`, {
    method,
    headers:
      body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(path === "/settings/check" ? 35_000 : 20_000)]) : AbortSignal.timeout(path === "/settings/check" ? 35_000 : 20_000),
  });
  const data = await response.json();
  if (!response.ok)
    throw new Error(data.error ?? `请求失败 (${response.status})`);
  return data as T;
}
export function dispatch(source: SourceContext, command: Command) {
  return api<Receipt>("POST", "/commands", {
    command_id: crypto.randomUUID(),
    source,
    command,
  });
}

export const capabilityClient: CapabilityClient = {
  getRegistry: signal => api("GET", "/capabilities", undefined, signal),
  command: (envelope, signal) => api("POST", "/capabilities/commands", envelope, signal),
};
