import { describe, expect, it } from "vitest";
import { allowDevelopmentProxy } from "./dev-proxy-policy";

describe("development proxy boundary", () => {
  it("accepts the workbench's same-origin JSON commands", () => {
    expect(allowDevelopmentProxy("GET", "/api/v1/provider/local-codex", undefined, "same-origin", undefined)).toBe(true);
    expect(allowDevelopmentProxy("POST", "/api/v1/settings/check", "http://127.0.0.1:1420", "same-origin", "application/json")).toBe(true);
    expect(allowDevelopmentProxy("POST", "/api/v1/models", "http://127.0.0.1:1420", "same-origin", "application/json")).toBe(true);
    expect(allowDevelopmentProxy("GET", "/api/v1/codex/status", undefined, "same-origin", undefined)).toBe(true);
    expect(
      allowDevelopmentProxy(
        "POST",
        "/api/v1/commands",
        "http://127.0.0.1:1420",
        "same-origin",
        "application/json",
      ),
    ).toBe(true);
    expect(
      allowDevelopmentProxy(
        "GET",
        "/api/v1/snapshot",
        undefined,
        "same-origin",
        undefined,
      ),
    ).toBe(true);
  });

  it("rejects cross-origin and simple-form requests before injecting a token", () => {
    for (const origin of [
      "https://example.com",
      "http://127.0.0.1:3000",
      "null",
    ]) {
      expect(
        allowDevelopmentProxy(
          "POST",
          "/api/v1/commands",
          origin,
          "cross-site",
          "application/json",
        ),
      ).toBe(false);
    }
    expect(
      allowDevelopmentProxy(
        "POST",
        "/api/v1/commands",
        undefined,
        undefined,
        "text/plain",
      ),
    ).toBe(false);
    expect(
      allowDevelopmentProxy(
        "PUT",
        "/api/v1/settings",
        "http://127.0.0.1:1420",
        "cross-site",
        "application/json",
      ),
    ).toBe(false);
  });

  it("rejects unlisted routes, methods, and path manipulation", () => {
    for (const path of [
      "/api/v1/../health",
      "/api/v1/settings?target=elsewhere",
      "//example.com/api/v1/settings",
      "/api/v1/events",
    ]) {
      expect(
        allowDevelopmentProxy("GET", path, undefined, undefined, undefined),
      ).toBe(false);
    }
    expect(
      allowDevelopmentProxy(
        "DELETE",
        "/api/v1/commands",
        undefined,
        undefined,
        "application/json",
      ),
    ).toBe(false);
  });
  it("仅允许带单一安全整数游标的同源事件订阅", () => {
    for (const path of ["/api/v1/events?after=0", "/api/v1/events?after=42"])
      expect(allowDevelopmentProxy("GET",path,undefined,"same-origin",undefined)).toBe(true);
    for (const path of ["/api/v1/events?after=-1", "/api/v1/events?after=1&target=http://example.com", "/api/v1/events?after=1&after=2", "/api/v1/events?after=9007199254740993", "/api/v1/events?after=1%0a", "/api/v1/events?after=01"])
      expect(allowDevelopmentProxy("GET",path,undefined,"same-origin",undefined)).toBe(false);
    expect(allowDevelopmentProxy("GET","/api/v1/events?after=0","https://example.com","cross-site",undefined)).toBe(false);
    expect(allowDevelopmentProxy("POST","/api/v1/events?after=0",undefined,"same-origin","application/json")).toBe(false);
  });

  it("改动报告只允许 GET 和单一安全任务 ID，不放宽同源边界", () => {
    const allowed = "/api/v1/tasks/task_123-ab/changes";
    expect(allowDevelopmentProxy("GET", allowed, undefined, "same-origin", undefined)).toBe(true);
    expect(allowDevelopmentProxy("POST", allowed, undefined, "same-origin", "application/json")).toBe(false);
    expect(allowDevelopmentProxy("GET", allowed, "https://example.com", "cross-site", undefined)).toBe(false);
    for (const path of ["/api/v1/tasks//changes", "/api/v1/tasks/../changes", "/api/v1/tasks/a%2fb/changes", allowed + "?token=x", allowed + "/", "/api/v1/tasks/" + "a".repeat(129) + "/changes"])
      expect(allowDevelopmentProxy("GET", path, undefined, "same-origin", undefined)).toBe(false);
  });

  it("能力管理和所有者清理确认仅开放准确同源端点，禁用通配和GET确认", () => {
    expect(allowDevelopmentProxy("GET","/api/v1/capabilities",undefined,"same-origin",undefined)).toBe(true);
    for(const path of ["/api/v1/capabilities/commands","/api/v1/tasks/task-1/cleanup-resolution"]){
      expect(allowDevelopmentProxy("POST",path,"http://127.0.0.1:1420","same-origin","application/json")).toBe(true);
      expect(allowDevelopmentProxy("GET",path,undefined,"same-origin",undefined)).toBe(false);
      expect(allowDevelopmentProxy("POST",path,"https://evil.invalid","cross-site","application/json")).toBe(false);
      expect(allowDevelopmentProxy("POST",path,undefined,"same-origin","text/plain")).toBe(false);
      expect(allowDevelopmentProxy("POST",path+"?target=remote",undefined,"same-origin","application/json")).toBe(false);
    }
    for(const path of ["/api/v1/tasks/../cleanup-resolution","/api/v1/tasks/t%2Fx/cleanup-resolution","/api/v1/capabilities/secret"])
      expect(allowDevelopmentProxy("POST",path,undefined,"same-origin","application/json")).toBe(false);
  });

  it("命名凭据只接受准确UUID和同源JSON POST，拒绝查询参数与路径绕过",()=>{
    const id="00000000-0000-0000-0000-000000000001";
    expect(allowDevelopmentProxy("GET","/api/v1/providers",undefined,"same-origin",undefined)).toBe(true);
    for(const path of ["/api/v1/providers/commands",`/api/v1/providers/${id}/credential`,`/api/v1/providers/${id}/credential-delete`,`/api/v1/credential-operations/${id}/cleanup`]){
      expect(allowDevelopmentProxy("POST",path,undefined,"same-origin","application/json")).toBe(true);
      expect(allowDevelopmentProxy("GET",path,undefined,"same-origin",undefined)).toBe(false);
      expect(allowDevelopmentProxy("POST",path,"https://outside.invalid","cross-site","application/json")).toBe(false);
      expect(allowDevelopmentProxy("POST",path+"?secret=x",undefined,"same-origin","application/json")).toBe(false);
    }
    expect(allowDevelopmentProxy("POST","/api/v1/providers/../credential",undefined,"same-origin","application/json")).toBe(false);
  });

});

