import { afterEach, describe, expect, it, vi } from "vitest";
import { installDesktopContextMenu } from "./desktop-chrome";

const nativeMenu = vi.hoisted(() => ({
  new: vi.fn(),
  popup: vi.fn().mockResolvedValue(undefined),
  close: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tauri-apps/api/menu", () => ({
  Menu: { new: nativeMenu.new },
}));

class FakeElement {
  constructor(
    readonly tagName = "DIV",
    private readonly attributes: Record<string, string> = {},
    private readonly editor: FakeElement | null = null,
  ) {}
  closest() {
    return ["INPUT", "TEXTAREA"].includes(this.tagName) || this.hasAttribute("contenteditable")
      ? this
      : this.editor;
  }
  matches() { return this.hasAttribute("disabled"); }
  getAttribute(name: string) { return this.attributes[name] ?? null; }
  hasAttribute(name: string) { return name in this.attributes; }
}

function setup({ desktop = true, platform = "MacIntel" } = {}) {
  let handler: ((event: MouseEvent) => void) | undefined;
  const documentMock = {
    addEventListener: vi.fn((type: string, listener: (event: MouseEvent) => void) => {
      if (type === "contextmenu") handler = listener;
    }),
    removeEventListener: vi.fn(),
  };
  const getSelection = vi.fn().mockReturnValue(null);
  vi.stubGlobal("Element", FakeElement);
  vi.stubGlobal("navigator", { platform });
  vi.stubGlobal("document", documentMock);
  vi.stubGlobal("window", desktop ? { __TAURI_INTERNALS__: {}, getSelection } : { getSelection });
  nativeMenu.new.mockResolvedValue({ popup: nativeMenu.popup, close: nativeMenu.close });
  const remove = installDesktopContextMenu();
  const dispatch = (target = new FakeElement(), clientX = 20, clientY = 20) => {
    const event = { target, clientX, clientY, preventDefault: vi.fn() };
    handler?.(event as unknown as MouseEvent);
    return event;
  };
  return { documentMock, getSelection, dispatch, remove };
}

async function menuSettled() {
  await vi.waitFor(() => expect(nativeMenu.popup).toHaveBeenCalled());
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetAllMocks();
  nativeMenu.popup.mockResolvedValue(undefined);
  nativeMenu.close.mockResolvedValue(undefined);
});

