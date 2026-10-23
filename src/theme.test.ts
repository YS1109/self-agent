import { afterEach, describe, expect, it, vi } from "vitest";
import { syncNativeTheme } from "./theme";

const nativeWindow = vi.hoisted(() => ({
  setTheme: vi.fn().mockResolvedValue(undefined),
  setBackgroundColor: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => nativeWindow,
}));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});
describe("原生窗口主题", () => {
  it("浏览器不调用原生API，桌面快速切换时按顺序更新主题和背景", async () => {
    vi.stubGlobal("window", {});
    await syncNativeTheme("dark");
    expect(nativeWindow.setTheme).not.toHaveBeenCalled();
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    const updates = [syncNativeTheme("dark"), syncNativeTheme("light")];
    await Promise.all(updates);
    expect(nativeWindow.setTheme.mock.calls).toEqual([["dark"], ["light"]]);
    expect(nativeWindow.setBackgroundColor.mock.calls).toEqual([["#1d1e21"], ["#fafafa"]]);
    expect(nativeWindow.setTheme.mock.invocationCallOrder[1])
      .toBeGreaterThan(nativeWindow.setBackgroundColor.mock.invocationCallOrder[0]);
  });
  it("报告原生调用失败，后续切换仍可恢复", async () => {
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    nativeWindow.setTheme.mockRejectedValueOnce(new Error("permission denied"));
    await expect(syncNativeTheme("dark")).rejects.toThrow("permission denied");
    await expect(syncNativeTheme("light")).resolves.toBeUndefined();
    expect(nativeWindow.setBackgroundColor).toHaveBeenLastCalledWith("#fafafa");
  });
});