// Single-operation recovery is read-only and cannot include secret/query data.
it("allows only exact read-only credential operation recovery",()=> {
 const path="/api/v1/credential-operations/94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
 expect(allowDevelopmentProxy("GET",path,undefined,undefined,undefined)).toBe(true);
 expect(allowDevelopmentProxy("POST",path,undefined,undefined,"application/json")).toBe(false);
 expect(allowDevelopmentProxy("GET",path+"?secret=x",undefined,undefined,undefined)).toBe(false);
});

it("allows only exact MCP credential method/UUID/slot routes and same origin JSON",()=>{
 const id="11111111-1111-4111-8111-111111111111",base=`/api/v1/capabilities/mcp/${id}/credentials/TOKEN`;
 for(const path of [base,base+"/delete",`/api/v1/capability-credential-operations/${id}/cleanup`]){expect(allowDevelopmentProxy("POST",path,undefined,"same-origin","application/json")).toBe(true);expect(allowDevelopmentProxy("GET",path,undefined,"same-origin",undefined)).toBe(false);expect(allowDevelopmentProxy("POST",path,"https://evil.invalid","cross-site","application/json")).toBe(false);}
 expect(allowDevelopmentProxy("GET",`/api/v1/capability-credential-operations/${id}`,undefined,"same-origin",undefined)).toBe(true);
 for(const path of [base+"?secret=x",base+"/../PATH",base.replace("TOKEN","token"),base.replace(id,"../bad")])expect(allowDevelopmentProxy("POST",path,undefined,"same-origin","application/json")).toBe(false);
});

