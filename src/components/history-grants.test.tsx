// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { QqBindingCard } from "./qq-settings";
import { GatewayBindingCard } from "./gateway-settings";
import type { QqBinding } from "../qq-client";
import type { GatewayBinding } from "../gateway-client";
const A = "11111111-1111-4111-8111-111111111111", B = "22222222-2222-4222-8222-222222222222";
const sessions = [{ id: A, title: "会话 A" }, { id: B, title: "会话 B" }];
const qq: QqBinding = { principal_id: A, identity: { app_id: "app", sender_open_id: "user", kind: "group", target_id: "group" }, revision: 1, status: "pending", grant: null, expires_at_ms: Date.now() + 600_000, created_at_ms: 0, updated_at_ms: 0 };
const gateway: GatewayBinding = { principal_id: A, device_id: A, config_revision: 1, revision: 1, status: "pending", label: "fixture browser", pairing_id: A, grant: null, expires_at_ms: Date.now() + 600_000, created_at_ms: 0, updated_at_ms: 0 };
let container: HTMLDivElement, root: Root;
beforeEach(() => { (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true; container = document.createElement("div"); document.body.append(container); root = createRoot(container); });
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });
function checkbox(prefix: string): HTMLInputElement {
  const label = [...container.querySelectorAll("label")].find(label => label.textContent?.startsWith(prefix));
  if (!label) throw new Error(`missing label ${prefix}`);
  return label.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
}
it.each(["qq", "gateway"] as const)("%s授权固定ID离窗后显式可见并fail closed，恢复也需重新确认", async kind => {
  const submit = vi.fn(async (_command: unknown) => undefined);
  const render = (values: typeof sessions) => root.render(kind === "qq" ? <QqBindingCard binding={qq} sessions={values} executor="native" disabled={false} onSubmit={submit} /> : <GatewayBindingCard binding={gateway} sessions={values} executor="native" disabled={false} onSubmit={submit} />);
  await act(async () => render(sessions));
  await act(async () => checkbox("会话 A").click()); await act(async () => checkbox("查看").click()); await act(async () => checkbox("我已核对").click());
  const confirm = () => [...container.querySelectorAll("button")].find(button => button.textContent === (kind === "qq" ? "确认此身份与范围" : "确认这次浏览器授权"))!;
  expect(confirm().disabled).toBe(false);
  await act(async () => render([sessions[1]]));
  expect(container.querySelector('[aria-label="已选固定会话范围"]')?.textContent).toContain(A);
  expect(container.textContent).toContain("未载入当前窗口"); expect(container.textContent).toContain("未删除或改选");
  expect(checkbox("我已核对").checked).toBe(false); expect(checkbox("我已核对").disabled).toBe(true); expect(confirm().disabled).toBe(true);
  await act(async () => confirm().click()); expect(submit).not.toHaveBeenCalled();
  await act(async () => render(sessions)); expect(checkbox("我已核对").checked).toBe(false); expect(confirm().disabled).toBe(true);
  await act(async () => checkbox("我已核对").click()); await act(async () => confirm().click());
  expect(submit).toHaveBeenCalledOnce(); expect(submit.mock.calls[0][0]).toMatchObject({ type: "confirm_binding", grant: { session_ids: [A] } });
});
