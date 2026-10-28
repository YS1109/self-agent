const localOrigins = new Set([
  "http://127.0.0.1:1420",
  "http://localhost:1420",
]);

const routes: Record<string, string> = {
  "GET /api/v1/snapshot": "GET",
  "GET /api/v1/providers": "GET",
  "POST /api/v1/providers/commands": "POST",
  "GET /api/v1/capabilities": "GET",
  "POST /api/v1/capabilities/commands": "POST",
  "GET /api/v1/settings": "GET",
  "GET /api/v1/provider/local-codex": "GET",
  "POST /api/v1/settings/check": "POST",
  "POST /api/v1/commands": "POST",
  "POST /api/v1/models": "POST",
  "GET /api/v1/codex/status": "GET",
  "PUT /api/v1/settings": "PUT",
};

export function eventProxyPath(path: string): boolean {
  const match = /^\/api\/v1\/events\?after=(0|[1-9]\d*)$/.exec(path);
  return !!match && Number.isSafeInteger(Number(match[1]));
}

export function changesProxyPath(path: string): boolean {
  return /^\/api\/v1\/tasks\/[A-Za-z0-9_-]{1,128}\/changes$/.test(path);
}

export function cleanupResolutionProxyPath(path: string): boolean {
  return /^\/api\/v1\/tasks\/[A-Za-z0-9_-]{1,128}\/cleanup-resolution$/.test(path);
}

export function credentialOperationProxyPath(path:string):boolean { return /^\/api\/v1\/credential-operations\/[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(path); }

export function providerMutationProxyPath(path:string):boolean {
  return /^\/api\/v1\/(?:providers\/[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}\/(?:credential|credential-delete)|credential-operations\/[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}\/cleanup)$/.test(path);
}

export function mcpCredentialProxyPath(method:string,path:string):boolean {
 const id="[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}";
 return method==="GET"?new RegExp(`^/api/v1/capability-credential-operations/${id}$`).test(path):method==="POST"&&(new RegExp(`^/api/v1/capabilities/mcp/${id}/credentials/[A-Z][A-Z0-9_]{0,63}(?:/delete)?$`).test(path)||new RegExp(`^/api/v1/capability-credential-operations/${id}/cleanup$`).test(path));
}

export function allowDevelopmentProxy(
  method: string,
  path: string,
  origin: string | undefined,
  fetchSite: string | undefined,
  contentType: string | undefined,
): boolean {
  if (!mcpCredentialProxyPath(method,path) && !Object.hasOwn(routes, `${method} ${path}`) && !(method === "GET" && (eventProxyPath(path) || changesProxyPath(path) || credentialOperationProxyPath(path))) && !(method === "POST" && (cleanupResolutionProxyPath(path)||providerMutationProxyPath(path)))) return false;
  if (fetchSite === "cross-site") return false;
  if (origin && !localOrigins.has(origin)) return false;
  return (
    method === "GET" ||
    contentType?.split(";")[0].trim().toLowerCase() === "application/json"
  );
}
