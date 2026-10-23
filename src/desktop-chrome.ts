export function hasMacosTitlebar(): boolean {
  return "__TAURI_INTERNALS__" in window && /Mac/i.test(navigator.platform);
}

export function installDesktopContextMenu(): () => void {
  if (!("__TAURI_INTERNALS__" in window)) return () => {};
  const handleContextMenu = (event: MouseEvent) => {
    const target = event.target instanceof Element ? event.target : null;
    if (target?.closest('input, textarea, [contenteditable]:not([contenteditable="false"])'))
      return;
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed) {
      for (let index = 0; index < selection.rangeCount; index += 1) {
        const rects = selection.getRangeAt(index).getClientRects();
        if (Array.from(rects).some((rect) =>
          event.clientX >= rect.left && event.clientX <= rect.right &&
          event.clientY >= rect.top && event.clientY <= rect.bottom
        )) return;
      }
    }
    event.preventDefault();
  };
  document.addEventListener("contextmenu", handleContextMenu, true);
  return () => document.removeEventListener("contextmenu", handleContextMenu, true);
}