it("limits QQ methods and canonical routes without broad credential proxy",()=>{
 const id="11111111-1111-4111-8111-111111111111";
 for(const [method,path] of [["GET","/api/v1/qq"],["POST","/api/v1/qq/commands"],["GET",`/api/v1/qq/commands/${id}`],["GET",`/api/v1/qq/credential-operations/${id}`],["POST",`/api/v1/qq/credentials/${id}`],["DELETE",`/api/v1/qq/credentials/${id}`],["POST",`/api/v1/qq/credentials/cleanup/${id}`]]){expect(allowDevelopmentProxy(method,path,undefined,"same-origin","application/json")).toBe(true);expect(allowDevelopmentProxy(method,path+"?owner=provider",undefined,"same-origin","application/json")).toBe(false);expect(allowDevelopmentProxy(method,path,"https://evil.invalid","cross-site","application/json")).toBe(false);}
 for(const method of ["GET","PUT","PATCH"]){expect(allowDevelopmentProxy(method,`/api/v1/qq/credentials/${id}`,undefined,"same-origin","application/json")).toBe(false);}expect(allowDevelopmentProxy("DELETE",`/api/v1/qq/credentials/${id}`,undefined,"same-origin","text/plain")).toBe(false);
});

it("owner backup routes are exact, read-only recovery/diagnostics cannot restore or control lifecycle", () => {
  const id = "94a846e4-7ac2-46d7-ac1d-fde573fd6e12";
  for (const [method, path] of [["GET", "/api/v1/reliability"], ["GET", "/api/v1/reliability/diagnostics"], ["POST", "/api/v1/reliability/backups"], ["GET", `/api/v1/reliability/backups/${id}`]]) {
    expect(allowDevelopmentProxy(method, path, undefined, "same-origin", "application/json")).toBe(true);
    expect(allowDevelopmentProxy(method, path, "https://outside.invalid", "cross-site", "application/json")).toBe(false);
    expect(allowDevelopmentProxy(method, path + "?path=/private", undefined, "same-origin", "application/json")).toBe(false);
  }
  for (const [method, path] of [["GET", "/api/v1/reliability/backups"], ["POST", `/api/v1/reliability/backups/${id}`], ["DELETE", `/api/v1/reliability/backups/${id}`], ["POST", "/api/v1/reliability/diagnostics"], ["POST", "/api/v1/reliability/restore"], ["POST", "/api/v1/reliability/stop"], ["GET", "/api/v1/reliability/backups/../snapshot"], ["GET", `/api/v1/reliability/backups/${id.toUpperCase()}`]]) expect(allowDevelopmentProxy(method, path, undefined, "same-origin", "application/json")).toBe(false);
  expect(allowDevelopmentProxy("POST", "/api/v1/reliability/backups", undefined, "same-origin", "text/plain")).toBe(false);
});

it("limits gateway methods and canonical routes without broad credential proxy",()=>{
 const id="11111111-1111-4111-8111-111111111111";
 for(const [method,path] of [["GET","/api/v1/gateway"],["POST","/api/v1/gateway/commands"],["GET",`/api/v1/gateway/commands/${id}`],["GET",`/api/v1/gateway/credential-operations/${id}`],["POST",`/api/v1/gateway/credentials/${id}`],["DELETE",`/api/v1/gateway/credentials/${id}`],["POST",`/api/v1/gateway/credentials/cleanup/${id}`]]){expect(allowDevelopmentProxy(method,path,undefined,"same-origin","application/json")).toBe(true);expect(allowDevelopmentProxy(method,path+"?owner=provider",undefined,"same-origin","application/json")).toBe(false);expect(allowDevelopmentProxy(method,path,"https://evil.invalid","cross-site","application/json")).toBe(false);}
 for(const method of ["GET","PUT","PATCH"]){expect(allowDevelopmentProxy(method,`/api/v1/gateway/credentials/${id}`,undefined,"same-origin","application/json")).toBe(false);}expect(allowDevelopmentProxy("DELETE",`/api/v1/gateway/credentials/${id}`,undefined,"same-origin","text/plain")).toBe(false);
});
