import { LogicalPosition } from "@tauri-apps/api/dpi";
import type { Menu, PredefinedMenuItemOptions } from "@tauri-apps/api/menu";

export function hasMacosTitlebar(): boolean {
  return "__TAURI_INTERNALS__" in window && /Mac/i.test(navigator.platform);
}

type ContextMenuKind = "editable" | "readonly" | "password";
const textInputTypes = new Set(["text", "search", "email", "url", "tel", "number", "password"]);

function contextMenuKind(event: MouseEvent): ContextMenuKind | null {
  const target = event.target instanceof Element ? event.target : null;
  const editor = target?.closest("input, textarea, [contenteditable]");
  if (editor) {
    if (editor.matches(":disabled")) return null;
    if (editor.tagName === "INPUT" || editor.tagName === "TEXTAREA") {
      const inputType = (editor.getAttribute("type") ?? "text").toLowerCase();
      if (editor.tagName === "INPUT" && !textInputTypes.has(inputType)) return null;
      if (inputType === "password") return editor.hasAttribute("readonly") ? null : "password";
      return editor.hasAttribute("readonly") ? "readonly" : "editable";
    }
    if (editor.getAttribute("contenteditable")?.toLowerCase() !== "false")
      return "editable";
  }
  const selection = window.getSelection();
  if (selection && !selection.isCollapsed) {
    for (let index = 0; index < selection.rangeCount; index += 1) {
      const rects = selection.getRangeAt(index).getClientRects();
      if (Array.from(rects).some((rect) =>
        event.clientX >= rect.left && event.clientX <= rect.right &&
        event.clientY >= rect.top && event.clientY <= rect.bottom
      )) return "readonly";
    }
  }
  return null;
}

function contextMenuItems(kind: ContextMenuKind): PredefinedMenuItemOptions[] {
  const items: PredefinedMenuItemOptions[] = [];
  if (kind !== "readonly" && /Mac/i.test(navigator.platform)) {
    items.push(
      { item: "Undo", text: "撤销" },
      { item: "Redo", text: "重做" },
      { item: "Separator" },
    );
  }
  if (kind === "editable") items.push({ item: "Cut", text: "剪切" });
  if (kind !== "password") items.push({ item: "Copy", text: "复制" });
  if (kind !== "readonly") items.push({ item: "Paste", text: "粘贴" });
  items.push({ item: "SelectAll", text: "全选" });
  return items;
}

export function installDesktopContextMenu(): () => void {
  if (!("__TAURI_INTERNALS__" in window)) return () => {};
  const menus = new Map<ContextMenuKind, Promise<Menu>>();
  let disposed = false;
  let latestRequest = 0;
  const getMenu = (kind: ContextMenuKind) => {
    const existing = menus.get(kind);
    if (existing) return existing;
    const created = import("@tauri-apps/api/menu")
      .then(({ Menu }) => Menu.new({ items: contextMenuItems(kind) }));
    menus.set(kind, created);
    void created.catch(() => {
      if (menus.get(kind) === created) menus.delete(kind);
    });
    return created;
  };
  const handleContextMenu = (event: MouseEvent) => {
    event.preventDefault();
    const request = ++latestRequest;
    const kind = contextMenuKind(event);
    if (!kind) return;
    void getMenu(kind)
      .then(async (menu) => {
        if (disposed || request !== latestRequest) return;
        await menu.popup(new LogicalPosition(event.clientX, event.clientY));
      })
      .catch(() => console.error("无法打开中文编辑菜单，请使用键盘快捷键。"));
  };
  document.addEventListener("contextmenu", handleContextMenu, true);
  return () => {
    disposed = true;
    document.removeEventListener("contextmenu", handleContextMenu, true);
    // A popup promise resolves when shown; keep the resource until this handler is removed.
    for (const menu of menus.values())
      void menu.then((resource) => resource.close()).catch(() => {});
    menus.clear();
  };
}
