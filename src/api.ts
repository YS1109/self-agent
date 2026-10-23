import type { Command, Receipt, SourceContext } from "./types";

export async function api<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  if ("__TAURI_INTERNALS__" in window) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>("api_request", {
      method,
      path: `/api/v1${path}`,
      body: body ?? null,
    });
  }
  const response = await fetch(`/api/v1${path}`, {
    method,
    headers:
      body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(20_000),
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
