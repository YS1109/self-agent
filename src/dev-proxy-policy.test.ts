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

});
