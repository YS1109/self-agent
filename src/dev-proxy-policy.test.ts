import { describe, expect, it } from "vitest";
import { allowDevelopmentProxy } from "./dev-proxy-policy";

describe("development proxy boundary", () => {
  it("accepts the workbench's same-origin JSON commands", () => {
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
});
