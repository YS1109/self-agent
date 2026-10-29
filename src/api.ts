import type {GatewayClient,GatewayCredentialClient} from "./gateway-client";
import type { ReliabilityClient } from "./reliability-client";
import type {QqClient,QqCredentialClient} from "./qq-client";
import {mcpSlotError,type McpCredentialClient} from "./mcp-credentials-client";
import { providerUuid, type ProvidersClient } from "./providers-client";
import type { Command, Receipt, SourceContext } from "./types";
import type { CapabilityClient } from "./capabilities-client";
import { readBoundedJson } from "./bounded-json";
import { createHistoryClient, HistoryError, historyPagePath, historyTaskPath, parseHistoryPage, parseHistoryTask, type HistoryClient } from "./history-client";

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
    let result: T;
    try {
      result = await invoke<T>("api_request", { method, path: `/api/v1${path}`, body: body ?? null });
    } catch (reason) {
      if (path === "/snapshot?window=1") throw new Error(reason === "HISTORY_RECORD_TOO_LARGE" ? "后台快照超过12 MiB限制，未采用不完整数据" : "后台快照读取未确认，请稍后只读刷新");
      throw reason;
    }
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
  const data = await readBoundedJson(response, response.ok ? (path === "/snapshot?window=1" ? 12 : 16) * 1024 * 1024 : 8192, signal) as T & { error?: string };
  if (!response.ok)
    throw new Error(data.error ?? `请求失败 (${response.status})`);
  return data as T;
}

const browserHistory = createHistoryClient((path, init) => fetch(path, init));
async function nativeHistory(path: string, signal: AbortSignal): Promise<unknown> {
  signal.throwIfAborted();
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    signal.throwIfAborted();
    const result = await invoke("api_request", { method: "GET", path, body: null });
    signal.throwIfAborted(); return result;
  } catch (reason) {
    if (signal.aborted) throw new HistoryError("aborted");
    const codes = { HISTORY_RESET: "history_reset", HISTORY_RECORD_TOO_LARGE: "record_too_large", HISTORY_NOT_FOUND: "not_found" } as const;
    throw new HistoryError(typeof reason === "string" && Object.hasOwn(codes, reason) ? codes[reason as keyof typeof codes] : "unavailable");
  }
}
export const historyClient: HistoryClient = {
  async page(query, signal) { return "__TAURI_INTERNALS__" in window ? parseHistoryPage(await nativeHistory(historyPagePath(query), signal), query) : browserHistory.page(query, signal); },
  async task(id, signal) { return "__TAURI_INTERNALS__" in window ? parseHistoryTask(await nativeHistory(historyTaskPath(id), signal), id) : browserHistory.task(id, signal); },
};
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

function qqId(id:string):string{if(!providerUuid(id))throw new Error("QQ 配置或操作标识无效");return id;}
async function qqRequest<T>(method:string,path:string,body:unknown,signal:AbortSignal):Promise<T>{try{return await api<T>(method,path,body,signal);}catch{throw new Error("QQ 请求未确认，请检查原操作；未回显凭据或底层错误。");}}
export const qqClient:QqClient={
 snapshot:signal=>qqRequest("GET","/qq",undefined,signal),
 command:(envelope,signal)=>qqRequest("POST","/qq/commands",envelope,signal),
 receipt:(id,signal)=>qqRequest("GET",`/qq/commands/${qqId(id)}`,undefined,signal),
};
export const qqCredentialClient:QqCredentialClient={
 setCredential:(id,body,signal)=>qqRequest("POST",`/qq/credentials/${qqId(id)}`,body,signal),
 deleteCredential:(id,body,signal)=>qqRequest("DELETE",`/qq/credentials/${qqId(id)}`,body,signal),
 cleanupCredential:(id,body,signal)=>qqRequest("POST",`/qq/credentials/cleanup/${qqId(id)}`,body,signal),
 operation:(id,signal)=>qqRequest("GET",`/qq/credential-operations/${qqId(id)}`,undefined,signal),
};

async function reliabilityRequest<T>(method:string,path:string,body:unknown,signal:AbortSignal):Promise<T>{try{return await api<T>(method,path,body,signal);}catch{throw new Error("备份或诊断请求未确认；请只读检查原操作，未回显路径或底层错误。");}}
export const reliabilityClient:ReliabilityClient={
 status:signal=>reliabilityRequest("GET","/reliability",undefined,signal),
 backup:(body,signal)=>reliabilityRequest("POST","/reliability/backups",body,signal),
 receipt:(id,signal)=>{if(!providerUuid(id))throw new Error("备份操作标识无效");return reliabilityRequest("GET",`/reliability/backups/${id}`,undefined,signal);},
 diagnostics:signal=>reliabilityRequest("GET","/reliability/diagnostics",undefined,signal),
};

function gatewayId(id:string):string{if(!providerUuid(id))throw new Error("网关 配置或操作标识无效");return id;}
async function gatewayRequest<T>(method:string,path:string,body:unknown,signal:AbortSignal):Promise<T>{try{return await api<T>(method,path,body,signal);}catch{throw new Error("网关 请求未确认，请检查原操作；未回显凭据或底层错误。");}}
export const gatewayClient:GatewayClient={
 snapshot:signal=>gatewayRequest("GET","/gateway",undefined,signal),
 command:(envelope,signal)=>gatewayRequest("POST","/gateway/commands",envelope,signal),
 receipt:(id,signal)=>gatewayRequest("GET",`/gateway/commands/${gatewayId(id)}`,undefined,signal),
};
export const gatewayCredentialClient:GatewayCredentialClient={
 setCredential:(id,body,signal)=>gatewayRequest("POST",`/gateway/credentials/${gatewayId(id)}`,body,signal),
 deleteCredential:(id,body,signal)=>gatewayRequest("DELETE",`/gateway/credentials/${gatewayId(id)}`,body,signal),
 cleanupCredential:(id,body,signal)=>gatewayRequest("POST",`/gateway/credentials/cleanup/${gatewayId(id)}`,body,signal),
 operation:(id,signal)=>gatewayRequest("GET",`/gateway/credential-operations/${gatewayId(id)}`,undefined,signal),
};
