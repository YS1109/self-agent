export type AppTheme = "light" | "dark";

export const themeBackground = (theme: AppTheme) =>
  theme === "dark" ? "#1d1e21" : "#fafafa";

let nativeThemeUpdate = Promise.resolve();

export function syncNativeTheme(theme: AppTheme): Promise<void> {
  if (!("__TAURI_INTERNALS__" in window)) return Promise.resolve();
  // Serialize rapid toggles so an older native update cannot overwrite the latest theme.
  nativeThemeUpdate = nativeThemeUpdate.catch(() => {}).then(async () => {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const nativeWindow = getCurrentWindow();
    await nativeWindow.setTheme(theme);
    await nativeWindow.setBackgroundColor(themeBackground(theme));
  });
  return nativeThemeUpdate;
}