describe("桌面中文右键菜单", () => {
  it("浏览器保留原有行为，桌面空白区域不弹出 Reload 菜单", () => {
    const browser = setup({ desktop: false });
    expect(browser.documentMock.addEventListener).not.toHaveBeenCalled();
    browser.remove();
    const desktop = setup();
    expect(desktop.dispatch().preventDefault).toHaveBeenCalledOnce();
    expect(nativeMenu.new).not.toHaveBeenCalled();
    desktop.remove();
  });

  it("编辑菜单使用中文原生操作并缓存，弹出后保留资源直到卸载", async () => {
    const app = setup();
    const editor = new FakeElement("TEXTAREA");
    const event = app.dispatch(editor, 48, 72);
    await menuSettled();
    expect(event.preventDefault).toHaveBeenCalledOnce();
    expect(nativeMenu.new.mock.calls[0][0].items).toEqual([
      { item: "Undo", text: "撤销" },
      { item: "Redo", text: "重做" },
      { item: "Separator" },
      { item: "Cut", text: "剪切" },
      { item: "Copy", text: "复制" },
      { item: "Paste", text: "粘贴" },
      { item: "SelectAll", text: "全选" },
    ]);
    expect(nativeMenu.popup.mock.calls[0][0]).toMatchObject({ x: 48, y: 72 });
    expect(nativeMenu.close).not.toHaveBeenCalled();
    app.dispatch(editor);
    await vi.waitFor(() => expect(nativeMenu.popup).toHaveBeenCalledTimes(2));
    expect(nativeMenu.new).toHaveBeenCalledOnce();
    app.remove();
    await vi.waitFor(() => expect(nativeMenu.close).toHaveBeenCalledOnce());
    expect(app.documentMock.removeEventListener).toHaveBeenCalledWith("contextmenu", expect.any(Function), true);
  });

  it.each(["Win32", "Linux x86_64"])("在 %s 不提供平台不支持的撤销和重做", async (platform) => {
    const app = setup({ platform });
    app.dispatch(new FakeElement("INPUT"));
    await menuSettled();
    expect(nativeMenu.new.mock.calls[0][0].items.map((item: { item: string }) => item.item))
      .toEqual(["Cut", "Copy", "Paste", "SelectAll"]);
    app.remove();
  });

  it("只读文本只允许复制和全选，密码不提供复制或剪切", async () => {
    const app = setup();
    app.dispatch(new FakeElement("INPUT", { readonly: "" }));
    await menuSettled();
    expect(nativeMenu.new.mock.calls[0][0].items).toEqual([
      { item: "Copy", text: "复制" },
      { item: "SelectAll", text: "全选" },
    ]);
    app.dispatch(new FakeElement("INPUT", { type: "password" }));
    await vi.waitFor(() => expect(nativeMenu.popup).toHaveBeenCalledTimes(2));
    expect(nativeMenu.new.mock.calls[1][0].items.map((item: { item: string }) => item.item))
      .toEqual(["Undo", "Redo", "Separator", "Paste", "SelectAll"]);
    app.remove();
  });

  it("禁用和非文本控件不允许编辑，contenteditable 子元素支持编辑", async () => {
    const app = setup();
    const nonEditableInputs: Record<string, string>[] = [
      { disabled: "" },
      { type: "checkbox" },
      { type: "password", readonly: "" },
    ];
    for (const attributes of nonEditableInputs)
      expect(app.dispatch(new FakeElement("INPUT", attributes)).preventDefault).toHaveBeenCalledOnce();
    expect(nativeMenu.new).not.toHaveBeenCalled();
    app.dispatch(new FakeElement("SPAN", {}, new FakeElement("DIV", { contenteditable: "plaintext-only" })));
    await menuSettled();
    expect(nativeMenu.new.mock.calls[0][0].items).toContainEqual({ item: "Paste", text: "粘贴" });
    app.remove();
  });

  it("正文只有右键位置落在当前选区内才提供复制，不改动选区", async () => {
    const app = setup();
    const range = { getClientRects: vi.fn(() => [{ left: 10, top: 10, right: 40, bottom: 40 }]) };
    const selection = { isCollapsed: false, rangeCount: 1, getRangeAt: vi.fn(() => range) };
    app.getSelection.mockReturnValue(selection);
    app.dispatch(new FakeElement(), 60, 60);
    expect(nativeMenu.new).not.toHaveBeenCalled();
    app.dispatch();
    await menuSettled();
    expect(nativeMenu.new.mock.calls[0][0].items).toEqual([
      { item: "Copy", text: "复制" },
      { item: "SelectAll", text: "全选" },
    ]);
    app.remove();
  });

  it("卸载时释放仍在创建的菜单，避免异步弹出和资源泄漏", async () => {
    const app = setup();
    let resolveMenu!: (menu: { popup: typeof nativeMenu.popup; close: typeof nativeMenu.close }) => void;
    nativeMenu.new.mockImplementationOnce(() => new Promise((resolve) => { resolveMenu = resolve; }));
    app.dispatch(new FakeElement("TEXTAREA"));
    await vi.waitFor(() => expect(nativeMenu.new).toHaveBeenCalledOnce());
    app.remove();
    resolveMenu({ popup: nativeMenu.popup, close: nativeMenu.close });
    await vi.waitFor(() => expect(nativeMenu.close).toHaveBeenCalledOnce());
    expect(nativeMenu.popup).not.toHaveBeenCalled();
  });

  it("创建失败仍阻止英文菜单，下次右键能够重新创建", async () => {
    const app = setup();
    const report = vi.spyOn(console, "error").mockImplementation(() => {});
    nativeMenu.new.mockRejectedValueOnce(new Error("permission denied"));
    expect(app.dispatch(new FakeElement("TEXTAREA")).preventDefault).toHaveBeenCalledOnce();
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith("无法打开中文编辑菜单，请使用键盘快捷键。"));
    app.dispatch(new FakeElement("TEXTAREA"));
    await menuSettled();
    expect(nativeMenu.new).toHaveBeenCalledTimes(2);
    app.remove();
    report.mockRestore();
  });
});
