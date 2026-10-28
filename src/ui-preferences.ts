export const UI_SCALE_KEY = "self-agent-ui-scale";
export const DEFAULT_UI_SCALE = 95;
export const UI_SCALE_OPTIONS = [80, 85, 90, 95, 100, 105, 110, 115, 120];

export function normalizeUiScale(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) return DEFAULT_UI_SCALE;
  return Math.min(120, Math.max(80, Math.round(value / 5) * 5));
}

export function readUiScale(): number {
  try {
    const value = localStorage.getItem(UI_SCALE_KEY);
    return value === null ? DEFAULT_UI_SCALE : normalizeUiScale(Number(value));
  } catch {
    return DEFAULT_UI_SCALE;
  }
}

export function saveUiScale(value: number): void {
  localStorage.setItem(UI_SCALE_KEY, String(normalizeUiScale(value)));
}

let nativeZoomUpdate = Promise.resolve();

export function applyUiScale(value: number): Promise<void> {
  const factor = normalizeUiScale(value) / 100;
  document.documentElement.style.setProperty("--ui-scale", String(factor));
  if (!("__TAURI_INTERNALS__" in window)) {
    document.documentElement.style.zoom = String(factor);
    document.documentElement.style.setProperty("--app-viewport-height", `calc(100dvh / ${factor})`);
    document.documentElement.style.setProperty("--app-viewport-width", `calc(100vw / ${factor})`);
    return Promise.resolve();
  }
  nativeZoomUpdate = nativeZoomUpdate.catch(() => {}).then(async () => {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    await getCurrentWebview().setZoom(factor);
  });
  return nativeZoomUpdate;
}
