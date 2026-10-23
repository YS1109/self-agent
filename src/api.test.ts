import { afterEach, describe, expect, it, vi } from "vitest";
import { api, dispatch } from "./api";
import { DESKTOP_SOURCE, REMOTE_SOURCE, sameSource } from "./types";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("private daemon transport", () => {
  it("uses same-origin browser API without providing a daemon credential", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi
      .fn()
      .mockResolvedValue({ ok: true, json: async () => ({ sessions: [] }) });
    vi.stubGlobal("fetch", fetcher);
    await expect(api("GET", "/snapshot")).resolves.toEqual({ sessions: [] });
    expect(fetcher).toHaveBeenCalledWith(
      "/api/v1/snapshot",
      expect.objectContaining({ method: "GET", headers: undefined }),
    );
    expect(JSON.stringify(fetcher.mock.calls)).not.toMatch(
      /bearer|authorization/i,
    );
  });

  it("forwards native commands through Rust without fetching directly", async () => {
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    invoke.mockResolvedValue({ status: "accepted" });
    await api("POST", "/commands", { command_id: "one" });
    expect(invoke).toHaveBeenCalledWith("api_request", {
      method: "POST",
      path: "/api/v1/commands",
      body: { command_id: "one" },
    });
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("shows daemon validation errors instead of reporting a successful command", async () => {
    vi.stubGlobal("window", {});
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue({
          ok: false,
          status: 400,
          json: async () => ({ error: "No selected session" }),
        }),
    );
    await expect(api("POST", "/commands", {})).rejects.toThrow(
      "No selected session",
    );
  });

  it("preserves exact remote source and omits an implicit desktop session", async () => {
    vi.stubGlobal("window", {});
    const fetcher = vi
      .fn()
      .mockResolvedValue({
        ok: true,
        json: async () => ({ status: "accepted" }),
      });
    vi.stubGlobal("fetch", fetcher);
    await dispatch(REMOTE_SOURCE, { type: "start_task", title: "Build" });
    const envelope = JSON.parse(fetcher.mock.calls[0][1].body);
    expect(envelope.source).toEqual(REMOTE_SOURCE);
    expect(envelope.command).toEqual({ type: "start_task", title: "Build" });
    expect(envelope.command_id).toMatch(/^[a-f0-9-]{36}$/);
    expect(sameSource(REMOTE_SOURCE, DESKTOP_SOURCE)).toBe(false);
    expect(
      sameSource(REMOTE_SOURCE, {
        ...REMOTE_SOURCE,
        conversation_id: "remote-2",
      }),
    ).toBe(false);
  });
});
