import {mcpSlotError,type McpCredentialClient} from "./mcp-credentials-client";
import { providerUuid, type ProvidersClient } from "./providers-client";
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

function providerPath(id:string,suffix:string){if(!providerUuid(id))throw new Error("服务或操作标识无效");return `${suffix.startsWith("operation:")?"/credential-operations":"/providers"}/${id}/${suffix.replace("operation:","")}`;}
export const providersClient:ProvidersClient={
 registry:signal=>api("GET","/providers",undefined,signal),
 command:(envelope,signal)=>api("POST","/providers/commands",envelope,signal),
 setCredential:(id,body,signal)=>api("POST",providerPath(id,"credential"),body,signal),
 deleteCredential:(id,body,signal)=>api("POST",providerPath(id,"credential-delete"),body,signal),
 cleanupCredential:(id,body,signal)=>api("POST",providerPath(id,"operation:cleanup"),body,signal),
};

function mcpCredentialPath(id:string,slot?:string):string {if(!providerUuid(id)||(slot!==undefined&&mcpSlotError(slot)))throw new Error("MCP 凭据标识或槽名无效");return slot===undefined?`/capability-credential-operations/${id}`:`/capabilities/mcp/${id}/credentials/${slot}`;}
async function mcpCredentialRequest<T>(method:string,path:string,body:unknown,signal:AbortSignal):Promise<T>{try{return await api<T>(method,path,body,signal);}catch{throw new Error("MCP 凭据请求未确认，请只读检查原操作；未回显凭据或底层错误。");}}
export const mcpCredentialClient:McpCredentialClient={
 setCredential:(id,slot,body,signal)=>mcpCredentialRequest("POST",mcpCredentialPath(id,slot),body,signal),
 deleteCredential:(id,slot,body,signal)=>mcpCredentialRequest("POST",`${mcpCredentialPath(id,slot)}/delete`,body,signal),
 operation:(id,signal)=>mcpCredentialRequest("GET",mcpCredentialPath(id),undefined,signal),
 cleanupCredential:(id,body,signal)=>mcpCredentialRequest("POST",`${mcpCredentialPath(id)}/cleanup`,body,signal),
};
