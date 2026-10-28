import { afterEach, describe, expect, it, vi } from "vitest";
import { applyUiScale, DEFAULT_UI_SCALE, normalizeUiScale, readUiScale, saveUiScale, UI_SCALE_KEY } from "./ui-preferences";

const nativeWebview = vi.hoisted(() => ({
  setZoom: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => nativeWebview,
}));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("本机界面缩放", () => {
  it("保留较适中的默认值，读取重启后的本机偏好并限制非法缩放", () => {
    const values = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
    });
    expect(readUiScale()).toBe(DEFAULT_UI_SCALE);
    saveUiScale(85);
    expect(readUiScale()).toBe(85);
    expect(values.get(UI_SCALE_KEY)).toBe("85");
    expect(normalizeUiScale(1000)).toBe(120);
    expect(normalizeUiScale(10)).toBe(80);
    expect(normalizeUiScale(Number.NaN)).toBe(DEFAULT_UI_SCALE);
    values.set(UI_SCALE_KEY, "invalid");
    expect(readUiScale()).toBe(DEFAULT_UI_SCALE);
  });

  it("本机偏好不可读取时仍可启动", () => {
    vi.stubGlobal("localStorage", { getItem: () => { throw new Error("blocked"); } });
    expect(readUiScale()).toBe(DEFAULT_UI_SCALE);
  });

  it("桌面串行应用原生缩放，不通过后台保存界面偏好", async () => {
    const style = { setProperty: vi.fn() };
    vi.stubGlobal("document", { documentElement: { style } });
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    await Promise.all([applyUiScale(80), applyUiScale(120)]);
    expect(nativeWebview.setZoom.mock.calls).toEqual([[0.8], [1.2]]);
    expect(style.setProperty).toHaveBeenLastCalledWith("--ui-scale", "1.2");
  });

  it("原生缩放失败可报告，并允许之后恢复", async () => {
    vi.stubGlobal("document", { documentElement: { style: { setProperty: vi.fn() } } });
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {} });
    nativeWebview.setZoom.mockRejectedValueOnce(new Error("permission denied"));
    await expect(applyUiScale(90)).rejects.toThrow("permission denied");
    await expect(applyUiScale(95)).resolves.toBeUndefined();
  });

  it("浏览器预览同步修正可用画布，避免缩放后底部被裁切", async () => {
    const style = { zoom: "", setProperty: vi.fn() };
    vi.stubGlobal("document", { documentElement: { style } });
    vi.stubGlobal("window", {});
    await applyUiScale(110);
    expect(style.zoom).toBe("1.1");
    expect(style.setProperty).toHaveBeenCalledWith("--app-viewport-height", "calc(100dvh / 1.1)");
    expect(nativeWebview.setZoom).not.toHaveBeenCalled();
  });
});
