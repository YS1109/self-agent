const localOrigins = new Set([
  "http://127.0.0.1:1420",
  "http://localhost:1420",
]);

const routes: Record<string, string> = {
  "GET /api/v1/snapshot": "GET",
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

export function allowDevelopmentProxy(
  method: string,
  path: string,
  origin: string | undefined,
  fetchSite: string | undefined,
  contentType: string | undefined,
): boolean {
  if (!Object.hasOwn(routes, `${method} ${path}`) && !(method === "GET" && eventProxyPath(path))) return false;
  if (fetchSite === "cross-site") return false;
  if (origin && !localOrigins.has(origin)) return false;
  return (
    method === "GET" ||
    contentType?.split(";")[0].trim().toLowerCase() === "application/json"
  );
}
